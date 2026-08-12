//! The one bit.
//!
//! A verdict is the entire result an agent receives from attestation. It has
//! two inhabitants and carries no score, no confidence and no coverage
//! figure. This is not stylistic: revealing a numeric score to the party
//! being scored makes the score the next optimisation target, and the
//! collapse that follows is measured, not theorised (SEAL, arXiv 2607.24300).

use serde::{Deserialize, Serialize};

use crate::context::ContextRenderable;
use crate::ids::ReceiptRef;

/// The result of discharging a claim.
///
/// `Warranted` carries a content address, not a receipt. The receipt itself
/// is human- and third-party-facing and is fetched from the ledger by the
/// review plane; it never travels back along the path the agent can read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "lowercase")]
pub enum Verdict {
    /// The predicate held on the post-state, and the null test passed.
    Warranted {
        /// Where the full receipt can be found.
        receipt: ReceiptRef,
    },
    /// The predicate did not hold, or held vacuously.
    Unproven,
}

impl Verdict {
    /// The single bit.
    pub fn is_warranted(&self) -> bool {
        matches!(self, Verdict::Warranted { .. })
    }

    /// Where the receipt lives, if there is one.
    pub fn receipt(&self) -> Option<ReceiptRef> {
        match self {
            Verdict::Warranted { receipt } => Some(*receipt),
            Verdict::Unproven => None,
        }
    }
}

impl ContextRenderable for Verdict {
    /// What the agent sees. One word, chosen from two.
    fn render_for_model(&self) -> String {
        if self.is_warranted() { "warranted".into() } else { "unproven".into() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn contains_number(v: &Value) -> bool {
        match v {
            Value::Number(_) => true,
            Value::Array(items) => items.iter().any(contains_number),
            Value::Object(map) => map.values().any(contains_number),
            _ => false,
        }
    }

    /// Invariant 2 from the architecture record: a verdict carries no numeric
    /// field, ever. Enforced here rather than in review, because "relax it
    /// just for logging" is exactly how it would go.
    #[test]
    fn no_verdict_serialisation_contains_a_number() {
        let cases =
            [Verdict::Unproven, Verdict::Warranted { receipt: ReceiptRef::derive(&[b"r"]) }];
        for verdict in cases {
            let json: Value = serde_json::to_value(verdict).unwrap();
            assert!(!contains_number(&json), "verdict leaked a numeric field: {json}");
        }
    }

    #[test]
    fn what_the_model_sees_is_one_of_exactly_two_strings() {
        let warranted = Verdict::Warranted { receipt: ReceiptRef::derive(&[b"r"]) };
        assert_eq!(warranted.render_for_model(), "warranted");
        assert_eq!(Verdict::Unproven.render_for_model(), "unproven");
    }

    /// The receipt address must not reach the model even though the verdict
    /// holds one.
    #[test]
    fn the_receipt_address_is_not_rendered_into_context() {
        let receipt = ReceiptRef::derive(&[b"secret-ish"]);
        let rendered = Verdict::Warranted { receipt }.render_for_model();
        assert!(!rendered.contains(&receipt.short()));
    }
}
