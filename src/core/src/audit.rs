//! Audit logging types for security-relevant event tracking.
//!
//! Provides structured audit events that capture who did what, when,
//! and with what outcome. Designed for compliance and forensic analysis.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A structured audit event capturing a security-relevant action.
///
/// Follows the W7 model: Who, What, When, Where, Why, hoW, outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    /// Unique event ID.
    pub id: String,

    /// ISO 8601 timestamp.
    pub timestamp: chrono::DateTime<chrono::Utc>,

    /// Action category.
    pub action: AuditAction,

    /// Target box ID (if applicable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub box_id: Option<String>,

    /// Actor who initiated the action (e.g., "cli", "sdk", "cri", "system").
    #[serde(default)]
    pub actor: String,

    /// Outcome of the action.
    pub outcome: AuditOutcome,

    /// Human-readable description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,

    /// Additional structured metadata.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, serde_json::Value>,
}

/// Categories of auditable actions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditAction {
    // Box lifecycle
    BoxCreate,
    BoxStart,
    BoxStop,
    BoxDestroy,
    BoxRestart,

    // Execution
    ExecCommand,
    ExecAttach,

    // Image operations
    ImagePull,
    ImagePush,
    ImageBuild,
    ImageDelete,

    // Network operations
    NetworkCreate,
    NetworkDelete,
    NetworkConnect,
    NetworkDisconnect,

    // Volume operations
    VolumeCreate,
    VolumeDelete,

    // Security events
    SignatureVerify,
    AttestationVerify,
    SecretInject,
    SealData,
    UnsealData,

    // Authentication
    RegistryLogin,
    RegistryLogout,

    // System
    SystemPrune,
    ConfigChange,
}

/// Outcome of an audited action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditOutcome {
    /// Action completed successfully.
    Success,
    /// Action failed.
    Failure,
    /// Action was denied (e.g., signature verification failed).
    Denied,
}

impl AuditEvent {
    /// Create a new audit event.
    pub fn new(action: AuditAction, outcome: AuditOutcome) -> Self {
        let timestamp = chrono::Utc::now();
        // Generate a unique ID from timestamp nanos (no uuid dependency needed)
        let id = format!("audit-{}", timestamp.timestamp_nanos_opt().unwrap_or(0));
        Self {
            id,
            timestamp,
            action,
            box_id: None,
            actor: "cli".to_string(),
            outcome,
            message: None,
            metadata: HashMap::new(),
        }
    }

    /// Set the box ID.
    pub fn with_box_id(mut self, box_id: impl Into<String>) -> Self {
        self.box_id = Some(box_id.into());
        self
    }

    /// Set the actor.
    pub fn with_actor(mut self, actor: impl Into<String>) -> Self {
        self.actor = actor.into();
        self
    }

    /// Set a human-readable message.
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    /// Add a metadata key-value pair.
    pub fn with_metadata(
        mut self,
        key: impl Into<String>,
        value: impl Into<serde_json::Value>,
    ) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }
}

/// Configuration for the audit log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditConfig {
    /// Enable audit logging (default: true).
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Maximum audit log file size in bytes before rotation (default: 50 MB).
    #[serde(default = "default_max_size")]
    pub max_size: u64,

    /// Maximum number of rotated audit log files to keep (default: 10).
    #[serde(default = "default_max_files")]
    pub max_files: u32,
}

fn default_true() -> bool {
    true
}

fn default_max_size() -> u64 {
    50 * 1024 * 1024 // 50 MB
}

fn default_max_files() -> u32 {
    10
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_size: 50 * 1024 * 1024,
            max_files: 10,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_audit_event_new() {
        let event = AuditEvent::new(AuditAction::BoxCreate, AuditOutcome::Success);
        assert_eq!(event.action, AuditAction::BoxCreate);
        assert_eq!(event.outcome, AuditOutcome::Success);
        assert_eq!(event.actor, "cli");
        assert!(event.box_id.is_none());
        assert!(!event.id.is_empty());
    }

    #[test]
    fn test_audit_event_builder() {
        let event = AuditEvent::new(AuditAction::ExecCommand, AuditOutcome::Success)
            .with_box_id("box-123")
            .with_actor("sdk")
            .with_message("Executed /bin/ls")
            .with_metadata("exit_code", serde_json::json!(0));

        assert_eq!(event.box_id, Some("box-123".to_string()));
        assert_eq!(event.actor, "sdk");
        assert_eq!(event.message, Some("Executed /bin/ls".to_string()));
        assert_eq!(event.metadata["exit_code"], serde_json::json!(0));
    }

    #[test]
    fn test_audit_event_serde_roundtrip() {
        let event = AuditEvent::new(AuditAction::ImagePull, AuditOutcome::Success)
            .with_box_id("box-456")
            .with_message("Pulled nginx:latest")
            .with_metadata("image", serde_json::json!("nginx:latest"));

        let json = serde_json::to_string(&event).unwrap();
        let parsed: AuditEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.action, AuditAction::ImagePull);
        assert_eq!(parsed.outcome, AuditOutcome::Success);
        assert_eq!(parsed.box_id, Some("box-456".to_string()));
        assert_eq!(parsed.metadata["image"], serde_json::json!("nginx:latest"));
    }

    #[test]
    fn test_audit_action_serde() {
        let actions = vec![
            AuditAction::BoxCreate,
            AuditAction::BoxStart,
            AuditAction::BoxStop,
            AuditAction::BoxDestroy,
            AuditAction::ExecCommand,
            AuditAction::ImagePull,
            AuditAction::SignatureVerify,
            AuditAction::SecretInject,
        ];
        for action in actions {
            let json = serde_json::to_string(&action).unwrap();
            let parsed: AuditAction = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, action);
        }
    }

    #[test]
    fn test_audit_outcome_serde() {
        let outcomes = vec![
            AuditOutcome::Success,
            AuditOutcome::Failure,
            AuditOutcome::Denied,
        ];
        for outcome in outcomes {
            let json = serde_json::to_string(&outcome).unwrap();
            let parsed: AuditOutcome = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, outcome);
        }
    }

    #[test]
    fn test_audit_config_default() {
        let config = AuditConfig::default();
        assert!(config.enabled);
        assert_eq!(config.max_size, 50 * 1024 * 1024);
        assert_eq!(config.max_files, 10);
    }

    #[test]
    fn test_audit_config_serde() {
        let config = AuditConfig {
            enabled: false,
            max_size: 100 * 1024 * 1024,
            max_files: 5,
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: AuditConfig = serde_json::from_str(&json).unwrap();
        assert!(!parsed.enabled);
        assert_eq!(parsed.max_size, 100 * 1024 * 1024);
        assert_eq!(parsed.max_files, 5);
    }

    #[test]
    fn test_audit_event_empty_metadata_skipped() {
        let event = AuditEvent::new(AuditAction::BoxStop, AuditOutcome::Success);
        let json = serde_json::to_string(&event).unwrap();
        // Empty metadata should not appear in JSON
        assert!(!json.contains("metadata"));
    }

    #[test]
    fn test_audit_event_none_fields_skipped() {
        let event = AuditEvent::new(AuditAction::SystemPrune, AuditOutcome::Success);
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("box_id"));
        assert!(!json.contains("message"));
    }

    #[test]
    fn test_audit_action_all_variants() {
        // Ensure all variants serialize to snake_case
        let variants = vec![
            (AuditAction::BoxCreate, "\"box_create\""),
            (AuditAction::BoxStart, "\"box_start\""),
            (AuditAction::BoxStop, "\"box_stop\""),
            (AuditAction::BoxDestroy, "\"box_destroy\""),
            (AuditAction::BoxRestart, "\"box_restart\""),
            (AuditAction::ExecCommand, "\"exec_command\""),
            (AuditAction::ExecAttach, "\"exec_attach\""),
            (AuditAction::ImagePull, "\"image_pull\""),
            (AuditAction::ImagePush, "\"image_push\""),
            (AuditAction::ImageBuild, "\"image_build\""),
            (AuditAction::ImageDelete, "\"image_delete\""),
            (AuditAction::NetworkCreate, "\"network_create\""),
            (AuditAction::NetworkDelete, "\"network_delete\""),
            (AuditAction::NetworkConnect, "\"network_connect\""),
            (AuditAction::NetworkDisconnect, "\"network_disconnect\""),
            (AuditAction::VolumeCreate, "\"volume_create\""),
            (AuditAction::VolumeDelete, "\"volume_delete\""),
            (AuditAction::SignatureVerify, "\"signature_verify\""),
            (AuditAction::AttestationVerify, "\"attestation_verify\""),
            (AuditAction::SecretInject, "\"secret_inject\""),
            (AuditAction::SealData, "\"seal_data\""),
            (AuditAction::UnsealData, "\"unseal_data\""),
            (AuditAction::RegistryLogin, "\"registry_login\""),
            (AuditAction::RegistryLogout, "\"registry_logout\""),
            (AuditAction::SystemPrune, "\"system_prune\""),
            (AuditAction::ConfigChange, "\"config_change\""),
        ];
        for (action, expected) in variants {
            let json = serde_json::to_string(&action).unwrap();
            assert_eq!(json, expected, "Failed for {:?}", action);
        }
    }
}
