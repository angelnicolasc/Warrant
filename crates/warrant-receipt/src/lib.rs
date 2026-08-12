//! L7 — receipts.
//!
//! The attestation rendered for someone who was not there: third-party
//! verifiable, signed over content-addressed evidence, and legible without
//! any of this project's vocabulary.
//!
//! The wire format is an in-toto statement inside a DSSE envelope, so
//! existing supply-chain tooling consumes it unchanged. The one addition is a
//! sibling `key` field carrying the public key, which makes a receipt
//! self-checking for integrity. It does **not** make it self-authenticating:
//! a valid signature proves the evidence has not changed since whoever holds
//! that key produced it, and nothing about who that is. [`verify_pinned`]
//! exists for the reader who knows which key they expect.
//!
//! ```
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use warrant_receipt::{ReceiptFile, SigningIdentity};
//!
//! let identity = SigningIdentity::from_seed([42u8; 32]);
//! # let map = warrant_receipt::doc_support::example_map();
//! # let statement = warrant_receipt::doc_support::example_statement(&map);
//! let receipt = ReceiptFile::issue(&statement, &identity);
//! let json = receipt.to_json()?;
//!
//! // Anyone holding only the JSON can check it.
//! let parsed = ReceiptFile::from_json(json.as_bytes())?;
//! let checked = parsed.verify()?;
//! assert!(checked.summary().contains("Necessity is not sufficiency"));
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod dsse;
mod error;
mod identity;
mod statement;

pub use dsse::{Envelope, IN_TOTO_PAYLOAD_TYPE, Signature, pae};
pub use error::{ReceiptError, Result};
pub use identity::{KEY_FILE, SigningIdentity, key_id_for, key_path, verify};
pub use statement::{
    DISCLAIMER, Findings, OutcomeSummary, PREDICATE_TYPE, ProofMapPredicate, ProofSummary,
    RunSummary, STATEMENT_TYPE, Statement, Subject, Tool,
};

use serde::{Deserialize, Serialize};
use warrant_core::ReceiptRef;

/// How a receipt is written to disk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptFile {
    /// The signed envelope, in standard DSSE form.
    pub envelope: Envelope,
    /// The public key the signature should be checked against.
    pub key: PublicKeyInfo,
}

/// The key a receipt was signed with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicKeyInfo {
    /// Always `ed25519`.
    pub algorithm: String,
    /// Short, stable name derived from the key.
    pub keyid: String,
    /// The public key, hex-encoded.
    pub hex: String,
}

impl ReceiptFile {
    /// Sign a statement.
    pub fn issue(statement: &Statement, identity: &SigningIdentity) -> Self {
        // Serialised once, here, so the bytes that were signed are exactly the
        // bytes that travel. Re-serialising before verification would make the
        // signature depend on serde's field ordering.
        let payload = serde_json::to_vec(statement).expect("a statement always serialises");
        ReceiptFile {
            envelope: identity.sign(&payload, IN_TOTO_PAYLOAD_TYPE),
            key: PublicKeyInfo {
                algorithm: "ed25519".into(),
                keyid: identity.key_id(),
                hex: identity.public_hex(),
            },
        }
    }

    /// Check the signature against the key the receipt carries, and return
    /// the statement it covers.
    ///
    /// This establishes integrity. For identity, use [`ReceiptFile::verify_pinned`].
    pub fn verify(&self) -> Result<Statement> {
        self.verify_pinned(&self.key.hex)
    }

    /// Check the signature against a key the reader supplies.
    ///
    /// Fails if the receipt was signed by anyone else, which is the check
    /// that actually means something.
    pub fn verify_pinned(&self, public_key_hex: &str) -> Result<Statement> {
        let payload = verify(&self.envelope, public_key_hex)?;
        let statement: Statement = serde_json::from_slice(&payload)
            .map_err(|e| ReceiptError::NotAProofMap { reason: e.to_string() })?;
        if !statement.is_well_formed() {
            return Err(ReceiptError::NotAProofMap {
                reason: format!("expected {PREDICATE_TYPE}, found {}", statement.predicate_type),
            });
        }
        Ok(statement)
    }

    /// Read a receipt from JSON.
    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        Ok(serde_json::from_slice(bytes)?)
    }

    /// Write a receipt as indented JSON.
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Content address of this receipt, for the ledger and for the verdict
    /// that points at it.
    pub fn address(&self) -> ReceiptRef {
        ReceiptRef::derive(&[
            self.envelope.payload.as_bytes(),
            self.envelope.payload_type.as_bytes(),
            self.key.hex.as_bytes(),
        ])
    }
}

/// Fixtures used by this crate's documentation examples.
#[doc(hidden)]
pub mod doc_support {
    use super::*;
    use warrant_cell::{IsolationLevel, IsolationReport};
    use warrant_core::{Hash, PredicateHash, ProofTier, Ratio};
    use warrant_necessity::{MapOutcome, NecessityMap};

    /// A minimal mapped result.
    pub fn example_map() -> NecessityMap {
        let mut map = NecessityMap::no_changes(PredicateHash::derive(&[b"p"]), Hash::of(b"tree"));
        map.outcome = MapOutcome::Mapped;
        map.satisfied = true;
        map.null_passed = true;
        map.coverage = Ratio::new(14, 37);
        map
    }

    /// A statement over [`example_map`].
    pub fn example_statement(map: &NecessityMap) -> Statement {
        Statement::from_map(
            map,
            ProofSummary {
                predicate: PredicateHash::derive(&[b"p"]),
                source: "exit(pytest) == 0".into(),
                tier: ProofTier::Unit,
                commands: vec!["pytest".into()],
                defaulted: true,
            },
            IsolationReport {
                backend: "workspace".into(),
                filesystem: IsolationLevel::Directory,
                network: IsolationLevel::None,
                process: IsolationLevel::None,
                caveats: Vec::new(),
            },
            RunSummary::between(Hash::of(b"before"), Hash::of(b"after")),
            None,
            1_786_492_800_000,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt(identity: &SigningIdentity) -> ReceiptFile {
        let map = doc_support::example_map();
        let statement = doc_support::example_statement(&map);
        ReceiptFile::issue(&statement, identity)
    }

    #[test]
    fn a_receipt_survives_the_round_trip_a_third_party_would_make() {
        let identity = SigningIdentity::from_seed([11u8; 32]);
        let original = receipt(&identity);
        let json = original.to_json().unwrap();

        let parsed = ReceiptFile::from_json(json.as_bytes()).unwrap();
        assert_eq!(parsed, original);
        let statement = parsed.verify().unwrap();
        assert_eq!(statement.predicate.outcome.coverage_percent, Some(38));
    }

    #[test]
    fn tampering_with_the_number_breaks_the_receipt() {
        let identity = SigningIdentity::from_seed([12u8; 32]);
        let mut file = receipt(&identity);

        // Re-sign the payload? No — an attacker without the key can only
        // rewrite the payload, and that is what must be caught.
        let mut statement = file.verify().unwrap();
        statement.predicate.outcome.coverage_percent = Some(99);
        file.envelope.payload = dsse::encode(&serde_json::to_vec(&statement).unwrap());

        assert!(matches!(file.verify(), Err(ReceiptError::SignatureInvalid { .. })));
    }

    #[test]
    fn a_receipt_signed_by_another_key_fails_a_pinned_check() {
        let mine = SigningIdentity::from_seed([13u8; 32]);
        let theirs = SigningIdentity::from_seed([14u8; 32]);
        let file = receipt(&theirs);

        // Self-verification passes: the receipt is internally consistent.
        assert!(file.verify().is_ok());
        // Pinning to the key the reader expects is what catches the swap.
        assert!(file.verify_pinned(&mine.public_hex()).is_err());
    }

    #[test]
    fn the_envelope_is_standard_dsse_so_other_tooling_can_read_it() {
        let identity = SigningIdentity::from_seed([15u8; 32]);
        let json: serde_json::Value =
            serde_json::from_str(&receipt(&identity).to_json().unwrap()).unwrap();

        let envelope = &json["envelope"];
        assert_eq!(envelope["payloadType"], IN_TOTO_PAYLOAD_TYPE);
        assert!(envelope["payload"].is_string());
        assert!(envelope["signatures"][0]["keyid"].is_string());
        assert!(envelope["signatures"][0]["sig"].is_string());
    }

    #[test]
    fn a_receipt_address_is_content_derived() {
        let identity = SigningIdentity::from_seed([16u8; 32]);
        let a = receipt(&identity);
        let b = receipt(&identity);
        assert_eq!(a.address(), b.address(), "the same evidence addresses the same");

        let other = receipt(&SigningIdentity::from_seed([17u8; 32]));
        assert_ne!(a.address(), other.address(), "a different signer is different evidence");
    }

    #[test]
    fn a_payload_that_is_not_a_proof_map_is_rejected() {
        let identity = SigningIdentity::from_seed([18u8; 32]);
        let envelope = identity.sign(br#"{"_type":"other","subject":[]}"#, IN_TOTO_PAYLOAD_TYPE);
        let file = ReceiptFile {
            envelope,
            key: PublicKeyInfo {
                algorithm: "ed25519".into(),
                keyid: identity.key_id(),
                hex: identity.public_hex(),
            },
        };
        assert!(matches!(file.verify(), Err(ReceiptError::NotAProofMap { .. })));
    }
}
