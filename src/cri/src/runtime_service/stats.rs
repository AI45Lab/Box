//! Container and pod sandbox statistics and metrics for the CRI runtime service.
//!
//! Filesystem usage collection plus CPU/memory/network stat and metric
//! builders used by [`super::BoxRuntimeService`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use futures::future::join_all;

use crate::container::{Container, ContainerState};
use crate::cri_api::*;
use crate::sandbox::{PodSandbox, SandboxState};

fn zero_cpu_usage(now_ns: i64) -> CpuUsage {
    CpuUsage {
        timestamp: now_ns,
        usage_core_nano_seconds: Some(UInt64Value { value: 0 }),
        usage_nano_cores: Some(UInt64Value { value: 0 }),
    }
}

fn zero_memory_usage(now_ns: i64) -> MemoryUsage {
    MemoryUsage {
        timestamp: now_ns,
        working_set_bytes: Some(UInt64Value { value: 0 }),
        available_bytes: Some(UInt64Value { value: 0 }),
        usage_bytes: Some(UInt64Value { value: 0 }),
        rss_bytes: Some(UInt64Value { value: 0 }),
        page_faults: Some(UInt64Value { value: 0 }),
        major_page_faults: Some(UInt64Value { value: 0 }),
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct PathUsage {
    used_bytes: u64,
    inodes_used: u64,
}

impl PathUsage {
    fn add(&mut self, other: PathUsage) {
        self.used_bytes = self.used_bytes.saturating_add(other.used_bytes);
        self.inodes_used = self.inodes_used.saturating_add(other.inodes_used);
    }
}

fn should_collect_rootfs_usage(rootfs_path: &str) -> bool {
    let trimmed = rootfs_path.trim();
    !trimmed.is_empty() && Path::new(trimmed) != Path::new("/")
}

fn collect_path_usage(path: &Path) -> std::io::Result<PathUsage> {
    let metadata = std::fs::symlink_metadata(path)?;
    let mut usage = PathUsage {
        used_bytes: metadata.len(),
        inodes_used: 1,
    };

    if metadata.is_dir() {
        for entry in std::fs::read_dir(path)? {
            usage.add(collect_path_usage(&entry?.path())?);
        }
    }

    Ok(usage)
}

async fn rootfs_path_usage(rootfs_path: &str) -> Option<PathUsage> {
    if !should_collect_rootfs_usage(rootfs_path) {
        return None;
    }

    let path = PathBuf::from(rootfs_path);
    let display_path = path.display().to_string();
    match tokio::task::spawn_blocking(move || collect_path_usage(&path)).await {
        Ok(Ok(usage)) => Some(usage),
        Ok(Err(error)) => {
            tracing::debug!(
                rootfs_path = %display_path,
                error = %error,
                "Failed to collect CRI container rootfs usage"
            );
            None
        }
        Err(error) => {
            tracing::warn!(
                rootfs_path = %display_path,
                error = %error,
                "CRI container rootfs usage collection task failed"
            );
            None
        }
    }
}

fn writable_layer_usage(
    rootfs_path: &str,
    usage: Option<PathUsage>,
    now_ns: i64,
) -> FilesystemUsage {
    FilesystemUsage {
        timestamp: now_ns,
        fs_id: should_collect_rootfs_usage(rootfs_path).then(|| FilesystemIdentifier {
            mountpoint: rootfs_path.to_string(),
        }),
        used_bytes: Some(UInt64Value {
            value: usage.map(|usage| usage.used_bytes).unwrap_or(0),
        }),
        inodes_used: usage.map(|usage| UInt64Value {
            value: usage.inodes_used,
        }),
    }
}

pub(super) async fn container_stats(container: &Container) -> ContainerStats {
    let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
    let rootfs_usage = rootfs_path_usage(&container.rootfs_path).await;
    ContainerStats {
        attributes: Some(ContainerAttributes {
            id: container.id.clone(),
            metadata: Some(ContainerMetadata {
                name: container.name.clone(),
                attempt: 0,
            }),
            labels: container.labels.clone(),
            annotations: container.annotations.clone(),
        }),
        cpu: Some(zero_cpu_usage(now_ns)),
        memory: Some(zero_memory_usage(now_ns)),
        writable_layer: Some(writable_layer_usage(
            &container.rootfs_path,
            rootfs_usage,
            now_ns,
        )),
    }
}

pub(super) async fn pod_sandbox_stats(
    sandbox: &PodSandbox,
    containers: Vec<Container>,
) -> PodSandboxStats {
    let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
    let running_containers: Vec<Container> = containers
        .iter()
        .filter(|container| container.state == ContainerState::Running)
        .cloned()
        .collect();
    let container_stats = join_all(running_containers.iter().map(container_stats)).await;

    PodSandboxStats {
        attributes: Some(PodSandboxAttributes {
            id: sandbox.id.clone(),
            metadata: Some(PodSandboxMetadata {
                name: sandbox.name.clone(),
                uid: sandbox.uid.clone(),
                namespace: sandbox.namespace.clone(),
                attempt: 0,
            }),
            labels: sandbox.labels.clone(),
            annotations: sandbox.annotations.clone(),
        }),
        linux: Some(LinuxPodSandboxStats {
            cpu: Some(zero_cpu_usage(now_ns)),
            memory: Some(zero_memory_usage(now_ns)),
            network: Some(NetworkUsage {
                timestamp: now_ns,
                default_interface: None,
                interfaces: vec![],
            }),
            process: Some(ProcessUsage {
                timestamp: now_ns,
                process_count: Some(UInt64Value {
                    value: containers
                        .iter()
                        .filter(|container| container.state == ContainerState::Running)
                        .count() as u64,
                }),
            }),
        }),
        containers: container_stats,
    }
}

pub(super) fn metric_descriptors() -> Vec<MetricDescriptor> {
    vec![
        MetricDescriptor {
            name: "a3s_box_pod_sandbox_ready".to_string(),
            help: "Whether the CRI pod sandbox is Ready according to the runtime store."
                .to_string(),
            kind: "gauge".to_string(),
            unit: "1".to_string(),
        },
        MetricDescriptor {
            name: "a3s_box_pod_sandbox_vm_manager_present".to_string(),
            help: "Whether the runtime has an in-process VM manager for the pod sandbox."
                .to_string(),
            kind: "gauge".to_string(),
            unit: "1".to_string(),
        },
        MetricDescriptor {
            name: "a3s_box_pod_sandbox_containers_total".to_string(),
            help: "Number of containers tracked for the pod sandbox.".to_string(),
            kind: "gauge".to_string(),
            unit: "count".to_string(),
        },
        MetricDescriptor {
            name: "a3s_box_pod_sandbox_containers_running".to_string(),
            help: "Number of running containers tracked for the pod sandbox.".to_string(),
            kind: "gauge".to_string(),
            unit: "count".to_string(),
        },
        MetricDescriptor {
            name: "a3s_box_pod_sandbox_containers_exited".to_string(),
            help: "Number of exited containers tracked for the pod sandbox.".to_string(),
            kind: "gauge".to_string(),
            unit: "count".to_string(),
        },
    ]
}

fn pod_sandbox_metric_labels(sandbox: &PodSandbox) -> HashMap<String, String> {
    HashMap::from([
        ("pod_sandbox_id".to_string(), sandbox.id.clone()),
        ("namespace".to_string(), sandbox.namespace.clone()),
        ("name".to_string(), sandbox.name.clone()),
        ("uid".to_string(), sandbox.uid.clone()),
        (
            "runtime_handler".to_string(),
            sandbox.runtime_handler.clone(),
        ),
    ])
}

fn pod_metric(name: &str, labels: &HashMap<String, String>, value: f64, timestamp: i64) -> Metric {
    Metric {
        name: name.to_string(),
        labels: labels.clone(),
        value,
        timestamp,
    }
}

pub(super) fn pod_sandbox_metrics(
    sandbox: &PodSandbox,
    containers: &[Container],
    vm_manager_present: bool,
) -> PodSandboxMetrics {
    let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
    let labels = pod_sandbox_metric_labels(sandbox);
    let running_containers = containers
        .iter()
        .filter(|container| container.state == ContainerState::Running)
        .count();
    let exited_containers = containers
        .iter()
        .filter(|container| container.state == ContainerState::Exited)
        .count();

    PodSandboxMetrics {
        pod_sandbox_id: sandbox.id.clone(),
        metrics: vec![
            pod_metric(
                "a3s_box_pod_sandbox_ready",
                &labels,
                if sandbox.state == SandboxState::Ready {
                    1.0
                } else {
                    0.0
                },
                now_ns,
            ),
            pod_metric(
                "a3s_box_pod_sandbox_vm_manager_present",
                &labels,
                if vm_manager_present { 1.0 } else { 0.0 },
                now_ns,
            ),
            pod_metric(
                "a3s_box_pod_sandbox_containers_total",
                &labels,
                containers.len() as f64,
                now_ns,
            ),
            pod_metric(
                "a3s_box_pod_sandbox_containers_running",
                &labels,
                running_containers as f64,
                now_ns,
            ),
            pod_metric(
                "a3s_box_pod_sandbox_containers_exited",
                &labels,
                exited_containers as f64,
                now_ns,
            ),
        ],
    }
}
