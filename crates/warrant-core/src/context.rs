//! What is allowed to enter a model's context.
//!
//! Invariant 3 of the architecture record: the necessity map is never an
//! input to the agent. Proof coverage is a number for a human to read. Feed
//! it back to the agent and it becomes the next proxy to saturate — which is
//! the whole finding of SpecBench (arXiv 2605.21384).
//!
//! The enforcement is a capability, not a convention. Anything entering
//! context must implement [`ContextRenderable`]; the types that must not
//! enter simply do not implement it, and the compiler refuses the call.

/// Types that may be rendered into a model's context window.
///
/// Deliberately *not* implemented for `NecessityMap`, `Receipt`, or any type
/// carrying coverage. Adding such an impl is the one edit that would break
/// invariant 3, and it is a visible, reviewable line of code rather than a
/// silent field addition.
pub trait ContextRenderable {
    /// The exact text handed to the model.
    fn render_for_model(&self) -> String;
}

impl ContextRenderable for str {
    fn render_for_model(&self) -> String {
        self.to_owned()
    }
}

impl ContextRenderable for String {
    fn render_for_model(&self) -> String {
        self.clone()
    }
}

/// An ordered set of frames destined for a model.
///
/// The only way to add material is [`ModelContext::push`], which is generic
/// over [`ContextRenderable`]. There is no `push_raw`, and adding one would
/// be the point at which invariant 3 stopped holding.
#[derive(Clone, Debug, Default)]
pub struct ModelContext {
    frames: Vec<String>,
}

impl ModelContext {
    /// An empty context.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a renderable value.
    pub fn push<T: ContextRenderable + ?Sized>(&mut self, value: &T) -> &mut Self {
        self.frames.push(value.render_for_model());
        self
    }

    /// The frames, in order.
    pub fn frames(&self) -> &[String] {
        &self.frames
    }

    /// Render the whole context.
    pub fn render(&self) -> String {
        self.frames.join("\n\n")
    }

    /// Number of frames.
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// Whether anything has been pushed.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ReceiptRef;
    use crate::verdict::Verdict;

    #[test]
    fn a_verdict_may_enter_context_and_arrives_as_one_word() {
        let mut ctx = ModelContext::new();
        ctx.push("the task").push(&Verdict::Warranted { receipt: ReceiptRef::derive(&[b"r"]) });
        assert_eq!(ctx.frames(), ["the task", "warranted"]);
    }
}
