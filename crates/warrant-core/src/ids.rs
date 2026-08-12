//! Identifiers.
//!
//! Every id is derived from the content it names. There are no counters and
//! no random ids in the evidence path: two runs that produced the same hunk
//! produce the same [`HunkId`], which is what makes probe results cacheable
//! and cross-run comparison meaningful.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::hash::{Hash, HashParseError};

macro_rules! content_id {
    ($(#[$meta:meta])* $name:ident, $tag:literal) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Hash);

        impl $name {
            /// Domain tag mixed into every digest of this kind.
            pub const TAG: &'static str = $tag;

            /// Derive from the length-prefixed fields that define this value.
            pub fn derive(parts: &[&[u8]]) -> Self {
                $name(Hash::of_tagged(Self::TAG, parts))
            }

            /// Wrap an address that was already derived under this tag.
            pub const fn from_hash(hash: Hash) -> Self {
                $name(hash)
            }

            /// The underlying address.
            pub const fn hash(&self) -> Hash {
                self.0
            }

            /// Short form for terminal output.
            pub fn short(&self) -> String {
                self.0.short()
            }

            /// Parse from `blake3:<hex>` or bare hex.
            pub fn parse(s: &str) -> Result<Self, HashParseError> {
                Hash::parse(s).map($name)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({}…)", stringify!($name), self.0.short())
            }
        }

        impl AsRef<Hash> for $name {
            fn as_ref(&self) -> &Hash {
                &self.0
            }
        }
    };
}

content_id!(
    /// One invocation of Warrant against a repository.
    RunId,
    "warrant.run.v1"
);

content_id!(
    /// A claim, identified by its assertion, its sealed predicate and the
    /// instant it was declared. Two identical declarations at different times
    /// are different claims.
    ClaimId,
    "warrant.claim.v1"
);

content_id!(
    /// An isolation boundary in which an agent or a probe executed.
    CellId,
    "warrant.cell.v1"
);

content_id!(
    /// A contiguous group of changed lines in one file.
    HunkId,
    "warrant.hunk.v1"
);

content_id!(
    /// The compiled predicate module. Sealed at declaration time; the agent
    /// never sees the bytes behind it.
    PredicateHash,
    "warrant.predicate.v1"
);

content_id!(
    /// A pointer to a receipt stored in the ledger.
    ///
    /// A verdict carries this rather than the receipt itself, so the value
    /// returned to an agent is an opaque address and nothing else.
    ReceiptRef,
    "warrant.receipt.v1"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derivation_is_deterministic() {
        let a = HunkId::derive(&[b"src/lib.rs", b"12", b"+x"]);
        let b = HunkId::derive(&[b"src/lib.rs", b"12", b"+x"]);
        assert_eq!(a, b);
    }

    #[test]
    fn different_id_kinds_never_collide_on_the_same_input() {
        let parts: &[&[u8]] = &[b"identical"];
        assert_ne!(
            HunkId::derive(parts).hash(),
            ClaimId::derive(parts).hash(),
            "domain tags must keep id spaces disjoint"
        );
        assert_ne!(RunId::derive(parts).hash(), CellId::derive(parts).hash());
    }

    #[test]
    fn roundtrips_through_text() {
        let id = ClaimId::derive(&[b"add rate limiting"]);
        assert_eq!(ClaimId::parse(&id.to_string()).unwrap(), id);
    }
}
