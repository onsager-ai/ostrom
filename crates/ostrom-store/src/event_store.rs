use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::SystemTime,
};

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use ostrom_core::{
    EVENT_VERSION, EventEnvelope, EventInput, EventPayload, EventRunId, EventStore,
    EventStoreFault, EventType, WriteDisposition,
};
use serde_json::{Map, Value};

use crate::{OstromPaths, set_private_file_mode};

/// Append-only JSONL implementation of the ordered event contract.
///
/// The store is scoped to one run. It owns envelope metadata, including the
/// next sequence number, while producers submit only an event type and facts.
pub struct JsonlEventStore {
    journal: PathBuf,
    run_id: EventRunId,
}

impl JsonlEventStore {
    #[must_use]
    pub fn new(paths: &OstromPaths, run_id: EventRunId) -> Self {
        Self {
            journal: paths.event_journal_file(),
            run_id,
        }
    }

    fn at_path(journal: PathBuf, run_id: EventRunId) -> Self {
        Self { journal, run_id }
    }

    fn decode_record(line: &str) -> Result<EventEnvelope, EventStoreFault> {
        let event: EventEnvelope =
            serde_json::from_str(line).map_err(|_| EventStoreFault::MalformedRecord)?;
        if event.v != EVENT_VERSION {
            return Err(EventStoreFault::UnsupportedVersion);
        }
        if event.run_id.0.is_empty() || event.seq == 0 || event.ts.is_empty() {
            return Err(EventStoreFault::MalformedRecord);
        }
        Ok(event)
    }

    fn read_records(&self) -> Result<Vec<EventEnvelope>, EventStoreFault> {
        if !self.journal.exists() {
            return Ok(Vec::new());
        }
        let contents = fs::read_to_string(&self.journal).map_err(|_| EventStoreFault::Read)?;
        contents.lines().map(Self::decode_record).collect()
    }

    /// Synchronous append used by the existing synchronous CLI surfaces.
    ///
    /// This is the same implementation used by [`EventStore::write_event`].
    pub fn append_event(
        &mut self,
        event: &EventInput,
    ) -> Result<WriteDisposition, EventStoreFault> {
        let timestamp =
            DateTime::<Utc>::from(SystemTime::now()).to_rfc3339_opts(SecondsFormat::Secs, true);
        self.append_event_at(event, &timestamp)
    }

    pub(crate) fn append_event_at(
        &mut self,
        event: &EventInput,
        timestamp: &str,
    ) -> Result<WriteDisposition, EventStoreFault> {
        if self.run_id.0.is_empty() || timestamp.is_empty() {
            return Err(EventStoreFault::EventWrite);
        }
        let records = self.read_records()?;
        if records.iter().any(|stored| {
            stored.run_id == self.run_id
                && stored.event_type == event.event_type
                && stored.payload == event.payload
        }) {
            return Ok(WriteDisposition::Unchanged);
        }
        let next_sequence = records
            .iter()
            .filter(|stored| stored.run_id == self.run_id)
            .map(|stored| stored.seq)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(EventStoreFault::EventWrite)?;
        let envelope = EventEnvelope {
            v: EVENT_VERSION,
            event_type: event.event_type.clone(),
            run_id: self.run_id.clone(),
            seq: next_sequence,
            ts: timestamp.to_owned(),
            payload: event.payload.clone(),
        };
        let parent = self.journal.parent().ok_or(EventStoreFault::EventWrite)?;
        fs::create_dir_all(parent).map_err(|_| EventStoreFault::EventWrite)?;
        let mut bytes = serde_json::to_vec(&envelope).map_err(|_| EventStoreFault::PayloadWrite)?;
        bytes.push(b'\n');
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.journal)
            .map_err(|_| EventStoreFault::EventWrite)?;
        set_private_file_mode(&self.journal).map_err(|_| EventStoreFault::EventWrite)?;
        file.write_all(&bytes)
            .map_err(|_| EventStoreFault::PayloadWrite)?;
        Ok(WriteDisposition::Written)
    }
}

#[async_trait]
impl EventStore for JsonlEventStore {
    async fn write_event(
        &mut self,
        event: &EventInput,
    ) -> Result<WriteDisposition, EventStoreFault> {
        self.append_event(event)
    }

    async fn events(&self) -> Result<Vec<EventEnvelope>, EventStoreFault> {
        self.read_records()
    }
}

pub(crate) fn append_trace_event(
    trace_path: &Path,
    timestamp: &str,
    kind: &str,
    facts: &Map<String, Value>,
) -> Result<Option<WriteDisposition>, EventStoreFault> {
    let Some(event_type) = trace_event_type(kind) else {
        return Ok(None);
    };
    let payload = EventPayload::new(portable_trace_facts(facts))
        .map_err(|_| EventStoreFault::PayloadWrite)?;
    let input = EventInput {
        event_type,
        payload,
    };
    let run_id = trace_run_id(trace_path, facts);
    let journal = trace_path
        .parent()
        .ok_or(EventStoreFault::EventWrite)?
        .join("events.jsonl");
    JsonlEventStore::at_path(journal, run_id)
        .append_event_at(&input, timestamp)
        .map(Some)
}

fn trace_event_type(kind: &str) -> Option<EventType> {
    let canonical = match kind {
        "pass-started" => "pass.started",
        "pass-ended" => "pass.ended",
        "item-selected" => "item.selected",
        "work-dispatched" => "work.dispatched",
        "work-completed" => "work.completed",
        "work-failed" => "work.failed",
        "artifact-produced" => "artifact.produced",
        "gate-verdict-consumed" => "gate-verdict.consumed",
        "pr-repair" => "pr.repair",
        _ => return None,
    };
    Some(EventType::new(canonical).expect("settled event types are valid"))
}

fn portable_trace_facts(facts: &Map<String, Value>) -> Map<String, Value> {
    facts
        .iter()
        .filter(|(key, _)| !narration_field(key))
        .map(|(key, value)| (key.clone(), portable_trace_value(value)))
        .collect()
}

fn portable_trace_value(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(portable_trace_value).collect()),
        Value::Object(object) => Value::Object(portable_trace_facts(object)),
        other => other.clone(),
    }
}

fn narration_field(key: &str) -> bool {
    matches!(
        key,
        "detail"
            | "dossier"
            | "error"
            | "message"
            | "narration"
            | "operator_reason"
            | "prompt"
            | "reason"
            | "tool_output"
    )
}

fn trace_run_id(trace_path: &Path, facts: &Map<String, Value>) -> EventRunId {
    for key in ["owner", "order_id"] {
        if let Some(value) = facts
            .get(key)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
        {
            return EventRunId(value.to_owned());
        }
    }
    active_trace_run(trace_path).unwrap_or_else(|| EventRunId("local".to_owned()))
}

fn active_trace_run(trace_path: &Path) -> Option<EventRunId> {
    let contents = fs::read_to_string(trace_path).ok()?;
    let mut active = None;
    for line in contents.lines() {
        let value: Value = serde_json::from_str(line).ok()?;
        match value.get("kind").and_then(Value::as_str) {
            Some("pass-started") => {
                active = value
                    .get("fact")
                    .and_then(Value::as_object)
                    .and_then(|fact| fact.get("owner"))
                    .and_then(Value::as_str)
                    .filter(|owner| !owner.is_empty())
                    .map(|owner| EventRunId(owner.to_owned()));
            }
            Some("pass-ended") => active = None,
            _ => {}
        }
    }
    active
}

#[cfg(test)]
mod tests {
    use std::fs;

    use ostrom_core::{
        EventInput, EventPayload, EventRunId, EventStore, EventStoreFault, EventType,
        WriteDisposition, conformance::check_event_store,
    };
    use serde_json::{Map, json};
    use tempfile::tempdir;

    use super::{JsonlEventStore, append_trace_event, trace_event_type};
    use crate::OstromPaths;

    fn fixture() -> (tempfile::TempDir, OstromPaths) {
        let directory = tempdir().expect("temp dir");
        let paths = OstromPaths {
            config: directory.path().join("config"),
            state: directory.path().join("state"),
        };
        (directory, paths)
    }

    fn input(event_type: &str) -> EventInput {
        EventInput {
            event_type: EventType::new(event_type).expect("valid fixture type"),
            payload: EventPayload::new(Map::from_iter([(
                "item_id".to_owned(),
                json!("synthetic/project#42"),
            )]))
            .expect("fact payload"),
        }
    }

    #[tokio::test]
    async fn jsonl_sink_assigns_sequences_and_replay_is_a_no_op() {
        let (_fixture, paths) = fixture();
        let mut store = JsonlEventStore::new(&paths, EventRunId("synthetic-run".to_owned()));
        let completed = input("work.completed");
        assert_eq!(
            store.write_event(&completed).await.expect("first append"),
            WriteDisposition::Written
        );
        let before = fs::read(paths.event_journal_file()).expect("read journal");
        assert_eq!(
            store.write_event(&completed).await.expect("replay"),
            WriteDisposition::Unchanged
        );
        assert_eq!(
            fs::read(paths.event_journal_file()).expect("read replayed journal"),
            before
        );

        store
            .append_event_at(&input("artifact.produced"), "2030-01-02T03:04:07Z")
            .expect("second distinct append");
        let events = store.events().await.expect("read events");
        assert_eq!(
            events.iter().map(|event| event.seq).collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(events[0].run_id.0, "synthetic-run");
    }

    #[tokio::test]
    async fn file_store_passes_shared_event_conformance_battery() {
        let (_fixture, paths) = fixture();
        let mut store = JsonlEventStore::new(&paths, EventRunId("conformance-run".to_owned()));
        check_event_store(&mut store)
            .await
            .expect("file event store should conform");
    }

    #[tokio::test]
    async fn compact_envelope_bytes_and_required_field_failures_are_loud() {
        let (_fixture, paths) = fixture();
        let mut store = JsonlEventStore::new(&paths, EventRunId("synthetic-run".to_owned()));
        store
            .append_event_at(&input("work.dispatched"), "2030-01-02T03:04:05Z")
            .expect("append event");
        let expected = concat!(
            r#"{"v":1,"type":"work.dispatched","run_id":"synthetic-run","seq":1,"ts":"2030-01-02T03:04:05Z","payload":{"item_id":"synthetic/project#42"}}"#,
            "\n"
        );
        assert_eq!(
            fs::read(paths.event_journal_file()).expect("read journal"),
            expected.as_bytes()
        );

        fs::write(
            paths.event_journal_file(),
            concat!(
                r#"{"v":1,"type":"future.observed","run_id":"synthetic-run","seq":7,"ts":"2030-01-02T03:04:05Z","payload":{}}"#,
                "\n"
            ),
        )
        .expect("write unknown fixture");
        let unknown = store
            .events()
            .await
            .expect("unknown event remains readable");
        assert_eq!(unknown[0].event_type.as_str(), "future.observed");

        fs::write(
            paths.event_journal_file(),
            r#"{"v":1,"type":"future.observed","run_id":"synthetic-run","ts":"2030-01-02T03:04:05Z","payload":{}}"#,
        )
        .expect("write malformed fixture");
        assert_eq!(
            store.events().await.expect_err("missing seq must fail"),
            EventStoreFault::MalformedRecord
        );
    }

    #[test]
    fn complete_trace_vocabulary_maps_to_dot_namespaced_types() {
        let cases = [
            ("pass-started", "pass.started"),
            ("pass-ended", "pass.ended"),
            ("item-selected", "item.selected"),
            ("work-dispatched", "work.dispatched"),
            ("work-completed", "work.completed"),
            ("work-failed", "work.failed"),
            ("artifact-produced", "artifact.produced"),
            ("gate-verdict-consumed", "gate-verdict.consumed"),
            ("pr-repair", "pr.repair"),
        ];
        for (trace_kind, event_type) in cases {
            assert_eq!(
                trace_event_type(trace_kind)
                    .expect("transported type")
                    .as_str(),
                event_type
            );
        }
        assert!(trace_event_type("decision-taken").is_none());
    }

    #[tokio::test]
    async fn complete_trace_vocabulary_round_trips_through_the_port() {
        let (_fixture, paths) = fixture();
        let kinds = [
            "pass-started",
            "pass-ended",
            "item-selected",
            "work-dispatched",
            "work-completed",
            "work-failed",
            "artifact-produced",
            "gate-verdict-consumed",
            "pr-repair",
        ];
        for (index, kind) in kinds.iter().enumerate() {
            append_trace_event(
                &paths.trace_file(),
                &format!("2030-01-02T03:04:{index:02}Z"),
                kind,
                &Map::from_iter([
                    ("owner".to_owned(), json!("synthetic-run")),
                    ("ordinal".to_owned(), json!(index)),
                ]),
            )
            .expect("trace event writes through port");
        }
        let store = JsonlEventStore::new(&paths, EventRunId("synthetic-run".to_owned()));
        let events = store.events().await.expect("read transported vocabulary");
        assert_eq!(events.len(), kinds.len());
        assert_eq!(
            events
                .iter()
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>(),
            [
                "pass.started",
                "pass.ended",
                "item.selected",
                "work.dispatched",
                "work.completed",
                "work.failed",
                "artifact.produced",
                "gate-verdict.consumed",
                "pr.repair",
            ]
        );
        assert_eq!(
            events.iter().map(|event| event.seq).collect::<Vec<_>>(),
            (1..=9).collect::<Vec<_>>()
        );
    }

    #[cfg(unix)]
    #[test]
    fn event_journal_is_private() {
        use std::os::unix::fs::PermissionsExt;

        let (_fixture, paths) = fixture();
        let mut store = JsonlEventStore::new(&paths, EventRunId("synthetic-run".to_owned()));
        store
            .append_event_at(&input("item.selected"), "2030-01-02T03:04:05Z")
            .expect("append event");
        assert_eq!(
            fs::metadata(paths.event_journal_file())
                .expect("journal metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
