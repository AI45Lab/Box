//! Real core lifecycle smoke test for the a3s-box CLI.
//!
//! This test is ignored by default because it needs registry access, a built
//! `a3s-box` binary, libkrun/HVF or KVM, and a runnable Linux image.
//!
//! Run it from `crates/box/src`:
//!
//! ```bash
//! cargo test -p a3s-box-cli --test core_smoke -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Optional environment:
//!
//! - `A3S_BOX_SMOKE_IMAGE`: image to use (default: `docker.io/library/alpine:latest`)
//! - `A3S_BOX_SMOKE_IMAGE_TAR`: load this OCI archive into the isolated `A3S_HOME`
//!   before running, useful for offline HVF/KVM smoke tests
//! - `A3S_BOX_TEST_ALPINE_TAR`: fallback OCI archive path shared with host smoke coverage
//! - `A3S_BOX_SMOKE_SKIP_PULL=1`: skip the explicit `pull` step for cached images
//! - `A3S_BOX_SMOKE_TIMEOUT_SECS`: command and polling timeout (default: 300)

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const DEFAULT_IMAGE: &str = "docker.io/library/alpine:latest";
const DEFAULT_TIMEOUT_SECS: u64 = 300;

struct CommandResult {
    stdout: String,
    stderr: String,
    success: bool,
    code: Option<i32>,
}

struct BackgroundCommand {
    child: Option<Child>,
    args: String,
}

impl BackgroundCommand {
    fn assert_running(&mut self) {
        let status = {
            let child = self.child.as_mut().expect("background command child");
            child
                .try_wait()
                .unwrap_or_else(|e| panic!("failed to poll `a3s-box {}`: {e}", self.args))
        };

        if let Some(status) = status {
            let child = self.child.take().expect("background command child");
            let output = child.wait_with_output().unwrap_or_else(|e| {
                panic!("failed to collect `a3s-box {}` output: {e}", self.args)
            });
            panic!(
                "`a3s-box {}` exited unexpectedly with status {}\nstdout:\n{}\nstderr:\n{}",
                self.args,
                status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    fn kill_and_output(mut self) -> CommandResult {
        let mut child = self.child.take().expect("background command child");
        let _ = child.kill();
        let output = child
            .wait_with_output()
            .unwrap_or_else(|e| panic!("failed to collect `a3s-box {}` output: {e}", self.args));

        CommandResult {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            success: output.status.success(),
            code: output.status.code(),
        }
    }
}

impl Drop for BackgroundCommand {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct CoreSmoke {
    bin: PathBuf,
    home: tempfile::TempDir,
    name: String,
    timeout: Duration,
}

impl CoreSmoke {
    fn new() -> Self {
        Self {
            bin: find_binary(),
            home: tempfile::tempdir().expect("temp A3S_HOME"),
            name: unique_name(),
            timeout: smoke_timeout(),
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

    fn try_output(&self, args: &[&str], timeout: Duration) -> Result<CommandResult, String> {
        eprintln!("    $ a3s-box {}", args.join(" "));

        let capture_id = unique_capture_id();
        let stdout_path = self.home_path().join(format!("{capture_id}.stdout"));
        let stderr_path = self.home_path().join(format!("{capture_id}.stderr"));
        let stdout = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&stdout_path)
            .map_err(|e| format!("failed to create stdout capture file: {e}"))?;
        let stderr = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&stderr_path)
            .map_err(|e| format!("failed to create stderr capture file: {e}"))?;
        let mut child = self
            .command(args)
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .map_err(|e| format!("failed to run `a3s-box {}`: {e}", args.join(" ")))?;

        let start = Instant::now();
        loop {
            if let Some(status) = child
                .try_wait()
                .map_err(|e| format!("failed to poll `a3s-box {}`: {e}", args.join(" ")))?
            {
                let (stdout, stderr) = read_command_output(&stdout_path, &stderr_path)?;
                return Ok(CommandResult {
                    stdout,
                    stderr,
                    success: status.success(),
                    code: status.code(),
                });
            }

            if start.elapsed() >= timeout {
                let _ = child.kill();
                let _ = child.wait().map_err(|e| {
                    format!(
                        "timed out and failed to reap `a3s-box {}`: {e}",
                        args.join(" ")
                    )
                })?;
                let (stdout, stderr) = read_command_output(&stdout_path, &stderr_path)?;
                return Err(format!(
                    "`a3s-box {}` timed out after {:?}\nstdout:\n{}\nstderr:\n{}",
                    args.join(" "),
                    timeout,
                    stdout,
                    stderr
                ));
            }

            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn output(&self, args: &[&str]) -> CommandResult {
        self.try_output(args, self.timeout)
            .unwrap_or_else(|e| panic!("{e}"))
    }

    fn spawn_background(&self, args: &[&str]) -> BackgroundCommand {
        eprintln!("    $ a3s-box {}  # background", args.join(" "));

        let child = self
            .command(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("failed to run `a3s-box {}`: {e}", args.join(" ")));

        BackgroundCommand {
            child: Some(child),
            args: args.join(" "),
        }
    }

    fn ok(&self, args: &[&str]) -> String {
        let result = self.output(args);
        assert!(
            result.success,
            "`a3s-box {}` failed\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            result.stdout,
            result.stderr
        );
        result.stdout
    }

    fn fails(&self, args: &[&str], expected: &str) -> CommandResult {
        let result = self.output(args);
        assert!(
            !result.success,
            "`a3s-box {}` unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            result.stdout,
            result.stderr
        );
        let combined = format!("{}\n{}", result.stdout, result.stderr);
        assert_contains(&combined, expected, "failure output");
        result
    }

    fn best_effort(&self, args: &[&str]) {
        let _ = self.try_output(args, Duration::from_secs(20));
    }

    #[cfg(unix)]
    fn tty_output(&self, args: &[&str], input: &[u8]) -> CommandResult {
        self.try_tty_output(args, input, self.timeout)
            .unwrap_or_else(|e| panic!("{e}"))
    }

    #[cfg(unix)]
    fn try_tty_output(
        &self,
        args: &[&str],
        input: &[u8],
        timeout: Duration,
    ) -> Result<CommandResult, String> {
        use std::fs::File;
        use std::io::{Read, Write};
        use std::os::fd::{AsRawFd, FromRawFd, RawFd};

        eprintln!("    $ [pty] a3s-box {}", args.join(" "));

        let (master_fd, slave_fd) = open_pty()?;
        let stdout_fd = duplicate_fd(slave_fd).inspect_err(|_| {
            close_fd(master_fd);
            close_fd(slave_fd);
        })?;
        let stderr_fd = duplicate_fd(slave_fd).inspect_err(|_| {
            close_fd(master_fd);
            close_fd(slave_fd);
            close_fd(stdout_fd);
        })?;

        let mut master = unsafe { File::from_raw_fd(master_fd) };
        let slave_stdin = unsafe { File::from_raw_fd(slave_fd) };
        let slave_stdout = unsafe { File::from_raw_fd(stdout_fd) };
        let slave_stderr = unsafe { File::from_raw_fd(stderr_fd) };

        let mut child = self
            .command(args)
            .stdin(Stdio::from(slave_stdin))
            .stdout(Stdio::from(slave_stdout))
            .stderr(Stdio::from(slave_stderr))
            .spawn()
            .map_err(|e| format!("failed to run `a3s-box {}` under PTY: {e}", args.join(" ")))?;

        if !input.is_empty() {
            master
                .write_all(input)
                .map_err(|e| format!("failed to write PTY input: {e}"))?;
            let _ = master.flush();
        }

        set_nonblocking(master.as_raw_fd())?;

        let start = Instant::now();
        let mut output = Vec::new();
        let mut buf = [0u8; 4096];

        loop {
            drain_pty_output(&mut master, &mut buf, &mut output)?;

            if let Some(status) = child.try_wait().map_err(|e| {
                format!(
                    "failed to poll PTY command `a3s-box {}`: {e}",
                    args.join(" ")
                )
            })? {
                let drain_deadline = Instant::now() + Duration::from_millis(500);
                while Instant::now() < drain_deadline {
                    let before = output.len();
                    drain_pty_output(&mut master, &mut buf, &mut output)?;
                    if output.len() == before {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }

                return Ok(CommandResult {
                    stdout: String::from_utf8_lossy(&output).to_string(),
                    stderr: String::new(),
                    success: status.success(),
                    code: status.code(),
                });
            }

            if start.elapsed() >= timeout {
                let _ = child.kill();
                let _ = child.wait();
                drain_pty_output(&mut master, &mut buf, &mut output)?;
                return Err(format!(
                    "`a3s-box {}` timed out after {:?}\npty output:\n{}",
                    args.join(" "),
                    timeout,
                    String::from_utf8_lossy(&output)
                ));
            }

            std::thread::sleep(Duration::from_millis(50));
        }

        fn open_pty() -> Result<(RawFd, RawFd), String> {
            let mut master: libc::c_int = -1;
            let mut slave: libc::c_int = -1;
            let winsize = libc::winsize {
                ws_row: 24,
                ws_col: 80,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };

            let rc = unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &winsize as *const _ as *mut _,
                )
            };
            if rc != 0 {
                return Err(format!(
                    "failed to allocate PTY: {}",
                    std::io::Error::last_os_error()
                ));
            }

            Ok((master, slave))
        }

        fn duplicate_fd(fd: RawFd) -> Result<RawFd, String> {
            let dup = unsafe { libc::dup(fd) };
            if dup < 0 {
                return Err(format!(
                    "failed to duplicate PTY fd: {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(dup)
        }

        fn close_fd(fd: RawFd) {
            if fd >= 0 {
                unsafe {
                    libc::close(fd);
                }
            }
        }

        fn set_nonblocking(fd: RawFd) -> Result<(), String> {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 {
                return Err(format!(
                    "failed to read PTY fd flags: {}",
                    std::io::Error::last_os_error()
                ));
            }
            if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
                return Err(format!(
                    "failed to set PTY nonblocking: {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(())
        }

        fn drain_pty_output(
            master: &mut File,
            buf: &mut [u8],
            output: &mut Vec<u8>,
        ) -> Result<(), String> {
            loop {
                match master.read(buf) {
                    Ok(0) => return Ok(()),
                    Ok(n) => output.extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
                    Err(e) if e.raw_os_error() == Some(libc::EIO) => return Ok(()),
                    Err(e) => return Err(format!("failed to read PTY output: {e}")),
                }
            }
        }
    }

    fn wait_for_running(&self) {
        self.wait_for_named_running(&self.name);
    }

    fn wait_for_named_running(&self, name: &str) {
        let start = Instant::now();
        let mut last_ps = String::new();

        while start.elapsed() < self.timeout {
            let result = self.output(&["ps", "-a"]);
            let combined = format!("{}\n{}", result.stdout, result.stderr);
            last_ps = combined.clone();

            if result.success && combined.contains(name) && combined.contains("running") {
                return;
            }
            assert!(
                !(result.success && combined.contains(name) && combined.contains("dead")),
                "box {name} died during boot\n{}",
                combined
            );

            std::thread::sleep(Duration::from_millis(500));
        }

        let inspect = self.output(&["inspect", name]);
        panic!(
            "timeout waiting for {name} to become running\nlast ps:\n{}\ninspect stdout:\n{}\ninspect stderr:\n{}",
            last_ps, inspect.stdout, inspect.stderr
        );
    }

    fn wait_for_logs(&self, expected: &str) -> String {
        self.wait_for_named_logs(&self.name, expected)
    }

    fn wait_for_named_logs(&self, name: &str, expected: &str) -> String {
        let start = Instant::now();
        let mut last = CommandResult {
            stdout: String::new(),
            stderr: String::new(),
            success: false,
            code: None,
        };

        while start.elapsed() < self.timeout {
            let result = self.output(&["logs", "--tail", "50", name]);
            let combined = format!("{}\n{}", result.stdout, result.stderr);
            if result.success && combined.contains(expected) {
                return result.stdout;
            }
            last = result;
            std::thread::sleep(Duration::from_millis(500));
        }

        panic!(
            "timeout waiting for log marker {:?}\nlast stdout:\n{}\nlast stderr:\n{}",
            expected, last.stdout, last.stderr
        );
    }

    fn inspect_json(&self, name: &str) -> serde_json::Value {
        let stdout = self.ok(&["inspect", name]);
        let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
            panic!("failed to parse inspect JSON for {name}: {e}\nstdout:\n{stdout}")
        });
        // `a3s-box inspect` returns a Docker-shaped array `[{...}]`. Every caller
        // here wants the single object, so unwrap it (pass through if it is
        // already an object, for robustness).
        match parsed {
            serde_json::Value::Array(mut items) if !items.is_empty() => items.remove(0),
            other => other,
        }
    }

    fn wait_for_named_status(&self, name: &str, expected: &str) -> serde_json::Value {
        let start = Instant::now();
        let mut last = String::new();

        while start.elapsed() < self.timeout {
            let value = self.inspect_json(name);
            last = value.to_string();
            if json_string_field(&value, "status") == expected {
                return value;
            }
            std::thread::sleep(Duration::from_millis(500));
        }

        panic!("timeout waiting for {name} to become {expected}\nlast inspect:\n{last}");
    }

    fn wait_for_named_restart(
        &self,
        monitor: &mut BackgroundCommand,
        name: &str,
        previous_pid: u64,
        minimum_restart_count: u64,
    ) -> serde_json::Value {
        let start = Instant::now();
        let mut last = String::new();

        while start.elapsed() < self.timeout {
            monitor.assert_running();
            let value = self.inspect_json(name);
            last = value.to_string();
            let status = json_string_field(&value, "status");
            let restart_count = json_u64_field(&value, "restart_count");
            let pid = value.get("pid").and_then(serde_json::Value::as_u64);

            if status == "running"
                && restart_count >= minimum_restart_count
                && pid.is_some_and(|pid| pid != previous_pid)
            {
                return value;
            }
            std::thread::sleep(Duration::from_millis(500));
        }

        panic!(
            "timeout waiting for {name} to restart with count >= {minimum_restart_count}\nlast inspect:\n{last}"
        );
    }

    fn wait_for_tcp_text(&self, port: u16, expected: &str) -> String {
        let start = Instant::now();
        let mut last_response = String::new();
        let mut last_error = String::new();

        while start.elapsed() < self.timeout {
            match read_tcp_text(port) {
                Ok(response) if response.contains(expected) => return response,
                Ok(response) => last_response = response,
                Err(error) => last_error = error.to_string(),
            }

            std::thread::sleep(Duration::from_millis(500));
        }

        panic!(
            "timeout waiting for TCP response {:?} on 127.0.0.1:{}\nlast response:\n{}\nlast error:\n{}",
            expected, port, last_response, last_error
        );
    }
}

struct ComposeCleanup<'a> {
    smoke: &'a CoreSmoke,
    compose_file: String,
    project: String,
    service_boxes: Vec<String>,
    remove_volumes: bool,
}

impl Drop for ComposeCleanup<'_> {
    fn drop(&mut self) {
        if self.remove_volumes {
            self.smoke.best_effort(&[
                "compose",
                "--file",
                &self.compose_file,
                "--project-name",
                &self.project,
                "down",
                "--volumes",
            ]);
        } else {
            self.smoke.best_effort(&[
                "compose",
                "--file",
                &self.compose_file,
                "--project-name",
                &self.project,
                "down",
            ]);
        }
        for service_box in &self.service_boxes {
            self.smoke.best_effort(&["rm", "-f", service_box]);
        }
    }
}

struct NamedBoxCleanup<'a> {
    smoke: &'a CoreSmoke,
    name: String,
}

impl Drop for NamedBoxCleanup<'_> {
    fn drop(&mut self) {
        self.smoke.best_effort(&["stop", "-t", "1", &self.name]);
        self.smoke.best_effort(&["rm", "-f", &self.name]);
    }
}

struct NamedVolumeCleanup<'a> {
    smoke: &'a CoreSmoke,
    name: String,
}

impl Drop for NamedVolumeCleanup<'_> {
    fn drop(&mut self) {
        self.smoke
            .best_effort(&["volume", "rm", "--force", &self.name]);
    }
}

struct NetworkCleanup<'a> {
    smoke: &'a CoreSmoke,
    name: String,
}

impl Drop for NetworkCleanup<'_> {
    fn drop(&mut self) {
        self.smoke
            .best_effort(&["network", "rm", "--force", &self.name]);
    }
}

struct ImageCleanup<'a> {
    smoke: &'a CoreSmoke,
    reference: String,
}

impl Drop for ImageCleanup<'_> {
    fn drop(&mut self) {
        self.smoke.best_effort(&["rmi", "--force", &self.reference]);
    }
}

struct SnapshotCleanup<'a> {
    smoke: &'a CoreSmoke,
    id: Option<String>,
}

impl Drop for SnapshotCleanup<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id.as_deref() {
            // --force: teardown must remove the snapshot even if a (leaked) restored
            // box still references it as its CoW lower, or cleanup would leak it.
            self.smoke.best_effort(&["snapshot", "rm", "--force", id]);
        }
    }
}

impl Drop for CoreSmoke {
    fn drop(&mut self) {
        self.best_effort(&["stop", "-t", "1", &self.name]);
        self.best_effort(&["rm", "-f", &self.name]);
    }
}

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

fn smoke_timeout() -> Duration {
    let seconds = std::env::var("A3S_BOX_SMOKE_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_TIMEOUT_SECS);
    Duration::from_secs(seconds)
}

fn smoke_image() -> String {
    std::env::var("A3S_BOX_SMOKE_IMAGE").unwrap_or_else(|_| DEFAULT_IMAGE.to_string())
}

fn smoke_image_tar() -> Option<String> {
    std::env::var("A3S_BOX_SMOKE_IMAGE_TAR")
        .ok()
        .or_else(|| std::env::var("A3S_BOX_TEST_ALPINE_TAR").ok())
        .filter(|path| !path.trim().is_empty())
}

fn skip_pull() -> bool {
    matches!(
        std::env::var("A3S_BOX_SMOKE_SKIP_PULL").as_deref(),
        Ok("1" | "true" | "TRUE" | "yes" | "YES")
    )
}

fn unique_name() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_millis();
    format!("core-smoke-{}-{now}", std::process::id())
}

fn unique_capture_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_nanos();
    format!("core-smoke-output-{}-{now}", std::process::id())
}

fn read_command_output(stdout_path: &Path, stderr_path: &Path) -> Result<(String, String), String> {
    let stdout = std::fs::read(stdout_path)
        .map_err(|e| format!("failed to read stdout capture file: {e}"))?;
    let stderr = std::fs::read(stderr_path)
        .map_err(|e| format!("failed to read stderr capture file: {e}"))?;
    Ok((
        String::from_utf8_lossy(&stdout).to_string(),
        String::from_utf8_lossy(&stderr).to_string(),
    ))
}

fn unused_tcp_port() -> u16 {
    let listener =
        std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral loopback TCP port");
    listener
        .local_addr()
        .expect("read ephemeral TCP port")
        .port()
}

fn read_tcp_text(port: u16) -> std::io::Result<String> {
    use std::io::{Read, Write};

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(1))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;

    let _ = stream.write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");

    let mut response = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&buf[..n]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) && !response.is_empty() =>
            {
                break;
            }
            Err(error) => return Err(error),
        }
    }

    Ok(String::from_utf8_lossy(&response).to_string())
}

fn tar_entry_text(tar_path: &Path, expected_path: &str) -> Result<String, String> {
    use std::io::Read;

    let file = std::fs::File::open(tar_path)
        .map_err(|e| format!("failed to open tar {}: {e}", tar_path.display()))?;
    let mut archive = tar::Archive::new(file);
    let expected_path = expected_path.trim_start_matches('/');

    let entries = archive
        .entries()
        .map_err(|e| format!("failed to read tar entries: {e}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("failed to read tar entry: {e}"))?;
        let path = entry
            .path()
            .map_err(|e| format!("failed to read tar entry path: {e}"))?;
        let normalized = path.to_string_lossy().trim_start_matches("./").to_string();
        if normalized == expected_path {
            let mut text = String::new();
            entry
                .read_to_string(&mut text)
                .map_err(|e| format!("failed to read tar entry {expected_path}: {e}"))?;
            return Ok(text);
        }
    }

    Err(format!(
        "tar {} did not contain {}",
        tar_path.display(),
        expected_path
    ))
}

fn assert_contains(haystack: &str, needle: &str, context: &str) {
    assert!(
        haystack.contains(needle),
        "{context} did not contain {:?}\n{}",
        needle,
        haystack
    );
}

fn json_string_field<'a>(value: &'a serde_json::Value, field: &str) -> &'a str {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("inspect JSON missing string field {field}: {value}"))
}

fn json_u64_field(value: &serde_json::Value, field: &str) -> u64 {
    value
        .get(field)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_else(|| panic!("inspect JSON missing u64 field {field}: {value}"))
}

#[cfg(unix)]
fn kill_host_pid(pid: u64, signal: libc::c_int) {
    assert!(
        pid <= libc::pid_t::MAX as u64,
        "PID {pid} does not fit into pid_t"
    );
    let rc = unsafe { libc::kill(pid as libc::pid_t, signal) };
    assert_eq!(
        rc,
        0,
        "failed to send signal {signal} to host PID {pid}: {}",
        std::io::Error::last_os_error()
    );
}

fn seed_smoke_image(smoke: &CoreSmoke, image: &str) {
    if let Some(tar_path) = smoke_image_tar() {
        smoke.ok(&["load", "--input", &tar_path, "--tag", image]);
        return;
    }

    if !skip_pull() {
        smoke.ok(&["pull", image]);
    }
}

#[test]
#[ignore]
fn real_core_lifecycle_pull_run_exec_logs_stop_rm() {
    let smoke = CoreSmoke::new();
    let image = smoke_image();

    seed_smoke_image(&smoke, &image);

    let env_file = smoke.home_path().join("smoke.env");
    std::fs::write(
        &env_file,
        "A3S_SMOKE_FROM_FILE=file\nA3S_SMOKE_OVERRIDE=file\n",
    )
    .expect("write smoke env file");
    let env_file_arg = env_file.to_string_lossy().to_string();

    smoke.ok(&[
        "run",
        "-d",
        "--name",
        &smoke.name,
        "--env-file",
        &env_file_arg,
        "--env",
        "A3S_SMOKE_OVERRIDE=cli",
        "--env",
        "A3S_SMOKE_INLINE=inline",
        "--hostname",
        "smoke-box",
        "--add-host",
        "db.local:127.0.0.2",
        &image,
        "--",
        "/bin/sh",
        "-c",
        "printf 'core-smoke-booted:%s:%s:%s\\n' \"$A3S_SMOKE_FROM_FILE\" \"$A3S_SMOKE_OVERRIDE\" \"$A3S_SMOKE_INLINE\"; sleep 3600",
    ]);

    smoke.wait_for_running();

    let exec_env = smoke.ok(&[
        "exec",
        &smoke.name,
        "--env",
        "A3S_EXEC_SMOKE=ok",
        "--",
        "/bin/sh",
        "-c",
        "printf '%s' \"$A3S_EXEC_SMOKE\"",
    ]);
    assert_eq!(exec_env.trim(), "ok");

    let hostname = smoke.ok(&[
        "exec",
        &smoke.name,
        "--",
        "/bin/sh",
        "-c",
        "cat /etc/hostname",
    ]);
    assert_eq!(hostname.trim(), "smoke-box");

    let hosts = smoke.ok(&[
        "exec",
        &smoke.name,
        "--",
        "/bin/sh",
        "-c",
        "grep -q '^127\\.0\\.0\\.2[[:space:]].*db\\.local' /etc/hosts && printf hosts-ok",
    ]);
    assert_eq!(hosts.trim(), "hosts-ok");

    let logs = smoke.wait_for_logs("core-smoke-booted:file:cli:inline");
    assert_contains(&logs, "core-smoke-booted:file:cli:inline", "box logs");

    smoke.ok(&["stop", &smoke.name]);
    let stopped = smoke.ok(&["ps", "-a"]);
    assert_contains(&stopped, &smoke.name, "ps -a after stop");
    assert_contains(&stopped, "stopped", "ps -a after stop");

    smoke.ok(&["rm", &smoke.name]);
    let after_rm = smoke.ok(&["ps", "-a"]);
    assert!(
        !after_rm.contains(&smoke.name),
        "removed box still appeared in ps -a\n{}",
        after_rm
    );
}

#[test]
#[ignore]
fn real_core_create_start_preserves_command_override() {
    let smoke = CoreSmoke::new();
    let image = smoke_image();

    seed_smoke_image(&smoke, &image);

    smoke.ok(&[
        "create",
        "--name",
        &smoke.name,
        &image,
        "--",
        "/bin/sh",
        "-c",
        "printf 'core-smoke-created-started\\n'; sleep 3600",
    ]);

    let created = smoke.ok(&["inspect", &smoke.name]);
    assert_contains(
        &created,
        "core-smoke-created-started",
        "created box inspect",
    );

    smoke.ok(&["start", &smoke.name]);
    smoke.wait_for_running();

    let logs = smoke.wait_for_logs("core-smoke-created-started");
    assert_contains(&logs, "core-smoke-created-started", "box logs");

    smoke.ok(&["stop", &smoke.name]);
    smoke.ok(&["rm", &smoke.name]);
}

#[test]
#[ignore]
fn real_core_foreground_run_returns_exit_code_and_logs() {
    let smoke = CoreSmoke::new();
    let image = smoke_image();

    seed_smoke_image(&smoke, &image);

    let result = smoke.output(&[
        "run",
        "--name",
        &smoke.name,
        &image,
        "--",
        "/bin/sh",
        "-c",
        "printf 'core-smoke-foreground\\n'; exit 7",
    ]);
    assert!(
        !result.success,
        "foreground run with exit 7 unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        result.stdout, result.stderr
    );
    assert_eq!(result.code, Some(7), "foreground run exit code");
    assert_contains(&result.stdout, "core-smoke-foreground", "foreground stdout");

    let inspect = smoke.ok(&["inspect", &smoke.name]);
    assert_contains(&inspect, "\"exit_code\": 7", "foreground inspect");

    smoke.ok(&["rm", &smoke.name]);
}

#[test]
#[ignore]
fn real_core_utility_commands_cp_top_stats() {
    let smoke = CoreSmoke::new();
    let image = smoke_image();

    seed_smoke_image(&smoke, &image);

    smoke.ok(&[
        "run",
        "-d",
        "--name",
        &smoke.name,
        &image,
        "--",
        "/bin/sh",
        "-c",
        "sleep 3600",
    ]);
    smoke.wait_for_running();

    let stats = smoke.ok(&["stats", "--no-stream", &smoke.name]);
    assert_contains(&stats, &smoke.name, "stats output");
    assert_contains(&stats, "running", "stats output");

    let top = smoke.ok(&["top", &smoke.name, "--", "-o", "pid,comm"]);
    assert_contains(&top, "PID", "top output");
    assert_contains(&top, "sleep", "top output");

    let host_src = smoke.home_path().join("host-to-box.txt");
    std::fs::write(&host_src, "cp-smoke-ok\n").expect("write host source file");
    let host_src_arg = host_src.to_string_lossy().to_string();
    let box_path = format!("{}:/tmp/a3s-box-cp-smoke.txt", smoke.name);
    smoke.ok(&["cp", &host_src_arg, &box_path]);

    let copied_in_box = smoke.ok(&[
        "exec",
        &smoke.name,
        "--",
        "/bin/sh",
        "-c",
        "cat /tmp/a3s-box-cp-smoke.txt",
    ]);
    assert_eq!(copied_in_box, "cp-smoke-ok\n");

    let host_dst = smoke.home_path().join("box-to-host.txt");
    let host_dst_arg = host_dst.to_string_lossy().to_string();
    smoke.ok(&["cp", &box_path, &host_dst_arg]);
    let copied_back = std::fs::read_to_string(&host_dst).expect("read copied-back file");
    assert_eq!(copied_back, "cp-smoke-ok\n");

    smoke.ok(&["stop", &smoke.name]);
    smoke.ok(&["rm", &smoke.name]);
}

#[test]
#[ignore]
fn real_core_published_port_http_smoke() {
    let smoke = CoreSmoke::new();
    let image = smoke_image();
    let host_port = unused_tcp_port();
    let publish = format!("{host_port}:8080");

    seed_smoke_image(&smoke, &image);

    smoke.ok(&[
        "run",
        "-d",
        "--name",
        &smoke.name,
        "-p",
        &publish,
        &image,
        "--",
        "/bin/sh",
        "-c",
        "echo core-smoke-port-listening; while true; do printf 'HTTP/1.1 200 OK\\r\\nContent-Length: 19\\r\\nConnection: close\\r\\n\\r\\ncore-smoke-port-ok\\n' | nc -l -p 8080; done",
    ]);
    smoke.wait_for_running();
    smoke.wait_for_logs("core-smoke-port-listening");

    let port_output = smoke.ok(&["port", &smoke.name]);
    assert_contains(
        &port_output,
        &format!("8080/tcp -> 0.0.0.0:{host_port}"),
        "port output",
    );

    let response = smoke.wait_for_tcp_text(host_port, "core-smoke-port-ok");
    assert_contains(&response, "core-smoke-port-ok", "published port response");

    smoke.ok(&["stop", &smoke.name]);
    smoke.ok(&["rm", &smoke.name]);
}

#[test]
#[ignore]
fn real_core_named_volume_persists_across_stop_start() {
    let smoke = CoreSmoke::new();
    let image = smoke_image();
    let volume = format!("{}-data", smoke.name);
    let mount = format!("{volume}:/data");
    let _volume_cleanup = NamedVolumeCleanup {
        smoke: &smoke,
        name: volume.clone(),
    };

    seed_smoke_image(&smoke, &image);

    smoke.ok(&["volume", "create", &volume]);
    smoke.ok(&[
        "run",
        "-d",
        "--name",
        &smoke.name,
        "-v",
        &mount,
        &image,
        "--",
        "/bin/sh",
        "-c",
        "mkdir -p /data; if [ ! -f /data/value ]; then printf first >/data/value; fi; echo core-smoke-volume-ready; sleep 3600",
    ]);
    smoke.wait_for_running();
    smoke.wait_for_logs("core-smoke-volume-ready");

    let first = smoke.ok(&[
        "exec",
        &smoke.name,
        "--",
        "/bin/sh",
        "-c",
        "cat /data/value",
    ]);
    assert_eq!(first, "first");

    smoke.ok(&[
        "exec",
        &smoke.name,
        "--",
        "/bin/sh",
        "-c",
        "printf ':live' >>/data/value",
    ]);
    smoke.ok(&["stop", &smoke.name]);

    let volume_after_stop = smoke.ok(&["volume", "ls", "--quiet"]);
    assert_contains(&volume_after_stop, &volume, "volume ls after stop");

    smoke.ok(&["start", &smoke.name]);
    smoke.wait_for_running();

    let persisted = smoke.ok(&[
        "exec",
        &smoke.name,
        "--",
        "/bin/sh",
        "-c",
        "cat /data/value",
    ]);
    assert_eq!(persisted, "first:live");

    smoke.ok(&["stop", &smoke.name]);
    smoke.ok(&["rm", &smoke.name]);

    let volume_after_rm = smoke.ok(&["volume", "ls", "--quiet"]);
    assert_contains(&volume_after_rm, &volume, "volume ls after rm");

    smoke.ok(&["volume", "rm", &volume]);
    let volume_after_remove = smoke.ok(&["volume", "ls", "--quiet"]);
    assert!(
        !volume_after_remove
            .lines()
            .any(|line| line.trim() == volume),
        "named volume still appeared after volume rm\n{}",
        volume_after_remove
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
#[ignore]
fn real_core_bridge_network_hosts_and_endpoint_lifecycle() {
    let smoke = CoreSmoke::new();
    let image = smoke_image();
    let network = format!("{}-net", smoke.name);
    let db_box = format!("{}-db", smoke.name);
    let web_box = format!("{}-web", smoke.name);
    let _network_cleanup = NetworkCleanup {
        smoke: &smoke,
        name: network.clone(),
    };
    let _db_cleanup = NamedBoxCleanup {
        smoke: &smoke,
        name: db_box.clone(),
    };
    let _web_cleanup = NamedBoxCleanup {
        smoke: &smoke,
        name: web_box.clone(),
    };

    seed_smoke_image(&smoke, &image);

    smoke.ok(&["network", "create", &network, "--subnet", "10.91.0.0/24"]);

    smoke.ok(&[
        "run",
        "-d",
        "--name",
        &db_box,
        "--network",
        &network,
        &image,
        "--",
        "/bin/sh",
        "-c",
        "echo core-smoke-bridge-db-ready; sleep 3600",
    ]);
    smoke.wait_for_named_running(&db_box);
    smoke.wait_for_named_logs(&db_box, "core-smoke-bridge-db-ready");

    smoke.ok(&[
        "run",
        "-d",
        "--name",
        &web_box,
        "--network",
        &network,
        &image,
        "--",
        "/bin/sh",
        "-c",
        "echo core-smoke-bridge-web-ready; sleep 3600",
    ]);
    smoke.wait_for_named_running(&web_box);
    smoke.wait_for_named_logs(&web_box, "core-smoke-bridge-web-ready");

    let web_hosts = smoke.ok(&["exec", &web_box, "--", "/bin/sh", "-c", "cat /etc/hosts"]);
    assert_contains(&web_hosts, &db_box, "web /etc/hosts");
    assert_contains(&web_hosts, &web_box, "web /etc/hosts");

    let inspect = smoke.ok(&["network", "inspect", &network]);
    assert_contains(&inspect, &db_box, "network inspect");
    assert_contains(&inspect, &web_box, "network inspect");
    assert_contains(&inspect, "10.91.0.2", "network inspect");
    assert_contains(&inspect, "10.91.0.3", "network inspect");

    smoke.ok(&["stop", &web_box]);
    smoke.ok(&["rm", &web_box]);
    smoke.ok(&["stop", &db_box]);
    smoke.ok(&["rm", &db_box]);
    smoke.ok(&["network", "rm", &network]);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
#[ignore]
fn real_core_network_connect_disconnect_before_start() {
    let smoke = CoreSmoke::new();
    let image = smoke_image();
    let network = format!("{}-connect-net", smoke.name);
    let force_network = format!("{}-force-net", smoke.name);
    let box_name = format!("{}-connect-box", smoke.name);
    let _network_cleanup = NetworkCleanup {
        smoke: &smoke,
        name: network.clone(),
    };
    let _force_network_cleanup = NetworkCleanup {
        smoke: &smoke,
        name: force_network.clone(),
    };
    let _box_cleanup = NamedBoxCleanup {
        smoke: &smoke,
        name: box_name.clone(),
    };

    seed_smoke_image(&smoke, &image);

    smoke.ok(&["network", "create", &network, "--subnet", "10.92.0.0/24"]);
    smoke.ok(&[
        "create",
        "--name",
        &box_name,
        &image,
        "--",
        "/bin/sh",
        "-c",
        "echo core-smoke-network-connect-ready; sleep 3600",
    ]);

    let initial = smoke.inspect_json(&box_name);
    assert_eq!(initial["network_name"], serde_json::Value::Null);
    assert_eq!(initial["network_mode"], serde_json::json!("tsi"));

    smoke.ok(&["network", "connect", &network, &box_name]);
    let connected = smoke.inspect_json(&box_name);
    assert_eq!(
        json_string_field(&connected, "network_name"),
        network.as_str()
    );
    assert_eq!(
        connected["network_mode"],
        serde_json::json!({"bridge": {"network": network.as_str()}})
    );

    smoke.ok(&["start", &box_name]);
    smoke.wait_for_named_running(&box_name);
    smoke.wait_for_named_logs(&box_name, "core-smoke-network-connect-ready");

    let hosts = smoke.ok(&["exec", &box_name, "--", "/bin/sh", "-c", "cat /etc/hosts"]);
    assert_contains(&hosts, &box_name, "connected box /etc/hosts");
    assert_contains(&hosts, "10.92.0.2", "connected box /etc/hosts");

    let inspect = smoke.ok(&["network", "inspect", &network]);
    assert_contains(&inspect, &box_name, "network inspect after connect");
    assert_contains(&inspect, "10.92.0.2", "network inspect after connect");

    smoke.fails(
        &["network", "rm", "--force", &network],
        "network hot-plug is not supported yet",
    );
    smoke.fails(
        &["network", "disconnect", &network, &box_name],
        "network hot-plug is not supported yet",
    );

    smoke.ok(&["stop", &box_name]);
    smoke.ok(&["network", "disconnect", &network, &box_name]);
    let disconnected = smoke.inspect_json(&box_name);
    assert_eq!(disconnected["network_name"], serde_json::Value::Null);
    assert_eq!(disconnected["network_mode"], serde_json::json!("tsi"));

    let inspect_after_disconnect = smoke.ok(&["network", "inspect", &network]);
    assert!(
        !inspect_after_disconnect.contains(&box_name),
        "disconnected box still appeared in network inspect\n{}",
        inspect_after_disconnect
    );

    smoke.ok(&[
        "network",
        "create",
        &force_network,
        "--subnet",
        "10.93.0.0/24",
    ]);
    smoke.ok(&["network", "connect", &force_network, &box_name]);
    let force_connected = smoke.inspect_json(&box_name);
    assert_eq!(
        json_string_field(&force_connected, "network_name"),
        force_network.as_str()
    );
    smoke.ok(&["network", "rm", "--force", &force_network]);
    let force_disconnected = smoke.inspect_json(&box_name);
    assert_eq!(force_disconnected["network_name"], serde_json::Value::Null);
    assert_eq!(force_disconnected["network_mode"], serde_json::json!("tsi"));

    smoke.ok(&["start", &box_name]);
    smoke.wait_for_named_running(&box_name);
    let restarted = smoke.ok(&[
        "exec",
        &box_name,
        "--",
        "/bin/sh",
        "-c",
        "printf force-rm-restarted",
    ]);
    assert_eq!(restarted, "force-rm-restarted");
    smoke.ok(&["stop", &box_name]);

    smoke.ok(&["rm", &box_name]);
    smoke.ok(&["network", "rm", &network]);
}

#[test]
#[ignore]
fn real_core_filesystem_image_snapshot_commands() {
    let smoke = CoreSmoke::new();
    let image = smoke_image();
    let committed_image = format!("{}-committed:latest", smoke.name);
    let committed_box = format!("{}-committed-run", smoke.name);
    let restored_box = format!("{}-restored", smoke.name);
    let snapshot_name = format!("{}-snapshot", smoke.name);
    let _committed_image_cleanup = ImageCleanup {
        smoke: &smoke,
        reference: committed_image.clone(),
    };
    let _committed_box_cleanup = NamedBoxCleanup {
        smoke: &smoke,
        name: committed_box.clone(),
    };
    let _restored_box_cleanup = NamedBoxCleanup {
        smoke: &smoke,
        name: restored_box.clone(),
    };
    let mut snapshot_cleanup = SnapshotCleanup {
        smoke: &smoke,
        id: None,
    };

    seed_smoke_image(&smoke, &image);

    smoke.ok(&[
        "run",
        "-d",
        "--name",
        &smoke.name,
        &image,
        "--",
        "/bin/sh",
        "-c",
        "sleep 3600",
    ]);
    smoke.wait_for_running();

    smoke.ok(&[
        "exec",
        &smoke.name,
        "--",
        "/bin/sh",
        "-c",
        "printf core-smoke-storage-ok >/tmp/core-smoke-storage.txt",
    ]);

    let diff = smoke.ok(&["diff", &smoke.name]);
    assert_contains(&diff, "A /tmp/core-smoke-storage.txt", "diff output");

    let export_tar = smoke.home_path().join("core-smoke-export.tar");
    let export_tar_arg = export_tar.to_string_lossy().to_string();
    smoke.ok(&["export", &smoke.name, "--output", &export_tar_arg]);
    let exported_text =
        tar_entry_text(&export_tar, "/tmp/core-smoke-storage.txt").expect("read exported file");
    assert_eq!(exported_text, "core-smoke-storage-ok");

    let commit = smoke.ok(&[
        "commit",
        &smoke.name,
        &committed_image,
        "--message",
        "core smoke commit",
        "--change",
        "CMD cat /tmp/core-smoke-storage.txt",
    ]);
    assert_contains(&commit, "sha256:", "commit output");

    let committed_run = smoke.output(&["run", "--rm", "--name", &committed_box, &committed_image]);
    assert!(
        committed_run.success,
        "committed image run failed\nstdout:\n{}\nstderr:\n{}",
        committed_run.stdout, committed_run.stderr
    );
    assert_contains(
        &committed_run.stdout,
        "core-smoke-storage-ok",
        "committed image run",
    );

    let snapshot_id = smoke
        .ok(&[
            "snapshot",
            "create",
            &smoke.name,
            "--name",
            &snapshot_name,
            "--description",
            "core smoke snapshot",
        ])
        .trim()
        .to_string();
    assert!(
        snapshot_id.starts_with("snap-"),
        "unexpected snapshot id: {}",
        snapshot_id
    );
    snapshot_cleanup.id = Some(snapshot_id.clone());

    let snapshot_inspect = smoke.ok(&["snapshot", "inspect", &snapshot_id]);
    assert_contains(&snapshot_inspect, &snapshot_name, "snapshot inspect");

    let restored_id = smoke
        .ok(&["snapshot", "restore", &snapshot_id, "--name", &restored_box])
        .trim()
        .to_string();
    assert!(
        !restored_id.is_empty(),
        "snapshot restore did not print a box id"
    );

    smoke.ok(&["start", &restored_box]);
    smoke.wait_for_named_running(&restored_box);
    let restored_text = smoke.ok(&[
        "exec",
        &restored_box,
        "--",
        "/bin/sh",
        "-c",
        "cat /tmp/core-smoke-storage.txt",
    ]);
    assert_eq!(restored_text, "core-smoke-storage-ok");

    smoke.ok(&["stop", &restored_box]);
    smoke.ok(&["rm", &restored_box]);
    smoke.ok(&["snapshot", "rm", &snapshot_id]);
    snapshot_cleanup.id = None;

    smoke.ok(&["stop", &smoke.name]);
    smoke.ok(&["rm", &smoke.name]);
}

#[test]
#[ignore]
fn real_core_snapshot_cow_isolation_and_rm_guard() {
    // Copy-on-write snapshot restore: forks share the snapshot's rootfs as a
    // read-only overlay lower, each writing to its own upper. Verifies (1) fork
    // isolation — a write in one fork is invisible to another, (2) the shared
    // lower stays pristine, and (3) `snapshot rm` refuses while a fork still
    // references the snapshot, then succeeds once the forks are gone.
    let smoke = CoreSmoke::new();
    let image = smoke_image();
    let base = format!("{}-cow-base", smoke.name);
    let fork_a = format!("{}-cow-a", smoke.name);
    let fork_b = format!("{}-cow-b", smoke.name);
    let snapshot_name = format!("{}-cow-snap", smoke.name);
    let _base_cleanup = NamedBoxCleanup {
        smoke: &smoke,
        name: base.clone(),
    };
    let _fork_a_cleanup = NamedBoxCleanup {
        smoke: &smoke,
        name: fork_a.clone(),
    };
    let _fork_b_cleanup = NamedBoxCleanup {
        smoke: &smoke,
        name: fork_b.clone(),
    };
    let mut snapshot_cleanup = SnapshotCleanup {
        smoke: &smoke,
        id: None,
    };

    seed_smoke_image(&smoke, &image);

    // Warm a base, write a marker, snapshot it.
    smoke.ok(&[
        "run",
        "-d",
        "--name",
        &base,
        &image,
        "--",
        "/bin/sh",
        "-c",
        "sleep 3600",
    ]);
    smoke.wait_for_named_running(&base);
    smoke.ok(&[
        "exec",
        &base,
        "--",
        "/bin/sh",
        "-c",
        "printf BASE >/tmp/cow-shared.txt",
    ]);
    let snapshot_id = smoke
        .ok(&["snapshot", "create", &base, "--name", &snapshot_name])
        .trim()
        .to_string();
    assert!(
        snapshot_id.starts_with("snap-"),
        "snapshot id: {snapshot_id}"
    );
    snapshot_cleanup.id = Some(snapshot_id.clone());
    smoke.ok(&["stop", &base]);
    smoke.ok(&["rm", &base]);

    // Fork A: sees the warmed state, then writes to its own upper.
    smoke.ok(&["snapshot", "restore", &snapshot_id, "--name", &fork_a]);
    smoke.ok(&["start", &fork_a]);
    smoke.wait_for_named_running(&fork_a);
    assert_eq!(
        smoke.ok(&[
            "exec",
            &fork_a,
            "--",
            "/bin/sh",
            "-c",
            "cat /tmp/cow-shared.txt"
        ]),
        "BASE",
        "fork A should see the warmed snapshot state"
    );
    smoke.ok(&[
        "exec",
        &fork_a,
        "--",
        "/bin/sh",
        "-c",
        "printf A-WROTE >>/tmp/cow-shared.txt",
    ]);

    // rm guard: the snapshot is still referenced by fork A -> must be refused.
    let guarded = smoke.output(&["snapshot", "rm", &snapshot_id]);
    assert!(
        !guarded.success,
        "snapshot rm must be refused while a fork references it\nstdout:\n{}\nstderr:\n{}",
        guarded.stdout, guarded.stderr
    );
    assert_contains(&guarded.stderr, "still used", "rm guard message");
    smoke.ok(&["snapshot", "inspect", &snapshot_id]); // still present

    // Fork B from the SAME snapshot must NOT see fork A's write (isolation +
    // pristine shared lower).
    smoke.ok(&["snapshot", "restore", &snapshot_id, "--name", &fork_b]);
    smoke.ok(&["start", &fork_b]);
    smoke.wait_for_named_running(&fork_b);
    assert_eq!(
        smoke.ok(&[
            "exec",
            &fork_b,
            "--",
            "/bin/sh",
            "-c",
            "cat /tmp/cow-shared.txt"
        ]),
        "BASE",
        "fork B must not see fork A's write (CoW isolation)"
    );

    // Remove the forks, then `snapshot rm` succeeds (guard clears).
    smoke.ok(&["stop", &fork_a]);
    smoke.ok(&["rm", &fork_a]);
    smoke.ok(&["stop", &fork_b]);
    smoke.ok(&["rm", &fork_b]);
    smoke.ok(&["snapshot", "rm", &snapshot_id]);
    snapshot_cleanup.id = None;
}

#[cfg(unix)]
#[test]
#[ignore]
fn real_core_restart_policy_monitor_recovers_dead_box() {
    let smoke = CoreSmoke::new();
    let image = smoke_image();

    seed_smoke_image(&smoke, &image);

    smoke.ok(&[
        "run",
        "-d",
        "--name",
        &smoke.name,
        "--restart",
        "on-failure:2",
        &image,
        "--",
        "/bin/sh",
        "-c",
        "echo core-smoke-monitor-boot; sleep 3600",
    ]);
    smoke.wait_for_running();
    smoke.wait_for_logs("core-smoke-monitor-boot");

    let initial = smoke.inspect_json(&smoke.name);
    assert_eq!(json_string_field(&initial, "restart_policy"), "on-failure");
    assert_eq!(json_u64_field(&initial, "max_restart_count"), 2);
    assert_eq!(json_u64_field(&initial, "restart_count"), 0);
    let first_pid = json_u64_field(&initial, "pid");

    kill_host_pid(first_pid, libc::SIGKILL);
    let dead = smoke.wait_for_named_status(&smoke.name, "dead");
    assert_eq!(json_string_field(&dead, "status"), "dead");

    let mut monitor = smoke.spawn_background(&["monitor", "--interval", "1"]);
    let restarted = smoke.wait_for_named_restart(&mut monitor, &smoke.name, first_pid, 1);
    let monitor_output = monitor.kill_and_output();
    let combined_monitor_output = format!("{}\n{}", monitor_output.stdout, monitor_output.stderr);
    assert_contains(
        &combined_monitor_output,
        "a3s-box monitor started",
        "monitor output",
    );

    assert_eq!(json_string_field(&restarted, "status"), "running");
    assert_eq!(json_u64_field(&restarted, "restart_count"), 1);
    let second_pid = json_u64_field(&restarted, "pid");
    assert_ne!(second_pid, first_pid, "monitor reused the killed host PID");

    let exec = smoke.ok(&[
        "exec",
        &smoke.name,
        "--",
        "/bin/sh",
        "-c",
        "printf core-smoke-monitor-restarted",
    ]);
    assert_eq!(exec, "core-smoke-monitor-restarted");

    smoke.ok(&["stop", &smoke.name]);
    smoke.ok(&["rm", &smoke.name]);
}

#[test]
#[ignore]
fn real_core_pause_unpause_kill_wait() {
    let smoke = CoreSmoke::new();
    let image = smoke_image();

    seed_smoke_image(&smoke, &image);

    smoke.ok(&[
        "run",
        "-d",
        "--name",
        &smoke.name,
        &image,
        "--",
        "/bin/sh",
        "-c",
        "sleep 3600",
    ]);
    smoke.wait_for_running();

    smoke.ok(&["pause", &smoke.name]);
    let paused = smoke.ok(&["ps", "-a"]);
    assert_contains(&paused, &smoke.name, "ps after pause");
    assert_contains(&paused, "paused", "ps after pause");

    smoke.ok(&["unpause", &smoke.name]);
    smoke.wait_for_running();

    smoke.ok(&["kill", "--signal", "STOP", &smoke.name]);
    let stopped_by_signal = smoke.ok(&["ps", "-a"]);
    assert_contains(&stopped_by_signal, "paused", "ps after SIGSTOP");

    smoke.ok(&["kill", "--signal", "CONT", &smoke.name]);
    smoke.wait_for_running();

    smoke.ok(&["kill", "--signal", "KILL", &smoke.name]);
    let wait = smoke.ok(&["wait", &smoke.name]);
    assert_eq!(wait.trim(), "137");

    let stopped = smoke.ok(&["ps", "-a"]);
    assert_contains(&stopped, &smoke.name, "ps after SIGKILL");
    assert_contains(&stopped, "stopped", "ps after SIGKILL");

    smoke.ok(&["rm", &smoke.name]);
}

#[test]
#[ignore]
fn real_core_compose_single_service_lifecycle() {
    let smoke = CoreSmoke::new();
    let image = smoke_image();
    let project = format!("{}-compose", smoke.name);
    let service = "worker";
    let service_box = format!("{project}-{service}");

    seed_smoke_image(&smoke, &image);

    let compose_dir = smoke.home_path().join("compose");
    std::fs::create_dir_all(&compose_dir).expect("create compose dir");
    let compose_file = compose_dir.join("compose.yaml");
    std::fs::write(
        &compose_file,
        format!(
            r#"services:
  {service}:
    image: {image}
    command: ["/bin/sh", "-c", "printf 'core-smoke-compose:%s\n' \"$A3S_COMPOSE_SMOKE\"; sleep 3600"]
    environment:
      A3S_COMPOSE_SMOKE: "ok"
    labels:
      purpose: core-smoke
"#,
        ),
    )
    .expect("write compose file");
    let compose_file_arg = compose_file.to_string_lossy().to_string();
    let _cleanup = ComposeCleanup {
        smoke: &smoke,
        compose_file: compose_file_arg.clone(),
        project: project.clone(),
        service_boxes: vec![service_box.clone()],
        remove_volumes: false,
    };

    let config = smoke.ok(&[
        "compose",
        "--file",
        &compose_file_arg,
        "--project-name",
        &project,
        "config",
    ]);
    assert_contains(&config, "Configuration is valid.", "compose config");

    smoke.ok(&[
        "compose",
        "--file",
        &compose_file_arg,
        "--project-name",
        &project,
        "up",
        "--detach",
    ]);
    smoke.wait_for_named_running(&service_box);

    let ps = smoke.ok(&[
        "compose",
        "--file",
        &compose_file_arg,
        "--project-name",
        &project,
        "ps",
    ]);
    assert_contains(&ps, service, "compose ps");
    assert_contains(&ps, "running", "compose ps");

    let logs = smoke.ok(&[
        "compose",
        "--file",
        &compose_file_arg,
        "--project-name",
        &project,
        "logs",
        "--tail",
        "50",
        service,
    ]);
    assert_contains(&logs, "core-smoke-compose:ok", "compose logs");

    let env_value = smoke.ok(&[
        "exec",
        &service_box,
        "--",
        "/bin/sh",
        "-c",
        "printf '%s' \"$A3S_COMPOSE_SMOKE\"",
    ]);
    assert_eq!(env_value, "ok");

    smoke.ok(&[
        "compose",
        "--file",
        &compose_file_arg,
        "--project-name",
        &project,
        "down",
    ]);
    let after_down = smoke.ok(&["ps", "-a"]);
    assert!(
        !after_down.contains(&service_box),
        "compose service box still appeared after down\n{}",
        after_down
    );
}

#[test]
#[ignore]
fn real_core_compose_multi_service_health_and_volume_cleanup() {
    let smoke = CoreSmoke::new();
    let image = smoke_image();
    let project = format!("{}-multi", smoke.name);
    let db_service = "db";
    let web_service = "web";
    let db_box = format!("{project}-{db_service}");
    let web_box = format!("{project}-{web_service}");
    let volume = format!("{project}-shared");

    seed_smoke_image(&smoke, &image);

    let compose_dir = smoke.home_path().join("compose-multi");
    std::fs::create_dir_all(&compose_dir).expect("create compose dir");
    let compose_file = compose_dir.join("compose.yaml");
    std::fs::write(
        &compose_file,
        format!(
            r#"services:
  {db_service}:
    image: {image}
    command: ["/bin/sh", "-c", "mkdir -p /data; printf db-volume-ok >/data/from-db; echo core-smoke-db-ready; touch /tmp/healthy; sleep 3600"]
    volumes:
      - "{volume}:/data"
    healthcheck:
      test: ["CMD-SHELL", "test -f /tmp/healthy && grep -q db-volume-ok /data/from-db"]
      interval: 1s
      timeout: 2s
      retries: 10
  {web_service}:
    image: {image}
    depends_on:
      {db_service}:
        condition: service_healthy
    command: ["/bin/sh", "-c", "printf core-smoke-web-saw:; cat /data/from-db; echo; sleep 3600"]
    volumes:
      - "{volume}:/data"
volumes:
  {volume}:
"#,
        ),
    )
    .expect("write compose file");
    let compose_file_arg = compose_file.to_string_lossy().to_string();
    let _cleanup = ComposeCleanup {
        smoke: &smoke,
        compose_file: compose_file_arg.clone(),
        project: project.clone(),
        service_boxes: vec![db_box.clone(), web_box.clone()],
        remove_volumes: true,
    };

    let config = smoke.ok(&[
        "compose",
        "--file",
        &compose_file_arg,
        "--project-name",
        &project,
        "config",
    ]);
    assert_contains(&config, "Boot order: db", "compose multi config");
    assert_contains(&config, "web", "compose multi config");

    smoke.ok(&[
        "compose",
        "--file",
        &compose_file_arg,
        "--project-name",
        &project,
        "up",
        "--detach",
        "--timeout",
        "60",
    ]);
    smoke.wait_for_named_running(&db_box);
    smoke.wait_for_named_running(&web_box);

    let ps = smoke.ok(&[
        "compose",
        "--file",
        &compose_file_arg,
        "--project-name",
        &project,
        "ps",
    ]);
    assert_contains(&ps, db_service, "compose multi ps");
    assert_contains(&ps, web_service, "compose multi ps");
    assert_contains(&ps, "healthy", "compose multi ps");

    let web_logs = smoke.wait_for_named_logs(&web_box, "core-smoke-web-saw:db-volume-ok");
    assert_contains(
        &web_logs,
        "core-smoke-web-saw:db-volume-ok",
        "compose web logs",
    );

    let compose_logs = smoke.ok(&[
        "compose",
        "--file",
        &compose_file_arg,
        "--project-name",
        &project,
        "logs",
        "--tail",
        "50",
    ]);
    assert_contains(&compose_logs, "core-smoke-db-ready", "compose logs");
    assert_contains(
        &compose_logs,
        "core-smoke-web-saw:db-volume-ok",
        "compose logs",
    );

    let volumes = smoke.ok(&["volume", "ls", "--quiet"]);
    assert_contains(&volumes, &volume, "volume ls after compose up");

    smoke.ok(&[
        "compose",
        "--file",
        &compose_file_arg,
        "--project-name",
        &project,
        "down",
        "--volumes",
    ]);
    let after_down = smoke.ok(&["ps", "-a"]);
    assert!(
        !after_down.contains(&db_box) && !after_down.contains(&web_box),
        "compose service boxes still appeared after down\n{}",
        after_down
    );
    let volumes_after_down = smoke.ok(&["volume", "ls", "--quiet"]);
    assert!(
        !volumes_after_down.lines().any(|line| line.trim() == volume),
        "compose volume still appeared after down -v\n{}",
        volumes_after_down
    );
}

#[cfg(unix)]
#[test]
#[ignore]
fn real_core_interactive_pty_commands() {
    let smoke = CoreSmoke::new();
    let image = smoke_image();

    seed_smoke_image(&smoke, &image);

    let run_box = format!("{}-tty-run", smoke.name);
    let _run_cleanup = NamedBoxCleanup {
        smoke: &smoke,
        name: run_box.clone(),
    };
    let run = smoke.tty_output(
        &[
            "run",
            "--rm",
            "-it",
            "--name",
            &run_box,
            "--env",
            "A3S_TTY_SMOKE=run",
            &image,
            "--",
            "/bin/sh",
            "-c",
            "printf 'core-smoke-run-pty:%s\\n' \"$A3S_TTY_SMOKE\"",
        ],
        b"",
    );
    assert!(
        run.success,
        "interactive run failed\noutput:\n{}",
        run.stdout
    );
    assert_contains(&run.stdout, "core-smoke-run-pty:run", "interactive run");

    smoke.ok(&[
        "run",
        "-d",
        "--name",
        &smoke.name,
        "--env",
        "A3S_TTY_SMOKE=detached",
        &image,
        "--",
        "/bin/sh",
        "-c",
        "sleep 3600",
    ]);
    smoke.wait_for_running();

    let exec = smoke.tty_output(
        &[
            "exec",
            "-it",
            &smoke.name,
            "--env",
            "A3S_EXEC_TTY=ok",
            "--",
            "/bin/sh",
            "-c",
            "printf 'core-smoke-exec-pty:%s:%s\\n' \"$A3S_TTY_SMOKE\" \"$A3S_EXEC_TTY\"",
        ],
        b"",
    );
    assert!(
        exec.success,
        "interactive exec failed\noutput:\n{}",
        exec.stdout
    );
    assert_contains(
        &exec.stdout,
        "core-smoke-exec-pty:detached:ok",
        "interactive exec",
    );

    let attach = smoke.tty_output(
        &["attach", "-it", &smoke.name],
        b"printf 'core-smoke-attach-pty:%s\\n' \"$A3S_TTY_SMOKE\"; exit\n",
    );
    assert!(
        attach.success,
        "interactive attach failed\noutput:\n{}",
        attach.stdout
    );
    assert_contains(
        &attach.stdout,
        "core-smoke-attach-pty:detached",
        "interactive attach",
    );

    smoke.ok(&["stop", &smoke.name]);
    smoke.ok(&["rm", &smoke.name]);
}
