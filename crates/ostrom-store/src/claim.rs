//! An exclusive claim file: the one way ostrom lets exactly one process at a
//! time act on something shared across processes (#635).
//!
//! `std::fs::File::lock` needs Rust 1.89, above the MSRV, and unsafe code is
//! forbidden, so a raw `flock` is out. Exclusivity is `OpenOptions::create_new`
//! instead: of every process that tries to create the file, exactly one
//! succeeds. The file names its holder's pid and `/proc` start time, so a claim
//! whose holder has died is recognisable as stale and is taken over rather than
//! blocking every later process for ever. Two things use it: the lease
//! mutation guard (`lease.rs`) and the stall reaper's claim on a run
//! (`stalls.rs`). One implementation, one stale-recovery test (principle 6).
//!
//! Taking over a stale claim renames it aside, checks that what was moved is
//! byte for byte what was judged stale, and only then creates the new claim
//! with `create_new`. If a racing process replaced the stale claim in between,
//! the fresh claim is linked back and the take-over is refused. What remains is
//! a window of a few system calls in which a fresh claim can be moved aside and
//! a third process can create another; it needs three processes contending for
//! one dead holder's claim at once.

use std::{
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{Map, Value};

use crate::{lease::read_process_identity, set_private_file_mode};

/// The field names a claim records its holder under. The reaper's claim uses
/// the names its spec gives; the lease guard uses plain ones.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HolderKeys {
    pub pid: &'static str,
    pub start_time: &'static str,
}

/// Whether a claim's holder is still the process that created it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Holder {
    Alive,
    /// Gone, or its pid now belongs to another process.
    Dead,
    /// `/proc` cannot answer, or the claim names no holder that can be read.
    /// Never taken over on a guess.
    Unknown,
}

/// A claim this process holds. Dropping it leaves the file in place: the
/// reaper's claim outlives a stop it could not confirm. [`Claim::remove`]
/// releases it.
#[derive(Debug)]
pub(crate) struct Claim {
    path: PathBuf,
    record: Map<String, Value>,
}

impl Claim {
    pub(crate) fn record(&self) -> &Map<String, Value> {
        &self.record
    }

    pub(crate) fn remove(self) -> io::Result<()> {
        match fs::remove_file(&self.path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }
}

/// Why a claim could not be had.
#[derive(Debug)]
pub(crate) enum ClaimError {
    /// Another holder has it: alive, unknown, or it moved while being taken.
    Held,
    Io(io::Error),
}

/// A claim as it is on disk: its exact bytes and, when they parse, its record.
#[derive(Debug, Clone)]
pub(crate) struct ClaimFile {
    bytes: Vec<u8>,
    record: Option<Map<String, Value>>,
}

impl ClaimFile {
    pub(crate) fn record(&self) -> Option<&Map<String, Value>> {
        self.record.as_ref()
    }

    pub(crate) fn holder(&self, keys: HolderKeys) -> Holder {
        let (pid, start_time) = match &self.record {
            Some(record) => (
                record.get(keys.pid).and_then(Value::as_u64),
                record.get(keys.start_time).and_then(Value::as_u64),
            ),
            // A guard written before #635 held `<pid>.guard` and no start time.
            None => (
                String::from_utf8_lossy(&self.bytes)
                    .trim()
                    .strip_suffix(".guard")
                    .and_then(|pid| pid.parse().ok()),
                None,
            ),
        };
        let Some(pid) = pid.and_then(|pid| u32::try_from(pid).ok()) else {
            return Holder::Unknown;
        };
        holder_liveness(pid, start_time)
    }
}

fn holder_liveness(pid: u32, start_time: Option<u64>) -> Holder {
    match read_process_identity(pid) {
        Ok(Some(observed)) => {
            if matches!(observed.state, 'Z' | 'X')
                || start_time.is_some_and(|start_time| observed.start_time != start_time)
            {
                Holder::Dead
            } else {
                Holder::Alive
            }
        }
        Ok(None) => Holder::Dead,
        Err(_) => Holder::Unknown,
    }
}

/// Read a claim. `Ok(None)` when there is none.
pub(crate) fn read(path: &Path) -> io::Result<Option<ClaimFile>> {
    match fs::read(path) {
        Ok(bytes) => {
            let record = serde_json::from_slice::<Map<String, Value>>(&bytes).ok();
            Ok(Some(ClaimFile { bytes, record }))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Create the claim, holding `payload`, or learn that another process has it.
pub(crate) fn create(
    path: &Path,
    keys: HolderKeys,
    payload: Map<String, Value>,
) -> Result<Claim, ClaimError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(ClaimError::Io)?;
    }
    let mut record = payload;
    let pid = std::process::id();
    record.insert(keys.pid.to_owned(), Value::from(pid));
    record.insert(
        keys.start_time.to_owned(),
        read_process_identity(pid)
            .ok()
            .flatten()
            .map_or(Value::Null, |identity| Value::from(identity.start_time)),
    );
    let mut bytes = serde_json::to_vec(&record).expect("a claim record serializes");
    bytes.push(b'\n');
    let mut file = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Err(ClaimError::Held);
        }
        Err(error) => return Err(ClaimError::Io(error)),
    };
    if let Err(error) = file.write_all(&bytes).and_then(|()| file.flush()) {
        let _ = fs::remove_file(path);
        return Err(ClaimError::Io(error));
    }
    let _ = set_private_file_mode(path);
    Ok(Claim {
        path: path.to_path_buf(),
        record,
    })
}

/// Create the claim, or take it over when the process holding it has died.
/// A take-over keeps the stale claim's payload, so what its holder decided is
/// carried out rather than decided again.
pub(crate) fn create_or_take_over(
    path: &Path,
    keys: HolderKeys,
    payload: Map<String, Value>,
) -> Result<Claim, ClaimError> {
    match create(path, keys, payload.clone()) {
        Err(ClaimError::Held) => {}
        other => return other,
    }
    let Some(stale) = read(path).map_err(ClaimError::Io)? else {
        // Released between the two calls: one more ordinary attempt.
        return create(path, keys, payload);
    };
    if stale.holder(keys) != Holder::Dead {
        return Err(ClaimError::Held);
    }
    take_over(path, keys, &stale)
}

/// Take over `stale`, which the caller judged to belong to a dead holder.
pub(crate) fn take_over(
    path: &Path,
    keys: HolderKeys,
    stale: &ClaimFile,
) -> Result<Claim, ClaimError> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let aside = path.with_file_name(format!(
        ".{}.{}.{nanos}.stale",
        path.file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
        std::process::id()
    ));
    match fs::rename(path, &aside) {
        Ok(()) => {}
        // Another process took it first.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(ClaimError::Held),
        Err(error) => return Err(ClaimError::Io(error)),
    }
    let moved = fs::read(&aside).map_err(ClaimError::Io)?;
    if moved != stale.bytes {
        // A racing process replaced the stale claim before the rename: put
        // its fresh claim back and leave it to that process.
        let _ = fs::hard_link(&aside, path);
        let _ = fs::remove_file(&aside);
        return Err(ClaimError::Held);
    }
    let _ = fs::remove_file(&aside);
    let mut payload = stale.record.clone().unwrap_or_default();
    payload.remove(keys.pid);
    payload.remove(keys.start_time);
    create(path, keys, payload)
}

#[cfg(test)]
mod tests {
    use serde_json::Map;
    use tempfile::tempdir;

    use super::{ClaimError, HolderKeys, create};

    const KEYS: HolderKeys = HolderKeys {
        pid: "holder_pid",
        start_time: "holder_start_time",
    };

    /// Exclusivity itself. Stale recovery has its one test in `lease.rs`,
    /// through the lease guard that uses it.
    #[test]
    fn a_claim_is_created_exactly_once() {
        let fixture = tempdir().expect("claim fixture");
        let path = fixture.path().join("nested/run.claim");
        let first = create(&path, KEYS, Map::new());
        let second = create(&path, KEYS, Map::new());
        assert!(first.is_ok());
        assert!(matches!(second, Err(ClaimError::Held)));
        first.expect("first claim").remove().expect("remove claim");
        assert!(!path.exists());
    }
}
