//! Raw harness capture and golden-test scaffolding.
//!
//! This crate translates streams; it does not classify, score, or judge what
//! their contents mean.

pub mod claude;
mod fault;
pub mod golden;
mod source;

use ethogram::EventDraft;

pub use fault::CaptureFault;
pub use source::{ChildStdoutSource, FileLineSource, LineSource, SliceLineSource};

/// A stateful translation from raw harness lines into ethogram drafts.
pub trait Normaliser {
    /// Consume one raw line. Returns zero or more drafts.
    fn line(&mut self, raw: &str) -> Result<Vec<EventDraft>, CaptureFault>;

    /// The stream ended. Returns any drafts that implies.
    fn finish(&mut self) -> Result<Vec<EventDraft>, CaptureFault>;
}
