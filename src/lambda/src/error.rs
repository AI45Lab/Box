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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_messages_include_context() {
        let cases = [
            (
                SdkError::ExecutionFailed("exit 7".to_string()),
                "VM execution failed: exit 7",
            ),
            (
                SdkError::VmNotAvailable("pool empty".to_string()),
                "VM not available: pool empty",
            ),
            (
                SdkError::Timeout("30s".to_string()),
                "timeout exceeded: 30s",
            ),
            (
                SdkError::InvalidEnvelope("missing runtime".to_string()),
                "invalid workload envelope: missing runtime",
            ),
            (
                SdkError::RuntimeError("bad handler".to_string()),
                "runtime error: bad handler",
            ),
            (
                SdkError::AgentNotFound("agent-x".to_string()),
                "agent not found: agent-x",
            ),
            (
                SdkError::DownloadFailed("404".to_string()),
                "download failed: 404",
            ),
            (
                SdkError::InitFailed("config".to_string()),
                "initialization failed: config",
            ),
            (
                SdkError::CapabilityDenied("net.raw".to_string()),
                "capability validation failed: net.raw",
            ),
        ];

        for (error, expected) in cases {
            assert_eq!(error.to_string(), expected);
        }
    }

    #[test]
    fn result_alias_uses_sdk_error() {
        fn fail() -> Result<()> {
            Err(SdkError::Timeout("deadline".to_string()))
        }

        match fail() {
            Err(SdkError::Timeout(message)) => assert_eq!(message, "deadline"),
            other => panic!("unexpected result: {other:?}"),
        }
    }
}
