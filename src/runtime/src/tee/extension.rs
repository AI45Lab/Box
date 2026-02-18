//! TeeExtension - Trait for TEE operations on a running VM.

use std::path::{Path, PathBuf};

use a3s_box_core::error::Result;
use async_trait::async_trait;

use super::attestation::{AttestationReport, AttestationRequest};
use super::policy::AttestationPolicy;
use super::verifier::VerificationResult;
use crate::grpc::{
    RaTlsAttestationClient, SealClient, SealResult, SecretEntry, SecretInjectionResult,
    SecretInjector,
};

/// Extension trait for TEE operations on a running VM.
#[async_trait]
pub trait TeeExtension: Send + Sync {
    async fn request_attestation(&self, request: &AttestationRequest) -> Result<AttestationReport>;
    async fn verify_attestation_ratls(
        &self,
        policy: &AttestationPolicy,
        allow_simulated: bool,
    ) -> Result<VerificationResult>;
    async fn inject_secrets(
        &self,
        secrets: &[SecretEntry],
        allow_simulated: bool,
    ) -> Result<SecretInjectionResult>;
    async fn seal_data(
        &self,
        data: &[u8],
        context: &str,
        policy: &str,
        allow_simulated: bool,
    ) -> Result<SealResult>;
    async fn unseal_data(
        &self,
        blob: &str,
        context: &str,
        policy: &str,
        allow_simulated: bool,
    ) -> Result<Vec<u8>>;
}

/// AMD SEV-SNP TEE extension for VMs with TEE support.
pub struct SnpTeeExtension {
    box_id: String,
    attest_socket_path: PathBuf,
}

impl SnpTeeExtension {
    pub fn new(box_id: String, attest_socket_path: PathBuf) -> Self {
        Self {
            box_id,
            attest_socket_path,
        }
    }

    pub fn attest_socket_path(&self) -> &Path {
        &self.attest_socket_path
    }
}

#[async_trait]
impl TeeExtension for SnpTeeExtension {
    async fn request_attestation(&self, request: &AttestationRequest) -> Result<AttestationReport> {
        let client = crate::grpc::AttestationClient::connect(&self.attest_socket_path).await?;
        let report = client.get_report(request).await?;
        tracing::info!(box_id = %self.box_id, report_size = report.report.len(), "Attestation report received");
        Ok(report)
    }

    async fn verify_attestation_ratls(
        &self,
        policy: &AttestationPolicy,
        allow_simulated: bool,
    ) -> Result<VerificationResult> {
        let client = RaTlsAttestationClient::new(&self.attest_socket_path);
        let result = client.verify(policy.clone(), allow_simulated).await?;
        tracing::info!(box_id = %self.box_id, verified = result.verified, "RA-TLS verification completed");
        Ok(result)
    }

    async fn inject_secrets(
        &self,
        secrets: &[SecretEntry],
        allow_simulated: bool,
    ) -> Result<SecretInjectionResult> {
        let policy = AttestationPolicy::default();
        let injector = SecretInjector::new(&self.attest_socket_path);
        let result = injector.inject(secrets, policy, allow_simulated).await?;
        tracing::info!(box_id = %self.box_id, injected = result.injected, errors = result.errors.len(), "Secrets injected");
        Ok(result)
    }

    async fn seal_data(
        &self,
        data: &[u8],
        context: &str,
        policy: &str,
        allow_simulated: bool,
    ) -> Result<SealResult> {
        let ap = AttestationPolicy::default();
        let client = SealClient::new(&self.attest_socket_path);
        let result = client
            .seal(data, context, policy, ap, allow_simulated)
            .await?;
        tracing::info!(box_id = %self.box_id, context, policy, "Data sealed inside TEE");
        Ok(result)
    }

    async fn unseal_data(
        &self,
        blob: &str,
        context: &str,
        policy: &str,
        allow_simulated: bool,
    ) -> Result<Vec<u8>> {
        let ap = AttestationPolicy::default();
        let client = SealClient::new(&self.attest_socket_path);
        let result = client
            .unseal(blob, context, policy, ap, allow_simulated)
            .await?;
        tracing::info!(box_id = %self.box_id, context, policy, "Data unsealed inside TEE");
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_snp_tee_extension_new() {
        let ext = SnpTeeExtension::new("box-123".to_string(), PathBuf::from("/tmp/attest.sock"));
        assert_eq!(ext.attest_socket_path(), Path::new("/tmp/attest.sock"));
    }
}
