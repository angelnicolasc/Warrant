//! Claims: what the agent said it would achieve, and how it agreed to be judged.
//!
//! A claim is declared *before* any tool executes and is immutable from that
//! moment. The predicate is present only as a hash — the bytes live in the
//! ledger and are never handed back to the agent.

use serde::{Deserialize, Serialize};

use crate::ids::{ClaimId, PredicateHash};

/// How strong a proof the claim is judged by.
///
/// The agent proposes a tier; the attestor may demand a higher one for
/// claims that touch authentication, delete files, or open egress.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProofTier {
    /// The change parses and typechecks.
    Syntactic,
    /// A unit suite passes.
    Unit,
    /// An integration suite passes.
    Integration,
    /// Behaviour is compared against a reference implementation or prior build.
    Differential,
}

impl ProofTier {
    /// Every tier, weakest first.
    pub const ALL: [ProofTier; 4] =
        [ProofTier::Syntactic, ProofTier::Unit, ProofTier::Integration, ProofTier::Differential];

    /// Lowercase name, as it appears in the ledger and on the terminal.
    pub fn name(&self) -> &'static str {
        match self {
            ProofTier::Syntactic => "syntactic",
            ProofTier::Unit => "unit",
            ProofTier::Integration => "integration",
            ProofTier::Differential => "differential",
        }
    }

    /// Whether this tier satisfies a demand for `required`.
    pub fn satisfies(&self, required: ProofTier) -> bool {
        *self >= required
    }
}

/// Limits on a claim's execution. `None` means unbounded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    /// Wall-clock ceiling for the agent's work, in milliseconds.
    pub wall_ms: Option<u64>,
    /// Ceiling on necessity probes. The search degrades to file-level
    /// granularity rather than exceeding it.
    pub probes: Option<u32>,
    /// Ceiling on model tokens.
    pub tokens: Option<u64>,
}

impl Budget {
    /// An unbounded budget.
    pub const UNLIMITED: Budget = Budget { wall_ms: None, probes: None, tokens: None };

    /// Cap the number of necessity probes.
    pub fn with_probes(mut self, probes: u32) -> Self {
        self.probes = Some(probes);
        self
    }

    /// Cap wall-clock time.
    pub fn with_wall_ms(mut self, wall_ms: u64) -> Self {
        self.wall_ms = Some(wall_ms);
        self
    }
}

/// A pre-registered claim.
///
/// Construction is deliberately the only way to obtain a [`ClaimId`]: the id
/// is derived from the assertion, the sealed predicate and the declaration
/// instant, so a claim cannot be silently re-pointed at a different predicate
/// after the fact without changing its identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    /// Content address of this claim.
    pub id: ClaimId,
    /// What the agent said it would achieve, in its own words.
    pub assertion: String,
    /// Address of the compiled predicate module.
    pub proof: PredicateHash,
    /// Tier the claim is judged at.
    pub tier: ProofTier,
    /// Execution limits.
    pub budget: Budget,
    /// Milliseconds since the Unix epoch at declaration.
    pub declared_at_ms: u64,
}

impl Claim {
    /// Declare a claim. Call this before any tool executes.
    pub fn declare(
        assertion: impl Into<String>,
        proof: PredicateHash,
        tier: ProofTier,
        budget: Budget,
        declared_at_ms: u64,
    ) -> Self {
        let assertion = assertion.into();
        let id = ClaimId::derive(&[
            assertion.as_bytes(),
            proof.hash().as_bytes(),
            tier.name().as_bytes(),
            &declared_at_ms.to_le_bytes(),
        ]);
        Claim { id, assertion, proof, tier, budget, declared_at_ms }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn predicate(seed: &[u8]) -> PredicateHash {
        PredicateHash::derive(&[seed])
    }

    #[test]
    fn tier_ordering_is_weakest_first() {
        assert!(ProofTier::Syntactic < ProofTier::Unit);
        assert!(ProofTier::Unit < ProofTier::Integration);
        assert!(ProofTier::Integration < ProofTier::Differential);
        assert!(ProofTier::Differential.satisfies(ProofTier::Unit));
        assert!(!ProofTier::Syntactic.satisfies(ProofTier::Unit));
    }

    #[test]
    fn swapping_the_predicate_changes_the_claim_identity() {
        let a = Claim::declare("x", predicate(b"p1"), ProofTier::Unit, Budget::UNLIMITED, 1000);
        let b = Claim::declare("x", predicate(b"p2"), ProofTier::Unit, Budget::UNLIMITED, 1000);
        assert_ne!(a.id, b.id, "a claim cannot be re-pointed at another predicate silently");
    }

    #[test]
    fn identical_declarations_at_different_times_are_different_claims() {
        let a = Claim::declare("x", predicate(b"p"), ProofTier::Unit, Budget::UNLIMITED, 1);
        let b = Claim::declare("x", predicate(b"p"), ProofTier::Unit, Budget::UNLIMITED, 2);
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn declaration_is_reproducible() {
        let a = Claim::declare("x", predicate(b"p"), ProofTier::Unit, Budget::UNLIMITED, 7);
        let b = Claim::declare("x", predicate(b"p"), ProofTier::Unit, Budget::UNLIMITED, 7);
        assert_eq!(a, b);
    }
}
