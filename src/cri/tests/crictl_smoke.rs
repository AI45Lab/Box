#![cfg(unix)]

//! Opt-in CRI smoke test driven through `crictl`.
//!
//! Run manually on a Linux/macOS host that can start A3S Box microVMs.
//! The smoke flow starts one pod sandbox with two containers:
//!
//! ```ignore
//! A3S_BOX_CRI_SMOKE=1 \
//! cargo test -p a3s-box-cri --test crictl_smoke -- --ignored --nocapture
//! ```
//!
//! Optional environment:
//! - `A3S_BOX_CRI_CRICTL`: path to `crictl` (default: `crictl` from PATH)
//! - `A3S_BOX_CRI_SMOKE_IMAGE`: container workload image (default: `busybox:latest`)
//! - `A3S_BOX_CRI_SMOKE_AGENT_IMAGE`: sandbox agent image
//! - `A3S_BOX_CRI_SMOKE_IMAGE_DIR`: reusable A3S image store directory
//! - `A3S_BOX_CRI_SMOKE_SKIP_PULL=1`: use an image already present in the store

use std::error::Error;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const SMOKE_ENABLED_ENV: &str = "A3S_BOX_CRI_SMOKE";
const CRICTL_ENV: &str = "A3S_BOX_CRI_CRICTL";
const WORKLOAD_IMAGE_ENV: &str = "A3S_BOX_CRI_SMOKE_IMAGE";
const AGENT_IMAGE_ENV: &str = "A3S_BOX_CRI_SMOKE_AGENT_IMAGE";
const IMAGE_DIR_ENV: &str = "A3S_BOX_CRI_SMOKE_IMAGE_DIR";
const SKIP_PULL_ENV: &str = "A3S_BOX_CRI_SMOKE_SKIP_PULL";
const DEFAULT_WORKLOAD_IMAGE: &str = "busybox:latest";
const DEFAULT_AGENT_IMAGE: &str = "ghcr.io/a3s-box/code:v0.1.0";
const LOG_MARKER_ONE: &str = "a3s-cri-smoke-one-ready";
const LOG_MARKER_TWO: &str = "a3s-cri-smoke-two-ready";

#[test]
#[ignore = "requires crictl, a CRI-capable host, network/image availability, and microVM support"]
fn test_crictl_multi_container_pod_smoke() -> Result<(), Box<dyn Error>> {
    if std::env::var(SMOKE_ENABLED_ENV).ok().as_deref() != Some("1") {
        eprintln!("skipping crictl smoke; set {SMOKE_ENABLED_ENV}=1 to run the host-backed suite");
        return Ok(());
    }

    let crictl = std::env::var(CRICTL_ENV).unwrap_or_else(|_| "crictl".to_string());
    require_crictl(&crictl)?;

    let cri_bin = cri_binary_path();
    let tmp = tempfile::Builder::new().prefix("a3s-cri-smoke").tempdir()?;
    let socket_path = tmp.path().join("a3s-box.sock");
    let image_dir = std::env::var_os(IMAGE_DIR_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| tmp.path().join("images"));
    let log_dir = tmp.path().join("logs");
    std::fs::create_dir_all(&image_dir)?;
    std::fs::create_dir_all(&log_dir)?;

    let agent_image = std::env::var(AGENT_IMAGE_ENV).unwrap_or_else(|_| DEFAULT_AGENT_IMAGE.into());
    let workload_image =
        std::env::var(WORKLOAD_IMAGE_ENV).unwrap_or_else(|_| DEFAULT_WORKLOAD_IMAGE.into());

    let mut server = ChildGuard::spawn(
        Command::new(&cri_bin)
            .arg("--socket")
            .arg(&socket_path)
            .arg("--image-dir")
            .arg(&image_dir)
            .arg("--agent-image")
            .arg(&agent_image)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit()),
    )?;
    wait_for_cri(&crictl, &socket_path, Duration::from_secs(30))?;

    let pod_config = tmp.path().join("pod.json");
    let first_container_config = tmp.path().join("container-one.json");
    let second_container_config = tmp.path().join("container-two.json");
    write_pod_config(&pod_config, &log_dir)?;
    write_container_config(
        &first_container_config,
        &workload_image,
        "a3s-cri-smoke-container-one",
        "container-one.log",
        LOG_MARKER_ONE,
    )?;
    write_container_config(
        &second_container_config,
        &workload_image,
        "a3s-cri-smoke-container-two",
        "container-two.log",
        LOG_MARKER_TWO,
    )?;

    let mut sandbox_id = String::new();
    let mut container_ids = Vec::new();

    let flow_result = (|| -> Result<(), Box<dyn Error>> {
        if std::env::var(SKIP_PULL_ENV).ok().as_deref() != Some("1") {
            run_crictl(&crictl, &socket_path, &["pull", &workload_image])?;
        }

        sandbox_id = last_nonempty_line(&run_crictl(
            &crictl,
            &socket_path,
            &["runp", path_str(&pod_config)?],
        )?)
        .ok_or("crictl runp did not return a sandbox id")?
        .to_string();

        let first_create_args = [
            "create",
            sandbox_id.as_str(),
            path_str(&first_container_config)?,
            path_str(&pod_config)?,
        ];
        let first_container_id =
            last_nonempty_line(&run_crictl(&crictl, &socket_path, &first_create_args)?)
                .ok_or("crictl create did not return a container id")?
                .to_string();
        container_ids.push(first_container_id.clone());

        let second_create_args = [
            "create",
            sandbox_id.as_str(),
            path_str(&second_container_config)?,
            path_str(&pod_config)?,
        ];
        let second_container_id =
            last_nonempty_line(&run_crictl(&crictl, &socket_path, &second_create_args)?)
                .ok_or("second crictl create did not return a container id")?
                .to_string();
        container_ids.push(second_container_id.clone());

        assert_ne!(
            first_container_id, second_container_id,
            "multi-container smoke expected distinct container ids"
        );

        run_crictl(
            &crictl,
            &socket_path,
            &["start", first_container_id.as_str()],
        )?;
        run_crictl(
            &crictl,
            &socket_path,
            &["start", second_container_id.as_str()],
        )?;

        let logs = wait_for_logs(&crictl, &socket_path, &first_container_id, LOG_MARKER_ONE)?;
        assert!(
            logs.contains(LOG_MARKER_ONE),
            "first container logs did not contain smoke marker; logs:\n{logs}"
        );

        let logs = wait_for_logs(&crictl, &socket_path, &second_container_id, LOG_MARKER_TWO)?;
        assert!(
            logs.contains(LOG_MARKER_TWO),
            "second container logs did not contain smoke marker; logs:\n{logs}"
        );

        let pod_status = run_crictl(&crictl, &socket_path, &["inspectp", sandbox_id.as_str()])?;
        assert!(
            pod_status.contains("a3s-cri-smoke-pod"),
            "inspectp output did not include smoke pod metadata; output:\n{pod_status}"
        );

        Ok(())
    })();

    for container_id in container_ids.iter().rev() {
        let _ = run_crictl(&crictl, &socket_path, &["stop", container_id.as_str()]);
        let _ = run_crictl(&crictl, &socket_path, &["rm", container_id.as_str()]);
    }
    if !sandbox_id.is_empty() {
        let _ = run_crictl(&crictl, &socket_path, &["stopp", sandbox_id.as_str()]);
        let _ = run_crictl(&crictl, &socket_path, &["rmp", sandbox_id.as_str()]);
    }

    server.stop();
    flow_result
}

struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    fn spawn(command: &mut Command) -> Result<Self, Box<dyn Error>> {
        let child = command.spawn()?;
        Ok(Self { child: Some(child) })
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.stop();
    }
}

fn cri_binary_path() -> PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_a3s-box-cri") {
        return PathBuf::from(path);
    }

    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.push("target");
    path.push("debug");
    path.push("a3s-box-cri");
    path
}

fn require_crictl(crictl: &str) -> Result<(), Box<dyn Error>> {
    let output = Command::new(crictl)
        .arg("--version")
        .output()
        .map_err(|e| {
            format!("failed to execute crictl at '{crictl}'; set {CRICTL_ENV} if needed: {e}")
        })?;
    if !output.status.success() {
        return Err(format!(
            "crictl --version failed with status {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(())
}

fn wait_for_cri(crictl: &str, socket_path: &Path, timeout: Duration) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + timeout;
    let mut last_error = String::new();

    while Instant::now() < deadline {
        if socket_path.exists() {
            match run_crictl(crictl, socket_path, &["version"]) {
                Ok(_) => return Ok(()),
                Err(e) => last_error = e.to_string(),
            }
        }
        thread::sleep(Duration::from_millis(250));
    }

    Err(format!(
        "CRI server did not become ready at {} within {:?}; last error: {}",
        socket_path.display(),
        timeout,
        last_error
    )
    .into())
}

fn run_crictl(crictl: &str, socket_path: &Path, args: &[&str]) -> Result<String, Box<dyn Error>> {
    let endpoint = format!("unix://{}", socket_path.display());
    let output = Command::new(crictl)
        .arg("--runtime-endpoint")
        .arg(&endpoint)
        .arg("--image-endpoint")
        .arg(&endpoint)
        .args(args.iter().map(OsStr::new))
        .output()?;

    if !output.status.success() {
        return Err(format!(
            "crictl {} failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
            args.join(" "),
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn wait_for_logs(
    crictl: &str,
    socket_path: &Path,
    container_id: &str,
    marker: &str,
) -> Result<String, Box<dyn Error>> {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut last_logs = String::new();

    while Instant::now() < deadline {
        if let Ok(logs) = run_crictl(crictl, socket_path, &["logs", container_id]) {
            if logs.contains(marker) {
                return Ok(logs);
            }
            last_logs = logs;
        }
        thread::sleep(Duration::from_millis(500));
    }

    Err(format!(
        "container logs did not contain marker '{marker}' within timeout; last logs:\n{last_logs}"
    )
    .into())
}

fn write_pod_config(path: &Path, log_dir: &Path) -> Result<(), Box<dyn Error>> {
    let content = format!(
        r#"{{
  "metadata": {{
    "name": "a3s-cri-smoke-pod",
    "namespace": "default",
    "uid": "a3s-cri-smoke-pod-uid",
    "attempt": 0
  }},
  "log_directory": "{}",
  "labels": {{
    "a3s.box/smoke": "true"
  }},
  "annotations": {{}},
  "linux": {{}}
}}
"#,
        json_escape(&log_dir.to_string_lossy())
    );
    std::fs::write(path, content)?;
    Ok(())
}

fn write_container_config(
    path: &Path,
    image: &str,
    name: &str,
    log_path: &str,
    marker: &str,
) -> Result<(), Box<dyn Error>> {
    let content = format!(
        r#"{{
  "metadata": {{
    "name": "{}",
    "attempt": 0
  }},
  "image": {{
    "image": "{}"
  }},
  "command": ["/bin/sh", "-c", "printf '{}\\n'"],
  "log_path": "{}",
  "labels": {{
    "a3s.box/smoke": "true"
  }},
  "annotations": {{}},
  "linux": {{}}
}}
"#,
        json_escape(name),
        json_escape(image),
        marker,
        json_escape(log_path)
    );
    std::fs::write(path, content)?;
    Ok(())
}

fn json_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn path_str(path: &Path) -> Result<&str, Box<dyn Error>> {
    path.to_str()
        .ok_or_else(|| format!("path is not valid UTF-8: {}", path.display()).into())
}

fn last_nonempty_line(output: &str) -> Option<&str> {
    output.lines().rev().find(|line| !line.trim().is_empty())
}
