//! Guest rootfs management module.
//!
//! This module handles preparation and management of guest rootfs for MicroVM instances.
//! The rootfs contains the minimal filesystem required to boot the guest agent.
//!
//! Two rootfs providers are available:
//! - `CopyProvider` — full recursive copy (works everywhere)
//! - `OverlayProvider` — Linux overlayfs mount (near-instant CoW)

mod builder;
mod layout;
pub(crate) mod overlay;
mod provider;

pub use builder::RootfsBuilder;
pub use layout::{GuestLayout, GUEST_WORKDIR};
pub use provider::{default_provider, CopyProvider, OverlayProvider, RootfsProvider};

use std::path::Path;

/// Unmount a box's overlayfs `merged` view — best-effort and idempotent.
///
/// Box teardown must release this mount BEFORE removing the box dir, or
/// `remove_dir_all` deletes *into* the live mount and fails with "Stale file
/// handle", leaking the mount. A restart re-mounts without unmounting first, so
/// the overlay can be stacked (mounted 2–3×); unmount in a bounded loop until
/// `merged` is no longer a mountpoint. No-op if it was never mounted.
pub fn unmount_box_overlay(merged: &Path) {
    for _ in 0..8 {
        if !is_mountpoint(merged) {
            break;
        }
        if overlay::overlay_unmount(merged).is_err() {
            break;
        }
    }
}

/// True if `path` is a mountpoint (its device id differs from its parent's).
#[cfg(target_os = "linux")]
pub(crate) fn is_mountpoint(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(path), std::fs::metadata(path.join(".."))) {
        (Ok(here), Ok(parent)) => here.dev() != parent.dev(),
        _ => false,
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn is_mountpoint(_path: &Path) -> bool {
    false
}
