//! OCI image build support.
//!
//! Provides Dockerfile/Containerfile parsing, layer creation, and a build engine
//! that produces OCI images from Dockerfile-compatible build files.
//!
//! # Usage
//!
//! ```text
//! a3s-box build -t myimage:latest .
//! ```
//!
//! # Supported Instructions
//!
//! FROM, shell-form RUN, shell-form COPY/ADD, WORKDIR, ENV, ENTRYPOINT, CMD,
//! EXPOSE, LABEL, USER, ARG, SHELL, STOPSIGNAL, HEALTHCHECK, ONBUILD metadata
//! triggers, VOLUME.
//!
//! Unsupported Dockerfile flags and instructions fail with contextual errors
//! instead of being silently ignored.

pub(crate) mod cache;
pub mod dockerfile;
pub(crate) mod dockerignore;
pub mod engine;
pub mod layer;

pub use dockerfile::{Dockerfile, Instruction};
pub use engine::{build, BuildConfig, BuildResult};
pub use layer::{DirSnapshot, LayerInfo};
