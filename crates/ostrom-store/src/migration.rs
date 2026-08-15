use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::{fs::PermissionsExt, fs::symlink};

use serde_yaml::Value;

use crate::{OstromPaths, StoreError, io_error, read_lease};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationOutcome {
    Migrated,
    AlreadyMigrated,
    NothingToMigrate,
}

/// Move the legacy tree while keeping old Bash paths usable.
///
/// The legacy directory becomes a symlink to the state root. Configuration
/// entries inside that root are symlinks onward to the XDG config root. This
/// two-level pointer is deliberate: one old directory must continue to expose
/// files that now have two different owners until the Bash cutover happens.
pub fn migrate(
    legacy: &Path,
    destinations: &OstromPaths,
    now_epoch: u64,
) -> Result<MigrationOutcome, StoreError> {
    if legacy == destinations.config || legacy == destinations.state {
        return Err(StoreError::MigrationOverlap(legacy.display().to_string()));
    }
    if let Ok(target) = fs::read_link(legacy) {
        return if target == destinations.state {
            Ok(MigrationOutcome::AlreadyMigrated)
        } else {
            Err(StoreError::MigrationConflict(legacy.display().to_string()))
        };
    }
    if !legacy.exists() {
        return Ok(MigrationOutcome::NothingToMigrate);
    }

    refuse_held_leases(legacy, now_epoch)?;
    fs::create_dir_all(&destinations.config)
        .map_err(|error| io_error("create config directory", &destinations.config, error))?;
    fs::create_dir_all(&destinations.state)
        .map_err(|error| io_error("create state directory", &destinations.state, error))?;
    set_directory_mode(&destinations.config)?;
    set_directory_mode(&destinations.state)?;

    let secrets_path = legacy.join("secrets.yaml");
    let key_paths = if secrets_path.exists() {
        rewrite_secret_key_paths(&secrets_path, legacy, &destinations.config)?
    } else {
        Vec::new()
    };
    let key_roots: HashSet<PathBuf> = key_paths
        .iter()
        .filter_map(|path| path.strip_prefix(legacy).ok())
        .filter_map(|relative| relative.components().next())
        .map(|component| PathBuf::from(component.as_os_str()))
        .collect();

    let entries = fs::read_dir(legacy)
        .map_err(|error| io_error("read migration source", legacy, error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| io_error("read migration source", legacy, error))?;
    let mut config_entries = Vec::new();
    for entry in entries {
        let name = entry.file_name();
        let relative = PathBuf::from(&name);
        let is_config = is_config_entry(&relative) || key_roots.contains(&relative);
        let root = if is_config {
            config_entries.push(relative.clone());
            &destinations.config
        } else {
            &destinations.state
        };
        move_tree(&entry.path(), &root.join(&name))?;
    }

    for old_key in &key_paths {
        if let Ok(relative) = old_key.strip_prefix(legacy) {
            set_key_mode(&destinations.config.join(relative))?;
        }
    }

    if destinations.config != destinations.state {
        for relative in config_entries {
            let state_pointer = destinations.state.join(&relative);
            if state_pointer.exists() || state_pointer.symlink_metadata().is_ok() {
                return Err(StoreError::MigrationConflict(
                    state_pointer.display().to_string(),
                ));
            }
            create_symlink(&destinations.config.join(&relative), &state_pointer)?;
        }
    }

    fs::remove_dir(legacy)
        .map_err(|error| io_error("remove emptied legacy directory", legacy, error))?;
    create_symlink(&destinations.state, legacy)?;
    Ok(MigrationOutcome::Migrated)
}

fn refuse_held_leases(root: &Path, now_epoch: u64) -> Result<(), StoreError> {
    for entry in fs::read_dir(root).map_err(|error| io_error("scan leases", root, error))? {
        let entry = entry.map_err(|error| io_error("scan leases", root, error))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".lease") || !entry.path().is_file() {
            continue;
        }
        let lease = read_lease(&entry.path())?.ok_or_else(|| StoreError::MalformedLease {
            name: name.clone(),
            message: "lease disappeared during migration check".to_owned(),
        })?;
        if lease.expires_at > now_epoch {
            return Err(StoreError::LeaseHeld {
                name,
                owner: lease.owner,
            });
        }
    }
    Ok(())
}

fn is_config_entry(relative: &Path) -> bool {
    matches!(
        relative.to_str(),
        Some(
            "mandates.yaml"
                | "gate.yaml"
                | "secrets.yaml"
                | "config.yaml"
                | "rules.md"
                | "rules.d"
                | "roles"
        )
    )
}

fn rewrite_secret_key_paths(
    secrets_path: &Path,
    legacy: &Path,
    config: &Path,
) -> Result<Vec<PathBuf>, StoreError> {
    let contents = fs::read_to_string(secrets_path)
        .map_err(|error| io_error("read secrets", secrets_path, error))?;
    let mut document: Value =
        serde_yaml::from_str(&contents).map_err(|error| StoreError::Secrets(error.to_string()))?;
    let mut paths = Vec::new();
    rewrite_value(&mut document, legacy, config, &mut paths)?;
    let serialized =
        serde_yaml::to_string(&document).map_err(|error| StoreError::Secrets(error.to_string()))?;
    fs::write(secrets_path, serialized)
        .map_err(|error| io_error("rewrite secrets", secrets_path, error))?;
    set_key_mode(secrets_path)?;
    Ok(paths)
}

fn rewrite_value(
    value: &mut Value,
    legacy: &Path,
    config: &Path,
    paths: &mut Vec<PathBuf>,
) -> Result<(), StoreError> {
    match value {
        Value::Mapping(mapping) => {
            for (key, child) in mapping {
                if key.as_str() == Some("private_key_path") {
                    let Some(raw) = child.as_str() else {
                        return Err(StoreError::Secrets(
                            "private_key_path must be a string".to_owned(),
                        ));
                    };
                    let old = PathBuf::from(raw);
                    paths.push(old.clone());
                    if let Ok(relative) = old.strip_prefix(legacy) {
                        *child = Value::String(config.join(relative).display().to_string());
                    }
                } else {
                    rewrite_value(child, legacy, config, paths)?;
                }
            }
        }
        Value::Sequence(sequence) => {
            for child in sequence {
                rewrite_value(child, legacy, config, paths)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn move_tree(source: &Path, destination: &Path) -> Result<(), StoreError> {
    if destination.exists() || destination.symlink_metadata().is_ok() {
        return Err(StoreError::MigrationConflict(
            destination.display().to_string(),
        ));
    }
    match fs::rename(source, destination) {
        Ok(()) => return Ok(()),
        Err(error) if error.raw_os_error() != Some(18) => {
            return Err(io_error("move migration entry", source, error));
        }
        Err(_) => {}
    }
    let metadata = fs::symlink_metadata(source)
        .map_err(|error| io_error("inspect migration entry", source, error))?;
    if metadata.is_dir() {
        fs::create_dir(destination)
            .map_err(|error| io_error("create migration directory", destination, error))?;
        for child in fs::read_dir(source)
            .map_err(|error| io_error("read migration directory", source, error))?
        {
            let child =
                child.map_err(|error| io_error("read migration directory", source, error))?;
            move_tree(&child.path(), &destination.join(child.file_name()))?;
        }
        fs::remove_dir(source)
            .map_err(|error| io_error("remove migration directory", source, error))?;
    } else if metadata.file_type().is_symlink() {
        let target = fs::read_link(source)
            .map_err(|error| io_error("read migration symlink", source, error))?;
        create_symlink(&target, destination)?;
        fs::remove_file(source)
            .map_err(|error| io_error("remove migration symlink", source, error))?;
    } else {
        fs::copy(source, destination)
            .map_err(|error| io_error("copy migration file", source, error))?;
        fs::set_permissions(destination, metadata.permissions())
            .map_err(|error| io_error("preserve migration permissions", destination, error))?;
        fs::remove_file(source).map_err(|error| io_error("remove migrated file", source, error))?;
    }
    Ok(())
}

#[cfg(unix)]
fn set_key_mode(path: &Path) -> Result<(), StoreError> {
    if path.exists() {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| io_error("set key mode 0600", path, error))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_key_mode(_path: &Path) -> Result<(), StoreError> {
    Ok(())
}

#[cfg(unix)]
fn set_directory_mode(path: &Path) -> Result<(), StoreError> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| io_error("set directory mode 0700", path, error))
}

#[cfg(not(unix))]
fn set_directory_mode(_path: &Path) -> Result<(), StoreError> {
    Ok(())
}

#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path) -> Result<(), StoreError> {
    symlink(target, link).map_err(|error| io_error("create migration pointer", link, error))
}

#[cfg(not(unix))]
fn create_symlink(_target: &Path, link: &Path) -> Result<(), StoreError> {
    Err(StoreError::MigrationConflict(format!(
        "migration pointers require symlink support: {}",
        link.display()
    )))
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt};

    use tempfile::tempdir;

    use super::{MigrationOutcome, migrate};
    use crate::{LeaseRecord, OstromPaths, StoreError, write_lease};

    fn fixture() -> (tempfile::TempDir, std::path::PathBuf, OstromPaths) {
        let fixture = tempdir().expect("temp dir");
        let legacy = fixture.path().join("legacy-ostrom");
        fs::create_dir(&legacy).expect("legacy dir");
        let paths = OstromPaths {
            config: fixture.path().join("xdg-config/ostrom"),
            state: fixture.path().join("xdg-state/ostrom"),
        };
        (fixture, legacy, paths)
    }

    #[test]
    fn migration_is_idempotent_and_leaves_legacy_pointer() {
        let (_fixture, legacy, paths) = fixture();
        fs::write(legacy.join("mandates.yaml"), "provider: file\n").expect("roster");
        fs::write(legacy.join("queue.jsonl"), "").expect("queue");
        assert_eq!(
            migrate(&legacy, &paths, 10).expect("first migration"),
            MigrationOutcome::Migrated
        );
        assert_eq!(
            migrate(&legacy, &paths, 10).expect("second migration"),
            MigrationOutcome::AlreadyMigrated
        );
        assert_eq!(fs::read_link(&legacy).expect("legacy pointer"), paths.state);
        assert_eq!(
            fs::read_link(paths.state.join("mandates.yaml")).expect("config pointer"),
            paths.config.join("mandates.yaml")
        );
    }

    #[test]
    fn migration_refuses_and_names_held_lease() {
        let (_fixture, legacy, paths) = fixture();
        write_lease(
            &legacy.join("builder.lease"),
            &LeaseRecord {
                owner: "builder-synthetic".to_owned(),
                started_at: 10,
                expires_at: 30,
            },
        )
        .expect("lease");
        let error = migrate(&legacy, &paths, 20).expect_err("held lease must refuse");
        assert!(matches!(error, StoreError::LeaseHeld { .. }));
        let message = error.to_string();
        assert!(message.contains("builder.lease"));
        assert!(message.contains("builder-synthetic"));
    }

    #[test]
    fn both_private_keys_keep_mode_0600_and_paths_are_rewritten() {
        let (_fixture, legacy, paths) = fixture();
        let keys = legacy.join("keys");
        fs::create_dir(&keys).expect("keys dir");
        let builder = keys.join("builder.pem");
        let gatekeeper = keys.join("gatekeeper.pem");
        fs::write(&builder, "synthetic-builder-key").expect("builder key");
        fs::write(&gatekeeper, "synthetic-gatekeeper-key").expect("gatekeeper key");
        fs::set_permissions(&builder, fs::Permissions::from_mode(0o600)).expect("builder mode");
        fs::set_permissions(&gatekeeper, fs::Permissions::from_mode(0o600))
            .expect("gatekeeper mode");
        fs::write(
            legacy.join("secrets.yaml"),
            format!(
                "builder:\n  app_id: '1'\n  private_key_path: {}\ngatekeeper:\n  app_id: '2'\n  private_key_path: {}\n",
                builder.display(),
                gatekeeper.display()
            ),
        )
        .expect("secrets");

        migrate(&legacy, &paths, 10).expect("migration");
        for name in ["builder.pem", "gatekeeper.pem"] {
            let mode = fs::metadata(paths.config.join("keys").join(name))
                .expect("key metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "key mode invariant failed for {name}");
        }
        let secrets = fs::read_to_string(paths.config.join("secrets.yaml")).expect("secrets");
        assert!(secrets.contains(&paths.config.join("keys/builder.pem").display().to_string()));
        assert!(
            secrets.contains(
                &paths
                    .config
                    .join("keys/gatekeeper.pem")
                    .display()
                    .to_string()
            )
        );
    }
}
