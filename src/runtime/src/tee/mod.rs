//! TEE (Trusted Execution Environment) support.
//!
//! This module provides hardware detection, configuration, attestation,
//! and verification for Trusted Execution Environments (AMD SEV-SNP).
//!
//! - `snp`: Hardware detection for AMD SEV-SNP.
//! - `attestation`: Attestation report types and parsing.
//! - `verifier`: Host-side report verification (signature + policy).
//! - `policy`: Verification policy definitions.
//! - `certs`: AMD KDS certificate fetching and caching.

pub mod attestation;
pub mod certs;
pub mod extension;
pub mod kbs;
pub mod policy;
pub mod ratls;
pub mod reattest;
pub mod rollback;
pub mod sealed;
pub mod simulate;
pub mod snp;
pub mod verifier;

pub use attestation::{
    parse_platform_info, AttestationReport, AttestationRequest, CertificateChain, PlatformInfo,
    TcbVersion,
};
pub use certs::AmdKdsClient;
pub use extension::{SnpTeeExtension, TeeExtension};
pub use kbs::{KbsClient, KbsConfig, KbsRequest, KbsResponse, KbsSecret};
pub use policy::{AttestationPolicy, MinTcbPolicy, PolicyResult, PolicyViolation};
pub use reattest::{FailureAction, ReattestConfig, ReattestState, ReattestSummary};
pub use rollback::{seal_versioned, unseal_versioned, VersionStore, VersionedSealedData};
pub use sealed::{seal, unseal, SealedData, SealingPolicy};
pub use simulate::{
    build_simulated_report, is_simulate_mode, is_simulated_report, TEE_SIMULATE_ENV,
};
pub use snp::{check_sev_snp_support, require_sev_snp_support, SevSnpSupport};
pub use verifier::{verify_attestation, verify_attestation_with_time, VerificationResult};
