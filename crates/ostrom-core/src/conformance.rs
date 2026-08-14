//! Store conformance battery for in-tree and out-of-tree implementations.
//!
//! Enable the `conformance` feature and call [`check_store`]. Returning a
//! structured failure instead of panicking lets consumers integrate the same
//! battery into their preferred test framework.

use thiserror::Error;

use crate::{
    AttemptOutcome, PassAttempt, PassId, StoreFault, SweepPass, SweepStore, WriteDisposition,
    store::STORE_SCHEMA_VERSION,
};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConformanceFailure {
    #[error("write invariant failed: conforming store rejected a valid pass: {0}")]
    InitialWrite(StoreFault),
    #[error("idempotency invariant failed: the same pass was not a no-op")]
    Idempotency,
    #[error("read-back invariant failed: {0}")]
    ReadBack(StoreFault),
    #[error("read-back invariant failed: committed pass differs from its input")]
    Content,
    #[error("failed-attempt invariant failed: a failed pass was not recorded")]
    FailedAttempt,
}

fn pass(id: &str, outcome: AttemptOutcome) -> SweepPass {
    SweepPass {
        attempt: PassAttempt {
            schema_version: STORE_SCHEMA_VERSION,
            pass_id: PassId(id.to_owned()),
            started_at: "2030-01-02T03:04:05Z".to_owned(),
            outcome,
        },
        queue: Vec::new(),
        gates: Vec::new(),
        states: Vec::new(),
    }
}

pub async fn check_store<S: SweepStore>(store: &mut S) -> Result<(), ConformanceFailure> {
    let completed = pass("conformance-completed", AttemptOutcome::Completed);
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

    let failed = pass("conformance-failed", AttemptOutcome::Failed);
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
            if self.passes.iter().any(|existing| existing == pass) {
                return Ok(WriteDisposition::Unchanged);
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
}
