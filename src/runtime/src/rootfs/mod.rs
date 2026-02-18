//! Guest rootfs management module.
//!
//! This module handles preparation and management of guest rootfs for MicroVM instances.
//! The rootfs contains the minimal filesystem required to boot the guest agent.

mod builder;
mod layout;

pub use builder::RootfsBuilder;
pub use layout::{GuestLayout, GUEST_WORKDIR};
