//! SDK error types

use thiserror::Error;

/// SDK result type
pub type Result<T> = std::result::Result<T, SdkError>;

/// SDK error variants
#[derive(Error, Debug)]
pub enum SdkError {
    #[error("VM execution failed: {0}")]
    ExecutionFailed(String),

    #[error("VM not available: {0}")]
    VmNotAvailable(String),

    #[error("timeout exceeded: {0}")]
    Timeout(String),

    #[error("invalid workload envelope: {0}")]
    InvalidEnvelope(String),

    #[error("runtime error: {0}")]
    RuntimeError(String),

    #[error("agent not found: {0}")]
    AgentNotFound(String),

    #[error("download failed: {0}")]
    DownloadFailed(String),

    #[error("initialization failed: {0}")]
    InitFailed(String),

    #[error("capability validation failed: {0}")]
    CapabilityDenied(String),
}
