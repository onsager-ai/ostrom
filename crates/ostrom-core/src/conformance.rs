//! Store conformance battery for in-tree and out-of-tree implementations.
//!
//! Enable the `conformance` feature and call [`check_store`] or
//! [`check_check_store`] with a fresh, disposable implementation instance.
//! Returning a structured failure instead of panicking lets consumers
//! integrate the same batteries into their preferred test framework. They use
//! only the public ports and assume no filesystem, process-local runtime, or
//! other particular substrate.

use thiserror::Error;

use crate::{
    AttemptOutcome, CHECK_STORE_SCHEMA_VERSION, CheckRun, CheckRunId, CheckStore, CheckStoreFault,
    PassAttempt, PassId, QueueFact, QueueKind, QueueState, RepositoryName, StoreFault, SweepPass,
    SweepStore, WriteDisposition, store::STORE_SCHEMA_VERSION,
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

#[cfg(test)]
mod tests {
    use async_trait::async_trait;

    use super::{ConformanceFailure, check_store};
    use crate::{StoreFault, SweepPass, SweepStore, WriteDisposition};

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
}
