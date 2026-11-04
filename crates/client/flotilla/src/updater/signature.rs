//! Ed25519 signature verification for the release manifest.
//!
//! Release artifacts are signed by `scripts/fl-sign` (see
//! `docs/release-signing.md`). The client only ever holds public keys; the
//! private key exists solely as the `FLOTILLA_SIGNING_KEY` GitHub Actions
//! secret. A manifest whose signature does not verify against one of
//! [`TRUSTED_KEYS`] is never parsed, downloaded or installed.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use ed25519_dalek::{Signature, VerifyingKey};
#[cfg(test)]
use sha2::{Digest, Sha256};

/// A trusted signing key as `(key id, base64 Ed25519 public key)`; the id is
/// the first 8 bytes of SHA-256 over the 32 public-key bytes, hex.
pub type TrustedKey = (&'static str, &'static str);

/// Keys this build trusts. Rotation: append the new key, ship a client that
/// trusts both, then stop signing with the old one and remove it.
pub const TRUSTED_KEYS: &[TrustedKey] = &[(
    "d901808076693bdc",
    "IXfEN939GNZL0F7bRpAqzuHUuoCIKXIfvAB60KKUWw4=",
)];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VerifyError {
    #[error("signed by unknown key {0:?}")]
    UnknownKey(String),
    #[error("malformed signature: {0}")]
    Malformed(String),
    #[error("signature does not match the document")]
    Mismatch,
}

/// Verify a detached signature over `bytes`.
///
/// `key_id` comes from inside the signed manifest; an id the caller does not
/// trust is refused before any cryptography runs, which keeps rotation
/// explicit.
pub fn verify(
    keys: &[(&str, &str)],
    key_id: &str,
    bytes: &[u8],
    signature_b64: &str,
) -> Result<(), VerifyError> {
    let (_, public_key) = keys
        .iter()
        .find(|(id, _)| id.eq_ignore_ascii_case(key_id))
        .ok_or_else(|| VerifyError::UnknownKey(key_id.to_string()))?;
    let key = parse_public_key(public_key)?;
    let raw = B64
        .decode(signature_b64.trim())
        .map_err(|e| VerifyError::Malformed(e.to_string()))?;
    let signature =
        Signature::from_slice(&raw).map_err(|e| VerifyError::Malformed(e.to_string()))?;
    key.verify_strict(bytes, &signature)
        .map_err(|_| VerifyError::Mismatch)
}

/// The manifest signature file (`latest.json.sig`) is base64 text.
pub fn signature_text(signature_file: &[u8]) -> Result<&str, VerifyError> {
    std::str::from_utf8(signature_file).map_err(|e| VerifyError::Malformed(e.to_string()))
}

fn parse_public_key(public_key_b64: &str) -> Result<VerifyingKey, VerifyError> {
    let raw = B64
        .decode(public_key_b64.trim())
        .map_err(|e| VerifyError::Malformed(e.to_string()))?;
    let bytes: [u8; 32] = raw
        .try_into()
        .map_err(|_| VerifyError::Malformed("public key is not 32 bytes".into()))?;
    VerifyingKey::from_bytes(&bytes).map_err(|e| VerifyError::Malformed(e.to_string()))
}

/// The id of a public key, as written into the manifest by the signing tool.
#[cfg(test)]
pub(crate) fn key_id_of(public_key_b64: &str) -> Result<String, VerifyError> {
    let key = parse_public_key(public_key_b64)?;
    let digest = Sha256::digest(key.as_bytes());
    let mut id = String::with_capacity(16);
    for byte in &digest[..8] {
        id.push_str(&format!("{byte:02x}"));
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    /// Test-only key: the shipped key never signs test data.
    const TEST_SEED: [u8; 32] = [7u8; 32];

    struct TestKeys {
        key_id: String,
        public_key: String,
        signing: SigningKey,
    }

    impl TestKeys {
        fn new() -> Self {
            let signing = SigningKey::from_bytes(&TEST_SEED);
            let public_key = B64.encode(signing.verifying_key().to_bytes());
            let key_id = key_id_of(&public_key).expect("test key id");
            Self {
                key_id,
                public_key,
                signing,
            }
        }

        fn trusted(&self) -> [(&str, &str); 1] {
            [(self.key_id.as_str(), self.public_key.as_str())]
        }
    }

    #[test]
    fn accepts_a_signature_from_a_trusted_key() {
        let keys = TestKeys::new();
        let doc = br#"{"schema":1,"product":"flotilla"}"#;
        let sig = B64.encode(keys.signing.sign(doc).to_bytes());
        verify(&keys.trusted(), &keys.key_id, doc, &sig).expect("valid signature");
    }

    #[test]
    fn refuses_a_tampered_document() {
        let keys = TestKeys::new();
        let doc = br#"{"schema":1,"product":"flotilla","version":"0.5.0"}"#;
        let sig = B64.encode(keys.signing.sign(doc).to_bytes());
        let tampered = br#"{"schema":1,"product":"flotilla","version":"9.9.9"}"#;
        assert_eq!(
            verify(&keys.trusted(), &keys.key_id, tampered, &sig),
            Err(VerifyError::Mismatch)
        );
    }

    #[test]
    fn refuses_a_signature_from_an_unknown_key() {
        let keys = TestKeys::new();
        let doc = br#"{}"#;
        let sig = B64.encode(keys.signing.sign(doc).to_bytes());
        assert_eq!(
            verify(&keys.trusted(), "0000000000000000", doc, &sig),
            Err(VerifyError::UnknownKey("0000000000000000".into()))
        );
        // An empty key id never matches.
        assert!(matches!(
            verify(&keys.trusted(), "", doc, &sig),
            Err(VerifyError::UnknownKey(_))
        ));
    }

    #[test]
    fn refuses_a_malformed_signature() {
        let keys = TestKeys::new();
        assert!(matches!(
            verify(&keys.trusted(), &keys.key_id, b"{}", "not base64!"),
            Err(VerifyError::Malformed(_))
        ));
        let short = B64.encode([0u8; 10]);
        assert!(matches!(
            verify(&keys.trusted(), &keys.key_id, b"{}", &short),
            Err(VerifyError::Malformed(_))
        ));
    }

    #[test]
    fn the_shipped_key_is_self_consistent() {
        assert!(!TRUSTED_KEYS.is_empty());
        for (key_id, public_key) in TRUSTED_KEYS {
            assert_eq!(key_id_of(public_key).unwrap(), *key_id);
            assert_eq!(key_id.len(), 16);
        }
    }

    #[test]
    fn signature_files_are_base64_text() {
        assert!(signature_text(b"AAAA\n").is_ok());
        assert!(matches!(
            signature_text(&[0xff, 0xfe]),
            Err(VerifyError::Malformed(_))
        ));
    }
}
