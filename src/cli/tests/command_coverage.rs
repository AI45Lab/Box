//! Integration coverage for the a3s-box command surface.
//!
//! The non-ignored tests cover parser entrypoints and commands that operate
//! only on local state. The ignored test exercises the VM-backed command
//! matrix and requires libkrun/HVF or KVM plus registry access.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use flate2::write::GzEncoder;
use flate2::Compression;
use sha2::{Digest, Sha256};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(300);
const TEST_ALPINE_TAR_ENV: &str = "A3S_BOX_TEST_ALPINE_TAR";
const TOP_LEVEL_COMMANDS: &[&str] = &[
    "run",
    "create",
    "start",
    "stop",
    "restart",
    "rm",
    "kill",
    "pause",
    "unpause",
    "ps",
    "stats",
    "logs",
    "exec",
    "top",
    "inspect",
    "attach",
    "attest",
    "audit",
    "seal",
    "unseal",
    "inject-secret",
    "wait",
    "rename",
    "port",
    "export",
    "commit",
    "diff",
    "events",
    "container",
    "container-update",
    "compose",
    "snapshot",
    "build",
    "image",
    "images",
    "pull",
    "push",
    "login",
    "logout",
    "rmi",
    "image-inspect",
    "history",
    "image-prune",
    "tag",
    "save",
    "load",
    "cp",
    "network",
    "volume",
    "df",
    "system",
    "system-prune",
    "version",
    "info",
    "monitor",
    "pool",
    "shell",
    "help",
];

fn find_binary() -> PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_a3s-box") {
        let bin = PathBuf::from(path);
        if bin.exists() {
            return bin;
        }
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .expect("cli crate should be inside workspace");

    for profile in ["debug", "release"] {
        let bin = workspace_root.join("target").join(profile).join("a3s-box");
        if bin.exists() {
            return bin;
        }
    }

    PathBuf::from("a3s-box")
}

struct CliTest {
    bin: PathBuf,
    home: tempfile::TempDir,
}

impl CliTest {
    fn new() -> Self {
        Self {
            bin: find_binary(),
            home: tempfile::tempdir().expect("temp A3S_HOME"),
        }
    }

    fn home_path(&self) -> &Path {
        self.home.path()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(&self.bin);
        cmd.args(args).env("A3S_HOME", self.home_path());
        cmd
    }

    fn command_with_env(&self, args: &[&str], envs: &[(&str, &str)]) -> Command {
        let mut cmd = self.command(args);
        for (key, value) in envs {
            cmd.env(key, value);
        }
        cmd
    }

    fn output(&self, args: &[&str]) -> (String, String, bool) {
        self.output_with_env(args, &[])
    }

    fn output_with_env(&self, args: &[&str], envs: &[(&str, &str)]) -> (String, String, bool) {
        eprintln!("    $ a3s-box {}", args.join(" "));

        let mut command = self.command_with_env(args, envs);
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("failed to run `a3s-box {}`: {e}", args.join(" ")));

        let start = Instant::now();
        loop {
            if child
                .try_wait()
                .unwrap_or_else(|e| panic!("failed to poll `a3s-box {}`: {e}", args.join(" ")))
                .is_some()
            {
                break;
            }

            if start.elapsed() >= COMMAND_TIMEOUT {
                let _ = child.kill();
                let output = child.wait_with_output().unwrap_or_else(|e| {
                    panic!(
                        "timed out and failed to collect `a3s-box {}` output: {e}",
                        args.join(" ")
                    )
                });
                panic!(
                    "`a3s-box {}` timed out after {:?}\nstdout:\n{}\nstderr:\n{}",
                    args.join(" "),
                    COMMAND_TIMEOUT,
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }

            std::thread::sleep(Duration::from_millis(50));
        }

        let output = child.wait_with_output().unwrap_or_else(|e| {
            panic!("failed to collect `a3s-box {}` output: {e}", args.join(" "))
        });
        (
            String::from_utf8_lossy(&output.stdout).to_string(),
            String::from_utf8_lossy(&output.stderr).to_string(),
            output.status.success(),
        )
    }

    fn output_with_stdin(&self, args: &[&str], stdin: &[u8]) -> (String, String, bool) {
        eprintln!("    $ printf ... | a3s-box {}", args.join(" "));

        let mut command = self.command(args);
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("failed to run `a3s-box {}`: {e}", args.join(" ")));

        child
            .stdin
            .take()
            .expect("stdin should be piped")
            .write_all(stdin)
            .unwrap_or_else(|e| {
                panic!(
                    "failed to write stdin for `a3s-box {}`: {e}",
                    args.join(" ")
                )
            });

        let start = Instant::now();
        loop {
            if child
                .try_wait()
                .unwrap_or_else(|e| panic!("failed to poll `a3s-box {}`: {e}", args.join(" ")))
                .is_some()
            {
                break;
            }

            if start.elapsed() >= COMMAND_TIMEOUT {
                let _ = child.kill();
                let output = child.wait_with_output().unwrap_or_else(|e| {
                    panic!(
                        "timed out and failed to collect `a3s-box {}` output: {e}",
                        args.join(" ")
                    )
                });
                panic!(
                    "`a3s-box {}` timed out after {:?}\nstdout:\n{}\nstderr:\n{}",
                    args.join(" "),
                    COMMAND_TIMEOUT,
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }

            std::thread::sleep(Duration::from_millis(50));
        }

        let output = child.wait_with_output().unwrap_or_else(|e| {
            panic!("failed to collect `a3s-box {}` output: {e}", args.join(" "))
        });
        (
            String::from_utf8_lossy(&output.stdout).to_string(),
            String::from_utf8_lossy(&output.stderr).to_string(),
            output.status.success(),
        )
    }

    fn ok(&self, args: &[&str]) -> String {
        let (stdout, stderr, success) = self.output(args);
        assert!(
            success,
            "`a3s-box {}` failed\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            stdout,
            stderr
        );
        stdout
    }

    fn ok_with_stdin(&self, args: &[&str], stdin: &[u8]) -> String {
        let (stdout, stderr, success) = self.output_with_stdin(args, stdin);
        assert!(
            success,
            "`a3s-box {}` failed\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            stdout,
            stderr
        );
        stdout
    }

    fn ok_with_env(&self, args: &[&str], envs: &[(&str, &str)]) -> String {
        let (stdout, stderr, success) = self.output_with_env(args, envs);
        assert!(
            success,
            "`a3s-box {}` failed\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            stdout,
            stderr
        );
        stdout
    }

    fn ok_status(&self, args: &[&str]) {
        eprintln!("    $ a3s-box {}", args.join(" "));

        let mut command = self.command(args);
        let mut child = command
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|e| panic!("failed to run `a3s-box {}`: {e}", args.join(" ")));

        let start = Instant::now();
        let status = loop {
            if let Some(status) = child
                .try_wait()
                .unwrap_or_else(|e| panic!("failed to poll `a3s-box {}`: {e}", args.join(" ")))
            {
                break status;
            }

            if start.elapsed() >= COMMAND_TIMEOUT {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "`a3s-box {}` timed out after {:?}",
                    args.join(" "),
                    COMMAND_TIMEOUT
                );
            }

            std::thread::sleep(Duration::from_millis(50));
        };

        assert!(
            status.success(),
            "`a3s-box {}` failed with status {}",
            args.join(" "),
            status
        );
    }

    fn fails(&self, args: &[&str], expected: &str) {
        self.fails_with_env(args, &[], expected);
    }

    fn fails_with_env(&self, args: &[&str], envs: &[(&str, &str)], expected: &str) {
        let (stdout, stderr, success) = self.output_with_env(args, envs);
        assert!(
            !success,
            "`a3s-box {}` unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            stdout,
            stderr
        );
        let combined = format!("{stdout}\n{stderr}");
        assert!(
            combined.contains(expected),
            "`a3s-box {}` failure did not contain {:?}\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            expected,
            stdout,
            stderr
        );
    }

    fn runs_until_timeout(&self, args: &[&str], timeout: Duration) -> (String, String) {
        eprintln!("    $ a3s-box {}  # expect timeout", args.join(" "));

        let mut command = self.command(args);
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("failed to run `a3s-box {}`: {e}", args.join(" ")));

        let start = Instant::now();
        loop {
            if let Some(status) = child
                .try_wait()
                .unwrap_or_else(|e| panic!("failed to poll `a3s-box {}`: {e}", args.join(" ")))
            {
                let output = child.wait_with_output().unwrap_or_else(|e| {
                    panic!("failed to collect `a3s-box {}` output: {e}", args.join(" "))
                });
                panic!(
                    "`a3s-box {}` exited before {:?} with status {}\nstdout:\n{}\nstderr:\n{}",
                    args.join(" "),
                    timeout,
                    status,
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }

            if start.elapsed() >= timeout {
                let _ = child.kill();
                let output = child.wait_with_output().unwrap_or_else(|e| {
                    panic!(
                        "timed out and failed to collect `a3s-box {}` output: {e}",
                        args.join(" ")
                    )
                });
                return (
                    String::from_utf8_lossy(&output.stdout).to_string(),
                    String::from_utf8_lossy(&output.stderr).to_string(),
                );
            }

            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn interrupts_after_output(
        &self,
        args: &[&str],
        marker: &str,
        timeout: Duration,
    ) -> (String, String) {
        eprintln!(
            "    $ a3s-box {}  # interrupt after {:?}",
            args.join(" "),
            timeout
        );

        let stdout_path = self
            .home_path()
            .join(format!("cmd-{}-stdout.log", unique_tag("out")));
        let stderr_path = self
            .home_path()
            .join(format!("cmd-{}-stderr.log", unique_tag("err")));
        let stdout_file = std::fs::File::create(&stdout_path).expect("create stdout capture");
        let stderr_file = std::fs::File::create(&stderr_path).expect("create stderr capture");

        let mut command = self.command(args);
        let mut child = command
            .stdout(Stdio::from(stdout_file))
            .stderr(Stdio::from(stderr_file))
            .spawn()
            .unwrap_or_else(|e| panic!("failed to run `a3s-box {}`: {e}", args.join(" ")));

        let start = Instant::now();
        let saw_marker = loop {
            let stdout = std::fs::read_to_string(&stdout_path).unwrap_or_default();
            let stderr = std::fs::read_to_string(&stderr_path).unwrap_or_default();
            let combined = format!("{stdout}\n{stderr}");
            if combined.contains(marker) {
                break true;
            }

            if let Some(status) = child
                .try_wait()
                .unwrap_or_else(|e| panic!("failed to poll `a3s-box {}`: {e}", args.join(" ")))
            {
                panic!(
                    "`a3s-box {}` exited before marker {:?} with status {}\nstdout:\n{}\nstderr:\n{}",
                    args.join(" "),
                    marker,
                    status,
                    stdout,
                    stderr
                );
            }

            if start.elapsed() >= timeout {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "`a3s-box {}` did not print marker {:?} within {:?}\nstdout:\n{}\nstderr:\n{}",
                    args.join(" "),
                    marker,
                    timeout,
                    stdout,
                    stderr
                );
            }

            std::thread::sleep(Duration::from_millis(100));
        };
        assert!(saw_marker);

        #[cfg(unix)]
        unsafe {
            libc::kill(child.id() as i32, libc::SIGINT);
        }
        #[cfg(windows)]
        {
            let _ = child.kill();
        }

        let wait_start = Instant::now();
        let status = loop {
            if let Some(status) = child
                .try_wait()
                .unwrap_or_else(|e| panic!("failed to poll `a3s-box {}`: {e}", args.join(" ")))
            {
                break status;
            }
            if wait_start.elapsed() >= COMMAND_TIMEOUT {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "`a3s-box {}` did not exit after interrupt within {:?}",
                    args.join(" "),
                    COMMAND_TIMEOUT
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        };

        let stdout = std::fs::read_to_string(&stdout_path).unwrap_or_default();
        let stderr = std::fs::read_to_string(&stderr_path).unwrap_or_default();
        assert!(
            status.success(),
            "`a3s-box {}` failed after interrupt with status {}\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            status,
            stdout,
            stderr
        );
        (stdout, stderr)
    }
}

struct ErrorRegistry {
    addr: String,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ErrorRegistry {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind test registry");
        listener
            .set_nonblocking(true)
            .expect("set test registry nonblocking");
        let addr = listener
            .local_addr()
            .expect("test registry addr")
            .to_string();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);

        let handle = std::thread::spawn(move || {
            while !thread_stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut buf = [0u8; 1024];
                        let _ = stream.read(&mut buf);
                        let _ = stream.write_all(
                            b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(25));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            addr,
            stop,
            handle: Some(handle),
        }
    }

    fn addr(&self) -> &str {
        &self.addr
    }
}

impl Drop for ErrorRegistry {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = std::net::TcpStream::connect(&self.addr);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn wait_for_running(cli: &CliTest, name: &str, timeout: Duration) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        let stdout = cli.ok(&["ps"]);
        if stdout.contains(name) && stdout.contains("running") {
            return;
        }

        let all = cli.ok(&["ps", "-a"]);
        assert!(
            !all.contains(name) || !all.contains("dead"),
            "box {name} died during boot\n{all}"
        );

        std::thread::sleep(Duration::from_millis(500));
    }

    let inspect = cli.output(&["inspect", name]).0;
    panic!("timeout waiting for {name} to run\n{inspect}");
}

fn cleanup(cli: &CliTest, name: &str) {
    let _ = cli.output(&["stop", name]);
    let _ = cli.output(&["rm", "-f", name]);
}

fn unique_tag(prefix: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_millis();
    format!("{prefix}-{}-{now}", std::process::id())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn create_minimal_oci_tar(path: &Path, reference: &str) {
    let layout = tempfile::tempdir().expect("create oci layout tempdir");
    let blobs = layout.path().join("blobs").join("sha256");
    std::fs::create_dir_all(&blobs).expect("create oci blobs dir");

    let mut layer_tar = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut layer_tar);
        let content = b"packages-push-ok\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "message.txt", &content[..])
            .expect("append test layer file");
        builder.finish().expect("finish test layer tar");
    }
    let diff_id = sha256_hex(&layer_tar);

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(&layer_tar)
        .expect("write compressed test layer");
    let layer = encoder.finish().expect("finish compressed test layer");
    let layer_digest = sha256_hex(&layer);
    std::fs::write(blobs.join(&layer_digest), &layer).expect("write layer blob");

    let config = serde_json::json!({
        "architecture": "amd64",
        "os": "linux",
        "config": {
            "Cmd": ["cat", "/message.txt"]
        },
        "rootfs": {
            "type": "layers",
            "diff_ids": [format!("sha256:{diff_id}")]
        },
        "history": [
            {"created_by": "a3s-box test fixture"}
        ]
    });
    let config_bytes = serde_json::to_vec(&config).expect("serialize config");
    let config_digest = sha256_hex(&config_bytes);
    std::fs::write(blobs.join(&config_digest), &config_bytes).expect("write config blob");

    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": format!("sha256:{config_digest}"),
            "size": config_bytes.len()
        },
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": format!("sha256:{layer_digest}"),
            "size": layer.len()
        }]
    });
    let manifest_bytes = serde_json::to_vec(&manifest).expect("serialize manifest");
    let manifest_digest = sha256_hex(&manifest_bytes);
    std::fs::write(blobs.join(&manifest_digest), &manifest_bytes).expect("write manifest blob");

    std::fs::write(
        layout.path().join("oci-layout"),
        r#"{"imageLayoutVersion":"1.0.0"}"#,
    )
    .expect("write oci-layout");
    let index = serde_json::json!({
        "schemaVersion": 2,
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": format!("sha256:{manifest_digest}"),
            "size": manifest_bytes.len(),
            "annotations": {
                "org.opencontainers.image.ref.name": reference
            }
        }]
    });
    std::fs::write(
        layout.path().join("index.json"),
        serde_json::to_vec(&index).expect("serialize index"),
    )
    .expect("write index");

    let file = std::fs::File::create(path).expect("create oci tar");
    let mut archive = tar::Builder::new(file);
    archive
        .append_path_with_name(layout.path().join("oci-layout"), "oci-layout")
        .expect("archive oci-layout");
    archive
        .append_path_with_name(layout.path().join("index.json"), "index.json")
        .expect("archive index");
    archive
        .append_dir_all("blobs", layout.path().join("blobs"))
        .expect("archive blobs");
    archive.finish().expect("finish oci tar");
}

fn seed_runnable_alpine_image(cli: &CliTest, image: &str) {
    if let Ok(tar_path) = std::env::var(TEST_ALPINE_TAR_ENV) {
        cli.ok(&["load", "--input", &tar_path, "--tag", image]);
        return;
    }

    cli.ok(&["pull", image]);
}

#[cfg(unix)]
fn host_socket_dirs() -> std::collections::BTreeSet<PathBuf> {
    let socket_root = Path::new("/tmp/a3s-box-sockets");
    let Ok(entries) = std::fs::read_dir(socket_root) else {
        return std::collections::BTreeSet::new();
    };

    entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| match entry.file_type() {
            Ok(file_type) if file_type.is_dir() => Some(entry.path()),
            _ => None,
        })
        .collect()
}

#[cfg(not(unix))]
fn host_socket_dirs() -> std::collections::BTreeSet<PathBuf> {
    std::collections::BTreeSet::new()
}

fn assert_no_new_host_socket_dirs(before: &std::collections::BTreeSet<PathBuf>) {
    let after = host_socket_dirs();
    let new_dirs: Vec<_> = after.difference(before).collect();
    assert!(
        new_dirs.is_empty(),
        "new host socket directories were left behind: {new_dirs:?}"
    );
}

#[test]
fn test_all_top_level_command_help() {
    let cli = CliTest::new();
    for command in TOP_LEVEL_COMMANDS {
        let args = if *command == "help" {
            vec!["help"]
        } else {
            vec![*command, "--help"]
        };
        let (stdout, stderr, success) = cli.output(&args);
        assert!(
            success,
            "`a3s-box {}` help failed\nstdout:\n{}\nstderr:\n{}",
            command, stdout, stderr
        );
        assert!(
            stdout.contains("Usage:") || stdout.contains("Usage"),
            "`a3s-box {}` help did not contain usage\n{}",
            command,
            stdout
        );
    }
}

#[test]
fn test_top_level_help_matches_coverage_list() {
    let cli = CliTest::new();
    let help = cli.ok(&["--help"]);
    let mut discovered = Vec::new();
    let mut in_commands = false;

    for line in help.lines() {
        let trimmed = line.trim();
        if trimmed == "Commands:" {
            in_commands = true;
            continue;
        }
        if in_commands && (trimmed == "Options:" || trimmed.is_empty()) {
            break;
        }
        if in_commands {
            if let Some(command) = trimmed.split_whitespace().next() {
                discovered.push(command.to_string());
            }
        }
    }

    let covered: std::collections::BTreeSet<_> = TOP_LEVEL_COMMANDS.iter().copied().collect();
    let discovered: std::collections::BTreeSet<_> = discovered.iter().map(String::as_str).collect();

    let missing: Vec<_> = discovered.difference(&covered).copied().collect();
    let stale: Vec<_> = covered.difference(&discovered).copied().collect();
    assert!(
        missing.is_empty() && stale.is_empty(),
        "top-level command coverage list mismatch\nmissing from tests: {:?}\nstale entries: {:?}",
        missing,
        stale
    );
}

#[test]
fn test_nested_subcommand_help() {
    let cli = CliTest::new();
    let commands: &[&[&str]] = &[
        &["network", "create"],
        &["network", "ls"],
        &["network", "rm"],
        &["network", "inspect"],
        &["network", "connect"],
        &["network", "disconnect"],
        &["volume", "create"],
        &["volume", "ls"],
        &["volume", "rm"],
        &["volume", "inspect"],
        &["volume", "prune"],
        &["snapshot", "create"],
        &["snapshot", "restore"],
        &["snapshot", "ls"],
        &["snapshot", "rm"],
        &["snapshot", "inspect"],
        &["pool", "start"],
        &["pool", "stop"],
        &["pool", "status"],
        &["compose", "up"],
        &["compose", "down"],
        &["compose", "ps"],
        &["compose", "logs"],
        &["compose", "config"],
    ];

    for command in commands {
        let mut args = command.to_vec();
        args.push("--help");
        let (stdout, stderr, success) = cli.output(&args);
        assert!(
            success,
            "`a3s-box {}` help failed\nstdout:\n{}\nstderr:\n{}",
            command.join(" "),
            stdout,
            stderr
        );
    }
}

#[test]
fn test_local_state_command_smoke() {
    let cli = CliTest::new();

    cli.ok(&["version"]);
    cli.ok(&["info"]);
    cli.ok(&["ps"]);
    cli.ok(&["ps", "-a"]);
    cli.ok(&["images"]);
    cli.ok(&["df"]);
    cli.ok(&["df", "--verbose"]);
    cli.ok(&["audit", "--limit", "1"]);
    cli.ok(&["snapshot", "ls"]);
    cli.ok(&["snapshot", "ls", "--json"]);
    cli.ok(&["pool", "status"]);
    cli.ok(&["pool", "stop"]);
    cli.ok(&["image-prune", "--force"]);
    cli.ok(&["system-prune", "--force"]);
    cli.ok(&["rmi", "--force", "missing:latest"]);

    cli.ok(&[
        "network",
        "create",
        "covnet",
        "--subnet",
        "10.123.0.0/24",
        "--label",
        "purpose=coverage",
    ]);
    let networks = cli.ok(&["network", "ls", "--quiet"]);
    assert!(networks.contains("covnet"));
    let network_json = cli.ok(&["network", "inspect", "covnet"]);
    assert!(network_json.contains("10.123.0.0/24"));
    cli.ok(&["network", "rm", "covnet"]);

    cli.ok(&["volume", "create", "covvol", "--label", "purpose=coverage"]);
    let volumes = cli.ok(&["volume", "ls", "--quiet"]);
    assert!(volumes.contains("covvol"));
    let volume_json = cli.ok(&["volume", "inspect", "covvol"]);
    assert!(volume_json.contains("covvol"));
    cli.ok(&["volume", "rm", "covvol"]);
    cli.ok(&["volume", "prune", "--force"]);

    cli.ok(&[
        "create",
        "--name",
        "cov-created",
        "-p",
        "18080:80",
        "--label",
        "purpose=coverage",
        "docker.io/library/alpine:latest",
    ]);
    let inspect = cli.ok(&["inspect", "cov-created"]);
    assert!(inspect.contains("cov-created"));
    cli.ok(&["port", "cov-created"]);
    cli.ok(&["rename", "cov-created", "cov-renamed"]);
    let formatted = cli.ok(&["ps", "-a", "--format", "{{.Names}} {{.Status}}"]);
    assert!(formatted.contains("cov-renamed created"));
    cli.ok(&["rm", "cov-renamed"]);

    cli.ok(&[
        "login",
        "example.invalid",
        "--username",
        "coverage",
        "--password",
        "secret",
        "--skip-verify",
    ]);
    cli.ok(&["logout", "example.invalid"]);

    cli.ok(&["events", "--until", "1970-01-01T00:00:00Z"]);
}

#[test]
fn test_noninteractive_boundary_command_smoke() {
    let cli = CliTest::new();

    cli.fails(
        &["push", "example.invalid/a3s/missing:latest", "--quiet"],
        "not found locally",
    );
    cli.fails(&["attach", "missing-box"], "No such box");
    cli.fails(&["shell", "missing-box"], "No such box");
    cli.fails(&["stats", "--no-stream", "missing-box"], "No such box");
    cli.fails(
        &[
            "pool",
            "start",
            "--image",
            "docker.io/library/alpine:latest",
            "--size",
            "0",
        ],
        "--size must be greater than 0",
    );

    let (stdout, stderr) =
        cli.runs_until_timeout(&["monitor", "--interval", "1"], Duration::from_millis(1200));
    let combined = format!("{stdout}\n{stderr}");
    assert!(
        combined.contains("a3s-box monitor started"),
        "monitor did not announce startup\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[test]
#[ignore]
fn test_real_packages_service_push() {
    let target_template = match std::env::var("A3S_BOX_PUSH_TEST_REF") {
        Ok(value) => value,
        Err(_) => {
            eprintln!(
                "skipping packages push test: set A3S_BOX_PUSH_TEST_REF like registry.example/a3s/box-push-test:{{tag}}"
            );
            return;
        }
    };
    assert!(
        target_template.contains("{tag}"),
        "A3S_BOX_PUSH_TEST_REF must contain {{tag}} to avoid overwriting a stable tag"
    );

    let cli = CliTest::new();
    let local_tag = format!("coverage-push-local:{}", unique_tag("local"));
    let target_ref = target_template.replace("{tag}", &unique_tag("remote"));

    let oci_tar = cli.home_path().join("packages-push-fixture.tar");
    create_minimal_oci_tar(&oci_tar, &local_tag);
    let oci_tar_arg = oci_tar.to_string_lossy().to_string();
    cli.ok(&["load", "--input", &oci_tar_arg, "--tag", &local_tag]);
    cli.ok(&["image-inspect", &local_tag]);
    cli.ok(&["tag", &local_tag, &target_ref]);

    let mut push_env = Vec::new();
    if let Ok(protocol) = std::env::var("A3S_BOX_PUSH_PROTOCOL") {
        push_env.push(("A3S_REGISTRY_PROTOCOL", protocol));
    }
    if let Ok(username) = std::env::var("A3S_BOX_PUSH_USERNAME") {
        push_env.push(("REGISTRY_USERNAME", username));
    }
    if let Ok(password) = std::env::var("A3S_BOX_PUSH_PASSWORD") {
        push_env.push(("REGISTRY_PASSWORD", password));
    }
    let push_env_refs: Vec<(&str, &str)> = push_env
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect();

    let pushed = cli.ok_with_env(&["push", &target_ref, "--quiet"], &push_env_refs);
    assert!(
        pushed.contains("://") || pushed.contains(&target_ref),
        "push output did not include a manifest URL or reference\n{pushed}"
    );

    cli.ok(&["rmi", "--force", &local_tag, &target_ref]);
}

#[test]
#[ignore]
fn test_real_vm_command_matrix() {
    let cli = CliTest::new();
    let socket_dirs_before = host_socket_dirs();
    let image = "docker.io/library/alpine:latest";
    let built_image = "coverage-built:latest";
    let pool_image = "coverage-pool:latest";
    let main_box = "cov-vm-main";
    let built_box = "cov-vm-built";
    let foreground_box = "cov-vm-foreground";
    let renamed_box = "cov-vm-renamed";
    let restored_box = "cov-vm-restored";

    cleanup(&cli, main_box);
    cleanup(&cli, built_box);
    cleanup(&cli, foreground_box);
    cleanup(&cli, renamed_box);
    cleanup(&cli, restored_box);

    seed_runnable_alpine_image(&cli, image);
    let images = cli.ok(&["images"]);
    assert!(images.contains("alpine"));
    cli.ok(&["image-inspect", image]);
    cli.ok(&["history", image]);

    let image_tar = cli.home_path().join("alpine.tar");
    let image_tar = image_tar.to_string_lossy().to_string();
    cli.ok(&["save", image, "--output", &image_tar]);
    cli.ok(&[
        "load",
        "--input",
        &image_tar,
        "--tag",
        "coverage-loaded:latest",
    ]);
    cli.ok(&["tag", image, "coverage-alias:latest"]);

    let build_dir = cli.home_path().join("build-context");
    std::fs::create_dir_all(&build_dir).expect("create build context");
    std::fs::write(build_dir.join("message.txt"), "built-image-ok\n").expect("write build input");
    std::fs::write(
        build_dir.join("Dockerfile"),
        format!(
            "FROM {image}\nCOPY message.txt /message.txt\nENV A3S_BUILD_COVERAGE=1\nCMD [\"cat\", \"/message.txt\"]\n"
        ),
    )
    .expect("write Dockerfile");
    let build_dir_arg = build_dir.to_string_lossy().to_string();
    cli.ok(&["build", "--tag", built_image, "--quiet", &build_dir_arg]);
    cli.ok(&["image-inspect", built_image]);

    std::fs::remove_file(build_dir.join("Dockerfile")).expect("remove Dockerfile");
    std::fs::write(
        build_dir.join("Containerfile"),
        format!("FROM {image}\nCMD [\"sleep\", \"3600\"]\n"),
    )
    .expect("write pool Containerfile");
    cli.ok(&["build", "--tag", pool_image, "--quiet", &build_dir_arg]);
    cli.ok(&["image-inspect", pool_image]);
    let (pool_stdout, pool_stderr) = cli.interrupts_after_output(
        &[
            "pool", "start", "--image", pool_image, "--size", "1", "--max", "1", "--ttl", "30",
        ],
        "Warm pool started",
        Duration::from_secs(90),
    );
    assert!(
        pool_stdout.contains("Done."),
        "pool start did not drain cleanly\nstdout:\n{pool_stdout}\nstderr:\n{pool_stderr}"
    );

    let registry = ErrorRegistry::start();
    let push_ref = format!("{}/a3s/coverage-built:latest", registry.addr());
    cli.ok(&["tag", built_image, &push_ref]);
    cli.fails_with_env(
        &["push", &push_ref, "--quiet"],
        &[("A3S_REGISTRY_PROTOCOL", "http")],
        "Failed to push image",
    );

    let foreground_output = cli.ok(&[
        "run",
        "--rm",
        "--name",
        foreground_box,
        built_image,
        "--",
        "sh",
        "-c",
        "cat /message.txt",
    ]);
    assert!(foreground_output.contains("built-image-ok"));
    let ps_after_foreground = cli.ok(&["ps", "-a"]);
    assert!(!ps_after_foreground.contains(foreground_box));

    cli.ok_status(&[
        "run",
        "-d",
        "--name",
        built_box,
        built_image,
        "--",
        "sleep",
        "3600",
    ]);
    wait_for_running(&cli, built_box, Duration::from_secs(45));
    let built_message = cli.ok(&["exec", built_box, "--", "cat", "/message.txt"]);
    assert!(built_message.contains("built-image-ok"));
    cli.ok(&["rm", "--force", built_box]);

    cli.ok_status(&[
        "run",
        "-d",
        "--name",
        main_box,
        "-p",
        "18081:81",
        "-e",
        "A3S_COVERAGE=1",
        "--label",
        "purpose=coverage",
        image,
        "--",
        "sleep",
        "3600",
    ]);
    wait_for_running(&cli, main_box, Duration::from_secs(45));

    cli.ok(&["ps"]);
    cli.ok(&["ps", "-a", "--filter", "name=cov-vm"]);
    cli.ok(&["inspect", main_box]);
    cli.ok(&["stats", "--no-stream", main_box]);
    let (stats_stdout, stats_stderr) =
        cli.runs_until_timeout(&["stats", main_box], Duration::from_millis(1400));
    assert!(
        stats_stdout.contains(main_box),
        "streaming stats did not print box name\nstdout:\n{stats_stdout}\nstderr:\n{stats_stderr}"
    );
    cli.ok(&["logs", "--tail", "20", main_box]);
    let _ = cli.runs_until_timeout(
        &["logs", main_box, "--follow", "--tail", "1"],
        Duration::from_millis(1200),
    );
    let (attach_stdout, attach_stderr) =
        cli.runs_until_timeout(&["attach", main_box], Duration::from_millis(1200));
    assert!(
        attach_stdout.contains("Attached to box"),
        "attach did not enter attached mode\nstdout:\n{attach_stdout}\nstderr:\n{attach_stderr}"
    );
    cli.ok(&["top", main_box]);
    cli.ok(&["port", main_box]);
    cli.ok(&["exec", main_box, "--", "sh", "-c", "echo exec-ok"]);
    let exec_context = cli.ok(&[
        "exec",
        main_box,
        "--env",
        "COV_EXEC_ENV=ok",
        "--workdir",
        "/tmp",
        "--",
        "sh",
        "-c",
        "printf '%s:%s' \"$COV_EXEC_ENV\" \"$PWD\"",
    ]);
    assert_eq!(exec_context.trim(), "ok:/tmp");
    cli.ok_with_stdin(
        &[
            "exec",
            "--interactive",
            main_box,
            "--",
            "sh",
            "-c",
            "cat > /tmp/stdin-coverage.txt",
        ],
        b"stdin-ok\n",
    );
    let stdin_roundtrip = cli.ok(&["exec", main_box, "--", "cat", "/tmp/stdin-coverage.txt"]);
    assert_eq!(stdin_roundtrip.trim(), "stdin-ok");

    let host_file = cli.home_path().join("host-file.txt");
    std::fs::write(&host_file, "from-host\n").expect("write host file");
    let host_file = host_file.to_string_lossy().to_string();
    cli.ok(&["cp", &host_file, &format!("{main_box}:/tmp/host-file.txt")]);
    let guest_file = cli.home_path().join("guest-file.txt");
    let guest_file = guest_file.to_string_lossy().to_string();
    cli.ok(&["cp", &format!("{main_box}:/etc/os-release"), &guest_file]);
    let copied = std::fs::read_to_string(&guest_file).expect("read copied file");
    assert!(copied.contains("Alpine"));

    cli.ok(&["container-update", main_box, "--memory-reservation", "128m"]);
    cli.ok(&["diff", main_box]);

    let export_tar = cli.home_path().join("box-export.tar");
    let export_tar = export_tar.to_string_lossy().to_string();
    cli.ok(&["export", main_box, "--output", &export_tar]);
    assert!(Path::new(&export_tar).exists());

    cli.ok(&[
        "commit",
        main_box,
        "coverage-committed:latest",
        "--message",
        "coverage",
    ]);
    cli.ok(&["image-inspect", "coverage-committed:latest"]);

    let snapshot_id = cli
        .ok(&[
            "snapshot",
            "create",
            main_box,
            "--name",
            "covsnap",
            "--description",
            "coverage",
        ])
        .trim()
        .to_string();
    assert!(snapshot_id.starts_with("snap-"));
    cli.ok(&["snapshot", "ls"]);
    cli.ok(&["snapshot", "inspect", &snapshot_id]);
    cli.ok(&["snapshot", "restore", &snapshot_id, "--name", restored_box]);
    cli.ok(&["inspect", restored_box]);
    cli.ok(&["rm", restored_box]);
    cli.ok(&["snapshot", "rm", &snapshot_id]);

    cli.ok(&["network", "create", "covvmnet", "--subnet", "10.124.0.0/24"]);
    cli.ok(&["network", "connect", "covvmnet", main_box]);
    cli.ok(&["network", "inspect", "covvmnet"]);
    cli.ok(&["network", "disconnect", "covvmnet", main_box]);
    cli.ok(&["network", "rm", "covvmnet"]);

    cli.ok(&["pause", main_box]);
    cli.ok(&["unpause", main_box]);
    cli.ok_status(&["restart", main_box]);
    wait_for_running(&cli, main_box, Duration::from_secs(45));
    cli.ok(&["stop", main_box]);
    cli.ok(&["wait", main_box]);
    cli.ok_status(&["start", main_box]);
    wait_for_running(&cli, main_box, Duration::from_secs(45));
    cli.ok(&["rename", main_box, renamed_box]);
    cli.ok(&["kill", "--signal", "TERM", renamed_box]);
    cli.ok(&["wait", renamed_box]);
    cli.ok(&["rm", renamed_box]);

    cli.ok(&[
        "rmi",
        "--force",
        "coverage-alias:latest",
        "coverage-loaded:latest",
        &push_ref,
        pool_image,
    ]);
    cli.ok(&["image-prune", "--force", "--all"]);
    cli.ok(&["system-prune", "--force", "--all"]);
    assert_no_new_host_socket_dirs(&socket_dirs_before);
}

#[test]
#[ignore]
fn test_real_compose_smoke() {
    let cli = CliTest::new();
    let image = "docker.io/library/alpine:latest";
    let project = "covcompose";
    let service_box = "covcompose-worker";

    cleanup(&cli, service_box);
    seed_runnable_alpine_image(&cli, image);

    let compose_dir = cli.home_path().join("compose");
    std::fs::create_dir_all(&compose_dir).expect("create compose dir");
    let compose_file = compose_dir.join("compose.yaml");
    std::fs::write(
        &compose_file,
        format!(
            r#"services:
  worker:
    image: {image}
    command: ["sleep", "3600"]
    environment:
      A3S_COMPOSE_COVERAGE: "1"
    labels:
      purpose: coverage
"#,
        ),
    )
    .expect("write compose file");
    let compose_file_arg = compose_file.to_string_lossy().to_string();

    cli.ok(&[
        "compose",
        "--file",
        &compose_file_arg,
        "--project-name",
        project,
        "config",
    ]);
    cli.ok_status(&[
        "compose",
        "--file",
        &compose_file_arg,
        "--project-name",
        project,
        "up",
        "--detach",
    ]);
    wait_for_running(&cli, service_box, Duration::from_secs(45));
    cli.ok(&[
        "compose",
        "--file",
        &compose_file_arg,
        "--project-name",
        project,
        "ps",
    ]);
    cli.ok(&[
        "compose",
        "--file",
        &compose_file_arg,
        "--project-name",
        project,
        "logs",
        "--tail",
        "20",
    ]);
    let env_value = cli.ok(&[
        "exec",
        service_box,
        "--",
        "sh",
        "-c",
        "echo $A3S_COMPOSE_COVERAGE",
    ]);
    assert_eq!(env_value.trim(), "1");
    cli.ok(&[
        "compose",
        "--file",
        &compose_file_arg,
        "--project-name",
        project,
        "down",
    ]);

    let ps = cli.ok(&["ps", "-a"]);
    assert!(!ps.contains(service_box));
}
