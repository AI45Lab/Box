//! Build engine for constructing OCI images from Dockerfiles.
//!
//! Orchestrates the build process: parses the Dockerfile, pulls the base image,
//! executes each instruction, creates layers, and assembles the final OCI image.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::platform::Platform;

use super::dockerfile::{Dockerfile, Instruction};
use super::layer::{sha256_bytes, sha256_file, LayerInfo};
use crate::oci::image::OciImageConfig;
use crate::oci::layers::extract_layer;
use crate::oci::store::ImageStore;
use crate::oci::{ImagePuller, RegistryAuth};

mod handlers;
mod stages;
mod utils;

#[cfg(test)]
mod tests;

use handlers::{
    apply_base_config, execute_onbuild_trigger, handle_add, handle_copy, handle_run,
    instruction_to_string,
};
use stages::{resolve_stage_rootfs, split_into_stages};
use utils::{compute_diff_id, expand_args, format_size, resolve_path};

/// Configuration for a build operation.
#[derive(Debug, Clone)]
pub struct BuildConfig {
    /// Path to the build context directory
    pub context_dir: PathBuf,
    /// Path to the Dockerfile (relative to context or absolute)
    pub dockerfile_path: PathBuf,
    /// Image tag (e.g., "myimage:latest")
    pub tag: Option<String>,
    /// Build arguments (ARG overrides)
    pub build_args: HashMap<String, String>,
    /// Suppress build output
    pub quiet: bool,
    /// Target platforms for multi-platform builds.
    /// Empty means build for the host platform only.
    pub platforms: Vec<Platform>,
}

/// Result of a successful build.
#[derive(Debug)]
pub struct BuildResult {
    /// Image reference stored in the image store
    pub reference: String,
    /// Content digest
    pub digest: String,
    /// Total image size in bytes
    pub size: u64,
    /// Number of layers
    pub layer_count: usize,
}

/// Mutable state accumulated during the build.
pub(super) struct BuildState {
    /// Working directory inside the image
    pub(super) workdir: String,
    /// Environment variables
    pub(super) env: Vec<(String, String)>,
    /// Entrypoint
    pub(super) entrypoint: Option<Vec<String>>,
    /// Default command
    pub(super) cmd: Option<Vec<String>>,
    /// User
    pub(super) user: Option<String>,
    /// Exposed ports
    pub(super) exposed_ports: Vec<String>,
    /// Labels
    pub(super) labels: HashMap<String, String>,
    /// Layer info accumulated during build
    pub(super) layers: Vec<LayerInfo>,
    /// Diff IDs (uncompressed layer digests) for the OCI config
    pub(super) diff_ids: Vec<String>,
    /// History entries
    pub(super) history: Vec<HistoryEntry>,
    /// Build arguments
    pub(super) build_args: HashMap<String, String>,
    /// Shell override (default: ["/bin/sh", "-c"])
    pub(super) shell: Vec<String>,
    /// Stop signal
    pub(super) stop_signal: Option<String>,
    /// Health check configuration
    pub(super) health_check: Option<OciHealthCheck>,
    /// ONBUILD triggers to store in the image config
    pub(super) onbuild: Vec<String>,
    /// Volumes declared via VOLUME instruction
    pub(super) volumes: Vec<String>,
}

/// A single history entry for the OCI config.
#[derive(Debug, Clone)]
pub(super) struct HistoryEntry {
    pub(super) created_by: String,
    pub(super) empty_layer: bool,
}

/// Health check configuration for OCI image config.
#[derive(Debug, Clone)]
pub struct OciHealthCheck {
    pub test: Vec<String>,
    pub interval: Option<u64>,
    pub timeout: Option<u64>,
    pub retries: Option<u32>,
    pub start_period: Option<u64>,
}

impl BuildState {
    fn new(build_args: HashMap<String, String>) -> Self {
        Self {
            workdir: "/".to_string(),
            env: Vec::new(),
            entrypoint: None,
            cmd: None,
            user: None,
            exposed_ports: Vec::new(),
            labels: HashMap::new(),
            layers: Vec::new(),
            diff_ids: Vec::new(),
            history: Vec::new(),
            build_args,
            shell: vec!["/bin/sh".to_string(), "-c".to_string()],
            stop_signal: None,
            health_check: None,
            onbuild: Vec::new(),
            volumes: Vec::new(),
        }
    }
}

/// Execute a full image build from a Dockerfile.
///
/// # Process
///
/// 1. Parse the Dockerfile
/// 2. Pull the base image (FROM)
/// 3. Extract base image layers into a temporary rootfs
/// 4. Execute each instruction, creating layers as needed
/// 5. Assemble the final OCI image layout
/// 6. Store in the image store with the given tag
///
/// Supports multi-stage builds: each FROM starts a new stage. Only the final
/// stage produces the output image. `COPY --from=<stage>` copies from a
/// previous stage's rootfs.
pub async fn build(config: BuildConfig, store: Arc<ImageStore>) -> Result<BuildResult> {
    // Parse Dockerfile
    let dockerfile = Dockerfile::from_file(&config.dockerfile_path)?;

    if !config.quiet {
        println!("Building from {}", config.dockerfile_path.display());
    }

    // Split instructions into stages by FROM
    let stages = split_into_stages(&dockerfile.instructions);
    let total_stages = stages.len();

    // Track completed stages: (alias, rootfs_path)
    let mut completed_stages: Vec<(Option<String>, PathBuf)> = Vec::new();

    // Create temp directory for build workspace
    let build_dir = tempfile::TempDir::new()
        .map_err(|e| BoxError::BuildError(format!("Failed to create build directory: {}", e)))?;

    let mut final_state = BuildState::new(config.build_args.clone());
    let mut final_base_layers: Vec<LayerInfo> = Vec::new();
    let mut final_base_diff_ids: Vec<String> = Vec::new();

    let total_instructions = dockerfile.instructions.len();
    let mut global_step = 0;

    for (stage_idx, stage) in stages.iter().enumerate() {
        let is_final_stage = stage_idx == total_stages - 1;

        let rootfs_dir = build_dir.path().join(format!("rootfs_{}", stage_idx));
        let layers_dir = build_dir.path().join(format!("layers_{}", stage_idx));
        std::fs::create_dir_all(&rootfs_dir).map_err(|e| {
            BoxError::BuildError(format!("Failed to create rootfs directory: {}", e))
        })?;
        std::fs::create_dir_all(&layers_dir).map_err(|e| {
            BoxError::BuildError(format!("Failed to create layers directory: {}", e))
        })?;

        let mut state = BuildState::new(config.build_args.clone());
        let mut base_layers: Vec<LayerInfo> = Vec::new();
        let mut base_diff_ids: Vec<String> = Vec::new();

        for instruction in &stage.instructions {
            global_step += 1;
            let step = global_step;

            match instruction {
                Instruction::From { image, alias } => {
                    if !config.quiet {
                        if total_stages > 1 {
                            println!(
                                "Step {}/{}: FROM {} (stage {}/{}{})",
                                step,
                                total_instructions,
                                image,
                                stage_idx + 1,
                                total_stages,
                                alias
                                    .as_ref()
                                    .map(|a| format!(" as {}", a))
                                    .unwrap_or_default()
                            );
                        } else {
                            println!("Step {}/{}: FROM {}", step, total_instructions, image);
                        }
                    }
                    let (layers, diff_ids, base_config) =
                        handle_from(image, &rootfs_dir, &layers_dir, &store, &state.build_args)
                            .await?;
                    base_layers = layers;
                    base_diff_ids = diff_ids;

                    // Inherit config from base image
                    apply_base_config(&mut state, &base_config);

                    // Execute ONBUILD triggers from base image
                    if !base_config.onbuild.is_empty() && !config.quiet {
                        println!(
                            "  Executing {} ONBUILD trigger(s) from base image",
                            base_config.onbuild.len()
                        );
                    }
                    for trigger in &base_config.onbuild {
                        execute_onbuild_trigger(
                            trigger,
                            &mut state,
                            &config,
                            &rootfs_dir,
                            &layers_dir,
                            &base_layers,
                            &completed_stages,
                        )?;
                    }

                    state.history.push(HistoryEntry {
                        created_by: format!("FROM {}", image),
                        empty_layer: true,
                    });
                }

                Instruction::Copy { src, dst, from } => {
                    if let Some(from_ref) = from {
                        if !config.quiet {
                            println!(
                                "Step {}/{}: COPY --from={} {} {}",
                                step,
                                total_instructions,
                                from_ref,
                                src.join(" "),
                                dst
                            );
                        }
                        let from_rootfs = resolve_stage_rootfs(from_ref, &completed_stages)?;
                        let layer_info = handle_copy(
                            src,
                            dst,
                            from_rootfs,
                            &rootfs_dir,
                            &layers_dir,
                            &state.workdir,
                            state.layers.len() + base_layers.len(),
                        )?;
                        state.diff_ids.push(compute_diff_id(&layer_info.path)?);
                        state.layers.push(layer_info);
                        state.history.push(HistoryEntry {
                            created_by: format!(
                                "COPY --from={} {} {}",
                                from_ref,
                                src.join(" "),
                                dst
                            ),
                            empty_layer: false,
                        });
                    } else {
                        if !config.quiet {
                            println!(
                                "Step {}/{}: COPY {} {}",
                                step,
                                total_instructions,
                                src.join(" "),
                                dst
                            );
                        }
                        let layer_info = handle_copy(
                            src,
                            dst,
                            &config.context_dir,
                            &rootfs_dir,
                            &layers_dir,
                            &state.workdir,
                            state.layers.len() + base_layers.len(),
                        )?;
                        state.diff_ids.push(compute_diff_id(&layer_info.path)?);
                        state.layers.push(layer_info);
                        state.history.push(HistoryEntry {
                            created_by: format!("COPY {} {}", src.join(" "), dst),
                            empty_layer: false,
                        });
                    }
                }

                Instruction::Add { src, dst, chown } => {
                    if !config.quiet {
                        println!(
                            "Step {}/{}: ADD {} {}",
                            step,
                            total_instructions,
                            src.join(" "),
                            dst
                        );
                    }
                    let layer_info = handle_add(
                        src,
                        dst,
                        chown.as_deref(),
                        &config.context_dir,
                        &rootfs_dir,
                        &layers_dir,
                        &state.workdir,
                        state.layers.len() + base_layers.len(),
                    )?;
                    state.diff_ids.push(compute_diff_id(&layer_info.path)?);
                    state.layers.push(layer_info);
                    state.history.push(HistoryEntry {
                        created_by: format!("ADD {} {}", src.join(" "), dst),
                        empty_layer: false,
                    });
                }

                Instruction::Run { command } => {
                    if !config.quiet {
                        println!("Step {}/{}: RUN {}", step, total_instructions, command);
                    }
                    let layer_opt = handle_run(
                        command,
                        &rootfs_dir,
                        &layers_dir,
                        &state.workdir,
                        &state.env,
                        &state.shell,
                        state.layers.len() + base_layers.len(),
                        config.quiet,
                    )?;
                    if let Some(layer_info) = layer_opt {
                        state.diff_ids.push(compute_diff_id(&layer_info.path)?);
                        state.layers.push(layer_info);
                        state.history.push(HistoryEntry {
                            created_by: format!("RUN {}", command),
                            empty_layer: false,
                        });
                    } else {
                        state.history.push(HistoryEntry {
                            created_by: format!("RUN {}", command),
                            empty_layer: true,
                        });
                    }
                }

                Instruction::Workdir { path } => {
                    if !config.quiet {
                        println!("Step {}/{}: WORKDIR {}", step, total_instructions, path);
                    }
                    state.workdir = resolve_path(&state.workdir, path);
                    let full = rootfs_dir.join(state.workdir.trim_start_matches('/'));
                    let _ = std::fs::create_dir_all(&full);
                    state.history.push(HistoryEntry {
                        created_by: format!("WORKDIR {}", path),
                        empty_layer: true,
                    });
                }

                Instruction::Env { key, value } => {
                    if !config.quiet {
                        println!(
                            "Step {}/{}: ENV {}={}",
                            step, total_instructions, key, value
                        );
                    }
                    let expanded_value = expand_args(value, &state.build_args);
                    if let Some(existing) = state.env.iter_mut().find(|(k, _)| k == key) {
                        existing.1 = expanded_value;
                    } else {
                        state.env.push((key.clone(), expanded_value));
                    }
                    state.history.push(HistoryEntry {
                        created_by: format!("ENV {}={}", key, value),
                        empty_layer: true,
                    });
                }

                Instruction::Entrypoint { exec } => {
                    if !config.quiet {
                        println!(
                            "Step {}/{}: ENTRYPOINT {:?}",
                            step, total_instructions, exec
                        );
                    }
                    state.entrypoint = Some(exec.clone());
                    state.history.push(HistoryEntry {
                        created_by: format!("ENTRYPOINT {:?}", exec),
                        empty_layer: true,
                    });
                }

                Instruction::Cmd { exec } => {
                    if !config.quiet {
                        println!("Step {}/{}: CMD {:?}", step, total_instructions, exec);
                    }
                    state.cmd = Some(exec.clone());
                    state.history.push(HistoryEntry {
                        created_by: format!("CMD {:?}", exec),
                        empty_layer: true,
                    });
                }

                Instruction::Expose { port } => {
                    if !config.quiet {
                        println!("Step {}/{}: EXPOSE {}", step, total_instructions, port);
                    }
                    state.exposed_ports.push(port.clone());
                    state.history.push(HistoryEntry {
                        created_by: format!("EXPOSE {}", port),
                        empty_layer: true,
                    });
                }

                Instruction::Label { key, value } => {
                    if !config.quiet {
                        println!(
                            "Step {}/{}: LABEL {}={}",
                            step, total_instructions, key, value
                        );
                    }
                    state.labels.insert(key.clone(), value.clone());
                    state.history.push(HistoryEntry {
                        created_by: format!("LABEL {}={}", key, value),
                        empty_layer: true,
                    });
                }

                Instruction::User { user } => {
                    if !config.quiet {
                        println!("Step {}/{}: USER {}", step, total_instructions, user);
                    }
                    state.user = Some(user.clone());
                    state.history.push(HistoryEntry {
                        created_by: format!("USER {}", user),
                        empty_layer: true,
                    });
                }

                Instruction::Arg { name, default } => {
                    if !config.quiet {
                        println!("Step {}/{}: ARG {}", step, total_instructions, name);
                    }
                    if !state.build_args.contains_key(name) {
                        if let Some(val) = default {
                            state.build_args.insert(name.clone(), val.clone());
                        }
                    }
                    state.history.push(HistoryEntry {
                        created_by: format!("ARG {}", name),
                        empty_layer: true,
                    });
                }

                Instruction::Shell { exec } => {
                    if !config.quiet {
                        println!("Step {}/{}: SHELL {:?}", step, total_instructions, exec);
                    }
                    state.shell = exec.clone();
                    state.history.push(HistoryEntry {
                        created_by: format!("SHELL {:?}", exec),
                        empty_layer: true,
                    });
                }

                Instruction::StopSignal { signal } => {
                    if !config.quiet {
                        println!(
                            "Step {}/{}: STOPSIGNAL {}",
                            step, total_instructions, signal
                        );
                    }
                    state.stop_signal = Some(signal.clone());
                    state.history.push(HistoryEntry {
                        created_by: format!("STOPSIGNAL {}", signal),
                        empty_layer: true,
                    });
                }

                Instruction::HealthCheck {
                    cmd,
                    interval,
                    timeout,
                    retries,
                    start_period,
                } => {
                    if !config.quiet {
                        if cmd.is_some() {
                            println!("Step {}/{}: HEALTHCHECK CMD ...", step, total_instructions);
                        } else {
                            println!("Step {}/{}: HEALTHCHECK NONE", step, total_instructions);
                        }
                    }
                    state.health_check = cmd.as_ref().map(|c| OciHealthCheck {
                        test: c.clone(),
                        interval: *interval,
                        timeout: *timeout,
                        retries: *retries,
                        start_period: *start_period,
                    });
                    state.history.push(HistoryEntry {
                        created_by: if cmd.is_some() {
                            "HEALTHCHECK CMD ...".to_string()
                        } else {
                            "HEALTHCHECK NONE".to_string()
                        },
                        empty_layer: true,
                    });
                }

                Instruction::OnBuild { instruction } => {
                    let trigger = format!("{:?}", instruction);
                    if !config.quiet {
                        println!("Step {}/{}: ONBUILD {}", step, total_instructions, trigger);
                    }
                    // Store the raw instruction text for the image config
                    state.onbuild.push(instruction_to_string(instruction));
                    state.history.push(HistoryEntry {
                        created_by: format!("ONBUILD {}", instruction_to_string(instruction)),
                        empty_layer: true,
                    });
                }

                Instruction::Volume { paths } => {
                    if !config.quiet {
                        println!(
                            "Step {}/{}: VOLUME {}",
                            step,
                            total_instructions,
                            paths.join(" ")
                        );
                    }
                    for p in paths {
                        if !state.volumes.contains(p) {
                            state.volumes.push(p.clone());
                        }
                    }
                    // Create volume directories in rootfs
                    for p in paths {
                        let full = rootfs_dir.join(p.trim_start_matches('/'));
                        let _ = std::fs::create_dir_all(&full);
                    }
                    state.history.push(HistoryEntry {
                        created_by: format!("VOLUME {}", paths.join(" ")),
                        empty_layer: true,
                    });
                }
            }
        }

        // Store completed stage rootfs for COPY --from
        completed_stages.push((stage.alias.clone(), rootfs_dir.clone()));

        if is_final_stage {
            final_state = state;
            final_base_layers = base_layers;
            final_base_diff_ids = base_diff_ids;
        }
    }

    // Assemble the final OCI image from the last stage
    let reference = config
        .tag
        .clone()
        .unwrap_or_else(|| "a3s-build:latest".to_string());

    let final_layers_dir = build_dir
        .path()
        .join(format!("layers_{}", total_stages - 1));

    // Determine target platform (use first platform or host default)
    let target_platform = config
        .platforms
        .first()
        .cloned()
        .unwrap_or_else(Platform::host);

    let result = assemble_image(
        &reference,
        &final_state,
        &final_base_layers,
        &final_base_diff_ids,
        &final_layers_dir,
        &store,
        &target_platform,
    )
    .await?;

    if !config.quiet {
        println!(
            "Successfully built {} ({} layers, {}, {})",
            reference,
            result.layer_count,
            format_size(result.size),
            target_platform,
        );
    }

    Ok(result)
}

// =============================================================================
// Helper functions
// =============================================================================

/// Handle FROM: pull base image and extract layers into rootfs.
///
/// Returns (base_layers, base_diff_ids, base_config).
async fn handle_from(
    image: &str,
    rootfs_dir: &Path,
    _layers_dir: &Path,
    store: &Arc<ImageStore>,
    build_args: &HashMap<String, String>,
) -> Result<(Vec<LayerInfo>, Vec<String>, OciImageConfig)> {
    let image_ref = expand_args(image, build_args);

    // Pull the base image
    let puller = ImagePuller::new(store.clone(), RegistryAuth::from_env());
    let oci_image = puller.pull(&image_ref).await?;

    // Extract all layers into rootfs
    for layer_path in oci_image.layer_paths() {
        extract_layer(layer_path, rootfs_dir)?;
    }

    // Collect base layer info
    let mut base_layers = Vec::new();
    let mut base_diff_ids = Vec::new();

    for layer_path in oci_image.layer_paths() {
        let digest = sha256_file(layer_path)?;
        let size = std::fs::metadata(layer_path).map(|m| m.len()).unwrap_or(0);

        // Compute diff_id (SHA256 of uncompressed content)
        let diff_id = compute_diff_id(layer_path)?;
        base_diff_ids.push(diff_id);

        base_layers.push(LayerInfo {
            path: layer_path.to_path_buf(),
            digest,
            size,
        });
    }

    let config = oci_image.config().clone();
    Ok((base_layers, base_diff_ids, config))
}

/// Assemble the final OCI image layout and store it.
async fn assemble_image(
    reference: &str,
    state: &BuildState,
    base_layers: &[LayerInfo],
    base_diff_ids: &[String],
    layers_dir: &Path,
    store: &Arc<ImageStore>,
    target_platform: &Platform,
) -> Result<BuildResult> {
    // Create output directory
    let output_dir = layers_dir.join("_output");
    let blobs_dir = output_dir.join("blobs").join("sha256");
    std::fs::create_dir_all(&blobs_dir)
        .map_err(|e| BoxError::BuildError(format!("Failed to create output blobs dir: {}", e)))?;

    // Collect all layers: base + new
    let mut all_layer_descriptors = Vec::new();
    let mut all_diff_ids: Vec<String> = base_diff_ids.to_vec();

    // Copy base layers to output
    for layer in base_layers {
        let blob_path = blobs_dir.join(&layer.digest);
        if !blob_path.exists() {
            std::fs::copy(&layer.path, &blob_path)
                .map_err(|e| BoxError::BuildError(format!("Failed to copy base layer: {}", e)))?;
        }
        all_layer_descriptors.push(serde_json::json!({
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": layer.prefixed_digest(),
            "size": layer.size
        }));
    }

    // Copy new layers to output
    for (i, layer) in state.layers.iter().enumerate() {
        let blob_path = blobs_dir.join(&layer.digest);
        if !blob_path.exists() {
            std::fs::copy(&layer.path, &blob_path)
                .map_err(|e| BoxError::BuildError(format!("Failed to copy layer {}: {}", i, e)))?;
        }
        all_layer_descriptors.push(serde_json::json!({
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": layer.prefixed_digest(),
            "size": layer.size
        }));
    }

    // Merge diff_ids
    all_diff_ids.extend(state.diff_ids.iter().cloned());

    // Build OCI config
    let now = chrono::Utc::now().to_rfc3339();
    let arch = target_platform.oci_arch();

    let env_list: Vec<String> = state
        .env
        .iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect();

    let mut config_obj = serde_json::json!({
        "architecture": arch,
        "os": "linux",
        "created": now,
        "config": {},
        "rootfs": {
            "type": "layers",
            "diff_ids": all_diff_ids.iter()
                .map(|d| format!("sha256:{}", d))
                .collect::<Vec<_>>()
        },
        "history": state.history.iter().map(|h| {
            let mut entry = serde_json::json!({
                "created": now,
                "created_by": h.created_by
            });
            if h.empty_layer {
                entry["empty_layer"] = serde_json::json!(true);
            }
            entry
        }).collect::<Vec<_>>()
    });

    // Populate config section
    let config_section = config_obj["config"].as_object_mut().unwrap();
    if !env_list.is_empty() {
        config_section.insert("Env".to_string(), serde_json::json!(env_list));
    }
    if let Some(ref ep) = state.entrypoint {
        config_section.insert("Entrypoint".to_string(), serde_json::json!(ep));
    }
    if let Some(ref cmd) = state.cmd {
        config_section.insert("Cmd".to_string(), serde_json::json!(cmd));
    }
    if state.workdir != "/" {
        config_section.insert("WorkingDir".to_string(), serde_json::json!(state.workdir));
    }
    if let Some(ref user) = state.user {
        config_section.insert("User".to_string(), serde_json::json!(user));
    }
    if !state.exposed_ports.is_empty() {
        let ports: HashMap<String, serde_json::Value> = state
            .exposed_ports
            .iter()
            .map(|p| (p.clone(), serde_json::json!({})))
            .collect();
        config_section.insert("ExposedPorts".to_string(), serde_json::json!(ports));
    }
    if !state.labels.is_empty() {
        config_section.insert("Labels".to_string(), serde_json::json!(state.labels));
    }
    if let Some(ref sig) = state.stop_signal {
        config_section.insert("StopSignal".to_string(), serde_json::json!(sig));
    }
    if let Some(ref hc) = state.health_check {
        let mut hc_obj = serde_json::json!({
            "Test": hc.test,
        });
        if let Some(interval) = hc.interval {
            // OCI stores intervals in nanoseconds
            hc_obj["Interval"] = serde_json::json!(interval * 1_000_000_000);
        }
        if let Some(timeout) = hc.timeout {
            hc_obj["Timeout"] = serde_json::json!(timeout * 1_000_000_000);
        }
        if let Some(retries) = hc.retries {
            hc_obj["Retries"] = serde_json::json!(retries);
        }
        if let Some(start_period) = hc.start_period {
            hc_obj["StartPeriod"] = serde_json::json!(start_period * 1_000_000_000);
        }
        config_section.insert("Healthcheck".to_string(), hc_obj);
    }
    if !state.onbuild.is_empty() {
        config_section.insert("OnBuild".to_string(), serde_json::json!(state.onbuild));
    }
    if !state.volumes.is_empty() {
        let vols: HashMap<String, serde_json::Value> = state
            .volumes
            .iter()
            .map(|v| (v.clone(), serde_json::json!({})))
            .collect();
        config_section.insert("Volumes".to_string(), serde_json::json!(vols));
    }

    // Write config blob
    let config_bytes = serde_json::to_vec_pretty(&config_obj)?;
    let config_digest = sha256_bytes(&config_bytes);
    std::fs::write(blobs_dir.join(&config_digest), &config_bytes)
        .map_err(|e| BoxError::BuildError(format!("Failed to write config blob: {}", e)))?;

    // Build manifest
    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": format!("sha256:{}", config_digest),
            "size": config_bytes.len()
        },
        "layers": all_layer_descriptors
    });

    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    let manifest_digest = sha256_bytes(&manifest_bytes);
    std::fs::write(blobs_dir.join(&manifest_digest), &manifest_bytes)
        .map_err(|e| BoxError::BuildError(format!("Failed to write manifest blob: {}", e)))?;

    // Write index.json
    let mut platform_obj = serde_json::json!({
        "os": target_platform.os,
        "architecture": target_platform.architecture
    });
    if let Some(ref variant) = target_platform.variant {
        platform_obj["variant"] = serde_json::json!(variant);
    }

    let index = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": format!("sha256:{}", manifest_digest),
            "size": manifest_bytes.len(),
            "platform": platform_obj
        }]
    });
    std::fs::write(
        output_dir.join("index.json"),
        serde_json::to_string_pretty(&index)?,
    )
    .map_err(|e| BoxError::BuildError(format!("Failed to write index.json: {}", e)))?;

    // Write oci-layout
    std::fs::write(
        output_dir.join("oci-layout"),
        r#"{"imageLayoutVersion":"1.0.0"}"#,
    )
    .map_err(|e| BoxError::BuildError(format!("Failed to write oci-layout: {}", e)))?;

    // Store in image store
    let digest_str = format!("sha256:{}", manifest_digest);
    let stored = store.put(reference, &digest_str, &output_dir).await?;

    let total_layers = base_layers.len() + state.layers.len();

    Ok(BuildResult {
        reference: reference.to_string(),
        digest: digest_str,
        size: stored.size_bytes,
        layer_count: total_layers,
    })
}
