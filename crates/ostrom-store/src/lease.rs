use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::{StoreError, io_error, set_private_file_mode};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseRecord {
    pub owner: String,
    pub started_at: u64,
    pub expires_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_group_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_start_time: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProcessIdentity {
    pub pid: u32,
    pub state: char,
    pub process_group_id: u32,
    pub session_id: u32,
    pub start_time: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessLiveness {
    Live,
    NotLive,
    Unknown,
}

#[derive(Debug, Error)]
pub enum LeaseActionError {
    #[error("mandate lease: lease name must be a safe file name")]
    UnsafeName,
    #[error("mandate lease: current time must be Unix seconds")]
    InvalidNow,
    #[error("mandate lease: ttl-seconds must be a positive integer")]
    InvalidTtl,
    #[error("mandate lease: lease is held or unreadable")]
    HeldOrUnreadable,
    #[error("mandate lease: lease is held")]
    Held,
    #[error("mandate lease: lease reclamation is already in progress")]
    ReclamationInProgress,
    #[error("mandate lease: lease changed during reclamation")]
    ChangedDuringReclamation,
    #[error("mandate lease: lease was acquired concurrently")]
    AcquiredConcurrently,
    #[error("mandate lease: lease mutation is already in progress")]
    MutationInProgress,
    #[error("mandate lease: no readable lease")]
    NoReadableLease,
    #[error("mandate lease: owner mismatch")]
    OwnerMismatch,
}

impl LeaseActionError {
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::UnsafeName | Self::InvalidNow | Self::InvalidTtl => 2,
            Self::HeldOrUnreadable
            | Self::Held
            | Self::ReclamationInProgress
            | Self::ChangedDuringReclamation
            | Self::AcquiredConcurrently
            | Self::MutationInProgress
            | Self::NoReadableLease
            | Self::OwnerMismatch => 3,
        }
    }
}

impl LeaseRecord {
    pub(crate) fn validate(&self, name: &str) -> Result<(), StoreError> {
        if self.owner.is_empty() {
            return Err(StoreError::MalformedLease {
                name: name.to_owned(),
                message: "owner must not be empty".to_owned(),
            });
        }
        if self.expires_at < self.started_at {
            return Err(StoreError::MalformedLease {
                name: name.to_owned(),
                message: "expires_at precedes started_at".to_owned(),
            });
        }
        let process_fields = [
            self.pid.is_some(),
            self.process_group_id.is_some(),
            self.process_start_time.is_some(),
        ];
        if process_fields.iter().any(|present| *present)
            && !process_fields.iter().all(|present| *present)
        {
            return Err(StoreError::MalformedLease {
                name: name.to_owned(),
                message:
                    "process identity must include pid, process_group_id and process_start_time"
                        .to_owned(),
            });
        }
        if self.pid == Some(0) || self.process_group_id == Some(0) {
            return Err(StoreError::MalformedLease {
                name: name.to_owned(),
                message: "process identity must use positive ids".to_owned(),
            });
        }
        Ok(())
    }

    #[must_use]
    pub(crate) fn process_identity(&self) -> Option<(u32, u32, u64)> {
        Some((self.pid?, self.process_group_id?, self.process_start_time?))
    }

    #[must_use]
    pub(crate) fn is_live(&self, now: u64) -> bool {
        self.is_live_at(now, Path::new("/proc"))
    }

    #[must_use]
    pub(crate) fn is_live_at(&self, now: u64, proc_root: &Path) -> bool {
        self.process_identity()
            .map_or(
                self.expires_at > now,
                |(pid, _, start_time)| match process_identity_is_live_at(proc_root, pid, start_time)
                {
                    ProcessLiveness::Live => true,
                    ProcessLiveness::NotLive => false,
                    ProcessLiveness::Unknown => self.expires_at > now,
                },
            )
    }
}

pub(crate) fn read_process_identity(pid: u32) -> io::Result<Option<ProcessIdentity>> {
    read_process_identity_at(Path::new("/proc"), pid)
}

fn read_process_identity_at(proc_root: &Path, pid: u32) -> io::Result<Option<ProcessIdentity>> {
    let stat_path = proc_root.join(pid.to_string()).join("stat");
    let stat = match fs::read_to_string(&stat_path) {
        Ok(stat) => stat,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::read_to_string(proc_root.join("self/stat"))?;
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let fields = stat
        .rsplit_once(')')
        .ok_or_else(invalid_process_stat)?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    let identity = ProcessIdentity {
        pid,
        state: fields
            .first()
            .and_then(|field| field.chars().next())
            .ok_or_else(invalid_process_stat)?,
        process_group_id: fields
            .get(2)
            .ok_or_else(invalid_process_stat)?
            .parse()
            .map_err(|_| invalid_process_stat())?,
        session_id: fields
            .get(3)
            .ok_or_else(invalid_process_stat)?
            .parse()
            .map_err(|_| invalid_process_stat())?,
        start_time: fields
            .get(19)
            .ok_or_else(invalid_process_stat)?
            .parse()
            .map_err(|_| invalid_process_stat())?,
    };
    Ok(Some(identity))
}

fn invalid_process_stat() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid process stat")
}

pub(crate) fn process_identity_is_live(pid: u32, start_time: u64) -> ProcessLiveness {
    process_identity_is_live_at(Path::new("/proc"), pid, start_time)
}

pub(crate) fn process_identity_is_live_at(
    proc_root: &Path,
    pid: u32,
    start_time: u64,
) -> ProcessLiveness {
    match read_process_identity_at(proc_root, pid) {
        Ok(Some(observed))
            if observed.start_time == start_time && !matches!(observed.state, 'Z' | 'X') =>
        {
            ProcessLiveness::Live
        }
        Ok(Some(_) | None) => ProcessLiveness::NotLive,
        Err(_) => ProcessLiveness::Unknown,
    }
}

pub fn read_lease(path: &Path) -> Result<Option<LeaseRecord>, StoreError> {
    if !path.exists() {
        return Ok(None);
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("lease");
    let contents = fs::read_to_string(path).map_err(|error| io_error("read lease", path, error))?;
    let record: LeaseRecord =
        serde_json::from_str(&contents).map_err(|error| StoreError::MalformedLease {
            name: name.to_owned(),
            message: error.to_string(),
        })?;
    record.validate(name)?;
    Ok(Some(record))
}

pub fn write_lease(path: &Path, lease: &LeaseRecord) -> Result<(), StoreError> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("lease");
    lease.validate(name)?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|error| io_error("create lease directory", parent, error))?;
    let mut bytes = serde_json::to_vec(lease).expect("lease serializes");
    bytes.push(b'\n');
    let mut temporary = NamedTempFile::new_in(parent)
        .map_err(|error| io_error("create temporary lease", path, error))?;
    temporary
        .write_all(&bytes)
        .map_err(|error| io_error("write temporary lease", path, error))?;
    temporary
        .flush()
        .map_err(|error| io_error("flush temporary lease", path, error))?;
    set_private_file_mode(temporary.path())?;
    temporary
        .persist(path)
        .map_err(|error| io_error("replace lease", path, error.error))?;
    Ok(())
}

pub fn validate_lease_name(name: &str) -> Result<(), LeaseActionError> {
    if name.is_empty()
        || !name
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphanumeric())
        || !name.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        })
    {
        return Err(LeaseActionError::UnsafeName);
    }
    Ok(())
}

pub fn acquire_lease(
    state_root: &Path,
    name: &str,
    owner: &str,
    now: u64,
    ttl: u64,
) -> Result<Vec<u8>, LeaseActionError> {
    validate_lease_name(name)?;
    if ttl == 0 {
        return Err(LeaseActionError::InvalidTtl);
    }
    fs::create_dir_all(state_root).map_err(|_| LeaseActionError::HeldOrUnreadable)?;
    let path = state_root.join(name);
    let record = LeaseRecord {
        owner: owner.to_owned(),
        started_at: now,
        expires_at: now.saturating_add(ttl),
        pid: None,
        process_group_id: None,
        process_start_time: None,
    };
    let bytes = lease_bytes(&record);
    if install_exclusive(&path, &bytes) {
        return Ok(bytes);
    }
    let held = read_lease(&path)
        .map_err(|_| LeaseActionError::HeldOrUnreadable)?
        .ok_or(LeaseActionError::HeldOrUnreadable)?;
    if held.is_live(now) {
        return Err(LeaseActionError::Held);
    }

    let guard_path = state_root.join(format!(".{name}.guard"));
    let _guard = LeaseGuard::acquire(&guard_path).ok_or(LeaseActionError::ReclamationInProgress)?;
    let held = read_lease(&path)
        .map_err(|_| LeaseActionError::ChangedDuringReclamation)?
        .ok_or(LeaseActionError::ChangedDuringReclamation)?;
    if held.is_live(now) {
        return Err(LeaseActionError::Held);
    }
    fs::remove_file(&path).map_err(|_| LeaseActionError::ChangedDuringReclamation)?;
    if install_exclusive(&path, &bytes) {
        Ok(bytes)
    } else {
        Err(LeaseActionError::AcquiredConcurrently)
    }
}

pub fn release_lease(state_root: &Path, name: &str, owner: &str) -> Result<(), LeaseActionError> {
    validate_lease_name(name)?;
    let path = state_root.join(name);
    let guard_path = state_root.join(format!(".{name}.guard"));
    let _guard = LeaseGuard::acquire(&guard_path).ok_or(LeaseActionError::MutationInProgress)?;
    let held = read_lease(&path)
        .map_err(|_| LeaseActionError::NoReadableLease)?
        .ok_or(LeaseActionError::NoReadableLease)?;
    if held.owner != owner {
        return Err(LeaseActionError::OwnerMismatch);
    }
    fs::remove_file(path).map_err(|_| LeaseActionError::NoReadableLease)
}

pub fn lease_status(state_root: &Path, name: &str) -> Result<Vec<u8>, LeaseActionError> {
    validate_lease_name(name)?;
    let path = state_root.join(name);
    read_lease(&path)
        .map_err(|_| LeaseActionError::NoReadableLease)?
        .ok_or(LeaseActionError::NoReadableLease)?;
    let contents = fs::read_to_string(path).map_err(|_| LeaseActionError::NoReadableLease)?;
    let mut output = contents.trim_end_matches('\n').as_bytes().to_vec();
    output.push(b'\n');
    Ok(output)
}

/// RAII ownership for a lease acquired or adopted through the public lease API.
///
/// Acquisition remains exclusive-create in `acquire_lease`; this type only
/// adds reliable ordinary-error and unwind cleanup around that contract.
#[derive(Debug)]
pub struct OwnedLease {
    state_root: PathBuf,
    name: String,
    owner: String,
    armed: bool,
}

impl OwnedLease {
    pub fn acquire(
        state_root: &Path,
        name: &str,
        owner: &str,
        now: u64,
        ttl: u64,
    ) -> Result<Self, LeaseActionError> {
        acquire_lease(state_root, name, owner, now, ttl)?;
        Ok(Self {
            state_root: state_root.to_path_buf(),
            name: name.to_owned(),
            owner: owner.to_owned(),
            armed: true,
        })
    }

    pub fn adopt(state_root: &Path, name: &str, owner: &str) -> Result<Self, LeaseActionError> {
        validate_lease_name(name)?;
        let bytes = lease_status(state_root, name)?;
        let record: LeaseRecord =
            serde_json::from_slice(&bytes).map_err(|_| LeaseActionError::NoReadableLease)?;
        if record.owner != owner {
            return Err(LeaseActionError::OwnerMismatch);
        }
        Ok(Self {
            state_root: state_root.to_path_buf(),
            name: name.to_owned(),
            owner: owner.to_owned(),
            armed: true,
        })
    }

    pub fn release(&mut self) -> Result<(), LeaseActionError> {
        if !self.armed {
            return Ok(());
        }
        release_lease(&self.state_root, &self.name, &self.owner)?;
        self.armed = false;
        Ok(())
    }

    pub fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for OwnedLease {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

fn lease_bytes(record: &LeaseRecord) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(record).expect("lease serializes");
    bytes.push(b'\n');
    bytes
}

fn install_exclusive(path: &Path, bytes: &[u8]) -> bool {
    let Ok(mut file) = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    else {
        return false;
    };
    if set_private_file_mode(path).is_err() || file.write_all(bytes).is_err() {
        let _ = fs::remove_file(path);
        return false;
    }
    true
}

struct LeaseGuard {
    path: PathBuf,
}

impl LeaseGuard {
    fn acquire(path: &Path) -> Option<Self> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .ok()?;
        if set_private_file_mode(path).is_err()
            || writeln!(file, "{}.guard", std::process::id()).is_err()
        {
            let _ = fs::remove_file(path);
            return None;
        }
        Some(Self {
            path: path.to_path_buf(),
        })
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::Path,
        process::Command,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::{Duration, Instant},
    };

    use tempfile::tempdir;

    use super::{
        LeaseRecord, ProcessLiveness, process_identity_is_live, process_identity_is_live_at,
        read_lease, read_process_identity, write_lease,
    };

    #[test]
    fn lease_matches_bash_field_order() {
        let fixture = tempdir().expect("temp dir");
        let path = fixture.path().join("builder.lease");
        let lease = LeaseRecord {
            owner: "builder-synthetic".to_owned(),
            started_at: 10,
            expires_at: 20,
            pid: None,
            process_group_id: None,
            process_start_time: None,
        };
        write_lease(&path, &lease).expect("write lease");
        assert_eq!(read_lease(&path).expect("read lease"), Some(lease));
        assert_eq!(
            std::fs::read_to_string(path).expect("read bytes"),
            "{\"owner\":\"builder-synthetic\",\"started_at\":10,\"expires_at\":20}\n"
        );
    }

    #[test]
    fn process_identity_round_trips_and_rejects_a_recycled_pid() {
        let fixture = tempdir().expect("temp dir");
        let path = fixture.path().join("implementer-item-placeholder.lease");
        let identity = read_process_identity(std::process::id())
            .expect("read current process identity")
            .expect("current process identity");
        let lease = LeaseRecord {
            owner: "implementer-placeholder".to_owned(),
            started_at: 10,
            expires_at: 20,
            pid: Some(identity.pid),
            process_group_id: Some(identity.process_group_id),
            process_start_time: Some(identity.start_time),
        };
        write_lease(&path, &lease).expect("write process lease");
        assert_eq!(read_lease(&path).expect("read process lease"), Some(lease));
        assert_eq!(
            process_identity_is_live(identity.pid, identity.start_time),
            ProcessLiveness::Live
        );
        assert_eq!(
            process_identity_is_live(identity.pid, identity.start_time + 1),
            ProcessLiveness::NotLive
        );
    }

    #[test]
    fn exited_unreaped_process_is_not_live() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn short-lived child");
        let pid = child.id();
        let deadline = Instant::now() + Duration::from_secs(5);
        let identity = loop {
            if let Ok(Some(identity)) = read_process_identity(pid)
                && matches!(identity.state, 'Z' | 'X')
            {
                break Some(identity);
            }
            if Instant::now() >= deadline {
                break None;
            }
            thread::sleep(Duration::from_millis(10));
        };
        let liveness =
            identity.map(|identity| process_identity_is_live(identity.pid, identity.start_time));
        let status = child.wait().expect("reap short-lived child");
        assert!(status.success());
        assert!(identity.is_some(), "child did not enter a terminal state");
        assert_eq!(liveness, Some(ProcessLiveness::NotLive));
    }

    #[test]
    fn process_liveness_uses_procfs_without_invoking_a_kill_command() {
        let fixture = tempdir().expect("process information fixture");
        write_process_stat(fixture.path(), 42, 'S', 123);

        assert_eq!(
            process_identity_is_live_at(fixture.path(), 42, 123),
            ProcessLiveness::Live
        );
        assert_eq!(
            process_identity_is_live_at(fixture.path(), 42, 124),
            ProcessLiveness::NotLive
        );

        let production = include_str!("lease.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production lease source");
        assert!(!production.contains("/bin/kill"));
        assert!(!production.contains("Command::new(\"kill\")"));
    }

    #[test]
    fn terminal_missing_and_unreadable_process_entries_are_not_live() {
        let fixture = tempdir().expect("process information fixture");
        write_process_stat(fixture.path(), 42, 'Z', 123);
        assert_eq!(
            process_identity_is_live_at(fixture.path(), 42, 123),
            ProcessLiveness::NotLive
        );
        assert_eq!(
            process_identity_is_live_at(fixture.path(), 43, 123),
            ProcessLiveness::NotLive
        );

        fs::create_dir_all(fixture.path().join("44/stat")).expect("create unreadable stat entry");
        assert_eq!(
            process_identity_is_live_at(fixture.path(), 44, 123),
            ProcessLiveness::Unknown
        );
        assert_eq!(
            process_identity_is_live_at(&fixture.path().join("unavailable"), 45, 123),
            ProcessLiveness::Unknown
        );
    }

    #[test]
    fn lease_replacement_remains_readable_during_concurrent_reads() {
        let fixture = tempdir().expect("temp dir");
        let path = fixture.path().join("implementer-item-placeholder.lease");
        let mut lease = LeaseRecord {
            owner: "a".repeat(32_768),
            started_at: 10,
            expires_at: 20,
            pid: None,
            process_group_id: None,
            process_start_time: None,
        };
        write_lease(&path, &lease).expect("write initial lease");

        let reading = Arc::new(AtomicBool::new(true));
        let reader_path = path.clone();
        let reader_flag = Arc::clone(&reading);
        let reader = thread::spawn(move || {
            let mut unreadable = false;
            while reader_flag.load(Ordering::Relaxed) {
                unreadable |= read_lease(&reader_path).is_err();
            }
            unreadable |= read_lease(&reader_path).is_err();
            unreadable
        });
        for index in 0..200 {
            lease.owner = if index % 2 == 0 {
                "a".repeat(32_768)
            } else {
                "b".repeat(32_768)
            };
            write_lease(&path, &lease).expect("replace lease atomically");
        }
        reading.store(false, Ordering::Relaxed);
        assert!(!reader.join().expect("join lease reader"));
        assert_eq!(read_lease(&path).expect("read final lease"), Some(lease));
    }

    fn write_process_stat(root: &Path, pid: u32, state: char, start_time: u64) {
        let directory = root.join(pid.to_string());
        fs::create_dir_all(&directory).expect("create process entry");
        let self_directory = root.join("self");
        fs::create_dir_all(&self_directory).expect("create current process entry");
        fs::write(
            self_directory.join("stat"),
            "process information available\n",
        )
        .expect("write current process stat marker");
        let mut fields = vec!["0".to_owned(); 20];
        fields[0] = state.to_string();
        fields[2] = pid.to_string();
        fields[3] = pid.to_string();
        fields[19] = start_time.to_string();
        fs::write(
            directory.join("stat"),
            format!("{pid} (fixture process) {}\n", fields.join(" ")),
        )
        .expect("write process stat");
    }
}
