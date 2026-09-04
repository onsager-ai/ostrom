use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::{CheckReceipt, RepositoryName, Verdict};

pub const STORE_SCHEMA_VERSION: u32 = 1;
pub const CHECK_STORE_SCHEMA_VERSION: u32 = 1;
pub const EVENT_VERSION: u32 = 1;

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

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CheckRunId(pub String);

/// One durable executor pass over zero or more checks.
///
/// The run record is the transaction boundary. In particular, an empty
/// `receipts` collection is still persisted so callers can distinguish an
/// executor run that selected nothing from an executor that never ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckRun {
    pub schema_version: u32,
    pub run_id: CheckRunId,
    pub completed_at: String,
    pub receipts: Vec<CheckReceipt>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CheckStoreFault {
    #[error("check store record uses an unsupported schema version")]
    UnsupportedSchema,
    #[error("check run write failed")]
    RunWrite,
    #[error("check run payload write failed")]
    PayloadWrite,
    #[error("check store read failed")]
    Read,
    #[error("check run identifier was reused with different content")]
    RunConflict,
    #[error("check store contains a malformed record")]
    MalformedRecord,
}

/// Substrate-neutral persistence boundary for out-of-band check execution.
///
/// Implementations use [`CheckRun::run_id`] as the idempotency key. An exact
/// retry returns [`WriteDisposition::Unchanged`], while different content
/// under the same id returns [`CheckStoreFault::RunConflict`].
#[async_trait]
pub trait CheckStore: Send {
    async fn write_run(&mut self, run: &CheckRun) -> Result<WriteDisposition, CheckStoreFault>;

    async fn runs(&self) -> Result<Vec<CheckRun>, CheckStoreFault>;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventRunId(pub String);

#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("event type must use the domain.past_tense namespace")]
pub struct EventTypeFault;

/// An open event type using the `domain.past_tense` naming convention.
///
/// The type is deliberately not an enum: consumers must retain event types
/// introduced by a newer producer even when they do not yet interpret them.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct EventType(String);

impl EventType {
    pub fn new(value: impl Into<String>) -> Result<Self, EventTypeFault> {
        let value = value.into();
        let mut parts = value.split('.');
        let domain = parts.next().unwrap_or_default();
        let past_tense = parts.next().unwrap_or_default();
        if parts.next().is_some()
            || !valid_event_type_part(domain)
            || !valid_event_type_part(past_tense)
        {
            return Err(EventTypeFault);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn valid_event_type_part(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

impl<'de> Deserialize<'de> for EventType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("event payload contains a narration field")]
pub struct EventPayloadFault;

/// A fact-only event payload.
///
/// The map is private so every construction and deserialization path enforces
/// the same structural boundary. In particular, narration cannot be added by
/// extending a permissive JSON object at an adapter boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct EventPayload(Map<String, Value>);

impl EventPayload {
    pub fn new(facts: Map<String, Value>) -> Result<Self, EventPayloadFault> {
        reject_narration_fields(&Value::Object(facts.clone()))?;
        Ok(Self(facts))
    }

    #[must_use]
    pub fn empty() -> Self {
        Self(Map::new())
    }

    #[must_use]
    pub fn as_map(&self) -> &Map<String, Value> {
        &self.0
    }
}

impl<'de> Deserialize<'de> for EventPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let facts = Map::<String, Value>::deserialize(deserializer)?;
        Self::new(facts).map_err(serde::de::Error::custom)
    }
}

fn reject_narration_fields(value: &Value) -> Result<(), EventPayloadFault> {
    const NARRATION_FIELDS: &[&str] = &[
        "detail",
        "dossier",
        "error",
        "message",
        "narration",
        "operator_reason",
        "prompt",
        "reason",
        "tool_output",
    ];
    match value {
        Value::Array(values) => {
            for value in values {
                reject_narration_fields(value)?;
            }
        }
        Value::Object(object) => {
            for (key, value) in object {
                if NARRATION_FIELDS.contains(&key.as_str()) {
                    return Err(EventPayloadFault);
                }
                reject_narration_fields(value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Producer-facing event input. Sequence, version, run identity, and time are
/// store-owned envelope metadata and therefore have no representation here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventInput {
    #[serde(rename = "type")]
    pub event_type: EventType,
    pub payload: EventPayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventEnvelope {
    pub v: u32,
    #[serde(rename = "type")]
    pub event_type: EventType,
    pub run_id: EventRunId,
    pub seq: u64,
    pub ts: String,
    pub payload: EventPayload,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum EventStoreFault {
    #[error("event store record uses an unsupported version")]
    UnsupportedVersion,
    #[error("event record write failed")]
    EventWrite,
    #[error("event payload write failed")]
    PayloadWrite,
    #[error("event store read failed")]
    Read,
    #[error("event store contains a malformed record")]
    MalformedRecord,
}

/// Substrate-neutral append port for ordered lifecycle facts.
///
/// Implementations own the envelope metadata and assign a gap-detectable
/// sequence within each run. Replaying an identical [`EventInput`] in the
/// same run is a no-op and returns [`WriteDisposition::Unchanged`]. Event type
/// strings remain open so a reader retains facts it does not yet understand.
#[async_trait]
pub trait EventStore: Send {
    async fn write_event(
        &mut self,
        event: &EventInput,
    ) -> Result<WriteDisposition, EventStoreFault>;

    async fn events(&self) -> Result<Vec<EventEnvelope>, EventStoreFault>;
}

/// Gate records supplied for publication, including the source's ownership
/// assertion for that record kind.
///
/// `Authoritative(Vec::new())` means the source owns gate publication and has
/// observed no gate records. [`Self::NotAuthoritative`] means the source does
/// not own gate publication, so consumers must not infer anything from the
/// absence of records. Keeping the assertion and records in one enum prevents
/// an empty authoritative collection from being represented as a missing
/// optional value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatePublicationRecords {
    Authoritative(Vec<Value>),
    NotAuthoritative,
}

/// One substrate-neutral input snapshot for publication.
///
/// Queue and state are required publication inputs. Gate records carry an
/// explicit ownership assertion because a publisher that does not own that
/// record kind must preserve already-published gate history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationSnapshot {
    pub queue: Vec<Value>,
    pub gate: GatePublicationRecords,
    pub state: Value,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PublicationSourceFault {
    #[error("publication source record is missing: {0}")]
    QueueMissing(String),
    #[error("could not prepare publication path {0}")]
    QueueRead(String),
    #[error("invalid publication record at {0}")]
    QueueMalformed(String),
    #[error("could not prepare publication path {0}")]
    GateRead(String),
    #[error("invalid publication record at {0}")]
    GateMalformed(String),
    #[error("publication source record is missing: {0}")]
    StateMissing(String),
    #[error("could not prepare publication path {0}")]
    StateRead(String),
    #[error("invalid publication record at {0}")]
    StateMalformed(String),
}

/// Substrate-neutral read port for the records consumed by publication.
///
/// Implementations return values rather than paths, handles, or byte streams.
/// In particular, the gate ownership assertion is part of every successful
/// snapshot and cannot be inferred by the consumer from an empty collection.
pub trait PublicationSource: Send {
    fn snapshot(&self) -> Result<PublicationSnapshot, PublicationSourceFault>;
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{
        AttemptOutcome, EventEnvelope, EventInput, EventPayload, EventRunId, EventType, GateFact,
        PassAttempt, PassId, QueueFact, QueueKind, QueueState, RepoStateFact, SweepPass,
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
        for store_trait in [
            "SweepStore",
            "CheckStore",
            "EventStore",
            "PublicationSource",
        ] {
            let trait_source = source
                .split_once(&format!("pub trait {store_trait}"))
                .expect("store trait declaration")
                .1
                .split_once("\n}")
                .expect("store trait body")
                .0;
            for forbidden in ["std::io", "std::path", "PathBuf", "Path", "IoSlice"] {
                assert!(
                    !trait_source.contains(forbidden),
                    "{store_trait} signature contains forbidden I/O type {forbidden}"
                );
            }
        }
    }

    #[test]
    fn event_input_has_no_sequence_or_narration_channel() {
        let input = EventInput {
            event_type: EventType::new("work.completed").expect("valid type"),
            payload: EventPayload::new(serde_json::Map::from_iter([(
                "order_id".to_owned(),
                json!("synthetic-order"),
            )]))
            .expect("fact-only payload"),
        };
        assert_eq!(
            serde_json::to_value(&input).expect("input serializes"),
            json!({"type": "work.completed", "payload": {"order_id": "synthetic-order"}})
        );

        for forbidden in [
            "seq",
            "detail",
            "narration",
            "reason",
            "prompt",
            "tool_output",
        ] {
            let mut injected = serde_json::to_value(&input).expect("input serializes");
            if forbidden == "seq" {
                injected[forbidden] = json!(7);
            } else {
                injected["payload"][forbidden] = Value::Null;
            }
            assert!(
                serde_json::from_value::<EventInput>(injected).is_err(),
                "producer input must reject {forbidden}"
            );
        }
    }

    #[test]
    fn unknown_event_type_is_retained_and_required_fields_are_required() {
        let json = json!({
            "v": 1,
            "type": "future.observed",
            "run_id": "synthetic-run",
            "seq": 8,
            "ts": "2030-01-02T03:04:05Z",
            "payload": {"artifact_id": "synthetic-artifact"}
        });
        let parsed: EventEnvelope =
            serde_json::from_value(json.clone()).expect("unknown type remains portable");
        assert_eq!(parsed.event_type.as_str(), "future.observed");
        assert_eq!(
            serde_json::to_value(parsed).expect("envelope serializes"),
            json
        );

        let missing = json!({
            "v": 1,
            "type": "future.observed",
            "run_id": EventRunId("synthetic-run".to_owned()),
            "ts": "2030-01-02T03:04:05Z",
            "payload": {}
        });
        assert!(serde_json::from_value::<EventEnvelope>(missing).is_err());
    }
}
