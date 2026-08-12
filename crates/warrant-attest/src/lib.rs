//! L4 — the attestor.
//!
//! A proof is declared before any tool executes, compiled to a sealed
//! WebAssembly module, hashed into the ledger, and from that moment it is
//! beyond the agent's reach: unreadable, unmodifiable, and returning one bit.
//!
//! Three properties, each with a reason:
//!
//! - **Deterministic.** The module has no memory, no globals, no loops and no
//!   clock. Everything it can observe arrives through four host functions.
//! - **Hashable.** The constant table and the original text travel inside the
//!   module as custom sections, so the bytes are a complete description of
//!   what is checked. A third party with the ledger can re-run the proof
//!   without trusting anything the record says about it.
//! - **Opaque.** [`Predicate`] deliberately does not implement
//!   `ContextRenderable`, so there is no path by which the proof's text
//!   reaches a model's context window.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod ast;
mod attestor;
mod compile;
mod env;
mod error;
mod parse;
mod predicate;

pub use attestor::Attestor;
pub use compile::{
    CONSTANTS_SECTION, ENTRY_POINT, HOST_MODULE, SOURCE_SECTION, compile, read_constants,
    read_source,
};
pub use env::{CellEnvironment, ProbeEnvironment, ScriptedEnvironment, TIMEOUT_EXIT_CODE};
pub use error::{AttestError, Result};
pub use parse::{Parsed, parse};
pub use predicate::Predicate;
