//! Pure, side-effect-free logic for the a3s-box containerd shim.
//!
//! Everything here is free of process spawning, ttrpc, and filesystem state so it
//! can be unit-tested directly. `service.rs` is the thin async layer that wires
//! these into the containerd Task API and spawns the `a3s-box` CLI.

use serde_json::Value;

/// Annotations the containerd CRI plugin sets on the OCI spec.
pub const ANN_CONTAINER_TYPE: &str = "io.kubernetes.cri.container-type";
pub const ANN_IMAGE_NAME: &str = "io.kubernetes.cri.image-name";

/// Fields extracted from an OCI runtime spec (config.json) the shim needs.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ParsedSpec {
    pub is_sandbox: bool,
    pub image: Option<String>,
    pub args: Vec<String>,
    pub env: Vec<String>,
    pub cpus: Option<u32>,
    pub memory_mb: Option<u64>,
}

/// Read a string annotation from an OCI spec JSON value.
pub fn annotation<'a>(spec: &'a Value, key: &str) -> Option<&'a str> {
    spec.get("annotations")?.get(key)?.as_str()
}

/// Extract the shim-relevant fields from a parsed config.json value.
pub fn parse_spec(spec: &Value) -> ParsedSpec {
    let is_sandbox = annotation(spec, ANN_CONTAINER_TYPE)
        .map(|t| t == "sandbox")
        .unwrap_or(false);
    let image = annotation(spec, ANN_IMAGE_NAME).map(|s| s.to_string());
    let args = string_array(spec.pointer("/process/args"));
    let env = string_array(spec.pointer("/process/env"));
    let memory_mb = spec
        .pointer("/linux/resources/memory/limit")
        .and_then(|v| v.as_u64())
        .map(|b| (b / (1024 * 1024)).max(1));
    let cpus = match (
        spec.pointer("/linux/resources/cpu/quota")
            .and_then(|v| v.as_i64()),
        spec.pointer("/linux/resources/cpu/period")
            .and_then(|v| v.as_u64()),
    ) {
        (Some(q), Some(p)) if q > 0 && p > 0 => Some(((q as f64 / p as f64).ceil()) as u32),
        _ => None,
    };
    ParsedSpec {
        is_sandbox,
        image,
        args,
        env,
        cpus,
        memory_mb,
    }
}

fn string_array(v: Option<&Value>) -> Vec<String> {
    v.and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Build the `a3s-box` argv (after the binary) for a detached workload run.
pub fn run_args(
    id: &str,
    image: &str,
    cpus: Option<u32>,
    memory_mb: Option<u64>,
    env: &[String],
    cmd: &[String],
) -> Vec<String> {
    // Foreground `run` (no -d). This is launched as the main process of a transient
    // systemd unit (see service.rs): foreground run blocks until the box exits, so
    // the unit stays active for the container's whole life. A detached `run -d`
    // would exit right after boot, the unit would deactivate, and that exit gets
    // reported as the container exiting — tearing the task down seconds after start.
    let mut v = vec!["run".to_string(), "--name".to_string(), id.to_string()];
    if let Some(c) = cpus {
        v.push("--cpus".to_string());
        v.push(c.to_string());
    }
    if let Some(m) = memory_mb {
        v.push("--memory".to_string());
        v.push(format!("{m}m"));
    }
    for e in env {
        v.push("-e".to_string());
        v.push(e.clone());
    }
    v.push(image.to_string());
    if !cmd.is_empty() {
        v.push("--".to_string());
        v.extend(cmd.iter().cloned());
    }
    v
}

/// Build the `a3s-box exec` argv (after the binary) for an exec process.
pub fn exec_args(container_id: &str, cmd: &[String]) -> Vec<String> {
    let mut v = vec![
        "exec".to_string(),
        container_id.to_string(),
        "--".to_string(),
    ];
    v.extend(cmd.iter().cloned());
    v
}

/// Parse the OCI Process command from the JSON-encoded protobuf Any value that
/// containerd sends with an Exec request.
pub fn parse_exec_command(spec_value: &[u8]) -> Vec<String> {
    serde_json::from_slice::<Value>(spec_value)
        .ok()
        .map(|v| string_array(v.get("args")))
        .unwrap_or_default()
}

/// Whether `a3s-box inspect` stdout reports the box as running.
pub fn is_running(inspect_stdout: &str) -> bool {
    inspect_stdout.contains("\"running\"")
}

/// The stable A3S_HOME used so `run`/`exec`/`wait`/`stop`/`rm` share one
/// boxes.json regardless of the (often empty) env containerd gives the shim.
pub fn a3s_home() -> String {
    std::env::var("A3S_HOME").unwrap_or_else(|_| "/var/lib/a3s-box".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn annotation_present_and_absent() {
        let s = json!({"annotations": {"k": "v"}});
        assert_eq!(annotation(&s, "k"), Some("v"));
        assert_eq!(annotation(&s, "missing"), None);
        assert_eq!(annotation(&json!({}), "k"), None);
        assert_eq!(annotation(&json!(null), "k"), None);
    }

    #[test]
    fn parse_spec_sandbox() {
        let s = json!({"annotations": {ANN_CONTAINER_TYPE: "sandbox"}});
        let p = parse_spec(&s);
        assert!(p.is_sandbox);
        assert_eq!(p.image, None);
        assert!(p.args.is_empty());
    }

    #[test]
    fn parse_spec_workload_full() {
        let s = json!({
            "annotations": {
                ANN_CONTAINER_TYPE: "container",
                ANN_IMAGE_NAME: "docker.io/library/redis:7"
            },
            "process": {
                "args": ["redis-server", "--port", "0"],
                "env": ["A=1", "B=2"]
            },
            "linux": {"resources": {
                "memory": {"limit": 536870912u64},   // 512 MiB
                "cpu": {"quota": 150000, "period": 100000}  // 1.5 -> ceil 2
            }}
        });
        let p = parse_spec(&s);
        assert!(!p.is_sandbox);
        assert_eq!(p.image.as_deref(), Some("docker.io/library/redis:7"));
        assert_eq!(p.args, vec!["redis-server", "--port", "0"]);
        assert_eq!(p.env, vec!["A=1", "B=2"]);
        assert_eq!(p.memory_mb, Some(512));
        assert_eq!(p.cpus, Some(2));
    }

    #[test]
    fn parse_spec_defaults_when_missing() {
        let p = parse_spec(&json!({}));
        assert_eq!(p, ParsedSpec::default());
        // zero/invalid cpu quota -> None
        let s = json!({"linux": {"resources": {"cpu": {"quota": -1, "period": 100000}}}});
        assert_eq!(parse_spec(&s).cpus, None);
        // sub-MiB memory clamps to at least 1
        let s = json!({"linux": {"resources": {"memory": {"limit": 1024}}}});
        assert_eq!(parse_spec(&s).memory_mb, Some(1));
    }

    #[test]
    fn run_args_minimal() {
        assert_eq!(
            run_args("box1", "alpine", None, None, &[], &[]),
            vec!["run", "--name", "box1", "alpine"]
        );
    }

    #[test]
    fn run_args_full() {
        let v = run_args(
            "box1",
            "redis:7",
            Some(2),
            Some(256),
            &["A=1".into()],
            &["redis-server".into(), "--port".into(), "0".into()],
        );
        assert_eq!(
            v,
            vec![
                "run",
                "--name",
                "box1",
                "--cpus",
                "2",
                "--memory",
                "256m",
                "-e",
                "A=1",
                "redis:7",
                "--",
                "redis-server",
                "--port",
                "0"
            ]
        );
    }

    #[test]
    fn exec_args_builds_separator() {
        assert_eq!(
            exec_args("box1", &["sh".into(), "-c".into(), "echo hi".into()]),
            vec!["exec", "box1", "--", "sh", "-c", "echo hi"]
        );
        assert_eq!(exec_args("box1", &[]), vec!["exec", "box1", "--"]);
    }

    #[test]
    fn parse_exec_command_from_any() {
        let any = br#"{"args":["ls","-la"],"cwd":"/"}"#;
        assert_eq!(parse_exec_command(any), vec!["ls", "-la"]);
        assert!(parse_exec_command(b"not json").is_empty());
        assert!(parse_exec_command(br#"{"no_args":true}"#).is_empty());
    }

    #[test]
    fn is_running_detects_status() {
        assert!(is_running(r#"{"status": "running"}"#));
        assert!(is_running(r#"{"status":"running","pid":1}"#));
        assert!(!is_running(r#"{"status": "created"}"#));
        assert!(!is_running(""));
    }

    #[test]
    fn a3s_home_default() {
        // Default applies when unset (don't mutate global env in parallel tests).
        if std::env::var("A3S_HOME").is_err() {
            assert_eq!(a3s_home(), "/var/lib/a3s-box");
        }
    }
}
