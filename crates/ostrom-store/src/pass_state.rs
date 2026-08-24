use std::{fs, path::Path};

use crate::{StoreError, io_error, set_private_file_mode};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassState {
    pub role_id: String,
    pub wake: u64,
    pub dispatchability_hash: Option<String>,
}

pub fn read_pass_state(root: &Path, role: &str) -> Result<Option<PassState>, StoreError> {
    validate_role(role)?;
    let id_path = root.join(format!("{role}-pass-id"));
    let wake_path = root.join(format!("{role}-wake-counter"));
    if !id_path.exists() && !wake_path.exists() {
        return Ok(None);
    }
    let role_id = fs::read_to_string(&id_path)
        .map_err(|error| io_error("read pass id", &id_path, error))?
        .trim_end()
        .to_owned();
    if role_id.len() != 8
        || !role_id
            .chars()
            .all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase())
    {
        return Err(StoreError::MalformedPassState {
            role: role.to_owned(),
            message: "role id must be eight lowercase hexadecimal characters".to_owned(),
        });
    }
    let wake_text = fs::read_to_string(&wake_path)
        .map_err(|error| io_error("read wake counter", &wake_path, error))?;
    let wake = wake_text
        .trim_end()
        .parse()
        .map_err(|_| StoreError::MalformedPassState {
            role: role.to_owned(),
            message: "wake counter must be an unsigned integer".to_owned(),
        })?;
    let hash_path = root.join(format!("{role}-dispatchability-hash"));
    let dispatchability_hash = if hash_path.exists() {
        let hash = fs::read_to_string(&hash_path)
            .map_err(|error| io_error("read dispatchability hash", &hash_path, error))?
            .trim_end()
            .to_owned();
        if hash.len() != 64
            || !hash
                .chars()
                .all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase())
        {
            return Err(StoreError::MalformedPassState {
                role: role.to_owned(),
                message: "dispatchability hash must be a lowercase SHA-256 digest".to_owned(),
            });
        }
        Some(hash)
    } else {
        None
    };
    Ok(Some(PassState {
        role_id,
        wake,
        dispatchability_hash,
    }))
}

pub fn write_pass_state(root: &Path, role: &str, state: &PassState) -> Result<(), StoreError> {
    validate_role(role)?;
    if state.role_id.len() != 8
        || !state
            .role_id
            .chars()
            .all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase())
    {
        return Err(StoreError::MalformedPassState {
            role: role.to_owned(),
            message: "role id must be eight lowercase hexadecimal characters".to_owned(),
        });
    }
    if let Some(hash) = &state.dispatchability_hash {
        validate_dispatchability_hash(role, hash)?;
    }
    fs::create_dir_all(root).map_err(|error| io_error("create state directory", root, error))?;
    atomic_text(
        &root.join(format!("{role}-pass-id")),
        &format!("{}\n", state.role_id),
    )?;
    atomic_text(
        &root.join(format!("{role}-wake-counter")),
        &format!("{}\n", state.wake),
    )?;
    if let Some(hash) = &state.dispatchability_hash {
        atomic_text(
            &root.join(format!("{role}-dispatchability-hash")),
            &format!("{hash}\n"),
        )?;
    }
    Ok(())
}

fn validate_dispatchability_hash(role: &str, hash: &str) -> Result<(), StoreError> {
    if hash.len() == 64
        && hash
            .chars()
            .all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase())
    {
        Ok(())
    } else {
        Err(StoreError::MalformedPassState {
            role: role.to_owned(),
            message: "dispatchability hash must be a lowercase SHA-256 digest".to_owned(),
        })
    }
}

fn validate_role(role: &str) -> Result<(), StoreError> {
    if role.is_empty()
        || !role
            .chars()
            .all(|character| character.is_ascii_lowercase() || character == '-')
    {
        return Err(StoreError::MalformedPassState {
            role: role.to_owned(),
            message: "role must be a safe lowercase name".to_owned(),
        });
    }
    Ok(())
}

fn atomic_text(path: &Path, contents: &str) -> Result<(), StoreError> {
    let temporary = path.with_extension(format!("tmp.{}", std::process::id()));
    fs::write(&temporary, contents)
        .map_err(|error| io_error("write temporary pass state", &temporary, error))?;
    set_private_file_mode(&temporary)?;
    fs::rename(&temporary, path).map_err(|error| io_error("install pass state", path, error))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{PassState, read_pass_state, write_pass_state};

    #[test]
    fn pass_state_round_trips_existing_files() {
        let fixture = tempdir().expect("temp dir");
        let expected = PassState {
            role_id: "0123abcd".to_owned(),
            wake: 9,
            dispatchability_hash: Some(
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
            ),
        };
        write_pass_state(fixture.path(), "builder", &expected).expect("write state");
        assert_eq!(
            read_pass_state(fixture.path(), "builder").expect("read state"),
            Some(expected)
        );
    }

    #[test]
    fn pre_digest_pass_state_remains_readable() {
        let fixture = tempdir().expect("temp dir");
        fs::write(fixture.path().join("builder-pass-id"), "0123abcd\n").expect("write id");
        fs::write(fixture.path().join("builder-wake-counter"), "9\n").expect("write wake");
        assert_eq!(
            read_pass_state(fixture.path(), "builder").expect("read state"),
            Some(PassState {
                role_id: "0123abcd".to_owned(),
                wake: 9,
                dispatchability_hash: None,
            })
        );
    }
}
