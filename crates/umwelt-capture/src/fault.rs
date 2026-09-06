use thiserror::Error;

/// A refusal to continue capturing a raw stream.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum CaptureFault {
    /// A raw line is malformed for the normaliser consuming it.
    #[error("malformed raw line {line}: {reason}")]
    MalformedLine {
        /// One-based position of the line in the raw stream.
        line: u64,
        /// Why the line is malformed.
        reason: String,
    },

    /// A line source could not be opened or read.
    #[error("cannot read line source {origin}: {reason}")]
    UnreadableSource {
        /// Human-readable source identity, such as a path or `child stdout`.
        origin: String,
        /// The underlying I/O failure.
        reason: String,
    },

    /// A normaliser understood enough input to reject it deliberately.
    #[error("normaliser rejected raw input: {reason}")]
    NormaliserRejected {
        /// Why the input is not accepted.
        reason: String,
    },
}
