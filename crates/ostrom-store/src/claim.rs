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
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::{Map, Value};

use crate::{lease::read_process_identity, set_private_file_mode};

/// How long a claim whose holder cannot be verified is presumed to be in use:
/// an empty claim (its creator crashed between `create_new` and the write),
/// or one naming a live pid with no start time to prove it is the same
/// process. Past this it is stale. Every holder that can be verified writes
/// within milliseconds, and no reaper holds a claim this long without
/// recording its own start time.
const UNVERIFIED_HOLDER_GRACE: Duration = Duration::from_secs(120);

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
    /// Exactly what this process wrote: the file is still this claim only
    /// while it holds these bytes.
    bytes: Vec<u8>,
}

impl Claim {
    pub(crate) fn record(&self) -> &Map<String, Value> {
        &self.record
    }

    /// Whether the file is still the one this process wrote. A process that
    /// judged this one dead may have taken it over since.
    fn owned(&self) -> bool {
        fs::read(&self.path).is_ok_and(|bytes| !bytes.is_empty() || self.bytes.is_empty())
    }

    /// Release the claim, unless another process has taken it over: its claim
    /// is left alone.
    pub(crate) fn remove(self) -> io::Result<()> {
        if !self.owned() {
            return Ok(());
        }
        match fs::remove_file(&self.path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }

    /// Record `value` under `key`, replacing the file atomically. Refused
    /// when the file is no longer this claim.
    pub(crate) fn set(&mut self, key: &str, value: Value) -> io::Result<()> {
        if !self.owned() {
            return Err(io::Error::other("the claim was taken over"));
        }
        let mut record = self.record.clone();
        record.insert(key.to_owned(), value);
        let bytes = record_bytes(&record);
        let temporary = self.path.with_file_name(format!(
            ".{}.{}.write",
            file_name(&self.path),
            std::process::id()
        ));
        fs::write(&temporary, &bytes)?;
        let _ = set_private_file_mode(&temporary);
        if let Err(error) = fs::rename(&temporary, &self.path) {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        self.record = record;
        self.bytes = bytes;
        Ok(())
    }
}

fn record_bytes(record: &Map<String, Value>) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(record).expect("a claim record serializes");
    bytes.push(b'\n');
    bytes
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned())
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
    /// How long ago the file was last written, when that can be read.
    age: Option<Duration>,
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
        let past_grace = self
            .age
            .is_some_and(|age| age > UNVERIFIED_HOLDER_GRACE * 1_000);
        let Some(pid) = pid.and_then(|pid| u32::try_from(pid).ok()) else {
            // Empty or half-written: its creator crashed mid-write, or is
            // writing now.
            return if past_grace {
                Holder::Dead
            } else {
                Holder::Unknown
            };
        };
        match holder_liveness(pid, start_time) {
            // A live pid with no start time may be a recycled one: it is not
            // trusted as the holder, and past the grace it is presumed gone.
            Holder::Alive if start_time.is_none() => {
                if past_grace {
                    Holder::Dead
                } else {
                    Holder::Alive
                }
            }
            holder => holder,
        }
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
            let age = fs::metadata(path)
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| SystemTime::now().duration_since(modified).ok());
            Ok(Some(ClaimFile { bytes, record, age }))
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
    let bytes = record_bytes(&record);
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
        bytes,
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
        file_name(path),
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
    use std::{
        fs,
        time::{Duration, SystemTime},
    };

    use serde_json::{Map, Value, json};
    use tempfile::tempdir;

    use super::{ClaimError, Holder, HolderKeys, create, create_or_take_over, read};

    const KEYS: HolderKeys = HolderKeys {
        pid: "holder_pid",
        start_time: "holder_start_time",
    };

    /// Exclusivity itself. Stale recovery of a dead holder has its one test in
    /// `lease.rs`, through the lease guard that uses it.
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

    fn age(path: &std::path::Path, seconds: u64) {
        fs::File::options()
            .write(true)
            .open(path)
            .expect("open claim")
            .set_modified(SystemTime::now() - Duration::from_secs(seconds))
            .expect("age claim");
    }

    /// #635 review nits: a holder that cannot be verified (an empty claim, a
    /// live pid with no start time) is never trusted as alive, and past the
    /// grace it is stale.
    #[test]
    fn an_unverifiable_holder_is_never_alive_and_is_stale_past_the_grace() {
        let fixture = tempdir().expect("claim fixture");
        let empty = fixture.path().join("empty.claim");
        fs::write(&empty, "").expect("write an empty claim");
        let fresh_empty = read(&empty).expect("read").expect("claim").holder(KEYS);
        age(&empty, 600);
        let old_empty = read(&empty).expect("read").expect("claim").holder(KEYS);

        let unverified = fixture.path().join("unverified.claim");
        fs::write(
            &unverified,
            json!({"holder_pid": std::process::id(), "holder_start_time": Value::Null}).to_string(),
        )
        .expect("write a claim with no start time");
        let fresh_unverified = read(&unverified)
            .expect("read")
            .expect("claim")
            .holder(KEYS);
        age(&unverified, 600);
        let taken = create_or_take_over(&unverified, KEYS, Map::new()).is_ok();

        assert_eq!(
            (fresh_empty, old_empty, fresh_unverified, taken),
            (Holder::Unknown, Holder::Dead, Holder::Unknown, true)
        );
    }

    /// #635 review nit: a claim this process no longer owns (another took it
    /// over) is neither removed nor rewritten by it.
    #[test]
    fn a_claim_taken_over_by_another_is_left_alone() {
        let fixture = tempdir().expect("claim fixture");
        let path = fixture.path().join("run.claim");
        let mut claim = create(&path, KEYS, Map::new()).expect("claim");
        fs::write(&path, "{\"holder_pid\":1,\"holder_start_time\":1}\n")
            .expect("another process takes it over");
        let refused = claim.set("signalled_at", json!("now")).is_err();
        claim.remove().expect("remove is not an error");
        assert_eq!(
            (refused, fs::read_to_string(&path).expect("still there")),
            (
                true,
                "{\"holder_pid\":1,\"holder_start_time\":1}\n".to_owned()
            )
        );
    }
}
