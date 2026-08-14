use std::{fs, path::Path};

use serde::{Deserialize, Serialize};

use crate::{StoreError, io_error, set_private_file_mode};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseRecord {
    pub owner: String,
    pub started_at: u64,
    pub expires_at: u64,
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
