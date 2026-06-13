//! Pure integration coverage for the a3s-box command surface.
//!
//! These tests cover parser entrypoints and commands that operate only on local
//! state. VM, registry, and host-network smoke tests live in `host_smoke.rs` so
//! default command coverage stays deterministic.

use std::time::Duration;

mod support;
use support::{assert_json_array_contains, read_file_from_saved_oci_tar, unique_tag, CliTest};

/// Parse `inspect`/`image-inspect` output and return the single record. Both
/// commands emit a Docker-style JSON array `[{...}]`, so callers want element 0.
fn parse_inspect(out: &str) -> serde_json::Value {
    let v: serde_json::Value = serde_json::from_str(out).expect("inspect output should be JSON");
    match v {
        serde_json::Value::Array(mut a) if !a.is_empty() => a.remove(0),
        other => other,
    }
}

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
    "container-update",
    "compose",
    "snapshot",
    "build",
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
    "import",
    "cp",
    "network",
    "volume",
    "df",
    "prune",
    "system-prune",
    "version",
    "info",
    "monitor",
    "pool",
    "shell",
    "help",
];

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
    cli.ok(&["prune", "--force"]);
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
        "18080:80/tcp",
        "--label",
        "purpose=coverage",
        "docker.io/library/alpine:latest",
        "--",
        "/bin/sh",
        "-c",
        "echo created-command",
    ]);
    let inspect = cli.ok(&["inspect", "cov-created"]);
    assert!(inspect.contains("cov-created"));
    let inspect_json = parse_inspect(&inspect);
    assert_eq!(
        inspect_json["cmd"],
        serde_json::json!(["/bin/sh", "-c", "echo created-command"])
    );
    let empty_logs = cli.ok(&["logs", "cov-created"]);
    assert!(empty_logs.is_empty());
    let ports = cli.ok(&["port", "cov-created"]);
    assert!(ports.contains("80/tcp -> 0.0.0.0:18080"));
    cli.ok(&["rename", "cov-created", "cov-renamed"]);
    cli.ok(&[
        "network",
        "create",
        "covconnect",
        "--subnet",
        "10.124.0.0/24",
    ]);
    cli.ok(&["network", "connect", "covconnect", "cov-renamed"]);
    let connected = cli.ok(&["inspect", "cov-renamed"]);
    let connected = parse_inspect(&connected);
    assert_eq!(connected["network_name"], "covconnect");
    assert_eq!(
        connected["network_mode"],
        serde_json::json!({"bridge": {"network": "covconnect"}})
    );
    let connected_network = cli.ok(&["network", "inspect", "covconnect"]);
    assert!(connected_network.contains("cov-renamed"));
    assert!(connected_network.contains("10.124.0.2"));
    cli.ok(&["network", "disconnect", "covconnect", "cov-renamed"]);
    let disconnected = cli.ok(&["inspect", "cov-renamed"]);
    let disconnected = parse_inspect(&disconnected);
    assert_eq!(disconnected["network_name"], serde_json::Value::Null);
    assert_eq!(disconnected["network_mode"], serde_json::json!("tsi"));
    cli.ok(&["network", "rm", "covconnect"]);
    cli.ok(&["network", "create", "covforce", "--subnet", "10.125.0.0/24"]);
    cli.ok(&["network", "connect", "covforce", "cov-renamed"]);
    cli.ok(&["network", "rm", "--force", "covforce"]);
    let force_disconnected = cli.ok(&["inspect", "cov-renamed"]);
    let force_disconnected = parse_inspect(&force_disconnected);
    assert_eq!(force_disconnected["network_name"], serde_json::Value::Null);
    assert_eq!(force_disconnected["network_mode"], serde_json::json!("tsi"));
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
    ]);
    cli.ok(&["logout", "example.invalid"]);

    cli.ok(&["events", "--until", "1970-01-01T00:00:00Z"]);
}

#[test]
fn test_build_from_scratch_copy_metadata_cli_smoke() {
    let cli = CliTest::new();
    let image = format!("coverage-scratch:{}", unique_tag("build"));
    let build_dir = cli.home_path().join("scratch-build");
    std::fs::create_dir_all(&build_dir).expect("create scratch build context");
    std::fs::write(build_dir.join("message.txt"), "scratch-copy-ok\n")
        .expect("write scratch build input");
    std::fs::write(
        build_dir.join("Dockerfile"),
        r#"FROM scratch
COPY message.txt /opt/message.txt
ENV A3S_BUILD_SMOKE=1
WORKDIR /opt
USER 1000:1000
EXPOSE 8080/tcp
VOLUME /data
STOPSIGNAL SIGTERM
HEALTHCHECK --interval=5s --timeout=2s --retries=2 CMD ["cat", "/opt/message.txt"]
LABEL org.opencontainers.image.title="cli-scratch-smoke"
CMD ["cat", "/opt/message.txt"]
"#,
    )
    .expect("write scratch Dockerfile");

    let build_dir_arg = build_dir.to_string_lossy().to_string();
    let digest = cli.ok(&["build", "--tag", &image, "--quiet", &build_dir_arg]);
    assert!(
        digest.trim().starts_with("sha256:"),
        "quiet build output should be a digest\n{digest}"
    );

    let inspect = cli.ok(&["image-inspect", &image]);
    let inspect = parse_inspect(&inspect);
    assert_eq!(inspect["Reference"], image);
    assert_eq!(inspect["LayerCount"], 1);
    assert_eq!(
        inspect["Config"]["Cmd"],
        serde_json::json!(["cat", "/opt/message.txt"])
    );
    assert_eq!(inspect["Config"]["Env"]["A3S_BUILD_SMOKE"], "1");
    assert_eq!(inspect["Config"]["WorkingDir"], "/opt");
    assert_eq!(inspect["Config"]["User"], "1000:1000");
    assert_eq!(inspect["Config"]["StopSignal"], "SIGTERM");
    assert_eq!(
        inspect["Config"]["Labels"]["org.opencontainers.image.title"],
        "cli-scratch-smoke"
    );
    assert_eq!(
        inspect["Config"]["Healthcheck"]["Test"],
        serde_json::json!(["cat", "/opt/message.txt"])
    );
    assert_eq!(inspect["Config"]["Healthcheck"]["Interval"], 5);
    assert_eq!(inspect["Config"]["Healthcheck"]["Timeout"], 2);
    assert_eq!(inspect["Config"]["Healthcheck"]["Retries"], 2);
    assert_json_array_contains(&inspect["Config"]["ExposedPorts"], "8080/tcp");
    assert_json_array_contains(&inspect["Config"]["Volumes"], "/data");

    let history = cli.ok(&["history", "--no-trunc", &image]);
    assert!(history.contains("COPY message.txt /opt/message.txt"));
    assert!(history.contains("CMD [\"cat\", \"/opt/message.txt\"]"));

    let image_tar = cli.home_path().join("scratch-build.tar");
    let image_tar_arg = image_tar.to_string_lossy().to_string();
    cli.ok(&["save", &image, "--output", &image_tar_arg]);
    let copied = read_file_from_saved_oci_tar(&image_tar, "/opt/message.txt")
        .expect("saved scratch image should contain copied file");
    assert_eq!(copied, "scratch-copy-ok\n");

    cli.ok(&["rmi", "--force", &image]);
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
        &["run", "-d", "-t", "docker.io/library/alpine:latest"],
        "Cannot use -t",
    );
    cli.fails(
        &[
            "create",
            "-p",
            "18080:80/udp",
            "docker.io/library/alpine:latest",
        ],
        "only TCP is supported",
    );
    cli.fails(
        &[
            "run",
            "--restart",
            "never",
            "docker.io/library/alpine:latest",
        ],
        "Invalid restart policy",
    );
    let invalid_compose = cli.home_path().join("invalid-restart-compose.yaml");
    std::fs::write(
        &invalid_compose,
        "services:\n  web:\n    image: docker.io/library/alpine:latest\n    restart: never\n",
    )
    .expect("write invalid compose file");
    let invalid_compose_arg = invalid_compose.to_string_lossy().to_string();
    cli.fails(
        &["compose", "--file", &invalid_compose_arg, "config"],
        "Service 'web' has invalid restart policy",
    );
    cli.fails(
        &["network", "create", "bad-driver", "--driver", "overlay"],
        "Unsupported network driver",
    );
    cli.fails(
        &["network", "create", "strict-net", "--isolation", "strict"],
        "Unsupported network isolation mode",
    );
    cli.fails(
        &[
            "create",
            "--network",
            "missing-net",
            "docker.io/library/alpine:latest",
        ],
        "network 'missing-net' not found",
    );
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
