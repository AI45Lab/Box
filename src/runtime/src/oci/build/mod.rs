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
//! FROM, RUN, COPY, WORKDIR, ENV, ENTRYPOINT, CMD, EXPOSE, LABEL, USER, ARG

pub mod dockerfile;
pub mod engine;
pub mod layer;

pub use dockerfile::{Dockerfile, Instruction};
pub use engine::{build, BuildConfig, BuildResult};
pub use layer::{DirSnapshot, LayerInfo};
