//! Host-guest communication clients over Unix socket.
//!
//! - `ExecClient`: Executing commands in the guest (port 4089).
//! - `PtyClient`: Interactive terminal access (port 4090).
//! - `AttestationClient`: TEE attestation and secret injection (port 4091).

#[cfg(unix)]
mod attestation;
#[cfg(unix)]
mod exec;
#[cfg(unix)]
mod pty;

#[cfg(unix)]
pub use attestation::{
    AttestationClient, RaTlsAttestationClient, SealClient, SealResult, SecretEntry,
    SecretInjectionResult, SecretInjector, UnsealResult,
};
#[cfg(unix)]
pub use exec::{ExecClient, StreamingExec, StreamingExecInput};
#[cfg(unix)]
pub use pty::{PtyClient, StreamingPty, StreamingPtyInput};
