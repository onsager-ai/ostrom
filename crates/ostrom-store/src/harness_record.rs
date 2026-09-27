//! The harness child a run started, recorded where something other than the
//! run's own worker can find it (#633).
//!
//! A run is a supervisor (`ostrom implement` or `ostrom pass`), the worker it
//! starts, and the harness child the worker starts: Codex for an implementer,
//! Claude for a pass. The harness leads a process group of its own, apart from
//! the supervisor's. Signalling the supervisor or the worker therefore never
//! reaches it, and a worker killed before it stops its harness leaves that
//! harness running with nothing watching it.
//!
//! So the worker records its harness here as soon as it is spawned: the
//! harness's pid, process group and start time, under the lease's field names,
//! and the run id it belongs to. Two readers stop it from this record, and
//! each re-checks all three values before every signal, so a recycled pid is
//! never signalled:
//!
//! - the supervisor, when its worker exits while the harness still runs, or
//!   when a signal it forwarded has not stopped the harness in time;
//! - the stall reaper, after it has stopped a run's own process, whether it did
//!   so itself or a reaper that died before finishing did.
//!
//! The file is named by the supervisor's pid and start time, which the worker
//! is given and the supervisor knows, so neither has to tell the other where
//! it is; a worker run with no supervisor names it by its own. The reaper
//! finds it by run id, so either way the reaper can reach the harness. The
//! file is private (0600), as claims and leases are.
//!
//! The record is written just after the harness is spawned, so a worker
//! killed in the moment between the two leaves its harness unrecorded.
//! Closing that window would need code between fork and exec, which is
//! `unsafe` and forbidden here. The worker removes it once its
//! harness has exited; the supervisor or the reaper removes it once it has
//! confirmed the harness gone.
//!
//! Everything under `<state>/harness/` is private state that only ostrom reads.

use std::{
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
};

use serde_json::{Value, json};

use crate::lease::read_process_identity;

/// The harness recorded for a run: the lease's process-identity fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RecordedHarness {
    pub pid: u32,
    pub process_group_id: u32,
    pub start_time: u64,
}

fn directory(state: &Path) -> PathBuf {
    state.join("harness")
}

/// Where the worker under supervisor `supervisor_pid` records its harness
/// (a worker with no supervisor passes its own pid).
/// The supervisor's start time is part of the name, so a recycled supervisor
/// pid never shares a file with an earlier run. `None` when that start time
/// cannot be read, and then nothing is recorded.
pub(crate) fn record_path(state: &Path, supervisor_pid: u32) -> Option<PathBuf> {
    let start_time = read_process_identity(supervisor_pid)
        .ok()
        .flatten()?
        .start_time;
    Some(directory(state).join(format!("{supervisor_pid}-{start_time}.json")))
}

/// Record the harness child `pid` for `run_id` at `path`. Its identity is read
/// from `/proc` now, while it is certainly the child just spawned. The record
/// is written whole and renamed into place, so a reader never sees half of it.
pub(crate) fn record(path: &Path, run_id: &str, pid: u32) -> io::Result<()> {
    let identity = read_process_identity(pid)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("harness process {pid} is already gone"),
        )
    })?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let body = json!({
        "run_id": run_id,
        "pid": identity.pid,
        "process_group_id": identity.process_group_id,
        "process_start_time": identity.start_time,
    });
    let temporary = path.with_extension("json.tmp");
    // Private before anything is written to it.
    let mut file = fs::File::create(&temporary)?;
    if temporary.as_os_str().is_empty() {
        crate::set_private_file_mode(&temporary)
            .map_err(|error| io::Error::other(error.to_string()))?;
    }
    file.write_all(format!("{body}\n").as_bytes())?;
    drop(file);
    fs::rename(&temporary, path)
}

/// Record the harness, or say on stderr why it could not be. A run whose
/// harness is unrecorded still runs; only a stop that bypasses its worker
/// cannot reach the harness.
pub(crate) fn record_or_warn(path: &Path, run_id: &str, pid: u32) {
    if let Err(error) = record(path, run_id, pid) {
        eprintln!(
            "ostrom: could not record harness process {pid} at {}: {error}",
            path.display()
        );
    }
}

/// The run id and harness a record names, when it is whole.
pub(crate) fn read(path: &Path) -> Option<(String, RecordedHarness)> {
    let record = serde_json::from_slice::<Value>(&fs::read(path).ok()?).ok()?;
    let number = |key: &str| record.get(key).and_then(Value::as_u64);
    Some((
        record.get("run_id")?.as_str()?.to_owned(),
        RecordedHarness {
            pid: u32::try_from(number("pid")?).ok()?,
            process_group_id: u32::try_from(number("process_group_id")?).ok()?,
            start_time: number("process_start_time")?,
        },
    ))
}

/// Every whole record, with where it is.
pub(crate) fn all(state: &Path) -> Vec<(PathBuf, String, RecordedHarness)> {
    let Ok(entries) = fs::read_dir(directory(state)) else {
        return Vec::new();
    };
    let mut paths = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths
        .into_iter()
        .filter_map(|path| read(&path).map(|(run_id, harness)| (path, run_id, harness)))
        .collect()
}

/// The record naming `run_id`, and where it is.
pub(crate) fn find(state: &Path, run_id: &str) -> Option<(PathBuf, RecordedHarness)> {
    all(state)
        .into_iter()
        .find(|(_, recorded, _)| recorded == run_id)
        .map(|(path, _, harness)| (path, harness))
}

/// Remove a record. One already gone is not an error.
pub(crate) fn remove(path: &Path) {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => eprintln!(
            "ostrom: could not remove harness record {}: {error}",
            path.display()
        ),
        _ => {}
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        fs,
        os::unix::fs::PermissionsExt as _,
        process::{Command, Stdio},
    };

    use tempfile::tempdir;

    use super::{find, read, record, record_path};

    /// The worker and the supervisor name the same file from the supervisor's
    /// pid, and the reaper finds it by run id with the identity `/proc` gave.
    /// The file is private, as claims and leases are.
    #[test]
    fn a_recorded_harness_is_found_by_its_run_with_its_identity() {
        let state = tempdir().expect("state root");
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .spawn()
            .expect("start a process to record");
        let path = record_path(state.path(), std::process::id()).expect("this process's identity");
        let recorded = record(&path, "run-a", child.id());
        let found = find(state.path(), "run-a");
        let other = find(state.path(), "run-b");
        let _ = child.kill();
        let _ = child.wait();
        recorded.expect("record the harness");
        let (found_path, harness) = found.expect("the record is found by its run");
        let mode = fs::metadata(&path)
            .expect("the record exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            (
                found_path == path,
                harness.pid,
                read(&path).map(|(run_id, _)| run_id),
                other,
                mode,
            ),
            (true, child.id(), Some("run-a".to_owned()), None, 0o600)
        );
    }
}
