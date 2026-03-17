//! Instruction handlers for the build engine.

use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};

use super::super::dockerfile::Instruction;
use super::super::layer::{create_layer, create_layer_from_dir, LayerInfo};
use super::utils::{
    copy_dir_recursive, expand_args, extract_tar_to_dst, is_tar_archive, resolve_path,
};
use super::BuildState;

/// Handle COPY: copy files from build context into rootfs, create a layer.
pub(super) fn handle_copy(
    src_patterns: &[String],
    dst: &str,
    context_dir: &Path,
    rootfs_dir: &Path,
    layers_dir: &Path,
    workdir: &str,
    layer_index: usize,
) -> Result<LayerInfo> {
    // Resolve destination path
    let resolved_dst = resolve_path(workdir, dst);
    let dst_in_rootfs = rootfs_dir.join(resolved_dst.trim_start_matches('/'));

    // Ensure destination directory exists
    if dst.ends_with('/') || src_patterns.len() > 1 {
        std::fs::create_dir_all(&dst_in_rootfs).map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to create COPY destination {}: {}",
                dst_in_rootfs.display(),
                e
            ))
        })?;
    } else if let Some(parent) = dst_in_rootfs.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            BoxError::BuildError(format!("Failed to create parent directory: {}", e))
        })?;
    }

    // Copy each source
    for src in src_patterns {
        let src_path = context_dir.join(src);
        if !src_path.exists() {
            return Err(BoxError::BuildError(format!(
                "COPY source not found: {} (in context {})",
                src,
                context_dir.display()
            )));
        }

        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_in_rootfs)?;
        } else {
            // If dst ends with / or is a directory, copy into it
            let target = if dst_in_rootfs.is_dir() {
                dst_in_rootfs.join(
                    src_path
                        .file_name()
                        .unwrap_or_else(|| std::ffi::OsStr::new(src)),
                )
            } else {
                dst_in_rootfs.clone()
            };
            std::fs::copy(&src_path, &target).map_err(|e| {
                BoxError::BuildError(format!(
                    "Failed to copy {} to {}: {}",
                    src_path.display(),
                    target.display(),
                    e
                ))
            })?;
        }
    }

    // Create a layer from the copied files
    // We use create_layer_from_dir approach: snapshot the destination
    let layer_path = layers_dir.join(format!("layer_{}.tar.gz", layer_index));

    // For COPY, create a layer containing just the destination files
    let target_prefix = Path::new(resolved_dst.trim_start_matches('/'));
    if dst_in_rootfs.is_dir() {
        create_layer_from_dir(&dst_in_rootfs, target_prefix, &layer_path)
    } else if dst_in_rootfs.parent().is_some() {
        // Single file copy: create layer with just that file
        let changed = vec![PathBuf::from(
            dst_in_rootfs
                .strip_prefix(rootfs_dir)
                .unwrap_or(target_prefix),
        )];
        create_layer(rootfs_dir, &changed, &layer_path)
    } else {
        Err(BoxError::BuildError("Invalid COPY destination".to_string()))
    }
}

/// Handle RUN: execute a command in the rootfs.
///
/// On Linux, uses chroot. On macOS, tries Docker/Podman, or skips with a warning.
/// Returns Some(LayerInfo) if a layer was created, None if skipped.
#[allow(clippy::too_many_arguments)]
pub(super) fn handle_run(
    command: &str,
    rootfs_dir: &Path,
    layers_dir: &Path,
    workdir: &str,
    env: &[(String, String)],
    shell: &[String],
    layer_index: usize,
    quiet: bool,
) -> Result<Option<LayerInfo>> {
    #[cfg(target_os = "macos")]
    {
        // On macOS, try to use Docker or Podman to execute RUN commands
        return handle_run_via_container(
            command,
            rootfs_dir,
            layers_dir,
            workdir,
            env,
            shell,
            layer_index,
            quiet,
        );
    }

    // Linux: execute via chroot
    #[cfg(target_os = "linux")]
    {
        use super::super::layer::DirSnapshot;

        let before = DirSnapshot::capture(rootfs_dir)?;

        // Build the command using the configured shell
        let mut cmd = std::process::Command::new("chroot");
        cmd.arg(rootfs_dir);
        if shell.len() >= 2 {
            cmd.arg(&shell[0]);
            for arg in &shell[1..] {
                cmd.arg(arg);
            }
        } else if shell.len() == 1 {
            cmd.arg(&shell[0]);
        } else {
            cmd.arg("/bin/sh");
            cmd.arg("-c");
        }
        cmd.arg(command);

        // Set environment
        cmd.env_clear();
        cmd.env(
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        );
        cmd.env("HOME", "/root");
        for (key, value) in env {
            cmd.env(key, value);
        }

        let output = cmd
            .output()
            .map_err(|e| BoxError::BuildError(format!("Failed to execute RUN command: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(BoxError::BuildError(format!(
                "RUN command failed (exit {}): {}",
                output.status.code().unwrap_or(-1),
                stderr.trim()
            )));
        }

        if !quiet {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if !stdout.is_empty() {
                print!("{}", stdout);
            }
        }

        // Capture diff
        let after = DirSnapshot::capture(rootfs_dir)?;
        let changed = before.diff(&after);

        if changed.is_empty() {
            return Ok(None);
        }

        let layer_path = layers_dir.join(format!("layer_{}.tar.gz", layer_index));
        let layer_info = create_layer(rootfs_dir, &changed, &layer_path)?;
        Ok(Some(layer_info))
    }

    // Other platforms: not supported
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (command, rootfs_dir, layers_dir, workdir, env, shell, layer_index, quiet);
        Ok(None)
    }
}

/// Execute RUN command via Docker or Podman on macOS.
///
/// This function:
/// 1. Detects available container runtime (docker or podman)
/// 2. Creates a temporary image from the current rootfs
/// 3. Runs the command in a container based on that image
/// 4. Captures filesystem changes and creates a layer
#[cfg(target_os = "macos")]
#[allow(clippy::too_many_arguments)]
fn handle_run_via_container(
    command: &str,
    rootfs_dir: &Path,
    layers_dir: &Path,
    workdir: &str,
    env: &[(String, String)],
    shell: &[String],
    layer_index: usize,
    quiet: bool,
) -> Result<Option<LayerInfo>> {
    use super::super::layer::DirSnapshot;

    // Detect available container runtime
    let runtime = detect_container_runtime()?;

    if !quiet {
        println!("→ Using {} to execute RUN command", runtime);
    }

    // Capture filesystem state before execution
    let before = DirSnapshot::capture(rootfs_dir)?;

    // Verify rootfs exists and has content
    if !rootfs_dir.exists() {
        return Err(BoxError::BuildError(format!(
            "Rootfs directory does not exist: {}",
            rootfs_dir.display()
        )));
    }

    // Check if rootfs has any content
    let has_content = std::fs::read_dir(rootfs_dir)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false);

    if !has_content {
        return Err(BoxError::BuildError(format!(
            "Rootfs directory is empty: {}",
            rootfs_dir.display()
        )));
    }

    // Create a temporary image name
    let temp_image = format!("a3s-box-build-temp-{}", uuid::Uuid::new_v4());

    // Create a tar archive of the rootfs using system tar command
    let temp_tar = std::env::temp_dir().join(format!("{}.tar", temp_image));

    let tar_output = std::process::Command::new("tar")
        .arg("-cf")
        .arg(&temp_tar)
        .arg("-C")
        .arg(rootfs_dir)
        .arg(".")
        .output()
        .map_err(|e| {
            BoxError::BuildError(format!("Failed to execute tar command: {}", e))
        })?;

    if !tar_output.status.success() {
        let stderr = String::from_utf8_lossy(&tar_output.stderr);
        return Err(BoxError::BuildError(format!(
            "Failed to create rootfs tar: {}",
            stderr.trim()
        )));
    }

    // Import the tar as a Docker image
    let import_output = std::process::Command::new(&runtime)
        .arg("import")
        .arg(&temp_tar)
        .arg(&temp_image)
        .output()
        .map_err(|e| {
            BoxError::BuildError(format!("Failed to import image to {}: {}", runtime, e))
        })?;

    if !import_output.status.success() {
        let stderr = String::from_utf8_lossy(&import_output.stderr);
        std::fs::remove_file(&temp_tar).ok();
        return Err(BoxError::BuildError(format!(
            "Failed to import rootfs as image: {}",
            stderr.trim()
        )));
    }

    // Build the shell command
    let shell_cmd = if !shell.is_empty() {
        let mut cmd_parts = shell.to_vec();
        cmd_parts.push(command.to_string());
        cmd_parts
    } else {
        vec!["/bin/sh".to_string(), "-c".to_string(), command.to_string()]
    };

    // Run the command in a container
    let container_name = format!("a3s-box-build-run-{}", uuid::Uuid::new_v4());
    let mut cmd = std::process::Command::new(&runtime);
    cmd.arg("run")
        .arg("--name")
        .arg(&container_name)
        .arg("-w")
        .arg(workdir);

    // Add environment variables
    for (key, value) in env {
        cmd.arg("-e").arg(format!("{}={}", key, value));
    }

    // Add the image and command
    cmd.arg(&temp_image);
    for part in &shell_cmd {
        cmd.arg(part);
    }

    if !quiet {
        println!("→ Executing: {}", command);
    }

    let output = cmd.output().map_err(|e| {
        BoxError::BuildError(format!("Failed to execute {} run: {}", runtime, e))
    })?;

    // Clean up: remove the container and image
    let cleanup = || {
        std::process::Command::new(&runtime)
            .arg("rm")
            .arg(&container_name)
            .output()
            .ok();
        std::process::Command::new(&runtime)
            .arg("rmi")
            .arg(&temp_image)
            .output()
            .ok();
        std::fs::remove_file(&temp_tar).ok();
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        cleanup();
        return Err(BoxError::BuildError(format!(
            "RUN command failed (exit {}): {}",
            output.status.code().unwrap_or(-1),
            stderr.trim()
        )));
    }

    if !quiet {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if !stdout.is_empty() {
            print!("{}", stdout);
        }
    }

    // Export the container's filesystem
    let export_output = std::process::Command::new(&runtime)
        .arg("export")
        .arg(&container_name)
        .output()
        .map_err(|e| {
            cleanup();
            BoxError::BuildError(format!("Failed to export container: {}", e))
        })?;

    if !export_output.status.success() {
        let stderr = String::from_utf8_lossy(&export_output.stderr);
        cleanup();
        return Err(BoxError::BuildError(format!(
            "Failed to export container: {}",
            stderr.trim()
        )));
    }

    // Write exported tar to a temporary file
    let export_tar_path = std::env::temp_dir().join(format!("{}-export.tar", temp_image));
    std::fs::write(&export_tar_path, &export_output.stdout).map_err(|e| {
        cleanup();
        BoxError::BuildError(format!("Failed to write export tar: {}", e))
    })?;

    // Extract using system tar command (handles hardlinks and special files better)
    let extract_output = std::process::Command::new("tar")
        .arg("-xf")
        .arg(&export_tar_path)
        .arg("-C")
        .arg(rootfs_dir)
        .output()
        .map_err(|e| {
            cleanup();
            std::fs::remove_file(&export_tar_path).ok();
            BoxError::BuildError(format!("Failed to extract exported tar: {}", e))
        })?;

    if !extract_output.status.success() {
        let stderr = String::from_utf8_lossy(&extract_output.stderr);
        cleanup();
        std::fs::remove_file(&export_tar_path).ok();
        return Err(BoxError::BuildError(format!(
            "Failed to extract exported container: {}",
            stderr.trim()
        )));
    }

    std::fs::remove_file(&export_tar_path).ok();
    cleanup();

    // Capture filesystem changes
    let after = DirSnapshot::capture(rootfs_dir)?;
    let changed = before.diff(&after);

    if changed.is_empty() {
        if !quiet {
            println!("→ No filesystem changes detected");
        }
        return Ok(None);
    }

    // Create layer from changes
    let layer_path = layers_dir.join(format!("layer_{}.tar.gz", layer_index));
    let layer_info = create_layer(rootfs_dir, &changed, &layer_path)?;

    if !quiet {
        println!("→ Created layer with {} changes", changed.len());
    }

    Ok(Some(layer_info))
}

/// Detect available container runtime (docker or podman).
#[cfg(target_os = "macos")]
fn detect_container_runtime() -> Result<String> {
    // Try docker first
    if std::process::Command::new("docker")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        return Ok("docker".to_string());
    }

    // Try podman
    if std::process::Command::new("podman")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        return Ok("podman".to_string());
    }

    Err(BoxError::BuildError(
        "No container runtime found. Please install Docker or Podman to build images on macOS.\n\
         Install Docker: https://docs.docker.com/desktop/install/mac-install/\n\
         Install Podman: brew install podman".to_string()
    ))
}

/// Handle ADD: like COPY but supports URL download and tar auto-extraction.
#[allow(clippy::too_many_arguments)]
pub(super) fn handle_add(
    src_patterns: &[String],
    dst: &str,
    _chown: Option<&str>,
    context_dir: &Path,
    rootfs_dir: &Path,
    layers_dir: &Path,
    workdir: &str,
    layer_index: usize,
) -> Result<LayerInfo> {
    let resolved_dst = resolve_path(workdir, dst);
    let dst_in_rootfs = rootfs_dir.join(resolved_dst.trim_start_matches('/'));

    // Ensure destination directory exists
    if dst.ends_with('/') || src_patterns.len() > 1 {
        std::fs::create_dir_all(&dst_in_rootfs).map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to create ADD destination {}: {}",
                dst_in_rootfs.display(),
                e
            ))
        })?;
    } else if let Some(parent) = dst_in_rootfs.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            BoxError::BuildError(format!("Failed to create parent directory: {}", e))
        })?;
    }

    for src in src_patterns {
        if src.starts_with("http://") || src.starts_with("https://") {
            // URL download — fetch and write to destination
            let bytes = download_url(src).map_err(|e| {
                BoxError::BuildError(format!("ADD URL download failed for {}: {}", src, e))
            })?;
            // Derive filename from URL path
            let filename = src
                .rsplit('/')
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or("downloaded");
            let dest_file = if dst_in_rootfs.is_dir() || src.ends_with('/') {
                dst_in_rootfs.join(filename)
            } else {
                dst_in_rootfs.clone()
            };
            if let Some(parent) = dest_file.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    BoxError::BuildError(format!("Failed to create parent for ADD URL: {}", e))
                })?;
            }
            std::fs::write(&dest_file, &bytes).map_err(|e| {
                BoxError::BuildError(format!("Failed to write downloaded file: {}", e))
            })?;
            tracing::info!(url = src.as_str(), dest = %dest_file.display(), "ADD URL downloaded");
            continue;
        }

        let src_path = context_dir.join(src);
        if !src_path.exists() {
            return Err(BoxError::BuildError(format!(
                "ADD source not found: {} (in context {})",
                src,
                context_dir.display()
            )));
        }

        // Check if it's a tar archive that should be auto-extracted
        if is_tar_archive(src) && !src_path.is_dir() {
            extract_tar_to_dst(&src_path, &dst_in_rootfs)?;
        } else if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_in_rootfs)?;
        } else {
            let target = if dst_in_rootfs.is_dir() {
                dst_in_rootfs.join(
                    src_path
                        .file_name()
                        .unwrap_or_else(|| std::ffi::OsStr::new(src)),
                )
            } else {
                dst_in_rootfs.clone()
            };
            std::fs::copy(&src_path, &target).map_err(|e| {
                BoxError::BuildError(format!(
                    "Failed to copy {} to {}: {}",
                    src_path.display(),
                    target.display(),
                    e
                ))
            })?;
        }
    }

    // Create a layer from the destination
    let layer_path = layers_dir.join(format!("layer_{}.tar.gz", layer_index));
    let target_prefix = Path::new(resolved_dst.trim_start_matches('/'));
    if dst_in_rootfs.is_dir() {
        create_layer_from_dir(&dst_in_rootfs, target_prefix, &layer_path)
    } else if dst_in_rootfs.parent().is_some() {
        let changed = vec![PathBuf::from(
            dst_in_rootfs
                .strip_prefix(rootfs_dir)
                .unwrap_or(target_prefix),
        )];
        create_layer(rootfs_dir, &changed, &layer_path)
    } else {
        Err(BoxError::BuildError("Invalid ADD destination".to_string()))
    }
}

/// Execute an ONBUILD trigger instruction.
pub(super) fn execute_onbuild_trigger(
    trigger: &str,
    state: &mut BuildState,
    _config: &super::BuildConfig,
    _rootfs_dir: &Path,
    _layers_dir: &Path,
    _base_layers: &[LayerInfo],
    _completed_stages: &[(Option<String>, PathBuf)],
) -> Result<()> {
    // Parse the trigger as an instruction
    let instruction = super::super::dockerfile::parse_single_instruction(trigger)?;

    // Only handle metadata instructions in ONBUILD triggers for now
    // (RUN/COPY would need full execution context)
    match &instruction {
        Instruction::Env { key, value } => {
            let expanded = expand_args(value, &state.build_args);
            if let Some(existing) = state.env.iter_mut().find(|(k, _)| k == key) {
                existing.1 = expanded;
            } else {
                state.env.push((key.clone(), expanded));
            }
        }
        Instruction::Label { key, value } => {
            state.labels.insert(key.clone(), value.clone());
        }
        Instruction::Workdir { path } => {
            state.workdir = resolve_path(&state.workdir, path);
        }
        Instruction::Expose { port } => {
            state.exposed_ports.push(port.clone());
        }
        Instruction::User { user } => {
            state.user = Some(user.clone());
        }
        _ => {
            tracing::warn!(
                trigger = trigger,
                "ONBUILD trigger requires execution context, skipping"
            );
        }
    }

    state.history.push(super::HistoryEntry {
        created_by: format!("ONBUILD {}", trigger),
        empty_layer: true,
    });

    Ok(())
}

/// Convert an Instruction back to a string representation for ONBUILD storage.
pub(super) fn instruction_to_string(instr: &Instruction) -> String {
    match instr {
        Instruction::Run { command } => format!("RUN {}", command),
        Instruction::Copy { src, dst, from } => {
            if let Some(f) = from {
                format!("COPY --from={} {} {}", f, src.join(" "), dst)
            } else {
                format!("COPY {} {}", src.join(" "), dst)
            }
        }
        Instruction::Add { src, dst, chown } => {
            if let Some(c) = chown {
                format!("ADD --chown={} {} {}", c, src.join(" "), dst)
            } else {
                format!("ADD {} {}", src.join(" "), dst)
            }
        }
        Instruction::Workdir { path } => format!("WORKDIR {}", path),
        Instruction::Env { key, value } => format!("ENV {}={}", key, value),
        Instruction::Entrypoint { exec } => format!("ENTRYPOINT {:?}", exec),
        Instruction::Cmd { exec } => format!("CMD {:?}", exec),
        Instruction::Expose { port } => format!("EXPOSE {}", port),
        Instruction::Label { key, value } => format!("LABEL {}={}", key, value),
        Instruction::User { user } => format!("USER {}", user),
        Instruction::Arg { name, default } => {
            if let Some(d) = default {
                format!("ARG {}={}", name, d)
            } else {
                format!("ARG {}", name)
            }
        }
        Instruction::Shell { exec } => format!("SHELL {:?}", exec),
        Instruction::StopSignal { signal } => format!("STOPSIGNAL {}", signal),
        Instruction::HealthCheck { cmd, .. } => {
            if let Some(c) = cmd {
                format!("HEALTHCHECK CMD {}", c.join(" "))
            } else {
                "HEALTHCHECK NONE".to_string()
            }
        }
        Instruction::OnBuild { instruction } => {
            format!("ONBUILD {}", instruction_to_string(instruction))
        }
        Instruction::Volume { paths } => format!("VOLUME {}", paths.join(" ")),
        Instruction::From { image, alias } => {
            if let Some(a) = alias {
                format!("FROM {} AS {}", image, a)
            } else {
                format!("FROM {}", image)
            }
        }
    }
}

/// Apply base image config to build state.
pub(super) fn apply_base_config(
    state: &mut BuildState,
    config: &crate::oci::image::OciImageConfig,
) {
    state.env = config.env.clone();
    state.entrypoint = config.entrypoint.clone();
    state.cmd = config.cmd.clone();
    state.user = config.user.clone();
    state.exposed_ports = config.exposed_ports.clone();
    state.labels = config.labels.clone();
    if let Some(ref wd) = config.working_dir {
        state.workdir = wd.clone();
    }
    if let Some(ref sig) = config.stop_signal {
        state.stop_signal = Some(sig.clone());
    }
    if let Some(ref hc) = config.health_check {
        state.health_check = Some(hc.clone());
    }
    // Inherit volumes from base image
    for v in &config.volumes {
        if !state.volumes.contains(v) {
            state.volumes.push(v.clone());
        }
    }
    // Note: onbuild triggers are NOT inherited — they are executed, not stored
}

/// Download a URL and return the response bytes.
///
/// Uses `tokio::task::block_in_place` to run async reqwest from a sync context
/// while inside a tokio runtime (the build engine runs inside `async fn build()`).
fn download_url(url: &str) -> std::result::Result<Vec<u8>, String> {
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .map_err(|e| format!("Failed to build HTTP client: {}", e))?;

            let response = client
                .get(url)
                .send()
                .await
                .map_err(|e| format!("HTTP request failed: {}", e))?;

            if !response.status().is_success() {
                return Err(format!("HTTP {} for {}", response.status(), url));
            }

            response
                .bytes()
                .await
                .map(|b| b.to_vec())
                .map_err(|e| format!("Failed to read response body: {}", e))
        })
    })
}
