//! Credential provider abstraction.
//!
//! Decouples registry authentication from the file-based credential
//! store in `a3s-box-runtime`. Implementations can source credentials
//! from any backend: files, Vault, cloud IAM, OS keychain, etc.

use crate::error::Result;

/// Abstraction over container registry credential lookup.
///
/// The runtime calls `get` when authenticating with a container registry
/// during image pull/push operations.
///
/// # Thread Safety
///
/// Implementations must be `Send + Sync`.
pub trait CredentialProvider: Send + Sync {
    /// Get credentials for a registry.
    ///
    /// Returns `Some((username, password))` if credentials are available,
    /// `None` if the registry is not configured (anonymous access).
    fn get(&self, registry: &str) -> Result<Option<(String, String)>>;

    /// Store credentials for a registry.
    ///
    /// Not all backends support writes (e.g., environment-variable-based
    /// providers are read-only). Default implementation returns an error.
    fn store(&self, _registry: &str, _username: &str, _password: &str) -> Result<()> {
        Err(crate::error::BoxError::Other(
            "This credential provider does not support storing credentials".to_string(),
        ))
    }

    /// Remove credentials for a registry.
    ///
    /// Default implementation returns an error.
    fn remove(&self, _registry: &str) -> Result<bool> {
        Err(crate::error::BoxError::Other(
            "This credential provider does not support removing credentials".to_string(),
        ))
    }
}
