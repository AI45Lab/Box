//! Cryptographic primitives and Fulcio X.509 helpers for image signature verification.

use base64::Engine;
use der::Decode;

use super::FULCIO_ISSUER_OID;

/// Verify the OIDC issuer in a Fulcio certificate matches the expected value.
///
/// The issuer is stored in a custom X.509 extension with OID 1.3.6.1.4.1.57264.1.1.
pub(super) fn verify_fulcio_issuer(
    cert: &x509_cert::Certificate,
    expected_issuer: &str,
) -> std::result::Result<(), String> {
    let issuer_oid = der::asn1::ObjectIdentifier::new(FULCIO_ISSUER_OID)
        .map_err(|e| format!("Failed to construct Fulcio issuer OID: {}", e))?;

    let extensions = cert
        .tbs_certificate
        .extensions
        .as_ref()
        .ok_or("Certificate has no extensions")?;

    for ext in extensions.iter() {
        if ext.extn_id == issuer_oid {
            // The extension value is a DER-encoded UTF8String or OCTET STRING containing the issuer
            let issuer_value =
                if let Ok(utf8) = der::asn1::Utf8StringRef::from_der(ext.extn_value.as_bytes()) {
                    utf8.to_string()
                } else {
                    // Fallback: treat as raw UTF-8 bytes
                    String::from_utf8(ext.extn_value.as_bytes().to_vec())
                        .map_err(|e| format!("Fulcio issuer extension is not valid UTF-8: {}", e))?
                };

            if issuer_value == expected_issuer {
                return Ok(());
            } else {
                return Err(format!(
                    "expected '{}', got '{}'",
                    expected_issuer, issuer_value
                ));
            }
        }
    }

    Err("Fulcio issuer extension (OID 1.3.6.1.4.1.57264.1.1) not found in certificate".into())
}

/// Verify the identity (email or URI) in a Fulcio certificate's Subject Alternative Name.
pub(super) fn verify_fulcio_identity(
    cert: &x509_cert::Certificate,
    expected_identity: &str,
) -> std::result::Result<(), String> {
    use x509_cert::ext::pkix::SubjectAltName;

    let extensions = cert
        .tbs_certificate
        .extensions
        .as_ref()
        .ok_or("Certificate has no extensions")?;

    // Find the SAN extension (OID 2.5.29.17)
    let san_oid = der::asn1::ObjectIdentifier::new("2.5.29.17")
        .map_err(|e| format!("Failed to construct SAN OID: {}", e))?;

    for ext in extensions.iter() {
        if ext.extn_id == san_oid {
            let san = SubjectAltName::from_der(ext.extn_value.as_bytes())
                .map_err(|e| format!("Failed to parse SAN extension: {}", e))?;

            for name in san.0.iter() {
                match name {
                    x509_cert::ext::pkix::name::GeneralName::Rfc822Name(email) => {
                        let email_str: &str = email.as_ref();
                        if email_str == expected_identity {
                            return Ok(());
                        }
                    }
                    x509_cert::ext::pkix::name::GeneralName::UniformResourceIdentifier(uri) => {
                        let uri_str: &str = uri.as_ref();
                        if uri_str == expected_identity {
                            return Ok(());
                        }
                    }
                    _ => continue,
                }
            }

            // Collect found identities for error message
            let found: Vec<String> = san
                .0
                .iter()
                .filter_map(|n| match n {
                    x509_cert::ext::pkix::name::GeneralName::Rfc822Name(e) => Some(e.to_string()),
                    x509_cert::ext::pkix::name::GeneralName::UniformResourceIdentifier(u) => {
                        Some(u.to_string())
                    }
                    _ => None,
                })
                .collect();

            return Err(format!(
                "expected '{}', found [{}]",
                expected_identity,
                found.join(", ")
            ));
        }
    }

    Err("Subject Alternative Name extension not found in certificate".into())
}

/// Extract the public key bytes (SEC1 uncompressed point) from an X.509 certificate.
pub(super) fn extract_cert_public_key(
    cert: &x509_cert::Certificate,
) -> std::result::Result<Vec<u8>, String> {
    cert.tbs_certificate
        .subject_public_key_info
        .subject_public_key
        .as_bytes()
        .map(|b| b.to_vec())
        .ok_or_else(|| "Failed to extract public key bytes from certificate".to_string())
}

/// Decode a PEM block into DER bytes.
pub(super) fn pem_to_der(pem_str: &str) -> std::result::Result<Vec<u8>, String> {
    // Find the first PEM block
    let begin = pem_str
        .find("-----BEGIN ")
        .ok_or("No PEM begin marker found")?;
    let begin_end = pem_str[begin..]
        .find("-----\n")
        .or_else(|| pem_str[begin..].find("-----\r\n"))
        .ok_or("Malformed PEM begin marker")?
        + begin
        + 6; // skip past "-----\n"

    let end = pem_str[begin_end..]
        .find("-----END ")
        .ok_or("No PEM end marker found")?
        + begin_end;

    let b64: String = pem_str[begin_end..end]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();

    base64_decode(&b64).map_err(|e| format!("Failed to decode PEM base64: {}", e))
}
/// Parse a PEM-encoded public key (ECDSA P-256) into raw SEC1 bytes.
///
/// Supports both "PUBLIC KEY" (SPKI/PKIX) and "EC PUBLIC KEY" (SEC1) PEM formats.
pub(super) fn parse_pem_public_key(pem_bytes: &[u8]) -> std::result::Result<Vec<u8>, String> {
    let pem_str = std::str::from_utf8(pem_bytes)
        .map_err(|e| format!("PEM file is not valid UTF-8: {}", e))?;

    // Extract the base64 content between PEM headers
    let (begin_marker, end_marker) = if pem_str.contains("BEGIN PUBLIC KEY") {
        ("-----BEGIN PUBLIC KEY-----", "-----END PUBLIC KEY-----")
    } else if pem_str.contains("BEGIN EC PUBLIC KEY") {
        (
            "-----BEGIN EC PUBLIC KEY-----",
            "-----END EC PUBLIC KEY-----",
        )
    } else {
        return Err("Unsupported PEM format: expected PUBLIC KEY or EC PUBLIC KEY".to_string());
    };

    let start = pem_str
        .find(begin_marker)
        .ok_or("Missing PEM begin marker")?
        + begin_marker.len();
    let end = pem_str.find(end_marker).ok_or("Missing PEM end marker")?;

    let b64: String = pem_str[start..end]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();

    let der_bytes =
        base64_decode(&b64).map_err(|e| format!("Failed to decode PEM base64: {}", e))?;

    // If SPKI format, extract the public key bytes from the SubjectPublicKeyInfo structure
    if begin_marker.contains("BEGIN PUBLIC KEY") {
        extract_spki_public_key(&der_bytes)
    } else {
        // SEC1 format — the DER bytes are the raw EC point
        Ok(der_bytes)
    }
}

/// Extract the public key bytes from a DER-encoded SubjectPublicKeyInfo.
pub(super) fn extract_spki_public_key(der: &[u8]) -> std::result::Result<Vec<u8>, String> {
    use spki::SubjectPublicKeyInfo;

    let spki =
        SubjectPublicKeyInfo::<der::asn1::AnyRef<'_>, der::asn1::BitStringRef<'_>>::from_der(der)
            .map_err(|e| format!("Failed to parse SPKI: {}", e))?;

    spki.subject_public_key
        .as_bytes()
        .map(|b| b.to_vec())
        .ok_or_else(|| "Failed to extract public key bytes from SPKI".to_string())
}

/// Verify an ECDSA P-256 signature over a message.
pub(super) fn verify_ecdsa_p256(
    public_key_bytes: &[u8],
    message: &[u8],
    signature: &[u8],
) -> std::result::Result<(), String> {
    use p256::ecdsa::{signature::Verifier, Signature, VerifyingKey};

    let verifying_key = VerifyingKey::from_sec1_bytes(public_key_bytes)
        .map_err(|e| format!("Invalid P-256 public key: {}", e))?;

    // Cosign produces DER-encoded signatures. Try DER first, then fixed-size.
    let result = if let Ok(sig) = p256::ecdsa::DerSignature::from_bytes(signature) {
        verifying_key.verify(message, &sig)
    } else if signature.len() == 64 {
        let sig = Signature::from_slice(signature)
            .map_err(|e| format!("Invalid P-256 signature: {}", e))?;
        verifying_key.verify(message, &sig)
    } else {
        return Err(format!(
            "Unrecognized signature format ({} bytes)",
            signature.len()
        ));
    };

    result.map_err(|_| "ECDSA P-256 signature verification failed".to_string())
}

/// Decode a base64 string (standard alphabet with padding).
pub(super) fn base64_decode(input: &str) -> std::result::Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD
        .decode(input.trim())
        .map_err(|e| format!("base64 decode error: {}", e))
}

/// Encode bytes to base64 (standard alphabet with padding).
pub(super) fn base64_encode(input: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(input)
}
