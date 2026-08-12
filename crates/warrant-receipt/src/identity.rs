//! Signing identity.
//!
//! One ed25519 key per machine, stored as hex beside the ledger. Deliberately
//! not a certificate, not a keyring, not a trust root: a receipt proves that
//! whoever holds this key produced this evidence, and says so in exactly
//! those words. Establishing *who* holds the key is the reader's problem, and
//! pretending otherwise would be the kind of overclaim this project exists to
//! avoid.

use std::fs;
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use rand::Rng;
use warrant_core::Hash;

use crate::dsse::{Envelope, Signature, encode, pae};
use crate::error::{ReceiptError, Result};

/// Standard filename for the signing key inside a ledger directory.
pub const KEY_FILE: &str = "signing.key";

/// A key that can sign receipts.
pub struct SigningIdentity {
    key: SigningKey,
}

impl SigningIdentity {
    /// Generate a fresh identity.
    pub fn generate() -> Self {
        let mut seed = [0u8; 32];
        rand::rng().fill_bytes(&mut seed);
        SigningIdentity { key: SigningKey::from_bytes(&seed) }
    }

    /// Build an identity from a 32-byte seed. Deterministic, for tests and
    /// for reproducing a receipt from a recorded key.
    pub fn from_seed(seed: [u8; 32]) -> Self {
        SigningIdentity { key: SigningKey::from_bytes(&seed) }
    }

    /// Load the identity at `path`, creating one if it is not there.
    ///
    /// On Unix the file is created with owner-only permissions. On Windows it
    /// inherits the directory's ACL, which is weaker; that is a platform
    /// difference worth knowing rather than one worth hiding.
    pub fn load_or_create(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if path.exists() {
            let text = fs::read_to_string(path)
                .map_err(|e| ReceiptError::io("reading the signing key", path, e))?;
            let mut seed = [0u8; 32];
            hex::decode_to_slice(text.trim(), &mut seed)
                .map_err(|_| ReceiptError::MalformedKey { path: path.to_path_buf() })?;
            return Ok(SigningIdentity::from_seed(seed));
        }

        let identity = SigningIdentity::generate();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| ReceiptError::io("creating the key directory", parent, e))?;
        }
        fs::write(path, identity.seed_hex())
            .map_err(|e| ReceiptError::io("writing the signing key", path, e))?;
        restrict_permissions(path);
        Ok(identity)
    }

    /// The private seed, as hex. Only for persisting the key.
    fn seed_hex(&self) -> String {
        hex::encode(self.key.to_bytes())
    }

    /// The public key, as hex. Goes in the receipt.
    pub fn public_hex(&self) -> String {
        hex::encode(self.key.verifying_key().to_bytes())
    }

    /// A short, stable name for this key, derived from the public key.
    pub fn key_id(&self) -> String {
        key_id_for(&self.key.verifying_key().to_bytes())
    }

    /// Sign a payload, producing a DSSE envelope.
    pub fn sign(&self, payload: &[u8], payload_type: &str) -> Envelope {
        let signature = self.key.sign(&pae(payload_type, payload));
        Envelope {
            payload: encode(payload),
            payload_type: payload_type.to_owned(),
            signatures: vec![Signature {
                keyid: self.key_id(),
                sig: encode(&signature.to_bytes()),
            }],
        }
    }
}

/// Derive a key id from a public key.
pub fn key_id_for(public_key: &[u8]) -> String {
    Hash::of_tagged("warrant.keyid.v1", &[public_key]).short()
}

/// Verify an envelope against a public key, returning the payload it covers.
///
/// A valid signature proves the payload has not changed since it was signed
/// by the holder of `public_key_hex`. It proves nothing about who that is.
pub fn verify(envelope: &Envelope, public_key_hex: &str) -> Result<Vec<u8>> {
    let mut key_bytes = [0u8; 32];
    hex::decode_to_slice(public_key_hex.trim(), &mut key_bytes)
        .map_err(|_| ReceiptError::MalformedPublicKey)?;
    let verifying =
        VerifyingKey::from_bytes(&key_bytes).map_err(|_| ReceiptError::MalformedPublicKey)?;

    let payload = envelope.decode_payload().ok_or(ReceiptError::MalformedEnvelope {
        reason: "the payload is not valid base64".into(),
    })?;
    let signing_bytes = pae(&envelope.payload_type, &payload);
    let expected_id = key_id_for(&key_bytes);

    for signature in &envelope.signatures {
        let Some(raw) = crate::dsse::decode(&signature.sig) else { continue };
        let Ok(bytes) = <[u8; 64]>::try_from(raw.as_slice()) else { continue };
        let candidate = ed25519_dalek::Signature::from_bytes(&bytes);
        if verifying.verify(&signing_bytes, &candidate).is_ok() {
            return Ok(payload);
        }
    }

    Err(ReceiptError::SignatureInvalid { expected_key_id: expected_id })
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}

/// Where the signing key lives for a given ledger directory.
pub fn key_path(ledger_root: impl AsRef<Path>) -> PathBuf {
    ledger_root.as_ref().join(KEY_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsse::IN_TOTO_PAYLOAD_TYPE;

    #[test]
    fn a_signature_verifies_against_its_own_key() {
        let identity = SigningIdentity::from_seed([7u8; 32]);
        let envelope = identity.sign(b"the evidence", IN_TOTO_PAYLOAD_TYPE);
        assert_eq!(verify(&envelope, &identity.public_hex()).unwrap(), b"the evidence");
    }

    #[test]
    fn a_signature_does_not_verify_against_another_key() {
        let signer = SigningIdentity::from_seed([1u8; 32]);
        let other = SigningIdentity::from_seed([2u8; 32]);
        let envelope = signer.sign(b"the evidence", IN_TOTO_PAYLOAD_TYPE);
        assert!(verify(&envelope, &other.public_hex()).is_err());
    }

    #[test]
    fn altering_the_payload_invalidates_the_signature() {
        let identity = SigningIdentity::from_seed([3u8; 32]);
        let mut envelope = identity.sign(b"coverage 38%", IN_TOTO_PAYLOAD_TYPE);
        envelope.payload = encode(b"coverage 98%");
        assert!(verify(&envelope, &identity.public_hex()).is_err());
    }

    /// The reason the pre-authentication encoding exists: a signature over one
    /// payload type must not verify as a signature over another.
    #[test]
    fn altering_the_payload_type_invalidates_the_signature() {
        let identity = SigningIdentity::from_seed([4u8; 32]);
        let mut envelope = identity.sign(b"the evidence", IN_TOTO_PAYLOAD_TYPE);
        envelope.payload_type = "application/vnd.something-else+json".into();
        assert!(verify(&envelope, &identity.public_hex()).is_err());
    }

    #[test]
    fn identity_is_deterministic_from_a_seed_and_random_otherwise() {
        assert_eq!(
            SigningIdentity::from_seed([9u8; 32]).public_hex(),
            SigningIdentity::from_seed([9u8; 32]).public_hex()
        );
        assert_ne!(
            SigningIdentity::generate().public_hex(),
            SigningIdentity::generate().public_hex()
        );
    }

    #[test]
    fn a_key_survives_a_round_trip_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = key_path(dir.path());

        let created = SigningIdentity::load_or_create(&path).unwrap();
        let reloaded = SigningIdentity::load_or_create(&path).unwrap();
        assert_eq!(created.public_hex(), reloaded.public_hex());
        assert_eq!(created.key_id(), reloaded.key_id());
    }

    #[test]
    fn a_corrupt_key_file_is_reported_rather_than_silently_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = key_path(dir.path());
        fs::write(&path, "not hex at all").unwrap();
        assert!(matches!(
            SigningIdentity::load_or_create(&path),
            Err(ReceiptError::MalformedKey { .. })
        ));
    }

    #[test]
    fn key_ids_differ_between_keys() {
        assert_ne!(
            SigningIdentity::from_seed([1u8; 32]).key_id(),
            SigningIdentity::from_seed([2u8; 32]).key_id()
        );
    }
}
