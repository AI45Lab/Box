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

/// Real CPU + memory usage of a pod's microVM, read from the host-side shim
/// process. In the microVM-per-pod model the shim process *is* the pod: its
/// `/proc/<pid>/stat` CPU time (vcpu threads are aggregated into the process)
/// and `/proc/<pid>/status` `VmRSS` (which backs the guest RAM) are the pod's
/// real resource usage.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct VmUsage {
    /// Cumulative CPU time in nanoseconds (utime + stime).
    cpu_core_nanos: u64,
    /// Resident set size in bytes.
    memory_bytes: u64,
}

impl VmUsage {
    /// Split the pod-VM usage evenly across its running containers so the sum of
    /// per-container stats equals the pod total (single-container pods — the
    /// common case — get the full usage; the per-container split is an
    /// approximation for multi-container pods sharing one VM).
    pub(super) fn per_container(self, running: usize) -> VmUsage {
        let n = running.max(1) as u64;
        VmUsage {
            cpu_core_nanos: self.cpu_core_nanos / n,
            memory_bytes: self.memory_bytes / n,
        }
    }
}

/// Read a process's cumulative CPU time + RSS from procfs.
#[cfg(target_os = "linux")]
pub(super) fn read_vm_usage(pid: u32) -> VmUsage {
    // Near-universal on Linux; avoids pulling libc into the CRI crate just for
    // sysconf(_SC_CLK_TCK).
    const CLK_TCK: u64 = 100;
    let mut usage = VmUsage::default();

    if let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                if let Some(kb) = rest
                    .split_whitespace()
                    .next()
                    .and_then(|value| value.parse::<u64>().ok())
                {
                    usage.memory_bytes = kb.saturating_mul(1024);
                }
                break;
            }
        }
    }

    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        // The comm field (2) may contain spaces/parens, so index fields after
        // the final ')': utime is field 14, stime field 15 (1-based), i.e.
        // offsets 11 and 12 in the post-')' token list (which starts at field 3).
        if let Some(rparen) = stat.rfind(')') {
            let after: Vec<&str> = stat[rparen + 1..].split_whitespace().collect();
            let utime = after
                .get(11)
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            let stime = after
                .get(12)
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            usage.cpu_core_nanos = utime
                .saturating_add(stime)
                .saturating_mul(1_000_000_000 / CLK_TCK);
        }
    }

    usage
}

#[cfg(not(target_os = "linux"))]
pub(super) fn read_vm_usage(_pid: u32) -> VmUsage {
    VmUsage::default()
}

fn cpu_usage(now_ns: i64, usage: VmUsage) -> CpuUsage {
    CpuUsage {
        timestamp: now_ns,
        // Cumulative CPU time; the kubelet derives the rate from deltas between
        // samples. usage_nano_cores (instantaneous) needs two samples, so it is
        // left at 0 and computed by the consumer.
        usage_core_nano_seconds: Some(UInt64Value {
            value: usage.cpu_core_nanos,
        }),
        usage_nano_cores: Some(UInt64Value { value: 0 }),
    }
}

fn memory_usage(now_ns: i64, usage: VmUsage) -> MemoryUsage {
    MemoryUsage {
        timestamp: now_ns,
        working_set_bytes: Some(UInt64Value {
            value: usage.memory_bytes,
        }),
        available_bytes: Some(UInt64Value { value: 0 }),
        usage_bytes: Some(UInt64Value {
            value: usage.memory_bytes,
        }),
        rss_bytes: Some(UInt64Value {
            value: usage.memory_bytes,
        }),
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

pub(super) async fn container_stats(container: &Container, usage: VmUsage) -> ContainerStats {
    let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
    let rootfs_usage = rootfs_path_usage(&container.rootfs_path).await;
    ContainerStats {
        attributes: Some(ContainerAttributes {
            id: container.id.clone(),
            metadata: Some(ContainerMetadata {
                name: container.name.clone(),
                attempt: container.attempt,
            }),
            labels: container.labels.clone(),
            annotations: container.annotations.clone(),
        }),
        cpu: Some(cpu_usage(now_ns, usage)),
        memory: Some(memory_usage(now_ns, usage)),
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
    vm_usage: VmUsage,
) -> PodSandboxStats {
    let now_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
    let running_containers: Vec<Container> = containers
        .iter()
        .filter(|container| container.state == ContainerState::Running)
        .cloned()
        .collect();
    // Pod-level stats report the VM total; per-container stats are split evenly
    // so their sum equals the pod total.
    let per_container = vm_usage.per_container(running_containers.len());
    let container_stats = join_all(
        running_containers
            .iter()
            .map(|c| container_stats(c, per_container)),
    )
    .await;

    PodSandboxStats {
        attributes: Some(PodSandboxAttributes {
            id: sandbox.id.clone(),
            metadata: Some(PodSandboxMetadata {
                name: sandbox.name.clone(),
                uid: sandbox.uid.clone(),
                namespace: sandbox.namespace.clone(),
                attempt: sandbox.attempt,
            }),
            labels: sandbox.labels.clone(),
            annotations: sandbox.annotations.clone(),
        }),
        linux: Some(LinuxPodSandboxStats {
            cpu: Some(cpu_usage(now_ns, vm_usage)),
            memory: Some(memory_usage(now_ns, vm_usage)),
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
            // Per-container stats belong inside LinuxPodSandboxStats (field 5),
            // matching the official CRI v1 layout (the box proto previously put
            // them on PodSandboxStats=3, colliding with `windows` and breaking
            // crictl/kubelet decode of pod sandbox stats).
            containers: container_stats,
        }),
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

#[cfg(test)]
mod stats_tests {
    use super::*;

    #[test]
    fn test_per_container_split_sums_to_total() {
        let total = VmUsage {
            cpu_core_nanos: 900,
            memory_bytes: 300,
        };
        let per = total.per_container(3);
        assert_eq!(per.cpu_core_nanos, 300);
        assert_eq!(per.memory_bytes, 100);
        // 0 or 1 container → the full usage (no divide-by-zero, single-container
        // pods get the whole VM's usage).
        assert_eq!(total.per_container(0).memory_bytes, 300);
        assert_eq!(total.per_container(1).cpu_core_nanos, 900);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_read_vm_usage_reports_real_memory_for_self() {
        // Reading the test process's own procfs must yield a non-zero RSS (CPU
        // may legitimately round to 0 immediately after start, so only memory
        // is asserted).
        let usage = read_vm_usage(std::process::id());
        assert!(
            usage.memory_bytes > 0,
            "expected a non-zero RSS reading the test process's own /proc"
        );
    }
}
