//! Ignored host-dependent smoke coverage for the a3s-box command surface.
//!
//! These tests require one or more host capabilities such as root-capable Linux
//! chroot builds, registry credentials/network access, or a working HVF/KVM
//! MicroVM runtime. Keep them separate from `command_coverage.rs` so pure
//! command coverage never depends on those capabilities.
//!
//! Useful environment:
//! - `A3S_BOX_TEST_ALPINE_TAR`: load an offline Alpine OCI archive instead of pulling.
//! - `A3S_BOX_HOST_SMOKE_IMAGE`: override the runnable Linux image reference.
//! - `A3S_BOX_HOST_SMOKE_TIMEOUT_SECS`: override host VM wait timeouts.

use std::path::Path;
use std::time::Duration;

mod support;
use support::*;

#[test]
#[ignore]
#[cfg(target_os = "linux")]
fn test_linux_build_run_chroot_smoke() {
    if !is_root_user() {
        eprintln!("skipping Linux RUN build smoke: chroot requires root");
        return;
    }

    let Ok(alpine_tar) = std::env::var(TEST_ALPINE_TAR_ENV) else {
        eprintln!("skipping Linux RUN build smoke: set {TEST_ALPINE_TAR_ENV}");
        return;
    };

    let cli = CliTest::new();
    let base_image = format!("coverage-run-base:{}", unique_tag("base"));
    let built_image = format!("coverage-run-built:{}", unique_tag("build"));
    cli.ok(&["load", "--input", &alpine_tar, "--tag", &base_image]);

    let build_dir = cli.home_path().join("run-build");
    std::fs::create_dir_all(&build_dir).expect("create RUN build context");
    std::fs::write(
        build_dir.join("Dockerfile"),
        format!(
            "FROM {base_image}\nRUN printf 'run-smoke-ok\\n' > /run-smoke.txt\nCMD [\"cat\", \"/run-smoke.txt\"]\n"
        ),
    )
    .expect("write RUN Dockerfile");

    let build_dir_arg = build_dir.to_string_lossy().to_string();
    cli.ok(&["build", "--tag", &built_image, "--quiet", &build_dir_arg]);

    let image_tar = cli.home_path().join("run-build.tar");
    let image_tar_arg = image_tar.to_string_lossy().to_string();
    cli.ok(&["save", &built_image, "--output", &image_tar_arg]);
    let copied = read_file_from_saved_oci_tar(&image_tar, "/run-smoke.txt")
        .expect("RUN-built image should contain generated file");
    assert_eq!(copied, "run-smoke-ok\n");

    cli.ok(&["rmi", "--force", &built_image, &base_image]);
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
    let image = host_smoke_image();
    let boot_timeout = host_smoke_timeout(45);
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

    seed_runnable_alpine_image(&cli, &image);
    let images = cli.ok(&["images"]);
    assert!(
        !images.trim().is_empty(),
        "images output should include the seeded smoke image"
    );
    cli.ok(&["image-inspect", &image]);
    cli.ok(&["history", &image]);

    let image_tar = cli.home_path().join("alpine.tar");
    let image_tar = image_tar.to_string_lossy().to_string();
    cli.ok(&["save", &image, "--output", &image_tar]);
    cli.ok(&[
        "load",
        "--input",
        &image_tar,
        "--tag",
        "coverage-loaded:latest",
    ]);
    cli.ok(&["tag", &image, "coverage-alias:latest"]);

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
    wait_for_running(&cli, built_box, boot_timeout);
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
        &image,
        "--",
        "sleep",
        "3600",
    ]);
    wait_for_running(&cli, main_box, boot_timeout);

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
    assert!(
        !copied.trim().is_empty(),
        "copied guest /etc/os-release should not be empty"
    );

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
    cli.ok(&["stop", main_box]);
    cli.ok(&["network", "connect", "covvmnet", main_box]);
    cli.ok(&["network", "inspect", "covvmnet"]);
    cli.ok_status(&["start", main_box]);
    wait_for_running(&cli, main_box, boot_timeout);
    cli.ok(&["stop", main_box]);
    cli.ok(&["network", "disconnect", "covvmnet", main_box]);
    cli.ok(&["network", "rm", "covvmnet"]);
    cli.ok_status(&["start", main_box]);
    wait_for_running(&cli, main_box, boot_timeout);

    cli.ok(&["pause", main_box]);
    cli.ok(&["unpause", main_box]);
    cli.ok_status(&["restart", main_box]);
    wait_for_running(&cli, main_box, boot_timeout);
    cli.ok(&["stop", main_box]);
    cli.ok(&["wait", main_box]);
    cli.ok_status(&["start", main_box]);
    wait_for_running(&cli, main_box, boot_timeout);
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
    let image = host_smoke_image();
    let boot_timeout = host_smoke_timeout(45);
    let project = "covcompose";
    let service_box = "covcompose-worker";

    cleanup(&cli, service_box);
    seed_runnable_alpine_image(&cli, &image);

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
    wait_for_running(&cli, service_box, boot_timeout);
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

/// Warm-pool daemon end-to-end: `pool start` pre-warms VMs, then `pool run`
/// executes a command in a fresh warm sandbox via the guest exec server (no cold
/// boot). Also exercises CONCURRENT `pool run`s (served concurrently) and a
/// SECOND image pre-warmed at startup via `--warm`, run with `pool run --image`.
/// Host-backed (needs KVM + a runnable image).
#[test]
#[ignore]
fn test_real_pool_warm_run() {
    let cli = CliTest::new();
    let image = host_smoke_image();
    seed_runnable_alpine_image(&cli, &image);
    // A second image (retag of the first) the daemon pre-warms at startup via --warm.
    let second = format!("coverage-pool-second:{}", unique_tag("img2"));
    cli.ok(&["tag", &image, &second]);
    let warm_spec = format!("{second}=2");
    let socket = cli
        .home_path()
        .join("pool.sock")
        .to_str()
        .expect("utf8 socket path")
        .to_string();

    // Daemon: pre-warm the default pool + a second image via --warm; listen on the socket.
    let mut daemon = cli.spawn_background(&[
        "pool",
        "start",
        "--image",
        image.as_str(),
        "--size",
        "3",
        "--max",
        "6",
        "--warm",
        warm_spec.as_str(),
        "--socket",
        socket.as_str(),
    ]);

    // The daemon binds the socket once WarmPool::start returns; wait for it.
    let sock_path = cli.home_path().join("pool.sock");
    let start = std::time::Instant::now();
    while !sock_path.exists() {
        if start.elapsed() > Duration::from_secs(120) {
            let _ = daemon.kill();
            panic!("pool daemon never created its socket");
        }
        if let Ok(Some(status)) = daemon.try_wait() {
            panic!("pool daemon exited early: {status}");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    // Give the pool a moment to have at least one idle VM ready.
    std::thread::sleep(Duration::from_secs(5));

    // Single warm run.
    let (out, err, ok) = cli.output(&[
        "pool",
        "run",
        "--socket",
        socket.as_str(),
        "--",
        "echo",
        "pool-e2e-ok",
    ]);
    assert!(ok, "pool run failed.\nstdout:\n{out}\nstderr:\n{err}");
    assert!(out.contains("pool-e2e-ok"), "unexpected output: {out:?}");

    // Concurrent runs — the daemon serves them concurrently.
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..3)
            .map(|i| {
                let cli = &cli;
                let socket = socket.as_str();
                s.spawn(move || {
                    let tag = format!("conc-{i}");
                    let (out, _e, ok) = cli.output(&[
                        "pool",
                        "run",
                        "--socket",
                        socket,
                        "--",
                        "echo",
                        tag.as_str(),
                    ]);
                    (ok, out, tag)
                })
            })
            .collect();
        for h in handles {
            let (ok, out, tag) = h.join().expect("concurrent run thread panicked");
            assert!(ok, "concurrent pool run failed: {out}");
            assert!(
                out.contains(&tag),
                "concurrent run output {out:?} missing {tag}"
            );
        }
    });

    // Multi-image: run in the second image, pre-warmed at startup via --warm.
    let (out2, err2, ok2) = cli.output(&[
        "pool",
        "run",
        "--socket",
        socket.as_str(),
        "--image",
        second.as_str(),
        "--",
        "echo",
        "multiimg-ok",
    ]);
    assert!(
        ok2,
        "multi-image pool run failed.\nstdout:\n{out2}\nstderr:\n{err2}"
    );
    assert!(out2.contains("multiimg-ok"), "unexpected output: {out2:?}");

    let _ = daemon.kill();
    let _ = daemon.wait();
}
