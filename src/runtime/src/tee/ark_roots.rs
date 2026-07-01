//! Pinned AMD SEV ARK (AMD Root Key) trust anchors.
//!
//! The SNP attestation certificate chain (VCEK → ASK → ARK) is only
//! trustworthy if it chains up to a **genuine AMD root**. Verifying that the
//! chain is internally self-consistent is NOT sufficient: an attacker can mint
//! their own self-signed "ARK" → ASK → VCEK and sign a forged report, and every
//! internal signature checks out. The defense is to **pin** the ARK against
//! AMD's real published root keys, so a self-minted root is rejected
//! (fail-closed).
//!
//! These are the genuine ARK certificates published by AMD's Key Distribution
//! Service (`https://kdsintf.amd.com/vcek/v1/<product>/cert_chain`), DER-encoded,
//! one per EPYC generation. Each was verified at vendoring time: self-signed
//! (subject == issuer), `openssl verify` OK, RSA-4096.
//!
//! | Product | CN | SHA-256(DER) |
//! |---------|----|--------------|
//! | Milan (3rd gen) | ARK-Milan | `69d063b4…40bcd` |
//! | Genoa (4th gen) | ARK-Genoa | `4c6598d1…3db2f1` |
//! | Turin (5th gen) | ARK-Turin | `1f084161…3dd3f6a` |

use der::{Decode, Encode};
use x509_cert::Certificate;

/// Genuine AMD ARK certificates (DER), one per EPYC product line.
const MILAN_ARK_DER: &[u8] = include_bytes!("ark_roots/milan.der");
const GENOA_ARK_DER: &[u8] = include_bytes!("ark_roots/genoa.der");
const TURIN_ARK_DER: &[u8] = include_bytes!("ark_roots/turin.der");

/// All pinned AMD ARK roots (DER), newest first.
pub const AMD_ARK_ROOTS: [&[u8]; 3] = [TURIN_ARK_DER, GENOA_ARK_DER, MILAN_ARK_DER];

/// DER of a certificate's `SubjectPublicKeyInfo` — the bytes that identify its
/// key independent of the rest of the certificate.
fn spki_der(cert_der: &[u8]) -> Option<Vec<u8>> {
    let cert = Certificate::from_der(cert_der).ok()?;
    cert.tbs_certificate.subject_public_key_info.to_der().ok()
}

/// Returns `true` if `ark_der` presents the same public key as one of the
/// genuine pinned AMD ARK roots.
///
/// Pinning is by **public key** (the `SubjectPublicKeyInfo`), not the full
/// certificate, so a benign AMD reissue of the ARK cert with the same key still
/// validates, while a self-minted root (any key AMD never published) is
/// rejected.
pub fn is_trusted_ark(ark_der: &[u8]) -> bool {
    let Some(candidate) = spki_der(ark_der) else {
        return false;
    };
    AMD_ARK_ROOTS
        .iter()
        .filter_map(|root| spki_der(root))
        .any(|root_spki| root_spki == candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_roots_parse_and_are_self_signed() {
        for (name, der) in [
            ("turin", TURIN_ARK_DER),
            ("genoa", GENOA_ARK_DER),
            ("milan", MILAN_ARK_DER),
        ] {
            let cert = Certificate::from_der(der)
                .unwrap_or_else(|e| panic!("{name} ARK must be valid DER: {e}"));
            assert_eq!(
                cert.tbs_certificate.issuer, cert.tbs_certificate.subject,
                "{name} ARK must be self-signed (issuer == subject)"
            );
        }
    }

    #[test]
    fn genuine_roots_are_trusted() {
        for der in AMD_ARK_ROOTS {
            assert!(
                is_trusted_ark(der),
                "an embedded genuine ARK must be trusted"
            );
        }
    }

    #[test]
    fn empty_and_garbage_are_not_trusted() {
        assert!(!is_trusted_ark(&[]));
        assert!(!is_trusted_ark(&[0x30, 0x00]));
        assert!(!is_trusted_ark(b"not a certificate"));
    }
}
