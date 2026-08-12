//! Projections.
//!
//! A session is not an object in Warrant; it is a fold over the log. Replay,
//! fork, resume and audit are the same mechanism viewed from different
//! angles, which is why none of them needed to be designed separately.

use crate::entry::Entry;
use crate::error::Result;

/// A fold over the log.
pub trait Projection {
    /// What the fold produces.
    type Out;

    /// Observe one entry together with its payload bytes.
    fn observe(&mut self, entry: &Entry, payload: &[u8]) -> Result<()>;

    /// Produce the result.
    fn finish(self) -> Self::Out;
}

/// One replayed event: the sealed header and the exact payload bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplayedEvent {
    /// The entry header, as recorded.
    pub entry: Entry,
    /// The payload, byte for byte as it was appended.
    pub payload: Vec<u8>,
}

/// Byte-exact replay of the whole log.
///
/// This is the phase-one gate: every event replays byte-identically from the
/// ledger alone, with no reference to the process that produced it.
#[derive(Debug, Default)]
pub struct Replay {
    events: Vec<ReplayedEvent>,
}

impl Replay {
    /// A fresh replay.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Projection for Replay {
    type Out = Vec<ReplayedEvent>;

    fn observe(&mut self, entry: &Entry, payload: &[u8]) -> Result<()> {
        self.events.push(ReplayedEvent { entry: *entry, payload: payload.to_vec() });
        Ok(())
    }

    fn finish(self) -> Self::Out {
        self.events
    }
}

/// A projection built from a closure, for one-off queries.
pub struct Fold<S, F> {
    state: S,
    step: F,
}

impl<S, F> Fold<S, F>
where
    F: FnMut(&mut S, &Entry, &[u8]) -> Result<()>,
{
    /// Fold the log into `initial` using `step`.
    pub fn new(initial: S, step: F) -> Self {
        Fold { state: initial, step }
    }
}

impl<S, F> Projection for Fold<S, F>
where
    F: FnMut(&mut S, &Entry, &[u8]) -> Result<()>,
{
    type Out = S;

    fn observe(&mut self, entry: &Entry, payload: &[u8]) -> Result<()> {
        (self.step)(&mut self.state, entry, payload)
    }

    fn finish(self) -> Self::Out {
        self.state
    }
}
