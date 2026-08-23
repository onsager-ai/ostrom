use std::{
    fs, io,
    path::{Component, Path, PathBuf},
};

use ostrom_core::PolicyManifest;
use ostrom_store::{OstromPaths, policy_manifest_digest};
use tempfile::{Builder, NamedTempFile};
use thiserror::Error;

use crate::policy_manifest::{self, PolicyLoadError};

const MATERIALIZED_MANIFEST: &str = "ostrom.yaml";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposeOutcome {
    pub digest: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RollbackOutcome {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone)]
pub(crate) struct CurrentPolicyVersion {
    pub digest: String,
    pub manifest: PolicyManifest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConfigVerifyOutcome {
    Pass { digest: String },
    Fail { drifted: Vec<PathBuf> },
    Inconclusive { cause: &'static str, path: PathBuf },
}

impl ConfigVerifyOutcome {
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Pass { .. } => 0,
            Self::Fail { .. } => 1,
            Self::Inconclusive { .. } => 2,
        }
    }

    #[must_use]
    pub fn render(&self) -> String {
        match self {
            Self::Pass { digest } => format!("pass digest={digest}"),
            Self::Fail { drifted } => format!(
                "fail drift={}",
                drifted
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            Self::Inconclusive { cause, path } => {
                format!("inconclusive:{cause} path={}", path.display())
            }
        }
    }
}

#[derive(Debug, Error)]
pub(crate) enum PolicyVersionError {
    #[error(transparent)]
    Policy(#[from] PolicyLoadError),
    #[error("could not canonicalise the composed policy: {0}")]
    Canonicalise(#[from] ostrom_store::PolicySignatureError),
    #[error("could not render the composed policy: {0}")]
    Render(serde_yaml::Error),
    #[error("{operation} failed for `{}`: {source}", path.display())]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("policy pointer `{}` is invalid: {cause}", path.display())]
    InvalidPointer { path: PathBuf, cause: &'static str },
    #[error("policy version `{}` is incomplete or drifted", path.display())]
    DriftedVersion { path: PathBuf },
    #[error(
        "rollback refused: previous_missing: no previous policy version exists at `{}`",
        path.display()
    )]
    PreviousMissing { path: PathBuf },
    #[error("rollback refused: previous_unreadable: `{}`: {source}", path.display())]
    PreviousUnreadable {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "rollback refused: {cause}: previous policy version `{}` is not verifiable",
        path.display()
    )]
    PreviousInvalid { path: PathBuf, cause: &'static str },
    #[error("rollback refused: current_missing: no current policy version exists at `{}`", path.display())]
    CurrentMissing { path: PathBuf },
}

#[derive(Debug, Error)]
pub(crate) enum CurrentPolicyError {
    #[error("current policy refused: {cause} path={}", path.display())]
    Inconclusive { cause: &'static str, path: PathBuf },
    #[error(
        "current policy refused: current_drift drift={}",
        drifted.iter().map(|path| path.display().to_string()).collect::<Vec<_>>().join(",")
    )]
    Drift { drifted: Vec<PathBuf> },
    #[error("current policy refused: current_manifest_invalid path={}", path.display())]
    InvalidManifest { path: PathBuf },
}

pub(crate) fn run_compose(
    paths: &OstromPaths,
    manifest_path: &Path,
) -> Result<ComposeOutcome, PolicyVersionError> {
    let manifest = policy_manifest::compose_manifest(paths, manifest_path)?;
    let digest = policy_manifest_digest(&manifest)?;
    let rendered = manifest
        .to_yaml()
        .map_err(PolicyVersionError::Render)?
        .into_bytes();
    install_version(paths, &manifest, &digest, &rendered, || Ok(()))?;
    Ok(ComposeOutcome {
        path: paths.policy_versions_dir().join(&digest),
        digest,
    })
}

pub(crate) fn verify_current(paths: &OstromPaths) -> ConfigVerifyOutcome {
    let current = paths.current_policy_version();
    let pointer = match read_pointer(paths, &current) {
        Ok(pointer) => pointer,
        Err(PointerReadError::Missing) => {
            return ConfigVerifyOutcome::Inconclusive {
                cause: "current_missing",
                path: current,
            };
        }
        Err(PointerReadError::Unreadable(_)) => {
            return ConfigVerifyOutcome::Inconclusive {
                cause: "current_unreadable",
                path: current,
            };
        }
        Err(PointerReadError::Invalid) => {
            return ConfigVerifyOutcome::Inconclusive {
                cause: "current_target_invalid",
                path: current,
            };
        }
        Err(PointerReadError::VersionMissing(path)) => {
            return ConfigVerifyOutcome::Inconclusive {
                cause: "version_missing",
                path,
            };
        }
        Err(PointerReadError::VersionNotDirectory(path)) => {
            return ConfigVerifyOutcome::Inconclusive {
                cause: "version_not_directory",
                path,
            };
        }
        Err(PointerReadError::VersionUnreadable(path)) => {
            return ConfigVerifyOutcome::Inconclusive {
                cause: "version_unreadable",
                path,
            };
        }
    };
    inspect_version(&pointer.directory, &pointer.digest)
}

pub(crate) fn load_current(
    paths: &OstromPaths,
) -> Result<CurrentPolicyVersion, CurrentPolicyError> {
    let current = paths.current_policy_version();
    let pointer = read_pointer(paths, &current).map_err(|error| match error {
        PointerReadError::Missing => CurrentPolicyError::Inconclusive {
            cause: "current_missing",
            path: current.clone(),
        },
        PointerReadError::Unreadable(_) => CurrentPolicyError::Inconclusive {
            cause: "current_unreadable",
            path: current.clone(),
        },
        PointerReadError::Invalid => CurrentPolicyError::Inconclusive {
            cause: "current_target_invalid",
            path: current.clone(),
        },
        PointerReadError::VersionMissing(path) => CurrentPolicyError::Inconclusive {
            cause: "version_missing",
            path,
        },
        PointerReadError::VersionNotDirectory(path) => CurrentPolicyError::Inconclusive {
            cause: "version_not_directory",
            path,
        },
        PointerReadError::VersionUnreadable(path) => CurrentPolicyError::Inconclusive {
            cause: "version_unreadable",
            path,
        },
    })?;
    match inspect_version(&pointer.directory, &pointer.digest) {
        ConfigVerifyOutcome::Pass { .. } => {}
        ConfigVerifyOutcome::Fail { drifted } => {
            return Err(CurrentPolicyError::Drift { drifted });
        }
        ConfigVerifyOutcome::Inconclusive { cause, path } => {
            return Err(CurrentPolicyError::Inconclusive { cause, path });
        }
    }
    let path = pointer.directory.join(MATERIALIZED_MANIFEST);
    let source = fs::read_to_string(&path).map_err(|error| CurrentPolicyError::Inconclusive {
        cause: if error.kind() == io::ErrorKind::NotFound {
            "manifest_missing"
        } else {
            "manifest_unreadable"
        },
        path: path.clone(),
    })?;
    let manifest = PolicyManifest::parse_yaml(&source)
        .ok()
        .ok_or(CurrentPolicyError::InvalidManifest { path })?;
    Ok(CurrentPolicyVersion {
        digest: pointer.digest,
        manifest,
    })
}

pub(crate) fn rollback(paths: &OstromPaths) -> Result<RollbackOutcome, PolicyVersionError> {
    let current_path = paths.current_policy_version();
    let current = read_pointer(paths, &current_path).map_err(|error| match error {
        PointerReadError::Missing => PolicyVersionError::CurrentMissing {
            path: current_path.clone(),
        },
        PointerReadError::Unreadable(source) => {
            io_error("read current policy pointer", current_path.clone(), source)
        }
        PointerReadError::Invalid => PolicyVersionError::InvalidPointer {
            path: current_path.clone(),
            cause: "current_target_invalid",
        },
        PointerReadError::VersionMissing(path) => PolicyVersionError::InvalidPointer {
            path,
            cause: "current_version_missing",
        },
        PointerReadError::VersionNotDirectory(path) => PolicyVersionError::InvalidPointer {
            path,
            cause: "current_version_not_directory",
        },
        PointerReadError::VersionUnreadable(path) => PolicyVersionError::InvalidPointer {
            path,
            cause: "current_version_unreadable",
        },
    })?;

    let previous_path = paths.previous_policy_version();
    let previous = read_pointer(paths, &previous_path).map_err(|error| match error {
        PointerReadError::Missing => PolicyVersionError::PreviousMissing {
            path: previous_path.clone(),
        },
        PointerReadError::Unreadable(source) => PolicyVersionError::PreviousUnreadable {
            path: previous_path.clone(),
            source,
        },
        PointerReadError::Invalid => PolicyVersionError::InvalidPointer {
            path: previous_path.clone(),
            cause: "previous_target_invalid",
        },
        PointerReadError::VersionMissing(path) => PolicyVersionError::InvalidPointer {
            path,
            cause: "previous_version_missing",
        },
        PointerReadError::VersionNotDirectory(path) => PolicyVersionError::InvalidPointer {
            path,
            cause: "previous_version_not_directory",
        },
        PointerReadError::VersionUnreadable(path) => PolicyVersionError::InvalidPointer {
            path,
            cause: "previous_version_unreadable",
        },
    })?;

    match inspect_version(&previous.directory, &previous.digest) {
        ConfigVerifyOutcome::Pass { .. } => {}
        ConfigVerifyOutcome::Fail { .. } => {
            return Err(PolicyVersionError::PreviousInvalid {
                path: previous.directory,
                cause: "previous_drift",
            });
        }
        ConfigVerifyOutcome::Inconclusive { cause, path } => {
            return Err(PolicyVersionError::PreviousInvalid { path, cause });
        }
    }

    atomic_pointer(&current_path, &previous.target)?;
    Ok(RollbackOutcome {
        from: current.digest,
        to: previous.digest,
    })
}

fn install_version(
    paths: &OstromPaths,
    manifest: &PolicyManifest,
    digest: &str,
    rendered: &[u8],
    after_manifest_write: impl FnOnce() -> io::Result<()>,
) -> Result<(), PolicyVersionError> {
    materialize_version(paths, manifest, digest, rendered, after_manifest_write)?;

    let current_path = paths.current_policy_version();
    let current = read_optional_pointer(paths, &current_path)?;
    if current
        .as_ref()
        .is_some_and(|current| current.digest == digest)
    {
        return Ok(());
    }

    if let Some(current) = &current {
        atomic_pointer(&paths.previous_policy_version(), &current.target)?;
    }
    let target = Path::new("versions").join(digest);
    atomic_pointer(&current_path, &target)
}

fn materialize_version(
    paths: &OstromPaths,
    manifest: &PolicyManifest,
    digest: &str,
    rendered: &[u8],
    after_manifest_write: impl FnOnce() -> io::Result<()>,
) -> Result<(), PolicyVersionError> {
    let versions = paths.policy_versions_dir();
    fs::create_dir_all(&versions)
        .map_err(|source| io_error("create policy versions directory", versions.clone(), source))?;
    let destination = versions.join(digest);
    if destination.exists() {
        return require_clean_version(&destination, digest);
    }

    let staging = Builder::new()
        .prefix(".compose-")
        .tempdir_in(&versions)
        .map_err(|source| io_error("create policy staging directory", versions, source))?;
    let manifest_path = staging.path().join(MATERIALIZED_MANIFEST);
    fs::write(&manifest_path, rendered).map_err(|source| {
        io_error(
            "write composed policy manifest",
            manifest_path.clone(),
            source,
        )
    })?;
    after_manifest_write().map_err(|source| {
        io_error(
            "complete policy materialization",
            manifest_path.clone(),
            source,
        )
    })?;
    set_file_read_only(&manifest_path)?;

    // Recompute from the bytes that actually reached disk before installing
    // the directory. The caller's in-memory manifest alone is not evidence
    // that materialization completed faithfully.
    match inspect_version(staging.path(), digest) {
        ConfigVerifyOutcome::Pass { .. } => {}
        ConfigVerifyOutcome::Fail { .. } | ConfigVerifyOutcome::Inconclusive { .. } => {
            return Err(PolicyVersionError::DriftedVersion {
                path: staging.path().to_path_buf(),
            });
        }
    }
    let staged_digest = policy_manifest_digest(manifest)?;
    if staged_digest != digest {
        return Err(PolicyVersionError::DriftedVersion {
            path: staging.path().to_path_buf(),
        });
    }

    fs::rename(staging.path(), &destination)
        .map_err(|source| io_error("install policy version", destination.clone(), source))?;
    set_directory_read_only(&destination)?;
    Ok(())
}

fn require_clean_version(path: &Path, digest: &str) -> Result<(), PolicyVersionError> {
    match inspect_version(path, digest) {
        ConfigVerifyOutcome::Pass { .. } => Ok(()),
        ConfigVerifyOutcome::Fail { .. } | ConfigVerifyOutcome::Inconclusive { .. } => {
            Err(PolicyVersionError::DriftedVersion {
                path: path.to_path_buf(),
            })
        }
    }
}

fn inspect_version(directory: &Path, expected_digest: &str) -> ConfigVerifyOutcome {
    let path = directory.join(MATERIALIZED_MANIFEST);
    let source = match fs::read(&path) {
        Ok(source) => source,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return ConfigVerifyOutcome::Inconclusive {
                cause: "manifest_missing",
                path,
            };
        }
        Err(_) => {
            return ConfigVerifyOutcome::Inconclusive {
                cause: "manifest_unreadable",
                path,
            };
        }
    };
    let manifest = match std::str::from_utf8(&source)
        .ok()
        .and_then(|source| PolicyManifest::parse_yaml(source).ok())
    {
        Some(manifest) => manifest,
        None => {
            return ConfigVerifyOutcome::Fail {
                drifted: vec![PathBuf::from(MATERIALIZED_MANIFEST)],
            };
        }
    };
    let observed_digest = match policy_manifest_digest(&manifest) {
        Ok(digest) => digest,
        Err(_) => {
            return ConfigVerifyOutcome::Inconclusive {
                cause: "digest_unavailable",
                path,
            };
        }
    };
    let canonical = match manifest.to_yaml() {
        Ok(rendered) => rendered.into_bytes(),
        Err(_) => {
            return ConfigVerifyOutcome::Inconclusive {
                cause: "canonical_form_unavailable",
                path,
            };
        }
    };
    if observed_digest != expected_digest || source != canonical {
        ConfigVerifyOutcome::Fail {
            drifted: vec![PathBuf::from(MATERIALIZED_MANIFEST)],
        }
    } else {
        ConfigVerifyOutcome::Pass {
            digest: observed_digest,
        }
    }
}

#[derive(Debug)]
struct VersionPointer {
    target: PathBuf,
    digest: String,
    directory: PathBuf,
}

#[derive(Debug)]
enum PointerReadError {
    Missing,
    Unreadable(io::Error),
    Invalid,
    VersionMissing(PathBuf),
    VersionNotDirectory(PathBuf),
    VersionUnreadable(PathBuf),
}

fn read_optional_pointer(
    paths: &OstromPaths,
    path: &Path,
) -> Result<Option<VersionPointer>, PolicyVersionError> {
    match read_pointer(paths, path) {
        Ok(pointer) => Ok(Some(pointer)),
        Err(PointerReadError::Missing) => Ok(None),
        Err(PointerReadError::Unreadable(source)) => {
            Err(io_error("read policy pointer", path.to_path_buf(), source))
        }
        Err(PointerReadError::Invalid) => Err(PolicyVersionError::InvalidPointer {
            path: path.to_path_buf(),
            cause: "target_invalid",
        }),
        Err(PointerReadError::VersionMissing(version)) => Err(PolicyVersionError::InvalidPointer {
            path: version,
            cause: "version_missing",
        }),
        Err(PointerReadError::VersionNotDirectory(version)) => {
            Err(PolicyVersionError::InvalidPointer {
                path: version,
                cause: "version_not_directory",
            })
        }
        Err(PointerReadError::VersionUnreadable(version)) => {
            Err(PolicyVersionError::InvalidPointer {
                path: version,
                cause: "version_unreadable",
            })
        }
    }
}

fn read_pointer(paths: &OstromPaths, path: &Path) -> Result<VersionPointer, PointerReadError> {
    let target = fs::read_link(path).map_err(|source| match source.kind() {
        io::ErrorKind::NotFound => PointerReadError::Missing,
        io::ErrorKind::InvalidInput => PointerReadError::Invalid,
        _ => PointerReadError::Unreadable(source),
    })?;
    let mut components = target.components();
    let valid = matches!(components.next(), Some(Component::Normal(part)) if part == "versions");
    let digest = match components.next() {
        Some(Component::Normal(digest)) => digest.to_str().map(str::to_owned),
        _ => None,
    };
    let digest = digest.filter(|digest| valid_digest(digest));
    if !valid || components.next().is_some() || digest.is_none() || target.is_absolute() {
        return Err(PointerReadError::Invalid);
    }
    let digest = digest.expect("validated digest is present");
    let directory = paths.state.join(&target);
    match fs::metadata(&directory) {
        Ok(metadata) if metadata.is_dir() => Ok(VersionPointer {
            target,
            digest,
            directory,
        }),
        Ok(_) => Err(PointerReadError::VersionNotDirectory(directory)),
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            Err(PointerReadError::VersionMissing(directory))
        }
        Err(_) => Err(PointerReadError::VersionUnreadable(directory)),
    }
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn atomic_pointer(path: &Path, target: &Path) -> Result<(), PolicyVersionError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| {
        io_error(
            "create policy state directory",
            parent.to_path_buf(),
            source,
        )
    })?;
    let temporary = NamedTempFile::new_in(parent).map_err(|source| {
        io_error(
            "create temporary policy pointer",
            path.to_path_buf(),
            source,
        )
    })?;
    let temporary_path = temporary.path().to_path_buf();
    drop(temporary);
    create_directory_symlink(target, &temporary_path).map_err(|source| {
        io_error(
            "create temporary policy pointer",
            temporary_path.clone(),
            source,
        )
    })?;
    if let Err(source) = fs::rename(&temporary_path, path) {
        let _ = fs::remove_file(&temporary_path);
        return Err(io_error(
            "activate policy pointer",
            path.to_path_buf(),
            source,
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn create_directory_symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_directory_symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}

#[cfg(unix)]
fn set_file_read_only(path: &Path) -> Result<(), PolicyVersionError> {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(path, fs::Permissions::from_mode(0o444))
        .map_err(|source| io_error("make policy manifest read-only", path.to_path_buf(), source))
}

#[cfg(not(unix))]
fn set_file_read_only(path: &Path) -> Result<(), PolicyVersionError> {
    let mut permissions = fs::metadata(path)
        .map_err(|source| io_error("inspect policy manifest", path.to_path_buf(), source))?
        .permissions();
    permissions.set_readonly(true);
    fs::set_permissions(path, permissions)
        .map_err(|source| io_error("make policy manifest read-only", path.to_path_buf(), source))
}

#[cfg(unix)]
fn set_directory_read_only(path: &Path) -> Result<(), PolicyVersionError> {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(path, fs::Permissions::from_mode(0o555))
        .map_err(|source| io_error("make policy version read-only", path.to_path_buf(), source))
}

#[cfg(not(unix))]
fn set_directory_read_only(path: &Path) -> Result<(), PolicyVersionError> {
    let mut permissions = fs::metadata(path)
        .map_err(|source| io_error("inspect policy version", path.to_path_buf(), source))?
        .permissions();
    permissions.set_readonly(true);
    fs::set_permissions(path, permissions)
        .map_err(|source| io_error("make policy version read-only", path.to_path_buf(), source))
}

fn io_error(operation: &'static str, path: PathBuf, source: io::Error) -> PolicyVersionError {
    PolicyVersionError::Io {
        operation,
        path,
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, io, path::Path};

    use ostrom_core::PolicyManifest;
    use ostrom_store::OstromPaths;
    use tempfile::tempdir;

    use super::{atomic_pointer, install_version};

    #[test]
    fn an_interruption_after_the_manifest_write_never_flips_current() {
        let root = tempdir().expect("temporary policy state");
        let paths = OstromPaths {
            config: root.path().to_path_buf(),
            state: root.path().to_path_buf(),
        };
        let old_digest = "1".repeat(64);
        let new_digest = "2".repeat(64);
        fs::create_dir_all(paths.policy_versions_dir().join(&old_digest))
            .expect("create old version");
        atomic_pointer(
            &paths.current_policy_version(),
            &Path::new("versions").join(&old_digest),
        )
        .expect("install old current pointer");
        let manifest =
            PolicyManifest::parse_yaml("manifest_version: 1\n").expect("minimal policy manifest");
        let rendered = manifest.to_yaml().expect("render manifest");

        let result = install_version(&paths, &manifest, &new_digest, rendered.as_bytes(), || {
            Err(io::Error::other("injected interruption"))
        });

        assert!(result.is_err());
        assert_eq!(
            fs::read_link(paths.current_policy_version()).expect("read current pointer"),
            Path::new("versions").join(&old_digest)
        );
        assert!(!paths.policy_versions_dir().join(new_digest).exists());
    }
}
