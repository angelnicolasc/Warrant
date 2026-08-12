//! Content addresses.
//!
//! Every identifier in Warrant is derived from content, so nothing in the
//! system can be renamed into something it is not. The ledger chain, claim
//! ids, hunk ids and predicate hashes all bottom out here.

use std::fmt;

use serde::de::{Error as DeError, Unexpected};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Length of a BLAKE3 digest in bytes.
pub const HASH_LEN: usize = 32;

/// Textual prefix, so a hash is self-describing wherever it is printed.
pub const HASH_PREFIX: &str = "blake3:";

/// A BLAKE3 content address.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hash([u8; HASH_LEN]);

impl Hash {
    /// The all-zero address. Used as the parent of the first ledger entry.
    pub const ZERO: Hash = Hash([0u8; HASH_LEN]);

    /// Hash a single byte string.
    pub fn of(bytes: &[u8]) -> Self {
        Hash(*blake3::hash(bytes).as_bytes())
    }

    /// Hash a sequence of fields under a domain tag.
    ///
    /// Each part is length-prefixed before being absorbed, so
    /// `["ab", "c"]` and `["a", "bc"]` produce different digests. Without
    /// this, a hash chain is trivially forgeable by shifting bytes across a
    /// field boundary.
    pub fn of_tagged(tag: &str, parts: &[&[u8]]) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&(tag.len() as u64).to_le_bytes());
        hasher.update(tag.as_bytes());
        for part in parts {
            hasher.update(&(part.len() as u64).to_le_bytes());
            hasher.update(part);
        }
        Hash(*hasher.finalize().as_bytes())
    }

    /// Borrow the raw digest.
    pub const fn as_bytes(&self) -> &[u8; HASH_LEN] {
        &self.0
    }

    /// Build from a raw digest.
    pub const fn from_bytes(bytes: [u8; HASH_LEN]) -> Self {
        Hash(bytes)
    }

    /// Lowercase hex, without the `blake3:` prefix.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// First 12 hex characters, for terminal output.
    pub fn short(&self) -> String {
        hex::encode(&self.0[..6])
    }

    /// Parse either `blake3:<hex>` or a bare 64-character hex string.
    pub fn parse(s: &str) -> Result<Self, HashParseError> {
        let body = s.strip_prefix(HASH_PREFIX).unwrap_or(s);
        if body.len() != HASH_LEN * 2 {
            return Err(HashParseError::Length { got: body.len() });
        }
        let mut out = [0u8; HASH_LEN];
        hex::decode_to_slice(body, &mut out).map_err(|_| HashParseError::NotHex)?;
        Ok(Hash(out))
    }

    /// Whether this address is the zero address.
    pub fn is_zero(&self) -> bool {
        self.0 == [0u8; HASH_LEN]
    }
}

/// Streaming hasher, for files that should not be read into memory whole.
#[derive(Default)]
pub struct Hasher(blake3::Hasher);

impl Hasher {
    /// A fresh hasher.
    pub fn new() -> Self {
        Hasher(blake3::Hasher::new())
    }

    /// Absorb more bytes.
    pub fn update(&mut self, bytes: &[u8]) -> &mut Self {
        self.0.update(bytes);
        self
    }

    /// Finish and produce the address.
    pub fn finalize(&self) -> Hash {
        Hash(*self.0.finalize().as_bytes())
    }
}

/// Why a string could not be read as a content address.
#[derive(Debug, thiserror::Error)]
pub enum HashParseError {
    /// Wrong number of hex characters.
    #[error("expected {} hex characters, got {got}", HASH_LEN * 2)]
    Length {
        /// How many characters were actually present.
        got: usize,
    },
    /// Non-hex characters present.
    #[error("not valid hex")]
    NotHex,
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{HASH_PREFIX}{}", self.to_hex())
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{HASH_PREFIX}{}…", self.short())
    }
}

impl Serialize for Hash {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Hash {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Hash::parse(&raw)
            .map_err(|_| D::Error::invalid_value(Unexpected::Str(&raw), &"a blake3 address"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vector_matches_blake3_reference() {
        // BLAKE3 of "abc", from the reference test vectors.
        let h = Hash::of(b"abc");
        assert_eq!(h.to_hex(), "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85");
    }

    #[test]
    fn empty_input_matches_blake3_reference() {
        assert_eq!(
            Hash::of(b"").to_hex(),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }

    #[test]
    fn length_prefixing_prevents_field_shifting() {
        let a = Hash::of_tagged("t", &[b"ab", b"c"]);
        let b = Hash::of_tagged("t", &[b"a", b"bc"]);
        assert_ne!(a, b, "field boundaries must be part of the digest");
    }

    #[test]
    fn domain_tag_separates_otherwise_identical_input() {
        let a = Hash::of_tagged("entry", &[b"x"]);
        let b = Hash::of_tagged("blob", &[b"x"]);
        assert_ne!(a, b);
    }

    #[test]
    fn roundtrips_through_text_and_json() {
        let h = Hash::of(b"warrant");
        assert_eq!(Hash::parse(&h.to_string()).unwrap(), h);
        assert_eq!(Hash::parse(&h.to_hex()).unwrap(), h);

        let json = serde_json::to_string(&h).unwrap();
        assert!(json.contains(HASH_PREFIX));
        assert_eq!(serde_json::from_str::<Hash>(&json).unwrap(), h);
    }

    #[test]
    fn rejects_malformed_addresses() {
        assert!(Hash::parse("blake3:zz").is_err());
        assert!(Hash::parse(&"g".repeat(64)).is_err());
    }
}
