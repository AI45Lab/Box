//! Logging driver types and configuration.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Logging driver type.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LogDriver {
    /// Docker-compatible JSON lines format (default).
    #[default]
    JsonFile,
    /// Forward logs to a syslog endpoint.
    ///
    /// Options:
    /// - `syslog-address`: UDP/TCP address (e.g., "udp://localhost:514")
    /// - `syslog-facility`: Syslog facility (default: "daemon")
    /// - `tag`: Log tag template (default: box name)
    Syslog,
    /// Disable logging entirely.
    None,
}

impl std::fmt::Display for LogDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::JsonFile => write!(f, "json-file"),
            Self::Syslog => write!(f, "syslog"),
            Self::None => write!(f, "none"),
        }
    }
}

impl std::str::FromStr for LogDriver {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "json-file" => Ok(Self::JsonFile),
            "syslog" => Ok(Self::Syslog),
            "none" => Ok(Self::None),
            _ => Err(format!(
                "unknown log driver: '{}' (supported: json-file, syslog, none)",
                s
            )),
        }
    }
}

/// Logging configuration for a box.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogConfig {
    pub driver: LogDriver,
    #[serde(default)]
    pub options: HashMap<String, String>,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            driver: LogDriver::JsonFile,
            options: HashMap::new(),
        }
    }
}

impl LogConfig {
    /// Maximum log file size in bytes before rotation.
    /// Default: 10 MiB. Set via `max-size` option (e.g., "10m", "1g").
    pub fn max_size(&self) -> u64 {
        self.options
            .get("max-size")
            .and_then(|s| parse_size(s).ok())
            .unwrap_or(10 * 1024 * 1024)
    }

    /// Maximum number of rotated log files to keep.
    /// Default: 3. Set via `max-file` option.
    pub fn max_file(&self) -> u32 {
        self.options
            .get("max-file")
            .and_then(|s| s.parse().ok())
            .unwrap_or(3)
    }

    /// Syslog address (e.g., "udp://localhost:514").
    /// Only relevant when driver is `Syslog`.
    pub fn syslog_address(&self) -> &str {
        self.options
            .get("syslog-address")
            .map(|s| s.as_str())
            .unwrap_or("udp://localhost:514")
    }

    /// Syslog facility (e.g., "daemon", "local0").
    /// Only relevant when driver is `Syslog`.
    pub fn syslog_facility(&self) -> &str {
        self.options
            .get("syslog-facility")
            .map(|s| s.as_str())
            .unwrap_or("daemon")
    }

    /// Log tag (used by syslog driver as the program name).
    pub fn tag(&self) -> Option<&str> {
        self.options.get("tag").map(|s| s.as_str())
    }
}

/// A single structured log entry (Docker-compatible JSON format).
#[derive(Debug, Serialize, Deserialize)]
pub struct LogEntry {
    /// The log message (including trailing newline).
    pub log: String,
    /// The output stream: "stdout" or "stderr".
    pub stream: String,
    /// RFC 3339 timestamp with nanosecond precision.
    pub time: String,
}

/// Parse a human-readable size string (e.g., "10m", "1g", "4096") into bytes.
fn parse_size(s: &str) -> std::result::Result<u64, String> {
    let s = s.trim().to_lowercase();
    if let Ok(n) = s.parse::<u64>() {
        return Ok(n);
    }
    let (num, mult) = if s.ends_with("gb") || s.ends_with('g') {
        (
            s.trim_end_matches("gb").trim_end_matches('g'),
            1024u64 * 1024 * 1024,
        )
    } else if s.ends_with("mb") || s.ends_with('m') {
        (
            s.trim_end_matches("mb").trim_end_matches('m'),
            1024u64 * 1024,
        )
    } else if s.ends_with("kb") || s.ends_with('k') {
        (s.trim_end_matches("kb").trim_end_matches('k'), 1024u64)
    } else if s.ends_with('b') {
        (s.trim_end_matches('b'), 1u64)
    } else {
        return Err(format!("unrecognized size format: {s}"));
    };
    let n: u64 = num.parse().map_err(|_| format!("invalid number: {num}"))?;
    Ok(n * mult)
}

// ===========================================================================
// Log processor — tails the VM console (`console.log`) and produces structured
// Docker-compatible output (`container.json`) or forwards to syslog.
//
// This runs in the SHIM (the box's own per-process lifetime), not the ephemeral
// CLI: the CLI exits on `run -d` detach, which would kill an in-CLI processor
// and truncate the logs. The shim writes `console.log` and lives exactly as
// long as the VM, so it is the correct, daemonless home (like containerd-shim).
// ===========================================================================

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// Path to the structured JSON log file inside a box's log dir.
pub fn json_log_path(log_dir: &Path) -> PathBuf {
    log_dir.join("container.json")
}

/// True for console lines that are VM/runtime boot internals, not container
/// output — libkrun's C-init preamble (`init.krun: ...`), printed before
/// `/sbin/init` (guest-init) takes over. guest-init's own tracing goes to
/// `/dev/kmsg`, so this is the only remaining non-container source on the
/// console.
pub fn is_runtime_console_noise(line: &str) -> bool {
    line.starts_with("init.krun:")
}

/// Read the next COMPLETE line from a tailed `console.log`, returning it without
/// the trailing newline. Polls on EOF like `tail -f` (so lines a container logs
/// after a quiet period are not dropped), accumulating a partial line across
/// reads. Returns `None` only when `stop` is set AND EOF is reached — i.e. the
/// VM has exited and `console.log` is fully drained — flushing any final partial
/// line as the last value before the subsequent `None`.
fn tail_next_line(
    reader: &mut impl BufRead,
    buf: &mut String,
    stop: &AtomicBool,
) -> Option<String> {
    loop {
        match reader.read_line(buf) {
            Ok(0) | Err(_) => {
                if stop.load(Ordering::Relaxed) {
                    // VM exited and we are at EOF: flush a trailing partial line
                    // (no newline) once, then signal completion.
                    if buf.is_empty() {
                        return None;
                    }
                    let line = std::mem::take(buf);
                    return Some(line.trim_end_matches(['\n', '\r']).to_string());
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
            Ok(_) => {}
        }
        if !buf.ends_with('\n') {
            // Partial line at EOF — keep it buffered and wait for the rest.
            continue;
        }
        let line = std::mem::take(buf);
        return Some(line.trim_end_matches(['\n', '\r']).to_string());
    }
}

/// Run the log processor for a box, blocking until `stop` is set and the console
/// is drained. Intended to run on a dedicated thread for the VM's lifetime; set
/// `stop` after the VM exits, then join, to guarantee the final lines are
/// captured (no teardown race).
pub fn run_log_processor(
    console_log: &Path,
    log_dir: &Path,
    config: &LogConfig,
    stop: &AtomicBool,
) {
    match config.driver {
        LogDriver::None => {}
        LogDriver::JsonFile => run_json_file_processor(
            console_log,
            log_dir,
            config.max_size(),
            config.max_file(),
            stop,
        ),
        LogDriver::Syslog => run_syslog_processor(
            console_log,
            config.syslog_address(),
            config.syslog_facility(),
            config.tag().unwrap_or("a3s-box"),
            stop,
        ),
    }
}

/// Wait (bounded) for `console.log` to appear, then open it. Returns `None` if it
/// never shows up or `stop` fires first.
fn open_console(console_log: &Path, stop: &AtomicBool) -> Option<std::fs::File> {
    for _ in 0..300 {
        if console_log.exists() {
            break;
        }
        if stop.load(Ordering::Relaxed) && !console_log.exists() {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    std::fs::File::open(console_log).ok()
}

/// Tail console.log and write one Docker-style JSON record per container line.
/// The stderr companion to `console.log` (libkrun's 3-fd console sends guest
/// stderr here, stdout to `console.log`).
pub fn stderr_console_path(console_log: &Path) -> PathBuf {
    console_log.with_file_name("console.err.log")
}

/// Tail one console file, emitting each container line via `emit(line, stream)`.
/// `filter_noise` drops libkrun's `init.krun:` preamble (only on the stdout
/// console). Blocks until `stop` is set and the file is drained.
fn run_tagged_tail(
    file: &Path,
    stream: &str,
    filter_noise: bool,
    stop: &AtomicBool,
    emit: &(dyn Fn(&str, &str) + Sync),
) {
    let f = match open_console(file, stop) {
        Some(f) => f,
        None => return,
    };
    let mut reader = BufReader::new(f);
    let mut buf = String::new();
    while let Some(line) = tail_next_line(&mut reader, &mut buf, stop) {
        if filter_noise && is_runtime_console_noise(&line) {
            continue;
        }
        emit(&line, stream);
    }
}

fn run_json_file_processor(
    console_log: &Path,
    log_dir: &Path,
    max_size: u64,
    max_file: u32,
    stop: &AtomicBool,
) {
    let json_path = json_log_path(log_dir);
    let writer = std::sync::Mutex::new(match RotatingWriter::new(&json_path, max_size, max_file) {
        Ok(w) => w,
        Err(_) => return,
    });
    let err_log = stderr_console_path(console_log);

    // Write one tagged JSON record per line; shared by the stdout and stderr
    // tail threads (the Mutex serializes their interleave into container.json).
    let emit = |line: &str, stream: &str| {
        let entry = LogEntry {
            log: format!("{line}\n"),
            stream: stream.to_string(),
            time: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        };
        if let Ok(json) = serde_json::to_string(&entry) {
            if let Ok(mut w) = writer.lock() {
                let _ = w.write_line(&json);
            }
        }
    };
    let emit: &(dyn Fn(&str, &str) + Sync) = &emit;

    std::thread::scope(|s| {
        s.spawn(|| run_tagged_tail(console_log, "stdout", true, stop, emit));
        // libkrun's `init.krun:` preamble can land on EITHER stream, so filter
        // the noise on stderr too.
        s.spawn(|| run_tagged_tail(&err_log, "stderr", true, stop, emit));
    });
}

/// Forward both console streams (stdout + stderr) to a syslog endpoint.
fn run_syslog_processor(
    console_log: &Path,
    address: &str,
    _facility: &str,
    tag: &str,
    stop: &AtomicBool,
) {
    use std::net::UdpSocket;

    let (proto, addr) = if let Some(rest) = address.strip_prefix("udp://") {
        ("udp", rest)
    } else if let Some(rest) = address.strip_prefix("tcp://") {
        ("tcp", rest)
    } else {
        ("udp", address)
    };
    let err_log = stderr_console_path(console_log);

    match proto {
        "udp" => {
            let socket = match UdpSocket::bind("0.0.0.0:0") {
                Ok(s) => s,
                Err(_) => return,
            };
            // RFC 3164: <priority>tag: message; daemon(3)*8 + info(6) = 30.
            let emit = |line: &str, _stream: &str| {
                let msg = format!("<30>{tag}: {line}");
                let _ = socket.send_to(msg.as_bytes(), addr);
            };
            let emit: &(dyn Fn(&str, &str) + Sync) = &emit;
            std::thread::scope(|s| {
                s.spawn(|| run_tagged_tail(console_log, "stdout", true, stop, emit));
                s.spawn(|| run_tagged_tail(&err_log, "stderr", true, stop, emit));
            });
        }
        "tcp" => {
            let stream = match std::net::TcpStream::connect(addr) {
                Ok(s) => std::sync::Mutex::new(s),
                Err(_) => return,
            };
            let emit = |line: &str, _stream: &str| {
                let msg = format!("<30>{tag}: {line}\n");
                if let Ok(mut s) = stream.lock() {
                    if s.write_all(msg.as_bytes()).is_err() {
                        if let Ok(news) = std::net::TcpStream::connect(addr) {
                            *s = news;
                            let _ = s.write_all(msg.as_bytes());
                        }
                    }
                }
            };
            let emit: &(dyn Fn(&str, &str) + Sync) = &emit;
            std::thread::scope(|sc| {
                sc.spawn(|| run_tagged_tail(console_log, "stdout", true, stop, emit));
                sc.spawn(|| run_tagged_tail(&err_log, "stderr", true, stop, emit));
            });
        }
        _ => {}
    }
}

/// A file writer that rotates (and gzips) when the file exceeds `max_size`.
struct RotatingWriter {
    path: PathBuf,
    file: std::fs::File,
    written: u64,
    max_size: u64,
    max_file: u32,
}

impl RotatingWriter {
    fn new(path: &Path, max_size: u64, max_file: u32) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let written = file.metadata()?.len();
        Ok(Self {
            path: path.to_path_buf(),
            file,
            written,
            max_size,
            max_file,
        })
    }

    fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        let bytes = format!("{line}\n");
        self.file.write_all(bytes.as_bytes())?;
        self.file.flush()?;
        self.written += bytes.len() as u64;
        if self.written >= self.max_size {
            self.rotate()?;
        }
        Ok(())
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        for i in (1..self.max_file).rev() {
            let from = rotated_path(&self.path, i);
            let to = rotated_path(&self.path, i + 1);
            if from.exists() {
                std::fs::rename(&from, &to)?;
            }
        }
        let oldest = rotated_path(&self.path, self.max_file);
        if oldest.exists() {
            std::fs::remove_file(&oldest)?;
        }
        let rotated = rotated_path(&self.path, 1);
        compress_file(&self.path, &rotated)?;
        std::fs::remove_file(&self.path)?;
        self.file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.written = 0;
        Ok(())
    }
}

/// Compress a file with gzip.
fn compress_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Read;

    let mut input = std::fs::File::open(src)?;
    let output = std::fs::File::create(dst)?;
    let mut encoder = GzEncoder::new(output, Compression::fast());
    let mut buf = [0u8; 8192];
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        encoder.write_all(&buf[..n])?;
    }
    encoder.finish()?;
    Ok(())
}

/// Generate a rotated file path: container.json → container.json.1.gz
fn rotated_path(base: &Path, index: u32) -> PathBuf {
    let mut p = base.as_os_str().to_owned();
    p.push(format!(".{index}.gz"));
    PathBuf::from(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_driver_from_str() {
        assert_eq!(
            "json-file".parse::<LogDriver>().unwrap(),
            LogDriver::JsonFile
        );
        assert_eq!("syslog".parse::<LogDriver>().unwrap(), LogDriver::Syslog);
        assert_eq!("none".parse::<LogDriver>().unwrap(), LogDriver::None);
        assert!("unknown".parse::<LogDriver>().is_err());
    }

    #[test]
    fn test_log_config_defaults() {
        let config = LogConfig::default();
        assert_eq!(config.driver, LogDriver::JsonFile);
        assert_eq!(config.max_size(), 10 * 1024 * 1024);
        assert_eq!(config.max_file(), 3);
    }

    #[test]
    fn test_log_config_custom_options() {
        let mut config = LogConfig::default();
        config
            .options
            .insert("max-size".to_string(), "50m".to_string());
        config
            .options
            .insert("max-file".to_string(), "5".to_string());
        assert_eq!(config.max_size(), 50 * 1024 * 1024);
        assert_eq!(config.max_file(), 5);
    }

    #[test]
    fn test_parse_size() {
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert_eq!(parse_size("10m").unwrap(), 10 * 1024 * 1024);
        assert_eq!(parse_size("1g").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size("512k").unwrap(), 512 * 1024);
        assert!(parse_size("abc").is_err());
    }

    #[test]
    fn test_log_entry_serialization() {
        let entry = LogEntry {
            log: "hello\n".to_string(),
            stream: "stdout".to_string(),
            time: "2026-02-12T06:00:00.000000000Z".to_string(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"log\":\"hello\\n\""));
        assert!(json.contains("\"stream\":\"stdout\""));
    }

    #[test]
    fn test_syslog_config_defaults() {
        let config = LogConfig {
            driver: LogDriver::Syslog,
            options: HashMap::new(),
        };
        assert_eq!(config.syslog_address(), "udp://localhost:514");
        assert_eq!(config.syslog_facility(), "daemon");
        assert_eq!(config.tag(), None);
    }

    #[test]
    fn test_syslog_config_custom() {
        let mut options = HashMap::new();
        options.insert(
            "syslog-address".to_string(),
            "tcp://loghost:1514".to_string(),
        );
        options.insert("syslog-facility".to_string(), "local0".to_string());
        options.insert("tag".to_string(), "myapp".to_string());
        let config = LogConfig {
            driver: LogDriver::Syslog,
            options,
        };
        assert_eq!(config.syslog_address(), "tcp://loghost:1514");
        assert_eq!(config.syslog_facility(), "local0");
        assert_eq!(config.tag(), Some("myapp"));
    }

    #[test]
    fn test_log_driver_display() {
        assert_eq!(LogDriver::JsonFile.to_string(), "json-file");
        assert_eq!(LogDriver::Syslog.to_string(), "syslog");
        assert_eq!(LogDriver::None.to_string(), "none");
    }

    #[test]
    fn test_log_driver_serde_roundtrip() {
        let driver = LogDriver::Syslog;
        let json = serde_json::to_string(&driver).unwrap();
        assert_eq!(json, "\"syslog\"");
        let parsed: LogDriver = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, LogDriver::Syslog);
    }

    #[test]
    fn test_tail_next_line_returns_complete_lines() {
        use std::io::Cursor;
        // Two complete lines (CRLF then LF) returned newline-stripped; a third
        // read at EOF with stop=true returns None (VM exited, console drained).
        let mut reader = BufReader::new(Cursor::new(b"alpha\r\nbeta\n".to_vec()));
        let mut buf = String::new();
        let stop = AtomicBool::new(true);
        assert_eq!(
            tail_next_line(&mut reader, &mut buf, &stop),
            Some("alpha".to_string())
        );
        assert_eq!(
            tail_next_line(&mut reader, &mut buf, &stop),
            Some("beta".to_string())
        );
        assert_eq!(tail_next_line(&mut reader, &mut buf, &stop), None);
        assert!(buf.is_empty());
    }

    #[test]
    fn test_tail_next_line_flushes_trailing_partial_on_stop() {
        use std::io::Cursor;
        // A final line without a trailing newline is still flushed once when the
        // VM has exited (stop=true) — no dropped last line.
        let mut reader = BufReader::new(Cursor::new(b"only-partial".to_vec()));
        let mut buf = String::new();
        let stop = AtomicBool::new(true);
        assert_eq!(
            tail_next_line(&mut reader, &mut buf, &stop),
            Some("only-partial".to_string())
        );
        assert_eq!(tail_next_line(&mut reader, &mut buf, &stop), None);
    }

    #[test]
    fn test_is_runtime_console_noise() {
        assert!(is_runtime_console_noise("init.krun: mount_filesystems ok"));
        assert!(!is_runtime_console_noise("L1"));
        assert!(!is_runtime_console_noise(
            "starting app (init.krun: ignored)"
        ));
        assert!(!is_runtime_console_noise(""));
    }

    #[test]
    fn test_run_json_file_processor_captures_all_lines_after_stop() {
        // The processor must emit a record for EVERY console line, then stop
        // cleanly once the VM has exited (stop=true). The original bug dropped
        // every line logged after the first EOF (here: BBB after a quiet line).
        let dir = tempfile::tempdir().unwrap();
        let console = dir.path().join("console.log");
        std::fs::write(&console, "AAA\ninit.krun: noise\nBBB\n").unwrap();
        let stop = AtomicBool::new(true);
        run_json_file_processor(&console, dir.path(), 10 * 1024 * 1024, 3, &stop);
        let json = std::fs::read_to_string(json_log_path(dir.path())).unwrap();
        assert!(json.contains("\"log\":\"AAA\\n\""), "AAA missing: {json}");
        assert!(
            json.contains("\"log\":\"BBB\\n\""),
            "BBB (after a quiet line) missing: {json}"
        );
        assert!(
            !json.contains("init.krun"),
            "runtime noise must be filtered: {json}"
        );
    }

    #[test]
    fn test_rotating_writer_rotates_and_gzips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("container.json");
        let mut w = RotatingWriter::new(&path, 20, 3).unwrap();
        for i in 0..10 {
            w.write_line(&format!("line-{i}")).unwrap();
        }
        assert!(
            rotated_path(&path, 1).exists(),
            "expected a rotated .1.gz file"
        );
    }
}
