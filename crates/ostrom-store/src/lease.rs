use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{StoreError, io_error, set_private_file_mode};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseRecord {
    pub owner: String,
    pub started_at: u64,
    pub expires_at: u64,
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
        Ok(())
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
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| io_error("create lease directory", parent, error))?;
    }
    let mut bytes = serde_json::to_vec(lease).expect("lease serializes");
    bytes.push(b'\n');
    fs::write(path, bytes).map_err(|error| io_error("write lease", path, error))?;
    set_private_file_mode(path)
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
    };
    let bytes = lease_bytes(&record);
    if install_exclusive(&path, &bytes) {
        return Ok(bytes);
    }
    let held = read_lease(&path)
        .map_err(|_| LeaseActionError::HeldOrUnreadable)?
        .ok_or(LeaseActionError::HeldOrUnreadable)?;
    if now < held.expires_at {
        return Err(LeaseActionError::Held);
    }

    let guard_path = state_root.join(format!(".{name}.guard"));
    let _guard = LeaseGuard::acquire(&guard_path).ok_or(LeaseActionError::ReclamationInProgress)?;
    let held = read_lease(&path)
        .map_err(|_| LeaseActionError::ChangedDuringReclamation)?
        .ok_or(LeaseActionError::ChangedDuringReclamation)?;
    if now < held.expires_at {
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
    use tempfile::tempdir;

    use super::{LeaseRecord, read_lease, write_lease};

    #[test]
    fn lease_matches_bash_field_order() {
        let fixture = tempdir().expect("temp dir");
        let path = fixture.path().join("builder.lease");
        let lease = LeaseRecord {
            owner: "builder-synthetic".to_owned(),
            started_at: 10,
            expires_at: 20,
        };
        write_lease(&path, &lease).expect("write lease");
        assert_eq!(read_lease(&path).expect("read lease"), Some(lease));
        assert_eq!(
            std::fs::read_to_string(path).expect("read bytes"),
            "{\"owner\":\"builder-synthetic\",\"started_at\":10,\"expires_at\":20}\n"
        );
    }
}
