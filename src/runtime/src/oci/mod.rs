//! OCI image support for A3S Box.
//!
//! This module provides functionality to parse and extract OCI images
//! for use as VM rootfs. It supports:
//!
//! - OCI image layout parsing (manifest, config)
//! - Layer extraction (tar.gz)
//! - Rootfs composition from multiple images
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                    OCI Image Layout                          │
//! │                                                              │
//! │  image/                                                      │
//! │  ├── oci-layout           (OCI layout marker)               │
//! │  ├── index.json           (Image index)                     │
//! │  └── blobs/                                                 │
//! │      └── sha256/                                            │
//! │          ├── <manifest>   (Image manifest)                  │
//! │          ├── <config>     (Image configuration)             │
//! │          └── <layers>     (Filesystem layers)               │
//! └─────────────────────────────────────────────────────────────┘
//! ```

#[cfg(feature = "build")]
pub mod build;
pub mod credentials;
mod image;
mod layers;
mod pull;
pub mod reference;
pub mod registry;
mod rootfs;
pub mod signing;
pub mod store;

#[cfg(feature = "build")]
pub use build::{BuildConfig, BuildResult, Dockerfile, Instruction};
pub use credentials::CredentialStore;
pub use image::{OciHealthCheck, OciImage, OciImageConfig};
pub use layers::extract_layer;
pub use pull::ImagePuller;
pub use reference::ImageReference;
pub use registry::{PushResult, RegistryAuth, RegistryPuller, RegistryPusher};
pub use rootfs::OciRootfsBuilder;
pub use signing::{SignaturePolicy, VerifyResult};
pub use store::ImageStore;
