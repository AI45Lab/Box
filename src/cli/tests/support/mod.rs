#![allow(dead_code)]

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use sha2::{Digest, Sha256};

pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(300);
pub const HOST_SMOKE_IMAGE_ENV: &str = "A3S_BOX_HOST_SMOKE_IMAGE";
pub const HOST_SMOKE_TIMEOUT_SECS_ENV: &str = "A3S_BOX_HOST_SMOKE_TIMEOUT_SECS";
pub const TEST_ALPINE_TAR_ENV: &str = "A3S_BOX_TEST_ALPINE_TAR";

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

pub struct CliTest {
    bin: PathBuf,
    home: tempfile::TempDir,
}

impl CliTest {
    pub fn new() -> Self {
        Self {
            bin: find_binary(),
            home: tempfile::tempdir().expect("temp A3S_HOME"),
        }
    }

    pub fn home_path(&self) -> &Path {
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

    pub fn output(&self, args: &[&str]) -> (String, String, bool) {
        self.output_with_env(args, &[])
    }

    pub fn output_with_env(&self, args: &[&str], envs: &[(&str, &str)]) -> (String, String, bool) {
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

    /// Spawn `a3s-box <args>` as a background process (e.g. a daemon), inheriting
    /// the test env plus `A3S_HOME`. The caller owns the `Child` and must kill it.
    pub fn spawn_background(&self, args: &[&str]) -> std::process::Child {
        eprintln!("    $ a3s-box {} &", args.join(" "));
        self.command(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| {
                panic!(
                    "failed to spawn background `a3s-box {}`: {e}",
                    args.join(" ")
                )
            })
    }

    pub fn output_with_stdin(&self, args: &[&str], stdin: &[u8]) -> (String, String, bool) {
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

    pub fn ok(&self, args: &[&str]) -> String {
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

    pub fn ok_with_stdin(&self, args: &[&str], stdin: &[u8]) -> String {
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

    pub fn ok_with_env(&self, args: &[&str], envs: &[(&str, &str)]) -> String {
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

    pub fn ok_status(&self, args: &[&str]) {
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

    pub fn fails(&self, args: &[&str], expected: &str) {
        self.fails_with_env(args, &[], expected);
    }

    pub fn fails_with_env(&self, args: &[&str], envs: &[(&str, &str)], expected: &str) {
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

    pub fn runs_until_timeout(&self, args: &[&str], timeout: Duration) -> (String, String) {
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

    pub fn interrupts_after_output(
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

pub struct ErrorRegistry {
    addr: String,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ErrorRegistry {
    pub fn start() -> Self {
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

    pub fn addr(&self) -> &str {
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

pub fn wait_for_running(cli: &CliTest, name: &str, timeout: Duration) {
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

pub fn cleanup(cli: &CliTest, name: &str) {
    let _ = cli.output(&["stop", name]);
    let _ = cli.output(&["rm", "-f", name]);
}

pub fn unique_tag(prefix: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_millis();
    format!("{prefix}-{}-{now}", std::process::id())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn assert_json_array_contains(array: &serde_json::Value, expected: &str) {
    let values = array
        .as_array()
        .unwrap_or_else(|| panic!("expected JSON array, got {array}"));
    assert!(
        values.iter().any(|value| value.as_str() == Some(expected)),
        "expected JSON array to contain {expected:?}, got {array}"
    );
}

pub fn read_file_from_saved_oci_tar(path: &Path, file_path: &str) -> Option<String> {
    let extract = tempfile::tempdir().expect("extract saved OCI image");
    let file = std::fs::File::open(path).expect("open saved OCI tar");
    let mut archive = tar::Archive::new(file);
    archive
        .unpack(extract.path())
        .expect("unpack saved OCI image");

    let index = read_json_file(&extract.path().join("index.json"));
    let manifest_digest = index["manifests"][0]["digest"]
        .as_str()
        .expect("saved OCI index manifest digest");
    let manifest = read_json_file(&oci_blob_path(extract.path(), manifest_digest));
    let layers = manifest["layers"]
        .as_array()
        .expect("saved OCI manifest layers");
    let expected = normalize_archive_path(file_path);

    for layer in layers {
        let digest = layer["digest"].as_str().expect("saved OCI layer digest");
        let layer_file = std::fs::File::open(oci_blob_path(extract.path(), digest))
            .expect("open saved OCI layer blob");
        let decoder = GzDecoder::new(layer_file);
        let mut layer_archive = tar::Archive::new(decoder);
        let entries = layer_archive.entries().expect("read saved OCI layer");
        for entry in entries {
            let mut entry = entry.expect("read saved OCI layer entry");
            let path = entry.path().expect("saved OCI layer entry path");
            if normalize_archive_path(&path.to_string_lossy()) == expected {
                let mut content = String::new();
                entry
                    .read_to_string(&mut content)
                    .expect("read saved OCI layer file");
                return Some(content);
            }
        }
    }

    None
}

fn read_json_file(path: &Path) -> serde_json::Value {
    let content = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read JSON file {}: {e}", path.display()));
    serde_json::from_str(&content)
        .unwrap_or_else(|e| panic!("parse JSON file {}: {e}", path.display()))
}

fn oci_blob_path(root: &Path, digest: &str) -> PathBuf {
    let (algorithm, hex) = digest.split_once(':').unwrap_or(("sha256", digest));
    root.join("blobs").join(algorithm).join(hex)
}

fn normalize_archive_path(path: &str) -> String {
    path.trim_start_matches("./")
        .trim_start_matches('/')
        .to_string()
}

#[cfg(target_os = "linux")]
pub fn is_root_user() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .is_some_and(|uid| uid.trim() == "0")
}

pub fn create_minimal_oci_tar(path: &Path, reference: &str) {
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

pub fn seed_runnable_alpine_image(cli: &CliTest, image: &str) {
    if let Ok(tar_path) = std::env::var(TEST_ALPINE_TAR_ENV) {
        cli.ok(&["load", "--input", &tar_path, "--tag", image]);
        return;
    }

    cli.ok(&["pull", image]);
}

pub fn host_smoke_image() -> String {
    std::env::var(HOST_SMOKE_IMAGE_ENV)
        .unwrap_or_else(|_| "docker.io/library/alpine:latest".to_string())
}

pub fn host_smoke_timeout(default_secs: u64) -> Duration {
    std::env::var(HOST_SMOKE_TIMEOUT_SECS_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(default_secs))
}

#[cfg(unix)]
pub fn host_socket_dirs() -> std::collections::BTreeSet<PathBuf> {
    #[cfg(target_os = "macos")]
    let socket_root = Path::new("/private/tmp/a3s-box-sockets");
    #[cfg(not(target_os = "macos"))]
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
pub fn host_socket_dirs() -> std::collections::BTreeSet<PathBuf> {
    std::collections::BTreeSet::new()
}

pub fn assert_no_new_host_socket_dirs(before: &std::collections::BTreeSet<PathBuf>) {
    let after = host_socket_dirs();
    let new_dirs: Vec<_> = after.difference(before).collect();
    assert!(
        new_dirs.is_empty(),
        "new host socket directories were left behind: {new_dirs:?}"
    );
}
