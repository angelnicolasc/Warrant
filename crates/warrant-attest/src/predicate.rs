//! A sealed proof.
//!
//! Note what this type does *not* implement: `warrant_core::ContextRenderable`.
//! There is consequently no way to render a proof into a model's context. The
//! agent declares it, the ledger stores it, the attestor runs it, and the
//! agent never reads it back.

use warrant_core::PredicateHash;

use crate::ast::Expr;
use crate::compile::{compile, read_constants, read_source};
use crate::error::Result;
use crate::parse::{Parsed, parse};

/// A proof compiled to WebAssembly and hashed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Predicate {
    hash: PredicateHash,
    wasm: Vec<u8>,
    constants: Vec<String>,
    source: String,
}

impl Predicate {
    /// Compile a proof from its source text.
    ///
    /// ```
    /// # use warrant_attest::Predicate;
    /// let proof = Predicate::compile("exit(pytest -q) == 0").unwrap();
    /// assert_eq!(proof.commands(), ["pytest -q"]);
    /// ```
    pub fn compile(source: &str) -> Result<Self> {
        let parsed = parse(source)?;
        let wasm = compile(&parsed, source)?;
        Ok(Predicate {
            hash: PredicateHash::derive(&[&wasm]),
            wasm,
            constants: parsed.constants,
            source: source.to_owned(),
        })
    }

    /// Recover a proof from the bytes recorded in the ledger.
    ///
    /// This is the third-party verification path: no trust in the record's
    /// description of the proof, only in the bytes it committed to.
    pub fn from_wasm(wasm: Vec<u8>) -> Result<Self> {
        let constants = read_constants(&wasm)?;
        let source = read_source(&wasm)?;
        Ok(Predicate { hash: PredicateHash::derive(&[&wasm]), wasm, constants, source })
    }

    /// The address this proof was sealed under.
    pub fn hash(&self) -> PredicateHash {
        self.hash
    }

    /// The module bytes.
    pub fn wasm(&self) -> &[u8] {
        &self.wasm
    }

    /// The proof as it was written, for receipts and review.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// The constant table.
    pub fn constants(&self) -> &[String] {
        &self.constants
    }

    /// Every command this proof may run.
    ///
    /// Not every command it *will* run — conjunction short-circuits, so a
    /// proof reports its worst case here.
    pub fn commands(&self) -> Vec<String> {
        let Ok(parsed) = parse(&self.source) else {
            return Vec::new();
        };
        parsed.expr.commands(&parsed.constants)
    }

    /// The part of this proof that can be checked without running anything.
    ///
    /// Returns the conjunction of the top-level conjuncts that contain no
    /// `exit()`, or `None` when every conjunct needs a command.
    ///
    /// **What this is sound for.** If the result evaluates false, the full
    /// proof is false *at this instant*, because a false conjunct makes a
    /// conjunction false. That is enough to prune a best-of-N branch cheaply.
    ///
    /// **What it is not.** It is not a proof of unsatisfiability: an agent can
    /// still put back a file it deleted. Pruning on it is a decision about
    /// where to spend compute, and the map is always recomputed from the real
    /// proof before anything is reported.
    pub fn structural_only(&self) -> Result<Option<Predicate>> {
        let parsed = parse(&self.source)?;
        let kept: Vec<Expr> = parsed
            .expr
            .conjuncts()
            .into_iter()
            .filter(|conjunct| !conjunct.runs_commands())
            .cloned()
            .collect();

        let mut parts = kept.into_iter();
        let Some(first) = parts.next() else { return Ok(None) };
        let expr = parts.fold(first, |acc, next| Expr::And(Box::new(acc), Box::new(next)));

        // The constant table is carried over whole. Unused entries cost a few
        // bytes and keep every index in the sub-expression valid.
        let source = expr.render(&parsed.constants);
        let reduced = Parsed { expr, constants: parsed.constants };
        let wasm = compile(&reduced, &source)?;
        Ok(Some(Predicate {
            hash: PredicateHash::derive(&[&wasm]),
            wasm,
            constants: reduced.constants,
            source,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_proofs_seal_to_the_same_address() {
        let a = Predicate::compile("exit(pytest) == 0").unwrap();
        let b = Predicate::compile("exit(pytest) == 0").unwrap();
        assert_eq!(a.hash(), b.hash());
    }

    #[test]
    fn a_weakened_proof_is_a_different_proof() {
        let strict =
            Predicate::compile(r#"exit(pytest) == 0 AND NOT diff_touches("tests/**")"#).unwrap();
        let loose = Predicate::compile("exit(pytest) == 0").unwrap();
        assert_ne!(
            strict.hash(),
            loose.hash(),
            "dropping a clause must change the address the ledger recorded"
        );
    }

    #[test]
    fn whitespace_is_part_of_the_declaration() {
        // The source travels inside the module, so reformatting produces a
        // different address. That is the conservative direction: a claim
        // stays pinned to the exact text that was declared.
        let a = Predicate::compile("exit(pytest) == 0").unwrap();
        let b = Predicate::compile("exit(pytest)  ==  0").unwrap();
        assert_ne!(a.hash(), b.hash());
    }

    #[test]
    fn a_proof_survives_a_round_trip_through_its_bytes() {
        let original =
            Predicate::compile(r#"exit(cargo test) == 0 AND diff_touches("src/**")"#).unwrap();
        let recovered = Predicate::from_wasm(original.wasm().to_vec()).unwrap();

        assert_eq!(recovered.hash(), original.hash());
        assert_eq!(recovered.source(), original.source());
        assert_eq!(recovered.constants(), original.constants());
    }

    #[test]
    fn the_structural_part_of_a_proof_drops_the_clauses_that_run_commands() {
        let proof = Predicate::compile(
            r#"exit(pytest) == 0 AND diff_touches("src/**") AND NOT diff_touches("tests/**")"#,
        )
        .unwrap();

        let structural = proof.structural_only().unwrap().unwrap();
        assert!(structural.commands().is_empty(), "nothing may run: {:?}", structural.commands());
        assert!(structural.source().contains("src/**"));
        assert!(structural.source().contains("tests/**"));
        assert!(!structural.source().contains("pytest"));
    }

    #[test]
    fn a_proof_that_is_only_a_command_has_no_structural_part() {
        let proof = Predicate::compile("exit(cargo test) == 0").unwrap();
        assert!(proof.structural_only().unwrap().is_none());
    }

    /// Only `AND` is split. Taking a disjunction apart would turn a sound
    /// check into a wrong one, because a false disjunct proves nothing.
    #[test]
    fn a_disjunction_is_left_whole_and_therefore_dropped() {
        let proof =
            Predicate::compile(r#"exit(cargo test) == 0 OR diff_touches("src/**")"#).unwrap();
        assert!(proof.structural_only().unwrap().is_none());
    }

    #[test]
    fn the_structural_part_is_a_different_proof_with_its_own_address() {
        let proof = Predicate::compile(r#"exit(pytest) == 0 AND diff_touches("src/**")"#).unwrap();
        let structural = proof.structural_only().unwrap().unwrap();
        assert_ne!(structural.hash(), proof.hash());
    }

    #[test]
    fn commands_are_reported_for_review_before_anything_runs() {
        let proof = Predicate::compile(
            r#"exit(cargo build) == 0 AND exit(cargo test) == 0 AND NOT diff_touches("tests/**")"#,
        )
        .unwrap();
        assert_eq!(proof.commands(), ["cargo build", "cargo test"]);
    }
}
