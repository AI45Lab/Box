//! Sandbox configuration options.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Options for creating a new sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxOptions {
    /// OCI image reference (e.g., "alpine:latest", "python:3.12-slim").
    pub image: String,

    /// Number of vCPUs (default: 1).
    #[serde(default = "default_cpus")]
    pub cpus: u32,

    /// Memory in megabytes (default: 256).
    #[serde(default = "default_memory_mb")]
    pub memory_mb: u32,

    /// Environment variables to set in the guest.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// Working directory inside the guest.
    #[serde(default)]
    pub workdir: Option<String>,

    /// Host directories to mount into the guest as `host_path:guest_path`.
    #[serde(default)]
    pub mounts: Vec<MountSpec>,

    /// Enable outbound networking (default: true).
    #[serde(default = "default_true")]
    pub network: bool,

    /// Enable TEE (AMD SEV-SNP) if hardware supports it.
    #[serde(default)]
    pub tee: bool,

    /// Custom sandbox name (auto-generated if not set).
    #[serde(default)]
    pub name: Option<String>,

    /// Port forwarding rules (guest port → host port).
    #[serde(default)]
    pub port_forwards: Vec<PortForward>,

    /// Persistent workspace configuration.
    /// When set, the workspace directory survives sandbox restarts.
    #[serde(default)]
    pub workspace: Option<WorkspaceConfig>,
}

/// A host-to-guest mount specification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountSpec {
    /// Path on the host.
    pub host_path: String,
    /// Path inside the guest.
    pub guest_path: String,
    /// Read-only mount (default: false).
    #[serde(default)]
    pub readonly: bool,
}

/// Port forwarding rule: expose a guest port on the host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortForward {
    /// Port inside the guest.
    pub guest_port: u16,
    /// Port on the host (0 = auto-assign).
    #[serde(default)]
    pub host_port: u16,
    /// Protocol (default: "tcp").
    #[serde(default = "default_protocol")]
    pub protocol: String,
}

/// Persistent workspace configuration.
///
/// Named workspaces retain their contents across sandbox restarts,
/// eliminating rebuild overhead for agent workflows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    /// Workspace name (used as directory name under ~/.a3s/workspaces/).
    pub name: String,
    /// Guest path to mount the workspace (default: "/workspace").
    #[serde(default = "default_workspace_path")]
    pub guest_path: String,
}

impl Default for SandboxOptions {
    fn default() -> Self {
        Self {
            image: "alpine:latest".into(),
            cpus: default_cpus(),
            memory_mb: default_memory_mb(),
            env: HashMap::new(),
            workdir: None,
            mounts: Vec::new(),
            network: true,
            tee: false,
            name: None,
            port_forwards: Vec::new(),
            workspace: None,
        }
    }
}

fn default_cpus() -> u32 {
    1
}

fn default_memory_mb() -> u32 {
    256
}

fn default_true() -> bool {
    true
}

fn default_protocol() -> String {
    "tcp".to_string()
}

fn default_workspace_path() -> String {
    "/workspace".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_options() {
        let opts = SandboxOptions::default();
        assert_eq!(opts.image, "alpine:latest");
        assert_eq!(opts.cpus, 1);
        assert_eq!(opts.memory_mb, 256);
        assert!(opts.network);
        assert!(!opts.tee);
        assert!(opts.env.is_empty());
        assert!(opts.mounts.is_empty());
        assert!(opts.workdir.is_none());
        assert!(opts.name.is_none());
        assert!(opts.port_forwards.is_empty());
        assert!(opts.workspace.is_none());
    }

    #[test]
    fn test_options_serde_roundtrip() {
        let opts = SandboxOptions {
            image: "python:3.12-slim".into(),
            cpus: 4,
            memory_mb: 1024,
            env: [("KEY".into(), "val".into())].into(),
            workdir: Some("/app".into()),
            mounts: vec![MountSpec {
                host_path: "/tmp/data".into(),
                guest_path: "/data".into(),
                readonly: true,
            }],
            network: false,
            tee: true,
            name: Some("my-sandbox".into()),
            port_forwards: vec![PortForward {
                guest_port: 8080,
                host_port: 3000,
                protocol: "tcp".into(),
            }],
            workspace: Some(WorkspaceConfig {
                name: "my-project".into(),
                guest_path: "/workspace".into(),
            }),
        };
        let json = serde_json::to_string(&opts).unwrap();
        let parsed: SandboxOptions = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.image, "python:3.12-slim");
        assert_eq!(parsed.cpus, 4);
        assert_eq!(parsed.memory_mb, 1024);
        assert_eq!(parsed.env["KEY"], "val");
        assert_eq!(parsed.workdir.as_deref(), Some("/app"));
        assert_eq!(parsed.mounts.len(), 1);
        assert!(parsed.mounts[0].readonly);
        assert!(!parsed.network);
        assert!(parsed.tee);
        assert_eq!(parsed.name.as_deref(), Some("my-sandbox"));
        assert_eq!(parsed.port_forwards.len(), 1);
        assert_eq!(parsed.port_forwards[0].guest_port, 8080);
        assert_eq!(parsed.port_forwards[0].host_port, 3000);
        assert!(parsed.workspace.is_some());
        assert_eq!(parsed.workspace.as_ref().unwrap().name, "my-project");
    }

    #[test]
    fn test_options_from_minimal_json() {
        let json = r#"{"image":"ubuntu:22.04"}"#;
        let opts: SandboxOptions = serde_json::from_str(json).unwrap();
        assert_eq!(opts.image, "ubuntu:22.04");
        assert_eq!(opts.cpus, 1);
        assert_eq!(opts.memory_mb, 256);
        assert!(opts.network);
        assert!(opts.port_forwards.is_empty());
        assert!(opts.workspace.is_none());
    }

    #[test]
    fn test_mount_spec() {
        let mount = MountSpec {
            host_path: "/home/user/code".into(),
            guest_path: "/workspace".into(),
            readonly: false,
        };
        let json = serde_json::to_string(&mount).unwrap();
        assert!(json.contains("/workspace"));
        let parsed: MountSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.guest_path, "/workspace");
        assert!(!parsed.readonly);
    }

    #[test]
    fn test_port_forward_defaults() {
        let json = r#"{"guest_port":8080}"#;
        let pf: PortForward = serde_json::from_str(json).unwrap();
        assert_eq!(pf.guest_port, 8080);
        assert_eq!(pf.host_port, 0);
        assert_eq!(pf.protocol, "tcp");
    }

    #[test]
    fn test_port_forward_serde_roundtrip() {
        let pf = PortForward {
            guest_port: 3000,
            host_port: 9000,
            protocol: "tcp".into(),
        };
        let json = serde_json::to_string(&pf).unwrap();
        let parsed: PortForward = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.guest_port, 3000);
        assert_eq!(parsed.host_port, 9000);
    }

    #[test]
    fn test_workspace_config_defaults() {
        let json = r#"{"name":"my-ws"}"#;
        let ws: WorkspaceConfig = serde_json::from_str(json).unwrap();
        assert_eq!(ws.name, "my-ws");
        assert_eq!(ws.guest_path, "/workspace");
    }

    #[test]
    fn test_workspace_config_custom_path() {
        let ws = WorkspaceConfig {
            name: "project-x".into(),
            guest_path: "/home/user/project".into(),
        };
        let json = serde_json::to_string(&ws).unwrap();
        let parsed: WorkspaceConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "project-x");
        assert_eq!(parsed.guest_path, "/home/user/project");
    }
}
