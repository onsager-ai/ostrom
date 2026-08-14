use std::{fs, io::Write, path::PathBuf};

use async_trait::async_trait;
use ostrom_core::{STORE_SCHEMA_VERSION, StoreFault, SweepPass, SweepStore, WriteDisposition};

use crate::{OstromPaths, set_private_file_mode};

/// Append-only JSONL implementation of the portable pass contract.
///
/// One complete pass is one line and one `write_all`, so readers never observe
/// an attempt without its payload. The legacy queue/state writers remain
/// separate compatibility APIs until the sweep itself moves in phase 2.
pub struct JsonlSweepStore {
    journal: PathBuf,
}

impl JsonlSweepStore {
    #[must_use]
    pub fn new(paths: &OstromPaths) -> Self {
        Self {
            journal: paths.state.join("sweep-passes.jsonl"),
        }
    }

    fn read_records(&self) -> Result<Vec<SweepPass>, StoreFault> {
        if !self.journal.exists() {
            return Ok(Vec::new());
        }
        let contents = fs::read_to_string(&self.journal).map_err(|_| StoreFault::Read)?;
        contents
            .lines()
            .map(|line| serde_json::from_str(line).map_err(|_| StoreFault::MalformedRecord))
            .collect()
    }
}

#[async_trait]
impl SweepStore for JsonlSweepStore {
    async fn write_pass(&mut self, pass: &SweepPass) -> Result<WriteDisposition, StoreFault> {
        if pass.attempt.schema_version != STORE_SCHEMA_VERSION {
            return Err(StoreFault::UnsupportedSchema);
        }
        let records = self.read_records()?;
        if let Some(existing) = records
            .iter()
            .find(|record| record.attempt.pass_id == pass.attempt.pass_id)
        {
            return if existing == pass {
                Ok(WriteDisposition::Unchanged)
            } else {
                Err(StoreFault::PassConflict)
            };
        }
        let parent = self.journal.parent().ok_or(StoreFault::AttemptWrite)?;
        fs::create_dir_all(parent).map_err(|_| StoreFault::AttemptWrite)?;
        let mut bytes = serde_json::to_vec(pass).map_err(|_| StoreFault::PayloadWrite)?;
        bytes.push(b'\n');
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.journal)
            .map_err(|_| StoreFault::AttemptWrite)?;
        set_private_file_mode(&self.journal).map_err(|_| StoreFault::AttemptWrite)?;
        file.write_all(&bytes)
            .map_err(|_| StoreFault::PayloadWrite)?;
        Ok(WriteDisposition::Written)
    }

    async fn passes(&self) -> Result<Vec<SweepPass>, StoreFault> {
        self.read_records()
    }
}

#[cfg(test)]
mod tests {
    use ostrom_core::{
        AttemptOutcome, PassAttempt, PassId, STORE_SCHEMA_VERSION, StoreFault, SweepPass,
        SweepStore, conformance::check_store,
    };
    use tempfile::tempdir;

    use super::JsonlSweepStore;
    use crate::OstromPaths;

    #[tokio::test]
    async fn file_store_passes_shared_conformance_battery() {
        let fixture = tempdir().expect("temp dir");
        let paths = OstromPaths {
            config: fixture.path().join("config"),
            state: fixture.path().join("state"),
        };
        check_store(&mut JsonlSweepStore::new(&paths))
            .await
            .expect("file store should conform");
    }

    #[tokio::test]
    async fn file_write_failure_is_a_named_fault() {
        let fixture = tempdir().expect("temp dir");
        let blocked_state = fixture.path().join("state-is-a-file");
        std::fs::write(&blocked_state, "synthetic obstruction").expect("obstruction");
        let paths = OstromPaths {
            config: fixture.path().join("config"),
            state: blocked_state,
        };
        let pass = SweepPass {
            attempt: PassAttempt {
                schema_version: STORE_SCHEMA_VERSION,
                pass_id: PassId("synthetic-write-fault".to_owned()),
                started_at: "2030-01-02T03:04:05Z".to_owned(),
                outcome: AttemptOutcome::Failed,
            },
            queue: Vec::new(),
            gates: Vec::new(),
            states: Vec::new(),
        };
        let error = JsonlSweepStore::new(&paths)
            .write_pass(&pass)
            .await
            .expect_err("blocked path must fail loudly");
        assert_eq!(error, StoreFault::AttemptWrite);
        assert!(error.to_string().contains("attempt record write failed"));
    }
}
