//! Linux KVM backend implementation.
//!
//! This module provides the Linux-specific implementation of the platform
//! abstraction traits using KVM via libkrun.

use async_trait::async_trait;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::vmm::InstanceSpec;

use super::traits::{
    BackendCapabilities, ExecBackend, ExecResult, FsBackend, FsCapabilities, NetworkBackend,
    Protocol, PtySession, TerminalSize, VmMetrics, VmmBackend,
};

pub struct KvmBackend {}

impl KvmBackend {
    pub fn new() -> Self {
        Self {}
    }
}

#[async_trait]
impl VmmBackend for KvmBackend {
    async fn boot(&self, _spec: &InstanceSpec) -> Result<u32> {
        Err(BoxError::NotImplemented {
            feature: "KVM boot".to_string(),
        })
    }

    async fn shutdown(&self, _pid: u32, _signal: i32, _timeout_ms: u64) -> Result<()> {
        Err(BoxError::NotImplemented {
            feature: "KVM shutdown".to_string(),
        })
    }

    async fn pause(&self, _pid: u32) -> Result<()> {
        Err(BoxError::NotImplemented {
            feature: "KVM pause".to_string(),
        })
    }

    async fn resume(&self, _pid: u32) -> Result<()> {
        Err(BoxError::NotImplemented {
            feature: "KVM resume".to_string(),
        })
    }

    async fn configure_cpu(&self, _pid: u32, _vcpus: u8) -> Result<()> {
        Err(BoxError::NotSupported {
            feature: "Dynamic CPU configuration".to_string(),
            platform: "Linux KVM".to_string(),
        })
    }

    async fn configure_memory(&self, _pid: u32, _memory_mib: u32) -> Result<()> {
        Err(BoxError::NotSupported {
            feature: "Dynamic memory configuration".to_string(),
            platform: "Linux KVM".to_string(),
        })
    }

    async fn get_metrics(&self, _pid: u32) -> Result<VmMetrics> {
        Ok(VmMetrics::default())
    }

    fn is_running(&self, _pid: u32) -> bool {
        false
    }

    fn exit_code(&self, _pid: u32) -> Option<i32> {
        None
    }

    fn name(&self) -> &'static str {
        "kvm"
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            dynamic_cpu: false,
            dynamic_memory: false,
            pause_resume: true,
            vsock: true,
            virtiofs: true,
            port_forwarding: true,
            tee: true,
        }
    }
}

pub struct KvmNetworkBackend {}

impl KvmNetworkBackend {
    pub fn new() -> Self {
        Self {}
    }
}

#[async_trait]
impl NetworkBackend for KvmNetworkBackend {
    async fn create_bridge(&self, _name: &str, _subnet: &str) -> Result<String> {
        Err(BoxError::NotImplemented {
            feature: "bridge creation".to_string(),
        })
    }

    async fn delete_bridge(&self, _bridge_id: &str) -> Result<()> {
        Err(BoxError::NotImplemented {
            feature: "bridge deletion".to_string(),
        })
    }

    async fn attach_interface(
        &self,
        _bridge_id: &str,
        _vm_id: &str,
        _mac_address: [u8; 6],
    ) -> Result<Ipv4Addr> {
        Err(BoxError::NotImplemented {
            feature: "interface attachment".to_string(),
        })
    }

    async fn detach_interface(&self, _bridge_id: &str, _vm_id: &str) -> Result<()> {
        Err(BoxError::NotImplemented {
            feature: "interface detachment".to_string(),
        })
    }

    async fn configure_nat(&self, _bridge_id: &str, _enable: bool) -> Result<()> {
        Err(BoxError::NotImplemented {
            feature: "NAT configuration".to_string(),
        })
    }

    async fn configure_port_forward(
        &self,
        _vm_id: &str,
        _host_port: u16,
        _guest_port: u16,
        _protocol: Protocol,
    ) -> Result<()> {
        Err(BoxError::NotImplemented {
            feature: "port forwarding".to_string(),
        })
    }

    async fn remove_port_forward(&self, _vm_id: &str, _host_port: u16) -> Result<()> {
        Err(BoxError::NotImplemented {
            feature: "port forwarding removal".to_string(),
        })
    }

    async fn start_dns_server(&self, _bridge_id: &str) -> Result<Ipv4Addr> {
        Err(BoxError::NotImplemented {
            feature: "DNS server".to_string(),
        })
    }

    async fn stop_dns_server(&self, _bridge_id: &str) -> Result<()> {
        Err(BoxError::NotImplemented {
            feature: "DNS server stop".to_string(),
        })
    }

    async fn register_dns_name(
        &self,
        _bridge_id: &str,
        _name: &str,
        _ip: Ipv4Addr,
    ) -> Result<()> {
        Err(BoxError::NotImplemented {
            feature: "DNS name registration".to_string(),
        })
    }

    async fn unregister_dns_name(&self, _bridge_id: &str, _name: &str) -> Result<()> {
        Err(BoxError::NotImplemented {
            feature: "DNS name unregistration".to_string(),
        })
    }

    fn name(&self) -> &'static str {
        "tap"
    }
}

pub struct KvmFsBackend {}

impl KvmFsBackend {
    pub fn new() -> Self {
        Self {}
    }
}

#[async_trait]
impl FsBackend for KvmFsBackend {
    async fn mount_host_path(
        &self,
        _vm_id: &str,
        _host_path: &Path,
        _guest_path: &Path,
        _read_only: bool,
    ) -> Result<String> {
        Err(BoxError::NotImplemented {
            feature: "virtiofs mount".to_string(),
        })
    }

    async fn unmount(&self, _vm_id: &str, _mount_tag: &str) -> Result<()> {
        Err(BoxError::NotImplemented {
            feature: "virtiofs unmount".to_string(),
        })
    }

    async fn create_volume(&self, _name: &str, _size_bytes: Option<u64>) -> Result<PathBuf> {
        Err(BoxError::NotImplemented {
            feature: "volume creation".to_string(),
        })
    }

    async fn delete_volume(&self, _name: &str) -> Result<()> {
        Err(BoxError::NotImplemented {
            feature: "volume deletion".to_string(),
        })
    }

    async fn get_volume_path(&self, _name: &str) -> Result<PathBuf> {
        Err(BoxError::NotImplemented {
            feature: "volume path lookup".to_string(),
        })
    }

    async fn snapshot_volume(&self, _name: &str, _snapshot_name: &str) -> Result<String> {
        Err(BoxError::NotImplemented {
            feature: "volume snapshot".to_string(),
        })
    }

    async fn restore_volume(&self, _name: &str, _snapshot_id: &str) -> Result<()> {
        Err(BoxError::NotImplemented {
            feature: "volume restore".to_string(),
        })
    }

    fn name(&self) -> &'static str {
        "virtiofs"
    }

    fn capabilities(&self) -> FsCapabilities {
        FsCapabilities {
            virtiofs: true,
            ninep: false,
            smb: false,
            snapshots: false,
        }
    }
}

pub struct KvmExecBackend {}

impl KvmExecBackend {
    pub fn new() -> Self {
        Self {}
    }
}

#[async_trait]
impl ExecBackend for KvmExecBackend {
    async fn exec(
        &self,
        _vm_id: &str,
        _command: &[String],
        _env: &[(String, String)],
        _workdir: &str,
        _user: Option<&str>,
    ) -> Result<ExecResult> {
        Err(BoxError::NotImplemented {
            feature: "vsock exec".to_string(),
        })
    }

    async fn exec_pty(
        &self,
        _vm_id: &str,
        _command: &[String],
        _env: &[(String, String)],
        _workdir: &str,
        _user: Option<&str>,
        _term_size: TerminalSize,
    ) -> Result<Box<dyn PtySession>> {
        Err(BoxError::NotImplemented {
            feature: "vsock PTY exec".to_string(),
        })
    }

    async fn attach(&self, _vm_id: &str) -> Result<Box<dyn PtySession>> {
        Err(BoxError::NotImplemented {
            feature: "PTY attach".to_string(),
        })
    }

    async fn send_signal(&self, _vm_id: &str, _pid: u32, _signal: i32) -> Result<()> {
        Err(BoxError::NotImplemented {
            feature: "signal sending".to_string(),
        })
    }

    fn name(&self) -> &'static str {
        "vsock"
    }
}
