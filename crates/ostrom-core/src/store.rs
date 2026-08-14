use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{RepositoryName, Verdict};

pub const STORE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PassId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AttemptOutcome {
    Completed,
    Failed,
    NoOp,
    TimedOut,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PassAttempt {
    pub schema_version: u32,
    pub pass_id: PassId,
    pub started_at: String,
    pub outcome: AttemptOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QueueKind {
    Tripwire,
    Decision,
    Moved,
    Stuck,
    Drift,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QueueState {
    Pending,
    Approved,
    Deferred,
}

/// The portable queue projection contains facts only. The local compatibility
/// adapter may retain the Bash queue's `mandate.reason`, but it cannot cross
/// this interface because there is nowhere to put it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueFact {
    pub id: String,
    pub repo: RepositoryName,
    pub reference: String,
    pub title: String,
    pub kind: QueueKind,
    pub state: QueueState,
    pub opened: String,
    pub blocked_by: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateFact {
    pub pull_request: String,
    pub head_sha: Option<String>,
    pub verdict: Verdict,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoStateFact {
    pub repo: RepositoryName,
    pub cursor: Option<String>,
    pub selector_hash: String,
    pub unclassified: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SweepPass {
    pub attempt: PassAttempt,
    pub queue: Vec<QueueFact>,
    pub gates: Vec<GateFact>,
    pub states: Vec<RepoStateFact>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteDisposition {
    Written,
    Unchanged,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum StoreFault {
    #[error("store record uses an unsupported schema version")]
    UnsupportedSchema,
    #[error("attempt record write failed")]
    AttemptWrite,
    #[error("pass payload write failed")]
    PayloadWrite,
    #[error("store read failed")]
    Read,
    #[error("pass identifier was reused with different content")]
    PassConflict,
    #[error("store contains a malformed record")]
    MalformedRecord,
}

/// The substrate-neutral write port used by the future Rust sweep.
///
/// `write_pass` must persist the attempt even when its outcome is `Failed`,
/// and must be idempotent by `pass_id`. Successful payload visibility is
/// atomic from the reader's perspective. The trait uses no paths, handles,
/// byte streams, or substrate-specific errors by design.
#[async_trait]
pub trait SweepStore: Send {
    async fn write_pass(&mut self, pass: &SweepPass) -> Result<WriteDisposition, StoreFault>;

    async fn passes(&self) -> Result<Vec<SweepPass>, StoreFault>;
}

#[cfg(test)]
mod architecture_tests {
    #[test]
    fn public_store_port_has_no_filesystem_or_io_types() {
        let source = include_str!("store.rs");
        let forbidden_types = [
            ["std", "path", "Path"].join("::"),
            ["std", "path", "PathBuf"].join("::"),
            ["std", "io", "Error"].join("::"),
        ];
        for forbidden in forbidden_types {
            assert!(
                !source.contains(&forbidden),
                "store trait invariant violated: public port contains {forbidden}"
            );
        }
    }
}
