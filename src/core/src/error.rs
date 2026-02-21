use thiserror::Error;

/// A3S Box error types
#[derive(Error, Debug)]
pub enum BoxError {
    /// VM failed to start
    #[error("VM boot failed: {message}")]
    BoxBootError {
        message: String,
        hint: Option<String>,
    },

    /// Timeout error
    #[error("Timeout: {0}")]
    TimeoutError(String),

    /// I/O error
    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),

    /// Serialization error
    #[error("Serialization error: {0}")]
    SerializationError(String),

    /// Configuration error
    #[error("Configuration error: {0}")]
    ConfigError(String),

    /// TEE configuration error
    #[error("TEE configuration error: {0}")]
    TeeConfig(String),

    /// TEE hardware not available
    #[error("TEE hardware not available: {0}")]
    TeeNotSupported(String),

    /// Attestation error
    #[error("Attestation error: {0}")]
    AttestationError(String),

    /// OCI image error
    #[error("OCI image error: {0}")]
    OciImageError(String),

    /// Container registry error
    #[error("Registry error: {registry} - {message}")]
    RegistryError { registry: String, message: String },

    /// Cache error
    #[error("Cache error: {0}")]
    CacheError(String),

    /// Pool error
    #[error("Pool error: {0}")]
    PoolError(String),

    /// Exec error
    #[error("Exec error: {0}")]
    ExecError(String),

    /// Build error
    #[error("Build error: {0}")]
    BuildError(String),

    /// Network error
    #[error("Network error: {0}")]
    NetworkError(String),

    /// VM state machine error (invalid state transition or precondition)
    #[error("VM state error: {0}")]
    StateError(String),

    /// Audit log error
    #[error("Audit error: {0}")]
    AuditError(String),

    /// Resource resize error (hot-resize not supported or failed)
    #[error("Resize error: {0}")]
    ResizeError(String),

    /// Generic error
    #[error("{0}")]
    Other(String),
}

impl From<serde_json::Error> for BoxError {
    fn from(err: serde_json::Error) -> Self {
        BoxError::SerializationError(err.to_string())
    }
}

impl From<serde_yaml::Error> for BoxError {
    fn from(err: serde_yaml::Error) -> Self {
        BoxError::SerializationError(err.to_string())
    }
}

/// Result type alias for A3S Box operations
pub type Result<T> = std::result::Result<T, BoxError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_box_boot_error_display() {
        let error = BoxError::BoxBootError {
            message: "Failed to start VM".to_string(),
            hint: Some("Check virtualization support".to_string()),
        };
        assert_eq!(error.to_string(), "VM boot failed: Failed to start VM");
    }

    #[test]
    fn test_box_boot_error_without_hint() {
        let error = BoxError::BoxBootError {
            message: "No kernel found".to_string(),
            hint: None,
        };
        assert_eq!(error.to_string(), "VM boot failed: No kernel found");
    }

    #[test]
    fn test_timeout_error_display() {
        let error = BoxError::TimeoutError("Operation timed out after 30s".to_string());
        assert_eq!(error.to_string(), "Timeout: Operation timed out after 30s");
    }

    #[test]
    fn test_io_error_conversion() {
        let io_error = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let box_error: BoxError = io_error.into();
        assert!(matches!(box_error, BoxError::IoError(_)));
        assert!(box_error.to_string().contains("file not found"));
    }

    #[test]
    fn test_serialization_error_display() {
        let error = BoxError::SerializationError("Invalid JSON".to_string());
        assert_eq!(error.to_string(), "Serialization error: Invalid JSON");
    }

    #[test]
    fn test_config_error_display() {
        let error = BoxError::ConfigError("Missing required field".to_string());
        assert_eq!(
            error.to_string(),
            "Configuration error: Missing required field"
        );
    }

    #[test]
    fn test_other_error_display() {
        let error = BoxError::Other("Unknown error occurred".to_string());
        assert_eq!(error.to_string(), "Unknown error occurred");
    }

    #[test]
    fn test_tee_config_error_display() {
        let error = BoxError::TeeConfig("Failed to set TEE config file".to_string());
        assert_eq!(
            error.to_string(),
            "TEE configuration error: Failed to set TEE config file"
        );
    }

    #[test]
    fn test_tee_not_supported_error_display() {
        let error = BoxError::TeeNotSupported("AMD SEV-SNP not available".to_string());
        assert_eq!(
            error.to_string(),
            "TEE hardware not available: AMD SEV-SNP not available"
        );
    }

    #[test]
    fn test_attestation_error_display() {
        let error = BoxError::AttestationError("Failed to get SNP report".to_string());
        assert_eq!(
            error.to_string(),
            "Attestation error: Failed to get SNP report"
        );
    }

    #[test]
    fn test_oci_image_error_display() {
        let error = BoxError::OciImageError("Invalid manifest".to_string());
        assert_eq!(error.to_string(), "OCI image error: Invalid manifest");
    }

    #[test]
    fn test_registry_error_display() {
        let error = BoxError::RegistryError {
            registry: "ghcr.io".to_string(),
            message: "Authentication failed".to_string(),
        };
        assert_eq!(
            error.to_string(),
            "Registry error: ghcr.io - Authentication failed"
        );
    }

    #[test]
    fn test_serde_json_error_conversion() {
        let json_str = "{ invalid json }";
        let result: std::result::Result<serde_json::Value, _> = serde_json::from_str(json_str);
        let json_error = result.unwrap_err();
        let box_error: BoxError = json_error.into();
        assert!(matches!(box_error, BoxError::SerializationError(_)));
    }

    #[test]
    fn test_serde_yaml_error_conversion() {
        let yaml_str = "invalid: yaml: content:";
        let result: std::result::Result<serde_yaml::Value, _> = serde_yaml::from_str(yaml_str);
        let yaml_error = result.unwrap_err();
        let box_error: BoxError = yaml_error.into();
        assert!(matches!(box_error, BoxError::SerializationError(_)));
    }

    #[test]
    fn test_result_type_alias() {
        fn returns_ok() -> Result<i32> {
            Ok(42)
        }

        fn returns_err() -> Result<i32> {
            Err(BoxError::Other("test error".to_string()))
        }

        assert_eq!(returns_ok().unwrap(), 42);
        assert!(returns_err().is_err());
    }

    #[test]
    fn test_resize_error_display() {
        let error = BoxError::ResizeError("Cannot change vCPU count".to_string());
        assert_eq!(error.to_string(), "Resize error: Cannot change vCPU count");
    }

    #[test]
    fn test_error_is_debug() {
        let error = BoxError::Other("test".to_string());
        let debug_str = format!("{:?}", error);
        assert!(debug_str.contains("Other"));
    }

    #[test]
    fn test_cache_error_display() {
        let error = BoxError::CacheError("Rootfs cache corrupted".to_string());
        assert_eq!(error.to_string(), "Cache error: Rootfs cache corrupted");
    }

    #[test]
    fn test_pool_error_display() {
        let error = BoxError::PoolError("No idle VMs available".to_string());
        assert_eq!(error.to_string(), "Pool error: No idle VMs available");
    }

    #[test]
    fn test_exec_error_display() {
        let error = BoxError::ExecError("Command not found".to_string());
        assert_eq!(error.to_string(), "Exec error: Command not found");
    }

    #[test]
    fn test_build_error_display() {
        let error = BoxError::BuildError("Dockerfile parse failed".to_string());
        assert_eq!(error.to_string(), "Build error: Dockerfile parse failed");
    }
}
