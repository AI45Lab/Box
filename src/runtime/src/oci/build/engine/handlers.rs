//! Instruction handlers for the build engine.

use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};

use super::super::dockerfile::Instruction;
use super::super::dockerignore::DockerIgnore;
use super::super::layer::{
    create_layer_from_dir_with_chown, create_layer_with_chown, create_layer_with_deletions,
    LayerInfo,
};
use super::utils::{
    copy_dir_filtered, expand_args, extract_tar_to_dst, is_tar_archive, resolve_chown, resolve_path,
};
use super::BuildState;

#[cfg(target_os = "macos")]
const UNSAFE_HOST_RUN_ENV: &str = "A3S_BOX_UNSAFE_HOST_RUN";

/// Whether a COPY/ADD source contains shell glob metacharacters.
fn has_glob_meta(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[')
}

/// Match a single path-segment glob (`*` = any run, `?` = one char) against a
/// name. Used for COPY/ADD wildcard expansion (the final segment of the source).
fn glob_segment_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    // Classic two-pointer wildcard match with `*` backtracking.
    let (mut pi, mut ni) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Expand a COPY/ADD source pattern against `base_dir` into concrete relative
/// paths. Globs are honored in the final path segment (the common Docker case,
/// e.g. `*.conf` or `src/*.txt`). Returns the matches sorted; empty if none.
fn expand_glob_sources(base_dir: &Path, pattern: &str) -> Vec<String> {
    let p = pattern.trim_start_matches('/');
    let (dir_part, name_pat) = match p.rsplit_once('/') {
        Some((d, n)) => (d, n),
        None => ("", p),
    };
    let search_dir = if dir_part.is_empty() {
        base_dir.to_path_buf()
    } else {
        base_dir.join(dir_part)
    };
    let mut matches = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&search_dir) {
        for entry in entries.flatten() {
            let fname = entry.file_name();
            let fname = fname.to_string_lossy();
            if glob_segment_match(name_pat, &fname) {
                matches.push(if dir_part.is_empty() {
                    fname.into_owned()
                } else {
                    format!("{}/{}", dir_part, fname)
                });
            }
        }
    }
    matches.sort();
    matches
}

/// Resolve COPY/ADD source patterns, expanding any globs against `base_dir`.
/// A non-glob source is passed through verbatim; a glob with no matches errors
/// like Docker ("no source files were specified").
fn resolve_source_patterns(base_dir: &Path, src_patterns: &[String]) -> Result<Vec<String>> {
    let mut resolved = Vec::new();
    for src in src_patterns {
        if src.starts_with("http://") || src.starts_with("https://") {
            // Remote ADD sources are never globbed (and may contain `?` query
            // strings); pass them through untouched.
            resolved.push(src.clone());
        } else if has_glob_meta(src) {
            let matches = expand_glob_sources(base_dir, src);
            if matches.is_empty() {
                return Err(BoxError::BuildError(format!(
                    "COPY/ADD source not found: no matches for pattern '{}'",
                    src
                )));
            }
            resolved.extend(matches);
        } else {
            resolved.push(src.clone());
        }
    }
    Ok(resolved)
}

/// Handle COPY: copy files from build context into rootfs, create a layer.
#[allow(clippy::too_many_arguments)]
pub(super) fn handle_copy(
    src_patterns: &[String],
    dst: &str,
    chown: Option<&str>,
    context_dir: &Path,
    rootfs_dir: &Path,
    layers_dir: &Path,
    workdir: &str,
    layer_index: usize,
    ignore: Option<&DockerIgnore>,
) -> Result<LayerInfo> {
    // Expand any glob source patterns against the context (Docker semantics).
    let src_patterns = &resolve_source_patterns(context_dir, src_patterns)?;

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
        // Resolve the source relative to the context (or, for COPY --from, the
        // source stage's rootfs). A leading "/" must NOT be treated as a host
        // absolute path: `Path::join` discards the base for an absolute arg, so
        // `rootfs.join("/run.sh")` would wrongly become "/run.sh". COPY --from
        // sources are conventionally absolute, so strip the leading slash.
        let rel = PathBuf::from(if src == "." {
            ""
        } else {
            src.trim_start_matches('/')
        });
        let src_path = context_dir.join(src.trim_start_matches('/'));
        if !src_path.exists() {
            return Err(BoxError::BuildError(format!(
                "COPY source not found: {} (in context {})",
                src,
                context_dir.display()
            )));
        }

        // A single source excluded by .dockerignore is not in the build context.
        if let Some(ign) = ignore {
            if !rel.as_os_str().is_empty() && src_path.is_file() && ign.is_excluded(&rel) {
                return Err(BoxError::BuildError(format!(
                    "COPY source not found: {} (excluded by .dockerignore)",
                    src
                )));
            }
        }

        if src_path.is_dir() {
            copy_dir_filtered(&src_path, &dst_in_rootfs, &rel, ignore)?;
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

    // Resolve --chown uid/gid (header-level, no host filesystem ownership change
    // required — Docker BuildKit sets tar headers rather than calling chown).
    let chown_ids = if let Some(spec) = chown {
        Some(resolve_chown(spec, rootfs_dir)?)
    } else {
        None
    };

    // Create a layer from the copied files
    let layer_path = layers_dir.join(format!("layer_{}.tar.gz", layer_index));
    let target_prefix = Path::new(resolved_dst.trim_start_matches('/'));
    if dst_in_rootfs.is_dir() {
        create_layer_from_dir_with_chown(&dst_in_rootfs, target_prefix, &layer_path, chown_ids)
    } else if dst_in_rootfs.parent().is_some() {
        let changed = vec![PathBuf::from(
            dst_in_rootfs
                .strip_prefix(rootfs_dir)
                .unwrap_or(target_prefix),
        )];
        create_layer_with_chown(rootfs_dir, &changed, &[], &layer_path, chown_ids)
    } else {
        Err(BoxError::BuildError("Invalid COPY destination".to_string()))
    }
}

/// Handle RUN: execute a command in the rootfs.
///
/// On Linux, uses chroot. On macOS, isolated RUN execution is not implemented yet.
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
        if !unsafe_host_run_enabled() {
            return Err(BoxError::BuildError(format!(
                "Dockerfile RUN is not supported on macOS yet because isolated Linux build \
                 execution is not implemented. Re-run on Linux or set {UNSAFE_HOST_RUN_ENV}=1 \
                 to opt into unsafe host-side execution for local experiments."
            )));
        }

        handle_run_on_host_unsafe(
            command,
            rootfs_dir,
            layers_dir,
            workdir,
            env,
            shell,
            layer_index,
            quiet,
        )
    }

    // Linux: execute via chroot
    #[cfg(target_os = "linux")]
    {
        use super::super::layer::DirSnapshot;

        validate_linux_run_preconditions(rootfs_dir, shell, linux_effective_uid())?;
        let workdir_path = ensure_linux_run_workdir(rootfs_dir, workdir)?;

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
        cmd.current_dir(&workdir_path);

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
        let deleted = before.deletions(&after);

        if changed.is_empty() && deleted.is_empty() {
            return Ok(None);
        }

        let layer_path = layers_dir.join(format!("layer_{}.tar.gz", layer_index));
        let layer_info =
            create_layer_with_deletions(rootfs_dir, &changed, &deleted, &layer_path)?;
        Ok(Some(layer_info))
    }

    // Other platforms: not supported
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (
            rootfs_dir,
            layers_dir,
            workdir,
            env,
            shell,
            layer_index,
            quiet,
        );
        Err(BoxError::BuildError(format!(
            "Dockerfile RUN is not supported on this platform yet because isolated Linux build execution is not implemented: {}",
            command
        )))
    }
}

#[cfg(any(target_os = "linux", test))]
fn linux_run_shell_path(shell: &[String]) -> &str {
    shell.first().map(String::as_str).unwrap_or("/bin/sh")
}

#[cfg(any(target_os = "linux", test))]
fn validate_linux_run_preconditions(
    rootfs_dir: &Path,
    shell: &[String],
    effective_uid: u32,
) -> Result<()> {
    if effective_uid != 0 {
        return Err(BoxError::BuildError(
            "Dockerfile RUN on Linux requires root privileges because the current isolated build path uses chroot. Re-run as root or build on a root-capable builder.".to_string(),
        ));
    }

    let shell_path = linux_run_shell_path(shell);
    if !shell_path.starts_with('/') {
        return Err(BoxError::BuildError(format!(
            "Dockerfile RUN shell '{}' is not absolute; SHELL must name an absolute in-rootfs executable",
            shell_path
        )));
    }
    let shell_in_rootfs = rootfs_dir.join(shell_path.trim_start_matches('/'));
    if !shell_in_rootfs.exists() {
        return Err(BoxError::BuildError(format!(
            "Dockerfile RUN shell '{}' was not found in rootfs at {}; the base image must contain the configured shell",
            shell_path,
            shell_in_rootfs.display()
        )));
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_effective_uid() -> u32 {
    unsafe { libc::geteuid() }
}

#[cfg(any(target_os = "linux", test))]
fn ensure_linux_run_workdir(rootfs_dir: &Path, workdir: &str) -> Result<PathBuf> {
    let workdir = if workdir.trim().is_empty() {
        "/"
    } else {
        workdir
    };
    if !workdir.starts_with('/') {
        return Err(BoxError::BuildError(format!(
            "Dockerfile RUN workdir '{}' is not absolute",
            workdir
        )));
    }

    let workdir_path = rootfs_dir.join(workdir.trim_start_matches('/'));
    std::fs::create_dir_all(&workdir_path).map_err(|e| {
        BoxError::BuildError(format!(
            "Failed to create RUN workdir {}: {}",
            workdir_path.display(),
            e
        ))
    })?;
    Ok(workdir_path)
}

#[cfg(target_os = "macos")]
fn unsafe_host_run_enabled() -> bool {
    std::env::var(UNSAFE_HOST_RUN_ENV)
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

/// Execute RUN command directly on host (unsafe macOS escape hatch).
///
/// This does not provide container/Linux build semantics. It exists only for
/// explicit local experiments while isolated macOS build execution is pending.
#[cfg(target_os = "macos")]
#[allow(clippy::too_many_arguments)]
fn handle_run_on_host_unsafe(
    command: &str,
    rootfs_dir: &Path,
    layers_dir: &Path,
    workdir: &str,
    _env: &[(String, String)],
    shell: &[String],
    layer_index: usize,
    quiet: bool,
) -> Result<Option<LayerInfo>> {
    use super::super::layer::DirSnapshot;

    if !quiet {
        println!("→ Executing RUN command on host (unsafe)");
    }

    // Capture filesystem state before execution
    let before = DirSnapshot::capture(rootfs_dir)?;

    // Build the shell command
    let shell_cmd = if !shell.is_empty() {
        let mut parts = shell.to_vec();
        parts.push(command.to_string());
        parts
    } else {
        vec!["/bin/sh".to_string(), "-c".to_string(), command.to_string()]
    };

    // Execute command in rootfs directory
    if !quiet {
        println!("→ Executing: {}", command);
    }

    let workdir_path = if workdir.is_empty() || workdir == "/" {
        rootfs_dir.to_path_buf()
    } else {
        rootfs_dir.join(workdir.trim_start_matches('/'))
    };

    // Ensure workdir exists
    if !workdir_path.exists() {
        std::fs::create_dir_all(&workdir_path).map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to create workdir {}: {}",
                workdir_path.display(),
                e
            ))
        })?;
    }

    let output = std::process::Command::new(&shell_cmd[0])
        .args(&shell_cmd[1..])
        .current_dir(&workdir_path)
        .output()
        .map_err(|e| BoxError::BuildError(format!("Failed to execute command: {}", e)))?;

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

    // Capture filesystem state after execution
    let after = DirSnapshot::capture(rootfs_dir)?;
    let changed = before.diff(&after);
    let deleted = before.deletions(&after);

    if changed.is_empty() && deleted.is_empty() {
        if !quiet {
            println!("→ No filesystem changes detected");
        }
        return Ok(None);
    }

    // Create layer from changes (and OCI whiteouts for deletions)
    let layer_path = layers_dir.join(format!("layer_{}.tar.gz", layer_index));
    let layer_info = create_layer_with_deletions(rootfs_dir, &changed, &deleted, &layer_path)?;

    if !quiet {
        println!(
            "→ Created layer with {} changes, {} deletions",
            changed.len(),
            deleted.len()
        );
    }

    Ok(Some(layer_info))
}

/// Handle ADD: like COPY but supports URL download and tar auto-extraction.
#[allow(clippy::too_many_arguments)]
pub(super) fn handle_add(
    src_patterns: &[String],
    dst: &str,
    chown: Option<&str>,
    context_dir: &Path,
    rootfs_dir: &Path,
    layers_dir: &Path,
    workdir: &str,
    layer_index: usize,
    ignore: Option<&DockerIgnore>,
) -> Result<LayerInfo> {
    let chown_ids = if let Some(spec) = chown {
        Some(resolve_chown(spec, rootfs_dir)?)
    } else {
        None
    };

    // Expand any glob source patterns against the context (Docker semantics);
    // remote URL sources pass through untouched.
    let src_patterns = &resolve_source_patterns(context_dir, src_patterns)?;

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

        // See handle_copy: strip a leading slash so an absolute src resolves
        // within the context rather than discarding the base in `Path::join`.
        let rel = PathBuf::from(if src == "." {
            ""
        } else {
            src.trim_start_matches('/')
        });
        let src_path = context_dir.join(src.trim_start_matches('/'));
        if !src_path.exists() {
            return Err(BoxError::BuildError(format!(
                "ADD source not found: {} (in context {})",
                src,
                context_dir.display()
            )));
        }

        if let Some(ign) = ignore {
            if !rel.as_os_str().is_empty() && src_path.is_file() && ign.is_excluded(&rel) {
                return Err(BoxError::BuildError(format!(
                    "ADD source not found: {} (excluded by .dockerignore)",
                    src
                )));
            }
        }

        // Check if it's a tar archive that should be auto-extracted
        if is_tar_archive(src) && !src_path.is_dir() {
            extract_tar_to_dst(&src_path, &dst_in_rootfs)?;
        } else if src_path.is_dir() {
            copy_dir_filtered(&src_path, &dst_in_rootfs, &rel, ignore)?;
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

    // Create a layer from the destination, stamping --chown into tar headers.
    let layer_path = layers_dir.join(format!("layer_{}.tar.gz", layer_index));
    let target_prefix = Path::new(resolved_dst.trim_start_matches('/'));
    if dst_in_rootfs.is_dir() {
        create_layer_from_dir_with_chown(&dst_in_rootfs, target_prefix, &layer_path, chown_ids)
    } else if dst_in_rootfs.parent().is_some() {
        let changed = vec![PathBuf::from(
            dst_in_rootfs
                .strip_prefix(rootfs_dir)
                .unwrap_or(target_prefix),
        )];
        create_layer_with_chown(rootfs_dir, &changed, &[], &layer_path, chown_ids)
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
        Instruction::Env { vars } => {
            for (key, value) in vars {
                let expanded = expand_args(value, &state.build_args);
                if let Some(existing) = state.env.iter_mut().find(|(k, _)| k == key) {
                    existing.1 = expanded;
                } else {
                    state.env.push((key.clone(), expanded));
                }
            }
        }
        Instruction::Label { pairs } => {
            for (key, value) in pairs {
                state.labels.insert(key.clone(), value.clone());
            }
        }
        Instruction::Workdir { path } => {
            state.workdir = resolve_path(&state.workdir, path);
        }
        Instruction::Expose { ports } => {
            for port in ports {
                if !state.exposed_ports.contains(port) {
                    state.exposed_ports.push(port.clone());
                }
            }
        }
        Instruction::User { user } => {
            state.user = Some(user.clone());
        }
        _ => {
            return Err(BoxError::BuildError(format!(
                "ONBUILD trigger '{}' is not supported yet because it requires build execution context",
                trigger
            )));
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
        Instruction::Copy {
            src,
            dst,
            from,
            chown,
        } => {
            let mut prefix = String::from("COPY");
            if let Some(f) = from {
                prefix.push_str(&format!(" --from={}", f));
            }
            if let Some(c) = chown {
                prefix.push_str(&format!(" --chown={}", c));
            }
            format!("{} {} {}", prefix, src.join(" "), dst)
        }
        Instruction::Add { src, dst, chown } => {
            if let Some(c) = chown {
                format!("ADD --chown={} {} {}", c, src.join(" "), dst)
            } else {
                format!("ADD {} {}", src.join(" "), dst)
            }
        }
        Instruction::Workdir { path } => format!("WORKDIR {}", path),
        Instruction::Env { vars } => {
            let pairs: Vec<String> = vars.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
            format!("ENV {}", pairs.join(" "))
        }
        Instruction::Entrypoint { exec } => format!("ENTRYPOINT {:?}", exec),
        Instruction::Cmd { exec } => format!("CMD {:?}", exec),
        Instruction::Expose { ports } => format!("EXPOSE {}", ports.join(" ")),
        Instruction::Label { pairs } => format!(
            "LABEL {}",
            pairs
                .iter()
                .map(|(k, v)| format!("{}={}", k, v))
                .collect::<Vec<_>>()
                .join(" ")
        ),
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
                .no_proxy()
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

#[cfg(test)]
mod tests {
    use super::super::super::dockerfile::Instruction;
    use super::{
        execute_onbuild_trigger, expand_glob_sources, glob_segment_match, handle_add,
        instruction_to_string,
    };
    use crate::oci::build::engine::{BuildConfig, BuildState};
    use std::collections::HashMap;
    use std::path::PathBuf;

    #[test]
    fn test_glob_segment_match() {
        assert!(glob_segment_match("*.conf", "alpha.conf"));
        assert!(glob_segment_match("*.conf", ".conf"));
        assert!(!glob_segment_match("*.conf", "skip.txt"));
        assert!(glob_segment_match("a?c", "abc"));
        assert!(!glob_segment_match("a?c", "ac"));
        assert!(glob_segment_match("*", "anything"));
        assert!(glob_segment_match("pre*post", "pre_middle_post"));
        assert!(!glob_segment_match("pre*post", "pre_middle"));
    }

    #[test]
    fn test_expand_glob_sources() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("alpha.conf"), "1").unwrap();
        std::fs::write(dir.path().join("beta.conf"), "2").unwrap();
        std::fs::write(dir.path().join("skip.txt"), "x").unwrap();
        let mut got = expand_glob_sources(dir.path(), "*.conf");
        got.sort();
        assert_eq!(got, vec!["alpha.conf".to_string(), "beta.conf".to_string()]);
        // Non-matching glob yields no entries.
        assert!(expand_glob_sources(dir.path(), "*.md").is_empty());
    }

    #[test]
    fn test_instruction_to_string_run() {
        let instr = Instruction::Run {
            command: "echo hello".to_string(),
        };
        assert_eq!(instruction_to_string(&instr), "RUN echo hello");
    }

    #[test]
    fn test_instruction_to_string_copy() {
        let instr = Instruction::Copy {
            src: vec!["file1.txt".to_string(), "file2.txt".to_string()],
            dst: "/app/".to_string(),
            from: None,
            chown: None,
        };
        assert_eq!(
            instruction_to_string(&instr),
            "COPY file1.txt file2.txt /app/"
        );
    }

    #[test]
    fn test_instruction_to_string_copy_from_stage() {
        let instr = Instruction::Copy {
            src: vec!["app".to_string()],
            dst: "/usr/local/bin/".to_string(),
            from: Some("builder".to_string()),
            chown: None,
        };
        assert_eq!(
            instruction_to_string(&instr),
            "COPY --from=builder app /usr/local/bin/"
        );
    }

    #[test]
    fn test_instruction_to_string_add() {
        let instr = Instruction::Add {
            src: vec!["app.tar.gz".to_string()],
            dst: "/app/".to_string(),
            chown: Some("1000:1000".to_string()),
        };
        assert_eq!(
            instruction_to_string(&instr),
            "ADD --chown=1000:1000 app.tar.gz /app/"
        );
    }

    #[test]
    fn test_instruction_to_string_add_no_chown() {
        let instr = Instruction::Add {
            src: vec!["file.tar.gz".to_string()],
            dst: "/tmp/".to_string(),
            chown: None,
        };
        assert_eq!(instruction_to_string(&instr), "ADD file.tar.gz /tmp/");
    }

    #[test]
    fn test_instruction_to_string_env() {
        let instr = Instruction::Env {
            vars: vec![("PATH".to_string(), "/usr/local/bin:/usr/bin".to_string())],
        };
        assert_eq!(
            instruction_to_string(&instr),
            "ENV PATH=/usr/local/bin:/usr/bin"
        );
    }

    #[test]
    fn test_instruction_to_string_workdir() {
        let instr = Instruction::Workdir {
            path: "/app".to_string(),
        };
        assert_eq!(instruction_to_string(&instr), "WORKDIR /app");
    }

    #[test]
    fn test_instruction_to_string_entrypoint() {
        let instr = Instruction::Entrypoint {
            exec: vec!["/bin/agent".to_string(), "--listen".to_string()],
        };
        assert_eq!(
            instruction_to_string(&instr),
            "ENTRYPOINT [\"/bin/agent\", \"--listen\"]"
        );
    }

    #[test]
    fn test_instruction_to_string_cmd() {
        let instr = Instruction::Cmd {
            exec: vec!["python".to_string(), "app.py".to_string()],
        };
        assert_eq!(
            instruction_to_string(&instr),
            "CMD [\"python\", \"app.py\"]"
        );
    }

    #[test]
    fn test_instruction_to_string_expose() {
        let instr = Instruction::Expose {
            ports: vec!["8080/tcp".to_string()],
        };
        assert_eq!(instruction_to_string(&instr), "EXPOSE 8080/tcp");
    }

    #[test]
    fn test_instruction_to_string_label() {
        let instr = Instruction::Label {
            pairs: vec![("version".to_string(), "1.0.0".to_string())],
        };
        assert_eq!(instruction_to_string(&instr), "LABEL version=1.0.0");
    }

    #[test]
    fn test_instruction_to_string_user() {
        let instr = Instruction::User {
            user: "nobody".to_string(),
        };
        assert_eq!(instruction_to_string(&instr), "USER nobody");
    }

    #[test]
    fn test_instruction_to_string_arg_no_default() {
        let instr = Instruction::Arg {
            name: "VERSION".to_string(),
            default: None,
        };
        assert_eq!(instruction_to_string(&instr), "ARG VERSION");
    }

    #[test]
    fn test_instruction_to_string_arg_with_default() {
        let instr = Instruction::Arg {
            name: "VERSION".to_string(),
            default: Some("1.0.0".to_string()),
        };
        assert_eq!(instruction_to_string(&instr), "ARG VERSION=1.0.0");
    }

    #[test]
    fn test_instruction_to_string_shell() {
        let instr = Instruction::Shell {
            exec: vec!["/bin/bash".to_string(), "-c".to_string()],
        };
        assert_eq!(
            instruction_to_string(&instr),
            "SHELL [\"/bin/bash\", \"-c\"]"
        );
    }

    #[test]
    fn test_instruction_to_string_stopsignal() {
        let instr = Instruction::StopSignal {
            signal: "SIGTERM".to_string(),
        };
        assert_eq!(instruction_to_string(&instr), "STOPSIGNAL SIGTERM");
    }

    #[test]
    fn test_instruction_to_string_healthcheck_none() {
        let instr = Instruction::HealthCheck {
            cmd: None,
            interval: None,
            timeout: None,
            retries: None,
            start_period: None,
        };
        assert_eq!(instruction_to_string(&instr), "HEALTHCHECK NONE");
    }

    #[test]
    fn test_instruction_to_string_healthcheck_with_cmd() {
        let instr = Instruction::HealthCheck {
            cmd: Some(vec![
                "curl".to_string(),
                "-f".to_string(),
                "http://localhost/".to_string(),
            ]),
            interval: Some(10),
            timeout: Some(5),
            retries: Some(3),
            start_period: Some(30),
        };
        assert_eq!(
            instruction_to_string(&instr),
            "HEALTHCHECK CMD curl -f http://localhost/"
        );
    }

    #[test]
    fn test_instruction_to_string_volume() {
        let instr = Instruction::Volume {
            paths: vec!["/data".to_string(), "/var/log".to_string()],
        };
        assert_eq!(instruction_to_string(&instr), "VOLUME /data /var/log");
    }

    #[test]
    fn test_instruction_to_string_from() {
        let instr = Instruction::From {
            image: "alpine:3.19".to_string(),
            alias: None,
        };
        assert_eq!(instruction_to_string(&instr), "FROM alpine:3.19");
    }

    #[test]
    fn test_instruction_to_string_from_with_alias() {
        let instr = Instruction::From {
            image: "golang:1.21".to_string(),
            alias: Some("builder".to_string()),
        };
        assert_eq!(instruction_to_string(&instr), "FROM golang:1.21 AS builder");
    }

    #[test]
    fn test_instruction_to_string_onbuild() {
        let inner = Instruction::Run {
            command: "echo triggered".to_string(),
        };
        let instr = Instruction::OnBuild {
            instruction: Box::new(inner),
        };
        assert_eq!(instruction_to_string(&instr), "ONBUILD RUN echo triggered");
    }

    #[test]
    fn test_handle_add_chown_numeric_uid_gid() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        let layers = tmp.path().join("layers");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&layers).unwrap();
        // Write the file so ADD can find it
        std::fs::write(tmp.path().join("file.txt"), "data").unwrap();

        // Numeric uid:gid — resolves without /etc/passwd, should succeed.
        let result = handle_add(
            &["file.txt".to_string()],
            "/tmp/file.txt",
            Some("1000:1000"),
            tmp.path(),
            &rootfs,
            &layers,
            "/",
            0,
            None,
        );
        assert!(
            result.is_ok(),
            "ADD --chown with numeric uid:gid should succeed: {:?}",
            result.err()
        );
        // Checking that the layer was created is sufficient for unit coverage.
        assert!(result.unwrap().path.exists());
    }

    #[test]
    fn test_execute_onbuild_trigger_rejects_execution_instruction() {
        let mut state = BuildState::new(HashMap::new());
        let config = BuildConfig {
            context_dir: PathBuf::from("/tmp/context"),
            dockerfile_path: PathBuf::from("/tmp/context/Dockerfile"),
            tag: None,
            build_args: HashMap::new(),
            quiet: true,
            platforms: vec![],
            target: None,
            no_cache: false,
            metrics: None,
        };
        let tmp = tempfile::TempDir::new().unwrap();

        let err = execute_onbuild_trigger(
            "RUN echo trigger",
            &mut state,
            &config,
            tmp.path(),
            tmp.path(),
            &[],
            &[],
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("ONBUILD trigger 'RUN echo trigger' is not supported yet"));
    }

    #[test]
    fn test_linux_run_shell_path_defaults_to_bin_sh() {
        assert_eq!(super::linux_run_shell_path(&[]), "/bin/sh");
        assert_eq!(
            super::linux_run_shell_path(&["/bin/bash".to_string(), "-c".to_string()]),
            "/bin/bash"
        );
    }

    #[test]
    fn test_linux_run_preconditions_reject_non_root() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        std::fs::create_dir_all(rootfs.join("bin")).unwrap();
        std::fs::write(rootfs.join("bin/sh"), "fake shell").unwrap();

        let err = super::validate_linux_run_preconditions(&rootfs, &[], 1000)
            .unwrap_err()
            .to_string();

        assert!(err.contains("requires root privileges"));
    }

    #[test]
    fn test_linux_run_preconditions_reject_missing_shell() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();

        let err = super::validate_linux_run_preconditions(&rootfs, &[], 0)
            .unwrap_err()
            .to_string();

        assert!(err.contains("was not found in rootfs"));
    }

    #[test]
    fn test_linux_run_preconditions_reject_relative_shell() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();

        let err = super::validate_linux_run_preconditions(&rootfs, &["sh".to_string()], 0)
            .unwrap_err()
            .to_string();

        assert!(err.contains("is not absolute"));
    }

    #[test]
    fn test_ensure_linux_run_workdir_creates_absolute_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();

        let workdir = super::ensure_linux_run_workdir(&rootfs, "/app/build").unwrap();

        assert_eq!(workdir, rootfs.join("app/build"));
        assert!(workdir.is_dir());
    }

    #[test]
    fn test_ensure_linux_run_workdir_rejects_relative_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();

        let err = super::ensure_linux_run_workdir(&rootfs, "app")
            .unwrap_err()
            .to_string();

        assert!(err.contains("is not absolute"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_handle_run_rejects_macos_without_unsafe_opt_in() {
        std::env::remove_var(super::UNSAFE_HOST_RUN_ENV);

        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        let layers = tmp.path().join("layers");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&layers).unwrap();

        let result = super::handle_run("echo unsafe", &rootfs, &layers, "/", &[], &[], 0, true);
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Dockerfile RUN is not supported on macOS yet"));
        assert!(err.contains(super::UNSAFE_HOST_RUN_ENV));
    }
}
