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
#[serde(deny_unknown_fields)]
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

/// The portable queue projection contains facts and Ostrom's classifications.
/// Narration such as `mandate.reason` and `mandate.dossier` stays in the local
/// compatibility adapter. Values derivable from facts already here (such as
/// `age_days` and `aged_out` from `opened` plus a threshold) are omitted, but
/// judgments computed by Ostrom must cross the port so consumers can display
/// them without re-deriving them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueFact {
    pub id: String,
    pub repo: RepositoryName,
    pub reference: String,
    pub title: String,
    pub kind: QueueKind,
    pub state: QueueState,
    pub opened: String,
    pub blocked_by: Vec<String>,
    pub needs_judgment: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateFact {
    pub pull_request: String,
    pub head_sha: Option<String>,
    pub verdict: Verdict,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoStateFact {
    pub repo: RepositoryName,
    pub cursor: Option<String>,
    pub selector_hash: String,
    pub unclassified: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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

/// The substrate-neutral write port used by the sweep.
///
/// One call is the transaction boundary: the attempt and every queue, gate,
/// and repository-state fact in [`SweepPass`] become visible together or not
/// at all. An implementation must use `pass.attempt.pass_id` as its
/// idempotency key. Repeating byte-for-byte equivalent content returns
/// [`WriteDisposition::Unchanged`]; reusing the identifier for different
/// content returns [`StoreFault::PassConflict`].
///
/// A failed sweep is still a pass. In particular, a [`SweepPass`] whose
/// outcome is [`AttemptOutcome::Failed`] and whose fact collections are empty
/// **must be persisted**. An empty store means "the pass never ran"; it must
/// never also mean "the pass ran and produced nothing". This is an
/// observability invariant, not an implementation detail.
///
/// The trait uses no paths, handles, byte streams, or implementation-specific
/// errors by design. Implementations may perform asynchronous I/O internally,
/// but no I/O concept crosses this boundary.
#[async_trait]
pub trait SweepStore: Send {
    async fn write_pass(&mut self, pass: &SweepPass) -> Result<WriteDisposition, StoreFault>;

    async fn passes(&self) -> Result<Vec<SweepPass>, StoreFault>;
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{
        AttemptOutcome, GateFact, PassAttempt, PassId, QueueFact, QueueKind, QueueState,
        RepoStateFact, SweepPass,
    };
    use crate::{RepositoryName, Verdict};

    fn fixture() -> SweepPass {
        SweepPass {
            attempt: PassAttempt {
                schema_version: 1,
                pass_id: PassId("synthetic-pass".to_owned()),
                started_at: "2030-01-02T03:04:05Z".to_owned(),
                outcome: AttemptOutcome::Completed,
            },
            queue: vec![QueueFact {
                id: "synthetic-org/project#42".to_owned(),
                repo: RepositoryName::new("synthetic-org/project").expect("valid repository"),
                reference: "#42".to_owned(),
                title: "Synthetic source title".to_owned(),
                kind: QueueKind::Decision,
                state: QueueState::Pending,
                opened: "2030-01-01T00:00:00Z".to_owned(),
                blocked_by: vec!["synthetic-org/dependency#7".to_owned()],
                needs_judgment: true,
            }],
            gates: vec![GateFact {
                pull_request: "synthetic-org/project#43".to_owned(),
                head_sha: Some("0123456789abcdef".to_owned()),
                verdict: Verdict::Pass,
            }],
            states: vec![RepoStateFact {
                repo: RepositoryName::new("synthetic-org/project").expect("valid repository"),
                cursor: Some("2030-01-02T03:00:00Z".to_owned()),
                selector_hash: "synthetic-selector-hash".to_owned(),
                unclassified: 0,
            }],
        }
    }

    fn assert_no_narration_key(value: &Value) {
        const NARRATION_KEYS: &[&str] = &[
            "detail",
            "dossier",
            "error",
            "narration",
            "operator_reason",
            "prompt",
            "reason",
            "tool_output",
        ];
        match value {
            Value::Array(values) => values.iter().for_each(assert_no_narration_key),
            Value::Object(object) => {
                for (key, value) in object {
                    assert!(
                        !NARRATION_KEYS.contains(&key.as_str()),
                        "portable record exposed narration key {key}"
                    );
                    assert_no_narration_key(value);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn portable_record_shape_has_no_narration_channel() {
        let encoded = serde_json::to_value(fixture()).expect("portable record serializes");
        assert_no_narration_key(&encoded);

        // `deny_unknown_fields` makes the absence structural on input too: an
        // implementor cannot accidentally smuggle a newly named narration
        // field through a permissive core record and discover it only later.
        let mut injected = encoded;
        injected["queue"][0]["reason"] = json!("operator-facing explanation");
        assert!(
            serde_json::from_value::<SweepPass>(injected).is_err(),
            "core records must reject narration fields rather than discard them"
        );
    }

    #[test]
    fn store_trait_source_has_no_io_or_path_signature() {
        // Clippy's crate-level `disallowed_types` lint is the compile-time
        // guard. This focused test makes the interface invariant visible in a
        // normal `cargo test` run as well, without relying on code review.
        let source = include_str!("store.rs");
        let trait_source = source
            .split_once("pub trait SweepStore")
            .expect("store trait declaration")
            .1
            .split_once("\n}")
            .expect("store trait body")
            .0;
        for forbidden in ["std::io", "std::path", "PathBuf", "Path", "IoSlice"] {
            assert!(
                !trait_source.contains(forbidden),
                "store trait signature contains forbidden I/O type {forbidden}"
            );
        }
    }
}
