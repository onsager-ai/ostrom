//! Store conformance battery for in-tree and out-of-tree implementations.
//!
//! Enable the `conformance` feature and call [`check_store`],
//! [`check_check_store`], [`check_event_store`], or
//! [`check_publication_source`] with fresh, disposable implementation
//! instances. Returning a structured failure instead of panicking lets
//! consumers integrate the same batteries into their preferred test framework.
//! They use only the public ports and assume no filesystem, process-local
//! runtime, or other particular substrate.

use thiserror::Error;

use crate::{
    AttemptOutcome, CHECK_STORE_SCHEMA_VERSION, CheckRun, CheckRunId, CheckStore, CheckStoreFault,
    EVENT_VERSION, EventEnvelope, EventInput, EventPayload, EventStore, EventStoreFault, EventType,
    GatePublicationRecords, PassAttempt, PassId, PublicationSnapshot, PublicationSource,
    PublicationSourceFault, QueueFact, QueueKind, QueueState, RepositoryName, StoreFault,
    SweepPass, SweepStore, WriteDisposition, store::STORE_SCHEMA_VERSION,
};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConformanceFailure {
    #[error("initial-read invariant failed: {0}")]
    InitialRead(StoreFault),
    #[error("write invariant failed: conforming store rejected a valid pass: {0}")]
    InitialWrite(StoreFault),
    #[error("idempotency invariant failed: the same pass was not a no-op")]
    Idempotency,
    #[error("read-back invariant failed: {0}")]
    ReadBack(StoreFault),
    #[error("read-back invariant failed: committed pass differs from its input")]
    Content,
    #[error("idempotency invariant failed: the pass identifier was stored more than once")]
    DuplicateRecord,
    #[error("identifier invariant failed: changed content did not produce PassConflict")]
    Conflict,
    #[error("failed-attempt invariant failed: a failed pass was not recorded")]
    FailedAttempt,
    #[error("attempt-observability invariant failed: an empty failed pass looked like no pass")]
    EmptyFailedAttempt,
    #[error("event fixture invariant failed: the battery could not construct a valid event")]
    EventFixture,
    #[error("event initial-read invariant failed: {0}")]
    EventInitialRead(EventStoreFault),
    #[error("event initial-read invariant failed: a fresh store was not empty")]
    EventInitialState,
    #[error("event write invariant failed: conforming store rejected a valid event: {0}")]
    EventInitialWrite(EventStoreFault),
    #[error("event write-disposition invariant failed: a new event was not reported as written")]
    EventInitialWriteDisposition,
    #[error("event read-back invariant failed: {0}")]
    EventReadBack(EventStoreFault),
    #[error("event read-back invariant failed: committed event differs from its input")]
    EventContent,
    #[error("event version invariant failed: a record did not use EVENT_VERSION")]
    EventVersion,
    #[error("event run-identity invariant failed: one store instance wrote more than one run")]
    EventRunIdentity,
    #[error("event idempotency invariant failed: replay was rejected: {0}")]
    EventIdempotencyWrite(EventStoreFault),
    #[error("event idempotency invariant failed: replay was not reported as unchanged")]
    EventIdempotency,
    #[error("event idempotency invariant failed: replay stored the event more than once")]
    EventDuplicateRecord,
    #[error("event append invariant failed: a distinct event was rejected: {0}")]
    EventDistinctWrite(EventStoreFault),
    #[error("event append invariant failed: a distinct event was not reported as written")]
    EventDistinctWriteDisposition,
    #[error("event ordering invariant failed: records were not returned in append order")]
    EventOrdering,
    #[error("event sequence invariant failed: sequence numbers were not strictly increasing")]
    EventSequence,
    #[error("empty-event invariant failed: a valid empty event was rejected: {0}")]
    EventEmptyWrite(EventStoreFault),
    #[error("empty-event invariant failed: a valid empty event was not reported as written")]
    EventEmptyWriteDisposition,
    #[error("empty-event observability invariant failed: an empty event looked like no event")]
    EventEmptyEvent,
}

fn pass(id: &str, outcome: AttemptOutcome, with_payload: bool) -> SweepPass {
    SweepPass {
        attempt: PassAttempt {
            schema_version: STORE_SCHEMA_VERSION,
            pass_id: PassId(id.to_owned()),
            started_at: "2030-01-02T03:04:05Z".to_owned(),
            outcome,
        },
        queue: if with_payload {
            vec![QueueFact {
                id: "synthetic-org/project#42".to_owned(),
                repo: RepositoryName::new("synthetic-org/project")
                    .expect("valid fixture repository"),
                reference: "#42".to_owned(),
                title: "Synthetic classification fixture".to_owned(),
                kind: QueueKind::Decision,
                state: QueueState::Pending,
                opened: "2030-01-01T00:00:00Z".to_owned(),
                blocked_by: vec!["synthetic-org/dependency#7".to_owned()],
                needs_judgment: true,
            }]
        } else {
            Vec::new()
        },
        gates: Vec::new(),
        states: Vec::new(),
    }
}

pub async fn check_store<S: SweepStore>(store: &mut S) -> Result<(), ConformanceFailure> {
    let before = store
        .passes()
        .await
        .map_err(ConformanceFailure::InitialRead)?;
    let completed = pass("conformance-completed", AttemptOutcome::Completed, true);
    if store
        .write_pass(&completed)
        .await
        .map_err(ConformanceFailure::InitialWrite)?
        != WriteDisposition::Written
    {
        return Err(ConformanceFailure::Content);
    }
    if store
        .write_pass(&completed)
        .await
        .map_err(ConformanceFailure::InitialWrite)?
        != WriteDisposition::Unchanged
    {
        return Err(ConformanceFailure::Idempotency);
    }

    let mut conflicting = completed.clone();
    conflicting.attempt.outcome = AttemptOutcome::NoOp;
    if store.write_pass(&conflicting).await != Err(StoreFault::PassConflict) {
        return Err(ConformanceFailure::Conflict);
    }

    // This is deliberately a failed pass with no queue, gate, or state facts.
    // Persisting the attempt itself is what distinguishes it from never run.
    let failed = pass("conformance-failed-empty", AttemptOutcome::Failed, false);
    store
        .write_pass(&failed)
        .await
        .map_err(ConformanceFailure::InitialWrite)?;

    let passes = store.passes().await.map_err(ConformanceFailure::ReadBack)?;
    if !passes.contains(&completed) {
        return Err(ConformanceFailure::Content);
    }
    if !passes.contains(&failed) {
        return Err(ConformanceFailure::FailedAttempt);
    }
    if passes
        .iter()
        .filter(|stored| stored.attempt.pass_id == completed.attempt.pass_id)
        .count()
        != 1
    {
        return Err(ConformanceFailure::DuplicateRecord);
    }
    if passes.len() != before.len() + 2
        || !failed.queue.is_empty()
        || !failed.gates.is_empty()
        || !failed.states.is_empty()
    {
        return Err(ConformanceFailure::EmptyFailedAttempt);
    }
    Ok(())
}

fn event(event_type: &str, item_id: Option<&str>) -> Result<EventInput, ConformanceFailure> {
    let event_type = EventType::new(event_type).map_err(|_| ConformanceFailure::EventFixture)?;
    let payload = match item_id {
        Some(item_id) => EventPayload::new(serde_json::Map::from_iter([(
            "item_id".to_owned(),
            serde_json::Value::String(item_id.to_owned()),
        )]))
        .map_err(|_| ConformanceFailure::EventFixture)?,
        None => EventPayload::empty(),
    };
    Ok(EventInput {
        event_type,
        payload,
    })
}

fn envelope_matches(envelope: &EventEnvelope, input: &EventInput) -> bool {
    envelope.event_type == input.event_type && envelope.payload == input.payload
}

fn check_event_snapshot(
    events: &[EventEnvelope],
    expected: &[&EventInput],
) -> Result<(), ConformanceFailure> {
    if events.len() != expected.len()
        || expected.iter().any(|input| {
            events
                .iter()
                .filter(|envelope| envelope_matches(envelope, input))
                .count()
                != 1
        })
    {
        return Err(ConformanceFailure::EventContent);
    }
    if events.iter().any(|event| event.v != EVENT_VERSION) {
        return Err(ConformanceFailure::EventVersion);
    }
    let Some(first) = events.first() else {
        return if expected.is_empty() {
            Ok(())
        } else {
            Err(ConformanceFailure::EventContent)
        };
    };
    if events.iter().any(|event| event.run_id != first.run_id) {
        return Err(ConformanceFailure::EventRunIdentity);
    }
    if events
        .iter()
        .zip(expected)
        .any(|(envelope, input)| !envelope_matches(envelope, input))
    {
        return Err(ConformanceFailure::EventOrdering);
    }
    if events.windows(2).any(|pair| pair[0].seq >= pair[1].seq) {
        return Err(ConformanceFailure::EventSequence);
    }
    Ok(())
}

/// Exercise the empty-store, append, read-back, idempotency, ordering,
/// envelope-version, and empty-event invariants of any [`EventStore`]
/// implementation.
pub async fn check_event_store<S: EventStore>(store: &mut S) -> Result<(), ConformanceFailure> {
    let before = store
        .events()
        .await
        .map_err(ConformanceFailure::EventInitialRead)?;
    if !before.is_empty() {
        return Err(ConformanceFailure::EventInitialState);
    }

    let completed = event("work.completed", Some("synthetic-org/project#42"))?;
    if store
        .write_event(&completed)
        .await
        .map_err(ConformanceFailure::EventInitialWrite)?
        != WriteDisposition::Written
    {
        return Err(ConformanceFailure::EventInitialWriteDisposition);
    }
    let events = store
        .events()
        .await
        .map_err(ConformanceFailure::EventReadBack)?;
    check_event_snapshot(&events, &[&completed])?;

    if store
        .write_event(&completed)
        .await
        .map_err(ConformanceFailure::EventIdempotencyWrite)?
        != WriteDisposition::Unchanged
    {
        return Err(ConformanceFailure::EventIdempotency);
    }
    let events = store
        .events()
        .await
        .map_err(ConformanceFailure::EventReadBack)?;
    if events.len() > 1 {
        return Err(ConformanceFailure::EventDuplicateRecord);
    }
    check_event_snapshot(&events, &[&completed])?;

    // EventInput has no caller-provided identifier. Under the public contract,
    // changed payload under the same open event type is a distinct fact, not an
    // identifier conflict, and must append without replacing the first fact.
    let changed = event("work.completed", Some("synthetic-org/project#43"))?;
    if store
        .write_event(&changed)
        .await
        .map_err(ConformanceFailure::EventDistinctWrite)?
        != WriteDisposition::Written
    {
        return Err(ConformanceFailure::EventDistinctWriteDisposition);
    }
    let events = store
        .events()
        .await
        .map_err(ConformanceFailure::EventReadBack)?;
    check_event_snapshot(&events, &[&completed, &changed])?;

    // A valid event may have no facts. It must remain distinguishable from an
    // event that was never written.
    let empty = event("pass.ended", None)?;
    if store
        .write_event(&empty)
        .await
        .map_err(ConformanceFailure::EventEmptyWrite)?
        != WriteDisposition::Written
    {
        return Err(ConformanceFailure::EventEmptyWriteDisposition);
    }
    let events = store
        .events()
        .await
        .map_err(ConformanceFailure::EventReadBack)?;
    if events.len() != 3 || !events.iter().any(|stored| envelope_matches(stored, &empty)) {
        return Err(ConformanceFailure::EventEmptyEvent);
    }
    check_event_snapshot(&events, &[&completed, &changed, &empty])
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CheckStoreConformanceFailure {
    #[error("initial empty-run write failed: {0}")]
    InitialWrite(CheckStoreFault),
    #[error("check run idempotency invariant failed")]
    Idempotency,
    #[error("check store content invariant failed")]
    Content,
    #[error("check run conflict invariant failed")]
    Conflict,
}

/// Exercise the durable empty-run, read-back, idempotency, and conflict
/// invariants of any [`CheckStore`] implementation.
pub async fn check_check_store<S: CheckStore>(
    store: &mut S,
) -> Result<(), CheckStoreConformanceFailure> {
    let run = CheckRun {
        schema_version: CHECK_STORE_SCHEMA_VERSION,
        run_id: CheckRunId("conformance-empty-check-run".to_owned()),
        completed_at: "2030-01-02T03:04:05Z".to_owned(),
        receipts: Vec::new(),
    };
    if store
        .write_run(&run)
        .await
        .map_err(CheckStoreConformanceFailure::InitialWrite)?
        != WriteDisposition::Written
    {
        return Err(CheckStoreConformanceFailure::Content);
    }
    if store
        .write_run(&run)
        .await
        .map_err(|_| CheckStoreConformanceFailure::Idempotency)?
        != WriteDisposition::Unchanged
    {
        return Err(CheckStoreConformanceFailure::Idempotency);
    }
    if store
        .runs()
        .await
        .map_err(|_| CheckStoreConformanceFailure::Content)?
        != vec![run.clone()]
    {
        return Err(CheckStoreConformanceFailure::Content);
    }

    let mut conflicting = run;
    conflicting.completed_at = "2030-01-02T03:04:06Z".to_owned();
    if store.write_run(&conflicting).await != Err(CheckStoreFault::RunConflict) {
        return Err(CheckStoreConformanceFailure::Conflict);
    }
    Ok(())
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PublicationSourceConformanceFailure {
    #[error("publication fixture setup failed: {0}")]
    FixtureSetup(String),
    #[error("populated publication read invariant failed: {0}")]
    PopulatedRead(PublicationSourceFault),
    #[error("publication queue content invariant failed")]
    QueueContent,
    #[error("publication state content invariant failed")]
    StateContent,
    #[error("populated authoritative-gate invariant failed")]
    PopulatedGate,
    #[error("authoritative-empty gate read invariant failed: {0}")]
    AuthoritativeEmptyRead(PublicationSourceFault),
    #[error("authoritative-empty gate invariant failed")]
    AuthoritativeEmptyGate,
    #[error("non-authoritative gate read invariant failed: {0}")]
    NonAuthoritativeRead(PublicationSourceFault),
    #[error("non-authoritative gate invariant failed")]
    NonAuthoritativeGate,
}

fn publication_snapshot(gate: GatePublicationRecords) -> PublicationSnapshot {
    PublicationSnapshot {
        queue: vec![serde_json::json!({
            "id": "synthetic-org/project#42",
            "kind": "decision",
        })],
        gate,
        state: serde_json::json!({
            "version": 2,
            "repos": {"synthetic-org/project": {}},
        }),
    }
}

fn check_publication_content(
    actual: &PublicationSnapshot,
    expected: &PublicationSnapshot,
) -> Result<(), PublicationSourceConformanceFailure> {
    if actual.queue != expected.queue {
        return Err(PublicationSourceConformanceFailure::QueueContent);
    }
    if actual.state != expected.state {
        return Err(PublicationSourceConformanceFailure::StateContent);
    }
    Ok(())
}

/// Exercise exact record read-back and the distinction between an
/// authoritative empty gate collection and a source that is not authoritative
/// for gate records.
///
/// The factory receives each desired snapshot and must return a fresh source
/// configured to expose it. This keeps fixture setup substrate-specific while
/// the battery itself observes only [`PublicationSource`].
pub fn check_publication_source<S, F, E>(
    mut source_from: F,
) -> Result<(), PublicationSourceConformanceFailure>
where
    S: PublicationSource,
    F: FnMut(&PublicationSnapshot) -> Result<S, E>,
    E: std::fmt::Display,
{
    let populated = publication_snapshot(GatePublicationRecords::Authoritative(vec![
        serde_json::json!({
            "ts": "2030-01-02T03:04:05Z",
            "pr": "synthetic-org/project#43",
            "verdict": "pass",
        }),
    ]));
    let source = source_from(&populated)
        .map_err(|error| PublicationSourceConformanceFailure::FixtureSetup(error.to_string()))?;
    let actual = source
        .snapshot()
        .map_err(PublicationSourceConformanceFailure::PopulatedRead)?;
    check_publication_content(&actual, &populated)?;
    if actual.gate != populated.gate {
        return Err(PublicationSourceConformanceFailure::PopulatedGate);
    }

    let authoritative_empty =
        publication_snapshot(GatePublicationRecords::Authoritative(Vec::new()));
    let source = source_from(&authoritative_empty)
        .map_err(|error| PublicationSourceConformanceFailure::FixtureSetup(error.to_string()))?;
    let actual = source
        .snapshot()
        .map_err(PublicationSourceConformanceFailure::AuthoritativeEmptyRead)?;
    check_publication_content(&actual, &authoritative_empty)?;
    if actual.gate != GatePublicationRecords::Authoritative(Vec::new()) {
        return Err(PublicationSourceConformanceFailure::AuthoritativeEmptyGate);
    }

    let non_authoritative = publication_snapshot(GatePublicationRecords::NotAuthoritative);
    let source = source_from(&non_authoritative)
        .map_err(|error| PublicationSourceConformanceFailure::FixtureSetup(error.to_string()))?;
    let actual = source
        .snapshot()
        .map_err(PublicationSourceConformanceFailure::NonAuthoritativeRead)?;
    check_publication_content(&actual, &non_authoritative)?;
    if actual.gate != GatePublicationRecords::NotAuthoritative {
        return Err(PublicationSourceConformanceFailure::NonAuthoritativeGate);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        convert::Infallible,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use async_trait::async_trait;

    use super::{
        ConformanceFailure, PublicationSourceConformanceFailure, check_event_store,
        check_publication_source, check_store, event,
    };
    use crate::{
        EVENT_VERSION, EventEnvelope, EventInput, EventRunId, EventStore, EventStoreFault,
        EventType, GatePublicationRecords, PublicationSnapshot, PublicationSource,
        PublicationSourceFault, StoreFault, SweepPass, SweepStore, WriteDisposition,
    };

    #[derive(Clone)]
    struct MemoryPublicationSource(PublicationSnapshot);

    impl PublicationSource for MemoryPublicationSource {
        fn snapshot(&self) -> Result<PublicationSnapshot, PublicationSourceFault> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn in_memory_publication_source_passes_without_a_substrate() {
        check_publication_source(|snapshot| {
            Ok::<_, Infallible>(MemoryPublicationSource(snapshot.clone()))
        })
        .expect("memory publication source should conform");
    }

    struct CollapsingPublicationSource(PublicationSnapshot);

    impl PublicationSource for CollapsingPublicationSource {
        fn snapshot(&self) -> Result<PublicationSnapshot, PublicationSourceFault> {
            let mut snapshot = self.0.clone();
            if snapshot.gate == GatePublicationRecords::Authoritative(Vec::new()) {
                snapshot.gate = GatePublicationRecords::NotAuthoritative;
            }
            Ok(snapshot)
        }
    }

    #[test]
    fn collapsing_authoritative_empty_gate_names_the_invariant() {
        let failure = check_publication_source(|snapshot| {
            Ok::<_, Infallible>(CollapsingPublicationSource(snapshot.clone()))
        })
        .expect_err("a source that collapses gate authority must fail conformance");
        assert_eq!(
            failure,
            PublicationSourceConformanceFailure::AuthoritativeEmptyGate
        );
        assert!(
            failure
                .to_string()
                .contains("authoritative-empty gate invariant")
        );
    }

    #[derive(Default)]
    struct MemoryStore {
        passes: Vec<SweepPass>,
    }

    #[async_trait]
    impl SweepStore for MemoryStore {
        async fn write_pass(&mut self, pass: &SweepPass) -> Result<WriteDisposition, StoreFault> {
            if let Some(existing) = self
                .passes
                .iter()
                .find(|existing| existing.attempt.pass_id == pass.attempt.pass_id)
            {
                return if existing == pass {
                    Ok(WriteDisposition::Unchanged)
                } else {
                    Err(StoreFault::PassConflict)
                };
            }
            self.passes.push(pass.clone());
            Ok(WriteDisposition::Written)
        }

        async fn passes(&self) -> Result<Vec<SweepPass>, StoreFault> {
            Ok(self.passes.clone())
        }
    }

    #[tokio::test]
    async fn in_memory_store_passes_without_the_file_crate() {
        check_store(&mut MemoryStore::default())
            .await
            .expect("memory store should conform");
    }

    #[derive(Default)]
    struct DuplicatingStore(MemoryStore);

    #[async_trait]
    impl SweepStore for DuplicatingStore {
        async fn write_pass(&mut self, pass: &SweepPass) -> Result<WriteDisposition, StoreFault> {
            self.0.passes.push(pass.clone());
            Ok(WriteDisposition::Written)
        }

        async fn passes(&self) -> Result<Vec<SweepPass>, StoreFault> {
            Ok(self.0.passes.clone())
        }
    }

    #[tokio::test]
    async fn broken_store_names_the_invariant() {
        let failure = check_store(&mut DuplicatingStore::default())
            .await
            .expect_err("duplicate writer must fail conformance");
        assert_eq!(failure, ConformanceFailure::Idempotency);
        assert!(failure.to_string().contains("idempotency invariant"));
    }

    #[derive(Default)]
    struct SilentFailureStore;

    #[async_trait]
    impl SweepStore for SilentFailureStore {
        async fn write_pass(&mut self, _: &SweepPass) -> Result<WriteDisposition, StoreFault> {
            Ok(WriteDisposition::Unchanged)
        }

        async fn passes(&self) -> Result<Vec<SweepPass>, StoreFault> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn silent_write_failure_cannot_masquerade_as_an_empty_result() {
        let failure = check_store(&mut SilentFailureStore)
            .await
            .expect_err("silent writer must fail conformance");
        assert_eq!(failure, ConformanceFailure::Content);
    }

    #[derive(Default)]
    struct FaultingStore;

    #[async_trait]
    impl SweepStore for FaultingStore {
        async fn write_pass(&mut self, _: &SweepPass) -> Result<WriteDisposition, StoreFault> {
            Err(StoreFault::PayloadWrite)
        }

        async fn passes(&self) -> Result<Vec<SweepPass>, StoreFault> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn write_fault_is_named_instead_of_becoming_an_empty_result() {
        let failure = check_store(&mut FaultingStore)
            .await
            .expect_err("faulting writer must fail conformance");
        assert_eq!(
            failure,
            ConformanceFailure::InitialWrite(StoreFault::PayloadWrite)
        );
    }

    #[derive(Default)]
    struct MemoryEventStore {
        events: Vec<EventEnvelope>,
    }

    fn envelope(input: &EventInput, seq: u64) -> EventEnvelope {
        EventEnvelope {
            v: EVENT_VERSION,
            event_type: input.event_type.clone(),
            run_id: EventRunId("conformance-run".to_owned()),
            seq,
            ts: format!("2030-01-02T03:04:{seq:02}Z"),
            payload: input.payload.clone(),
        }
    }

    #[async_trait]
    impl EventStore for MemoryEventStore {
        async fn write_event(
            &mut self,
            input: &EventInput,
        ) -> Result<WriteDisposition, EventStoreFault> {
            if self.events.iter().any(|stored| {
                stored.event_type == input.event_type && stored.payload == input.payload
            }) {
                return Ok(WriteDisposition::Unchanged);
            }
            let seq = u64::try_from(self.events.len())
                .map_err(|_| EventStoreFault::EventWrite)?
                .checked_add(1)
                .ok_or(EventStoreFault::EventWrite)?;
            self.events.push(envelope(input, seq));
            Ok(WriteDisposition::Written)
        }

        async fn events(&self) -> Result<Vec<EventEnvelope>, EventStoreFault> {
            Ok(self.events.clone())
        }
    }

    #[tokio::test]
    async fn in_memory_event_store_passes_without_a_substrate() {
        check_event_store(&mut MemoryEventStore::default())
            .await
            .expect("memory event store should conform");
    }

    #[derive(Debug, Clone, Copy)]
    enum BrokenEventInvariant {
        InitialRead,
        InitialState,
        InitialWrite,
        InitialDisposition,
        ReadBack,
        Content,
        Version,
        RunIdentity,
        IdempotencyWrite,
        IdempotencyDisposition,
        Duplicate,
        DistinctWrite,
        DistinctDisposition,
        Ordering,
        Sequence,
        EmptyWrite,
        EmptyDisposition,
        EmptyObservability,
    }

    struct BrokenEventStore {
        inner: MemoryEventStore,
        invariant: BrokenEventInvariant,
        reads: AtomicUsize,
        writes: usize,
    }

    impl BrokenEventStore {
        fn new(invariant: BrokenEventInvariant) -> Self {
            let mut inner = MemoryEventStore::default();
            if matches!(invariant, BrokenEventInvariant::InitialState) {
                let input = event("work.started", None).expect("valid test event");
                inner.events.push(envelope(&input, 1));
            }
            Self {
                inner,
                invariant,
                reads: AtomicUsize::new(0),
                writes: 0,
            }
        }
    }

    #[async_trait]
    impl EventStore for BrokenEventStore {
        async fn write_event(
            &mut self,
            input: &EventInput,
        ) -> Result<WriteDisposition, EventStoreFault> {
            self.writes += 1;
            match (self.invariant, self.writes) {
                (BrokenEventInvariant::InitialWrite, 1) => {
                    return Err(EventStoreFault::EventWrite);
                }
                (BrokenEventInvariant::IdempotencyWrite, 2) => {
                    return Err(EventStoreFault::PayloadWrite);
                }
                (BrokenEventInvariant::DistinctWrite, 3) => {
                    return Err(EventStoreFault::EventWrite);
                }
                (BrokenEventInvariant::EmptyWrite, 4) => {
                    return Err(EventStoreFault::PayloadWrite);
                }
                (BrokenEventInvariant::EmptyObservability, 4) => {
                    return Ok(WriteDisposition::Written);
                }
                _ => {}
            }

            let disposition = self.inner.write_event(input).await?;
            match (self.invariant, self.writes) {
                (BrokenEventInvariant::InitialDisposition, 1) => Ok(WriteDisposition::Unchanged),
                (BrokenEventInvariant::IdempotencyDisposition, 2) => Ok(WriteDisposition::Written),
                (BrokenEventInvariant::Duplicate, 2) => {
                    let seq = u64::try_from(self.inner.events.len())
                        .map_err(|_| EventStoreFault::EventWrite)?
                        .checked_add(1)
                        .ok_or(EventStoreFault::EventWrite)?;
                    self.inner.events.push(envelope(input, seq));
                    Ok(WriteDisposition::Unchanged)
                }
                (BrokenEventInvariant::DistinctDisposition, 3)
                | (BrokenEventInvariant::EmptyDisposition, 4) => Ok(WriteDisposition::Unchanged),
                _ => Ok(disposition),
            }
        }

        async fn events(&self) -> Result<Vec<EventEnvelope>, EventStoreFault> {
            let read = self.reads.fetch_add(1, Ordering::Relaxed);
            if matches!(self.invariant, BrokenEventInvariant::InitialRead) && read == 0 {
                return Err(EventStoreFault::Read);
            }
            if matches!(self.invariant, BrokenEventInvariant::ReadBack)
                && !self.inner.events.is_empty()
            {
                return Err(EventStoreFault::Read);
            }

            let mut events = self.inner.events.clone();
            if let Some(first) = events.first_mut() {
                match self.invariant {
                    BrokenEventInvariant::Content => {
                        first.event_type =
                            EventType::new("work.failed").expect("valid test event type");
                    }
                    BrokenEventInvariant::Version => first.v = EVENT_VERSION + 1,
                    _ => {}
                }
            }
            if events.len() >= 2 {
                match self.invariant {
                    BrokenEventInvariant::RunIdentity => {
                        events[1].run_id = EventRunId("another-run".to_owned());
                    }
                    BrokenEventInvariant::Ordering => events.reverse(),
                    BrokenEventInvariant::Sequence => events[1].seq = events[0].seq,
                    _ => {}
                }
            }
            Ok(events)
        }
    }

    #[tokio::test]
    async fn broken_event_stores_name_each_invariant() {
        let cases = [
            (
                BrokenEventInvariant::InitialRead,
                ConformanceFailure::EventInitialRead(EventStoreFault::Read),
            ),
            (
                BrokenEventInvariant::InitialState,
                ConformanceFailure::EventInitialState,
            ),
            (
                BrokenEventInvariant::InitialWrite,
                ConformanceFailure::EventInitialWrite(EventStoreFault::EventWrite),
            ),
            (
                BrokenEventInvariant::InitialDisposition,
                ConformanceFailure::EventInitialWriteDisposition,
            ),
            (
                BrokenEventInvariant::ReadBack,
                ConformanceFailure::EventReadBack(EventStoreFault::Read),
            ),
            (
                BrokenEventInvariant::Content,
                ConformanceFailure::EventContent,
            ),
            (
                BrokenEventInvariant::Version,
                ConformanceFailure::EventVersion,
            ),
            (
                BrokenEventInvariant::RunIdentity,
                ConformanceFailure::EventRunIdentity,
            ),
            (
                BrokenEventInvariant::IdempotencyWrite,
                ConformanceFailure::EventIdempotencyWrite(EventStoreFault::PayloadWrite),
            ),
            (
                BrokenEventInvariant::IdempotencyDisposition,
                ConformanceFailure::EventIdempotency,
            ),
            (
                BrokenEventInvariant::Duplicate,
                ConformanceFailure::EventDuplicateRecord,
            ),
            (
                BrokenEventInvariant::DistinctWrite,
                ConformanceFailure::EventDistinctWrite(EventStoreFault::EventWrite),
            ),
            (
                BrokenEventInvariant::DistinctDisposition,
                ConformanceFailure::EventDistinctWriteDisposition,
            ),
            (
                BrokenEventInvariant::Ordering,
                ConformanceFailure::EventOrdering,
            ),
            (
                BrokenEventInvariant::Sequence,
                ConformanceFailure::EventSequence,
            ),
            (
                BrokenEventInvariant::EmptyWrite,
                ConformanceFailure::EventEmptyWrite(EventStoreFault::PayloadWrite),
            ),
            (
                BrokenEventInvariant::EmptyDisposition,
                ConformanceFailure::EventEmptyWriteDisposition,
            ),
            (
                BrokenEventInvariant::EmptyObservability,
                ConformanceFailure::EventEmptyEvent,
            ),
        ];

        for (invariant, expected) in cases {
            let failure = check_event_store(&mut BrokenEventStore::new(invariant))
                .await
                .expect_err("broken event store must fail conformance");
            assert_eq!(failure, expected, "wrong failure for {invariant:?}");
            assert!(failure.to_string().contains("invariant"));
        }
    }
}
