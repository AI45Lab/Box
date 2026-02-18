//! A3S Box SDK — Embedded MicroVM Sandbox
//!
//! Create, execute commands in, and manage MicroVM sandboxes from your code.
//! No daemon required — everything runs in-process.
//!
//! # Quick Start
//!
//! ```rust,no_run
//! use a3s_box_sdk::{BoxSdk, SandboxOptions};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let sdk = BoxSdk::new().await?;
//!
//!     let sandbox = sdk.create(SandboxOptions {
//!         image: "alpine:latest".into(),
//!         ..Default::default()
//!     }).await?;
//!
//!     let result = sandbox.exec("echo", &["hello"]).await?;
//!     println!("{}", result.stdout);
//!
//!     sandbox.stop().await?;
//!     Ok(())
//! }
//! ```

mod options;
mod sandbox;
mod sdk;
pub(crate) mod shim_embed;

pub use options::{MountSpec, PortForward, SandboxOptions, WorkspaceConfig};
pub use sandbox::{ExecResult, Sandbox};
pub use sdk::BoxSdk;

// Re-export streaming types from runtime for convenience
pub use a3s_box_runtime::StreamingExec;

/// SDK version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
