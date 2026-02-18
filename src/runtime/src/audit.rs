//! Persistent audit log writer.
//!
//! Appends structured `AuditEvent` records to a JSON-lines file
//! with size-based rotation. Provides query support for reading
//! back events with time-range and action filters.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use a3s_box_core::audit::{AuditAction, AuditConfig, AuditEvent, AuditOutcome};
use a3s_box_core::error::{BoxError, Result};

/// Persistent audit log that appends events to a JSON-lines file.
///
/// Thread-safe via internal `Mutex`. Supports size-based rotation.
pub struct AuditLog {
    inner: Mutex<AuditLogInner>,
}

struct AuditLogInner {
    /// Path to the active audit log file.
    path: PathBuf,
    /// Current file handle (lazy-opened on first write).
    file: Option<File>,
    /// Current file size in bytes.
    current_size: u64,
    /// Configuration.
    config: AuditConfig,
}

impl AuditLog {
    /// Create a new audit log at the given path.
    pub fn new(path: impl Into<PathBuf>, config: AuditConfig) -> Result<Self> {
        let path = path.into();

        // Create parent directory if needed
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                BoxError::Other(format!(
                    "Failed to create audit log directory {}: {}",
                    parent.display(),
                    e
                ))
            })?;
        }

        // Get current file size if it exists
        let current_size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);

        Ok(Self {
            inner: Mutex::new(AuditLogInner {
                path,
                file: None,
                current_size,
                config,
            }),
        })
    }

    /// Open the audit log at the default path (~/.a3s/audit/audit.jsonl).
    pub fn default_path() -> Result<Self> {
        let path = dirs::home_dir()
            .map(|h| h.join(".a3s").join("audit").join("audit.jsonl"))
            .unwrap_or_else(|| PathBuf::from(".a3s/audit/audit.jsonl"));
        Self::new(path, AuditConfig::default())
    }

    /// Append an audit event to the log.
    pub fn log(&self, event: &AuditEvent) -> Result<()> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| BoxError::Other("Audit log lock poisoned".to_string()))?;

        if !inner.config.enabled {
            return Ok(());
        }

        // Rotate if needed
        if inner.current_size >= inner.config.max_size {
            Self::rotate(&mut inner)?;
        }

        // Open file if not yet open
        if inner.file.is_none() {
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&inner.path)
                .map_err(|e| {
                    BoxError::Other(format!(
                        "Failed to open audit log {}: {}",
                        inner.path.display(),
                        e
                    ))
                })?;
            inner.file = Some(file);
        }

        // Serialize and write
        let mut line = serde_json::to_string(event)
            .map_err(|e| BoxError::Other(format!("Failed to serialize audit event: {}", e)))?;
        line.push('\n');

        let bytes = line.as_bytes();
        if let Some(ref mut file) = inner.file {
            file.write_all(bytes)
                .map_err(|e| BoxError::Other(format!("Failed to write audit event: {}", e)))?;
            file.flush()
                .map_err(|e| BoxError::Other(format!("Failed to flush audit log: {}", e)))?;
        }

        inner.current_size += bytes.len() as u64;
        Ok(())
    }

    /// Rotate the audit log file.
    fn rotate(inner: &mut AuditLogInner) -> Result<()> {
        // Close current file
        inner.file = None;

        let max = inner.config.max_files;
        let base = &inner.path;

        // Remove oldest if at limit
        let oldest = rotated_path(base, max);
        if oldest.exists() {
            let _ = fs::remove_file(&oldest);
        }

        // Shift existing rotated files: .9 → .10, .8 → .9, etc.
        for i in (1..max).rev() {
            let from = rotated_path(base, i);
            let to = rotated_path(base, i + 1);
            if from.exists() {
                let _ = fs::rename(&from, &to);
            }
        }

        // Rename current to .1
        if base.exists() {
            let _ = fs::rename(base, rotated_path(base, 1));
        }

        inner.current_size = 0;
        Ok(())
    }

    /// Get the path to the audit log file.
    pub fn path(&self) -> PathBuf {
        self.inner
            .lock()
            .map(|inner| inner.path.clone())
            .unwrap_or_default()
    }
}

/// Generate a rotated file path (e.g., audit.jsonl.1, audit.jsonl.2).
fn rotated_path(base: &Path, index: u32) -> PathBuf {
    let name = format!(
        "{}.{}",
        base.file_name().unwrap_or_default().to_string_lossy(),
        index
    );
    base.with_file_name(name)
}

/// Query parameters for reading audit events.
#[derive(Debug, Clone, Default)]
pub struct AuditQuery {
    /// Filter by action type.
    pub action: Option<AuditAction>,
    /// Filter by box ID.
    pub box_id: Option<String>,
    /// Filter by outcome.
    pub outcome: Option<AuditOutcome>,
    /// Only events after this time.
    pub since: Option<chrono::DateTime<chrono::Utc>>,
    /// Only events before this time.
    pub until: Option<chrono::DateTime<chrono::Utc>>,
    /// Maximum number of events to return.
    pub limit: Option<usize>,
}

/// Read audit events from a log file, applying optional filters.
pub fn read_audit_log(path: &Path, query: &AuditQuery) -> Result<Vec<AuditEvent>> {
    if !path.exists() {
        return Ok(vec![]);
    }

    let file = File::open(path).map_err(|e| {
        BoxError::Other(format!(
            "Failed to open audit log {}: {}",
            path.display(),
            e
        ))
    })?;

    let reader = BufReader::new(file);
    let mut events = Vec::new();

    for line in reader.lines() {
        let line =
            line.map_err(|e| BoxError::Other(format!("Failed to read audit log line: {}", e)))?;

        if line.trim().is_empty() {
            continue;
        }

        let event: AuditEvent = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(_) => continue, // skip malformed lines
        };

        // Apply filters
        if let Some(ref action) = query.action {
            if event.action != *action {
                continue;
            }
        }
        if let Some(ref box_id) = query.box_id {
            if event.box_id.as_deref() != Some(box_id.as_str()) {
                continue;
            }
        }
        if let Some(ref outcome) = query.outcome {
            if event.outcome != *outcome {
                continue;
            }
        }
        if let Some(since) = query.since {
            if event.timestamp < since {
                continue;
            }
        }
        if let Some(until) = query.until {
            if event.timestamp > until {
                continue;
            }
        }

        events.push(event);

        if let Some(limit) = query.limit {
            if events.len() >= limit {
                break;
            }
        }
    }

    Ok(events)
}

impl a3s_box_core::traits::AuditSink for AuditLog {
    fn record(&self, event: &AuditEvent) -> Result<()> {
        self.log(event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_log(dir: &Path) -> AuditLog {
        let path = dir.join("audit.jsonl");
        AuditLog::new(path, AuditConfig::default()).unwrap()
    }

    #[test]
    fn test_audit_log_write_and_read() {
        let dir = TempDir::new().unwrap();
        let log = test_log(dir.path());

        let event = AuditEvent::new(AuditAction::BoxCreate, AuditOutcome::Success)
            .with_box_id("box-1")
            .with_message("Created box");

        log.log(&event).unwrap();

        let events = read_audit_log(&log.path(), &AuditQuery::default()).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].action, AuditAction::BoxCreate);
        assert_eq!(events[0].box_id, Some("box-1".to_string()));
    }

    #[test]
    fn test_audit_log_multiple_events() {
        let dir = TempDir::new().unwrap();
        let log = test_log(dir.path());

        for i in 0..5 {
            let event = AuditEvent::new(AuditAction::ExecCommand, AuditOutcome::Success)
                .with_box_id(format!("box-{}", i));
            log.log(&event).unwrap();
        }

        let events = read_audit_log(&log.path(), &AuditQuery::default()).unwrap();
        assert_eq!(events.len(), 5);
    }

    #[test]
    fn test_audit_log_filter_by_action() {
        let dir = TempDir::new().unwrap();
        let log = test_log(dir.path());

        log.log(&AuditEvent::new(
            AuditAction::BoxCreate,
            AuditOutcome::Success,
        ))
        .unwrap();
        log.log(&AuditEvent::new(
            AuditAction::BoxStop,
            AuditOutcome::Success,
        ))
        .unwrap();
        log.log(&AuditEvent::new(
            AuditAction::BoxCreate,
            AuditOutcome::Failure,
        ))
        .unwrap();

        let query = AuditQuery {
            action: Some(AuditAction::BoxCreate),
            ..Default::default()
        };
        let events = read_audit_log(&log.path(), &query).unwrap();
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn test_audit_log_filter_by_box_id() {
        let dir = TempDir::new().unwrap();
        let log = test_log(dir.path());

        log.log(&AuditEvent::new(AuditAction::BoxCreate, AuditOutcome::Success).with_box_id("a"))
            .unwrap();
        log.log(&AuditEvent::new(AuditAction::BoxCreate, AuditOutcome::Success).with_box_id("b"))
            .unwrap();

        let query = AuditQuery {
            box_id: Some("a".to_string()),
            ..Default::default()
        };
        let events = read_audit_log(&log.path(), &query).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].box_id, Some("a".to_string()));
    }

    #[test]
    fn test_audit_log_filter_by_outcome() {
        let dir = TempDir::new().unwrap();
        let log = test_log(dir.path());

        log.log(&AuditEvent::new(
            AuditAction::ImagePull,
            AuditOutcome::Success,
        ))
        .unwrap();
        log.log(&AuditEvent::new(
            AuditAction::ImagePull,
            AuditOutcome::Failure,
        ))
        .unwrap();
        log.log(&AuditEvent::new(
            AuditAction::ImagePull,
            AuditOutcome::Denied,
        ))
        .unwrap();

        let query = AuditQuery {
            outcome: Some(AuditOutcome::Failure),
            ..Default::default()
        };
        let events = read_audit_log(&log.path(), &query).unwrap();
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn test_audit_log_limit() {
        let dir = TempDir::new().unwrap();
        let log = test_log(dir.path());

        for _ in 0..10 {
            log.log(&AuditEvent::new(
                AuditAction::ExecCommand,
                AuditOutcome::Success,
            ))
            .unwrap();
        }

        let query = AuditQuery {
            limit: Some(3),
            ..Default::default()
        };
        let events = read_audit_log(&log.path(), &query).unwrap();
        assert_eq!(events.len(), 3);
    }

    #[test]
    fn test_audit_log_empty_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nonexistent.jsonl");
        let events = read_audit_log(&path, &AuditQuery::default()).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn test_audit_log_disabled() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("audit.jsonl");
        let config = AuditConfig {
            enabled: false,
            ..Default::default()
        };
        let log = AuditLog::new(&path, config).unwrap();

        log.log(&AuditEvent::new(
            AuditAction::BoxCreate,
            AuditOutcome::Success,
        ))
        .unwrap();

        // File should not exist or be empty
        let events = read_audit_log(&path, &AuditQuery::default()).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn test_audit_log_rotation() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("audit.jsonl");
        let config = AuditConfig {
            enabled: true,
            max_size: 100, // very small to trigger rotation
            max_files: 3,
        };
        let log = AuditLog::new(&path, config).unwrap();

        // Write enough events to trigger rotation
        for i in 0..20 {
            let event = AuditEvent::new(AuditAction::ExecCommand, AuditOutcome::Success)
                .with_message(format!("Event {}", i));
            log.log(&event).unwrap();
        }

        // Should have rotated files
        assert!(path.exists());
        let rotated_1 = dir.path().join("audit.jsonl.1");
        assert!(rotated_1.exists());
    }

    #[test]
    fn test_rotated_path() {
        let base = PathBuf::from("/var/log/audit.jsonl");
        assert_eq!(
            rotated_path(&base, 1),
            PathBuf::from("/var/log/audit.jsonl.1")
        );
        assert_eq!(
            rotated_path(&base, 10),
            PathBuf::from("/var/log/audit.jsonl.10")
        );
    }

    #[test]
    fn test_audit_log_path() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("audit.jsonl");
        let log = AuditLog::new(&path, AuditConfig::default()).unwrap();
        assert_eq!(log.path(), path);
    }
}
