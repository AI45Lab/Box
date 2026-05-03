//! CLI helpers for host-platform capability checks.

#[cfg(windows)]
use a3s_box_core::PlatformCapabilities;

#[cfg(windows)]
pub(crate) fn unsupported_command(command: &str, required: &str) -> Box<dyn std::error::Error> {
    let capabilities = PlatformCapabilities::current();
    format!(
        "'{command}' is not supported on {}/{}: requires {required} (host channel: {:?}, VM backend: {:?})",
        capabilities.os,
        capabilities.architecture,
        capabilities.host_guest_channel,
        capabilities.vm_backend
    )
    .into()
}
