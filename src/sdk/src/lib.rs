//! a3s-box SDK — drive a3s-box from Rust.
//!
//! Currently provides [`pipeline`]: programmable CI/CD where a pipeline is a Rust
//! program and a3s-box is the execution backend (one MicroVM kernel per step).
//! More capabilities will be added over time — this crate is intentionally not
//! limited to CI.

pub mod pipeline;
