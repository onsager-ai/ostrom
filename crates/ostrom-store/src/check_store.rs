use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::PathBuf,
};

use async_trait::async_trait;
use ostrom_core::{
    CHECK_STORE_SCHEMA_VERSION, CheckRun, CheckRunId, CheckStore, CheckStoreFault, WriteDisposition,
};
use serde::Deserialize;

use crate::{OstromPaths, set_private_file_mode};

/// Append-only JSONL check-run store beneath the resolved Ostrom state root.
pub struct JsonlCheckStore {
    journal: PathBuf,
}

#[derive(Deserialize)]
struct StoredRunIdentity {
    run_id: CheckRunId,
}

impl JsonlCheckStore {
    #[must_use]
    pub fn new(paths: &OstromPaths) -> Self {
        Self {
            journal: paths.check_journal_file(),
        }
    }

    fn decode_record(line: &str) -> Result<CheckRun, CheckStoreFault> {
        let run: CheckRun =
            serde_json::from_str(line).map_err(|_| CheckStoreFault::MalformedRecord)?;
        if run.schema_version != CHECK_STORE_SCHEMA_VERSION {
            return Err(CheckStoreFault::UnsupportedSchema);
        }
        Ok(run)
    }

    fn read_records(&self) -> Result<Vec<CheckRun>, CheckStoreFault> {
        if !self.journal.exists() {
            return Ok(Vec::new());
        }
        let contents = fs::read_to_string(&self.journal).map_err(|_| CheckStoreFault::Read)?;
        contents.lines().map(Self::decode_record).collect()
    }

    /// Read a point-in-time snapshot for consumers such as the plan pass.
    pub fn snapshot(&self) -> Result<Vec<CheckRun>, CheckStoreFault> {
        self.read_records()
    }

    fn find_record(&self, run_id: &CheckRunId) -> Result<Option<CheckRun>, CheckStoreFault> {
        if !self.journal.exists() {
            return Ok(None);
        }
        let file = fs::File::open(&self.journal).map_err(|_| CheckStoreFault::Read)?;
        for line in BufReader::new(file).lines() {
            let line = line.map_err(|_| CheckStoreFault::Read)?;
            let identity: StoredRunIdentity =
                serde_json::from_str(&line).map_err(|_| CheckStoreFault::MalformedRecord)?;
            if &identity.run_id == run_id {
                return Self::decode_record(&line).map(Some);
            }
        }
        Ok(None)
    }
}

#[async_trait]
impl CheckStore for JsonlCheckStore {
    async fn write_run(&mut self, run: &CheckRun) -> Result<WriteDisposition, CheckStoreFault> {
        if run.schema_version != CHECK_STORE_SCHEMA_VERSION {
            return Err(CheckStoreFault::UnsupportedSchema);
        }
        if let Some(existing) = self.find_record(&run.run_id)? {
            return if existing == *run {
                Ok(WriteDisposition::Unchanged)
            } else {
                Err(CheckStoreFault::RunConflict)
            };
        }
        let parent = self.journal.parent().ok_or(CheckStoreFault::RunWrite)?;
        fs::create_dir_all(parent).map_err(|_| CheckStoreFault::RunWrite)?;
        let mut bytes = serde_json::to_vec(run).map_err(|_| CheckStoreFault::PayloadWrite)?;
        bytes.push(b'\n');
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.journal)
            .map_err(|_| CheckStoreFault::RunWrite)?;
        set_private_file_mode(&self.journal).map_err(|_| CheckStoreFault::RunWrite)?;
        file.write_all(&bytes)
            .map_err(|_| CheckStoreFault::PayloadWrite)?;
        Ok(WriteDisposition::Written)
    }

    async fn runs(&self) -> Result<Vec<CheckRun>, CheckStoreFault> {
        self.read_records()
    }
}

#[cfg(test)]
mod tests {
    use std::{env, fs, process::Command};

    use ostrom_core::{
        CHECK_STORE_SCHEMA_VERSION, CheckRun, CheckRunId, CheckStore, CheckStoreFault,
        WriteDisposition, conformance::check_check_store,
    };
    use tempfile::tempdir;

    use super::JsonlCheckStore;
    use crate::OstromPaths;

    const OSTROM_HOME_CHILD: &str = "OSTROM_CHECK_STORE_HOME_CHILD";

    fn empty_run(id: &str) -> CheckRun {
        CheckRun {
            schema_version: CHECK_STORE_SCHEMA_VERSION,
            run_id: CheckRunId(id.to_owned()),
            completed_at: "2030-01-02T03:04:05Z".to_owned(),
            receipts: Vec::new(),
        }
    }

    #[tokio::test]
    async fn file_store_passes_check_conformance_battery() {
        let fixture = tempdir().expect("temp dir");
        let paths = OstromPaths {
            config: fixture.path().join("config"),
            state: fixture.path().join("state"),
        };
        check_check_store(&mut JsonlCheckStore::new(&paths))
            .await
            .expect("file check store should conform");
    }

    #[tokio::test]
    async fn empty_run_is_durable_and_idempotent() {
        let fixture = tempdir().expect("temp dir");
        let paths = OstromPaths {
            config: fixture.path().join("config"),
            state: fixture.path().join("state"),
        };
        let mut store = JsonlCheckStore::new(&paths);
        assert!(store.runs().await.expect("new store reads").is_empty());

        let run = empty_run("fixture-empty-run");
        assert_eq!(
            store.write_run(&run).await.expect("write empty run"),
            WriteDisposition::Written
        );
        assert_eq!(
            store.runs().await.expect("read empty run"),
            vec![run.clone()]
        );
        let before = fs::read(paths.check_journal_file()).expect("read journal");
        assert_eq!(
            store.write_run(&run).await.expect("repeat empty run"),
            WriteDisposition::Unchanged
        );
        assert_eq!(
            fs::read(paths.check_journal_file()).expect("read repeated journal"),
            before
        );
    }

    #[tokio::test]
    async fn reused_run_id_with_different_content_conflicts() {
        let fixture = tempdir().expect("temp dir");
        let paths = OstromPaths {
            config: fixture.path().join("config"),
            state: fixture.path().join("state"),
        };
        let mut store = JsonlCheckStore::new(&paths);
        let run = empty_run("fixture-conflicting-run");
        store.write_run(&run).await.expect("initial run");
        let mut different = run;
        different.completed_at = "2030-01-02T03:04:06Z".to_owned();
        assert_eq!(
            store
                .write_run(&different)
                .await
                .expect_err("reused id must conflict"),
            CheckStoreFault::RunConflict
        );
    }

    #[test]
    fn ostrom_home_override_contains_the_check_journal() {
        if let Some(expected) = env::var_os(OSTROM_HOME_CHILD) {
            let paths = OstromPaths::resolve().expect("OSTROM_HOME resolves");
            assert_eq!(
                paths.check_journal_file(),
                std::path::PathBuf::from(expected).join("check-runs.jsonl")
            );
            return;
        }

        let fixture = tempdir().expect("temp dir");
        let status = Command::new(env::current_exe().expect("test executable"))
            .env("OSTROM_HOME", fixture.path())
            .env(OSTROM_HOME_CHILD, fixture.path())
            .args([
                "--exact",
                "check_store::tests::ostrom_home_override_contains_the_check_journal",
                "--nocapture",
            ])
            .status()
            .expect("run OSTROM_HOME child");
        assert!(status.success());
    }
}
