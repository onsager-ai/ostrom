//! Compile-time embedded protocol assets and their network-free installer.

use std::{
    env,
    ffi::OsString,
    fmt, fs,
    path::{Path, PathBuf},
};

use ostrom_core::sha256_hex;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmbeddedAsset {
    pub path: &'static str,
    pub contents: &'static str,
    pub sha256: &'static str,
}

/// Every protocol file shipped by the binary, keyed relative to the Claude
/// Code harness root. Keep this table sorted so reports are deterministic.
pub const EMBEDDED_ASSETS: &[EmbeddedAsset] = &[
    EmbeddedAsset {
        path: "hooks/hooks.json",
        contents: include_str!("../../../plugins/ostrom/hooks/hooks.json"),
        sha256: "d7298f728a2d69e925bfa58f2f542ceb32b50554d6f592b0d047109bda6942bd",
    },
    EmbeddedAsset {
        path: "rules/frozen-rules.md",
        contents: include_str!("../../../plugins/ostrom/rules/frozen-rules.md"),
        sha256: "36a5e69b30bd022e442ff6b42fec6c7621683645215dc8b0ddd55e95a7ccf75d",
    },
    EmbeddedAsset {
        path: "skills/brief/SKILL.md",
        contents: include_str!("../../../plugins/ostrom/skills/brief/SKILL.md"),
        sha256: "04b4d5b6ab103ea40f931820b24dde6abf1f92f21d06ac16ec41585875dbabe4",
    },
    EmbeddedAsset {
        path: "skills/desk/SKILL.md",
        contents: include_str!("../../../plugins/ostrom/skills/desk/SKILL.md"),
        sha256: "c3694f08ee1bc31fa89d364040f8598f23b27323bd0daa9333eaa780e80037d2",
    },
    EmbeddedAsset {
        path: "skills/doctor/SKILL.md",
        contents: include_str!("../../../plugins/ostrom/skills/doctor/SKILL.md"),
        sha256: "e3a0f54ffed24eacb980c311a29fddae41071ec15309df2380b1927a56080e45",
    },
    EmbeddedAsset {
        path: "skills/gatekeep/SKILL.md",
        contents: include_str!("../../../plugins/ostrom/skills/gatekeep/SKILL.md"),
        sha256: "a3441cb7b4aafc3125347f9bd5e70343fdd0c40f4655a31820ff93af431bdfc8",
    },
    EmbeddedAsset {
        path: "skills/merge/SKILL.md",
        contents: include_str!("../../../plugins/ostrom/skills/merge/SKILL.md"),
        sha256: "134923e0628ae1387c958d5c0e489f962f76fec3bff715b81111dc5b66399de4",
    },
    EmbeddedAsset {
        path: "skills/touch/SKILL.md",
        contents: include_str!("../../../plugins/ostrom/skills/touch/SKILL.md"),
        sha256: "98dc14c0c75e78cbc0b6381465abdccf00dcf87d3f5b91cd97254dd8e8d237fb",
    },
    EmbeddedAsset {
        path: "skills/work/SKILL.md",
        contents: include_str!("../../../plugins/ostrom/skills/work/SKILL.md"),
        sha256: "2b6b4b8d869249e79058d02c1f6d9408f7216be4c9b3e624838fc3d5f30cc223",
    },
];

const PROTOCOL_DIRECTORIES: &[&str] = &["hooks", "rules", "skills"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerificationStatus {
    Match,
    Modified,
    Missing,
    UnexpectedExtra,
}

impl VerificationStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Match => "match",
            Self::Modified => "modified",
            Self::Missing => "missing",
            Self::UnexpectedExtra => "unexpected-extra",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerificationEntry {
    pub path: String,
    pub status: VerificationStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerificationReport {
    pub entries: Vec<VerificationEntry>,
}

impl VerificationReport {
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.entries
            .iter()
            .all(|entry| entry.status == VerificationStatus::Match)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstallStatus {
    Written,
    Changed,
    Unchanged,
    UnexpectedExtra,
}

impl InstallStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Written => "wrote",
            Self::Changed => "changed",
            Self::Unchanged => "left-alone",
            Self::UnexpectedExtra => "unexpected-extra",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallEntry {
    pub path: String,
    pub status: InstallStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstallReport {
    pub entries: Vec<InstallEntry>,
}

impl InstallReport {
    #[must_use]
    pub fn changed_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| {
                matches!(
                    entry.status,
                    InstallStatus::Written | InstallStatus::Changed
                )
            })
            .count()
    }
}

#[derive(Debug)]
pub struct ProtocolError {
    operation: &'static str,
    path: PathBuf,
    source: std::io::Error,
}

impl ProtocolError {
    fn new(operation: &'static str, path: &Path, source: std::io::Error) -> Self {
        Self {
            operation,
            path: path.to_path_buf(),
            source,
        }
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} failed for {}: {}",
            self.operation,
            self.path.display(),
            self.source
        )
    }
}

impl std::error::Error for ProtocolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Resolve the Claude Code harness root using its established configuration
/// convention: an explicit non-empty `CLAUDE_CONFIG_DIR`, otherwise
/// `$HOME/.claude` (relative to the current directory when HOME is relative).
#[must_use]
pub fn resolve_harness_root() -> PathBuf {
    let cwd = env::current_dir().unwrap_or_default();
    resolve_harness_root_from(env::var_os("CLAUDE_CONFIG_DIR"), env::var_os("HOME"), &cwd)
}

#[must_use]
pub fn resolve_harness_root_from(
    configured: Option<OsString>,
    home: Option<OsString>,
    cwd: &Path,
) -> PathBuf {
    if let Some(configured) = configured.filter(|value| !value.to_string_lossy().trim().is_empty())
    {
        return PathBuf::from(configured);
    }
    let home = home.map_or_else(PathBuf::new, PathBuf::from);
    if home.is_absolute() {
        home.join(".claude")
    } else {
        cwd.join(home).join(".claude")
    }
}

pub fn verify(root: &Path) -> Result<VerificationReport, ProtocolError> {
    let mut entries = Vec::new();
    let expected = EMBEDDED_ASSETS
        .iter()
        .map(|asset| asset.path)
        .collect::<std::collections::BTreeSet<_>>();

    for asset in EMBEDDED_ASSETS {
        let path = root.join(asset.path);
        let status = match fs::read(&path) {
            Ok(contents) if sha256_hex(&contents) == asset.sha256 => VerificationStatus::Match,
            Ok(_) => VerificationStatus::Modified,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                VerificationStatus::Missing
            }
            Err(error) => return Err(ProtocolError::new("read protocol asset", &path, error)),
        };
        entries.push(VerificationEntry {
            path: asset.path.to_owned(),
            status,
        });
    }

    for directory in PROTOCOL_DIRECTORIES {
        let path = root.join(directory);
        match fs::symlink_metadata(&path) {
            Ok(_) => collect_files(root, &path, &expected, &mut entries)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(ProtocolError::new(
                    "inspect protocol directory",
                    &path,
                    error,
                ));
            }
        }
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(VerificationReport { entries })
}

fn collect_files(
    root: &Path,
    path: &Path,
    expected: &std::collections::BTreeSet<&str>,
    entries: &mut Vec<VerificationEntry>,
) -> Result<(), ProtocolError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| ProtocolError::new("inspect protocol asset", path, error))?;
    if metadata.is_dir() {
        let directory = fs::read_dir(path)
            .map_err(|error| ProtocolError::new("read protocol directory", path, error))?;
        for entry in directory {
            let entry = entry
                .map_err(|error| ProtocolError::new("read protocol directory", path, error))?;
            collect_files(root, &entry.path(), expected, entries)?;
        }
        return Ok(());
    }
    let relative = path.strip_prefix(root).unwrap_or(path);
    let relative = relative
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/");
    if !expected.contains(relative.as_str()) {
        entries.push(VerificationEntry {
            path: relative,
            status: VerificationStatus::UnexpectedExtra,
        });
    }
    Ok(())
}

pub fn install(root: &Path) -> Result<InstallReport, ProtocolError> {
    let before = verify(root)?;
    let mut entries = Vec::with_capacity(before.entries.len());
    for entry in before.entries {
        let status = match entry.status {
            VerificationStatus::Match => InstallStatus::Unchanged,
            VerificationStatus::Missing => {
                write_asset(root, &entry.path)?;
                InstallStatus::Written
            }
            VerificationStatus::Modified => {
                write_asset(root, &entry.path)?;
                InstallStatus::Changed
            }
            VerificationStatus::UnexpectedExtra => InstallStatus::UnexpectedExtra,
        };
        entries.push(InstallEntry {
            path: entry.path,
            status,
        });
    }
    Ok(InstallReport { entries })
}

fn write_asset(root: &Path, relative: &str) -> Result<(), ProtocolError> {
    let asset = EMBEDDED_ASSETS
        .iter()
        .find(|asset| asset.path == relative)
        .expect("installer only writes embedded assets");
    let path = root.join(relative);
    let parent = path.parent().expect("embedded assets have a parent");
    fs::create_dir_all(parent)
        .map_err(|error| ProtocolError::new("create protocol directory", parent, error))?;
    fs::write(&path, asset.contents)
        .map_err(|error| ProtocolError::new("write protocol asset", &path, error))
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, fs, path::Path};

    use tempfile::tempdir;

    use super::{EMBEDDED_ASSETS, InstallStatus, VerificationStatus, install, verify};
    use ostrom_core::sha256_hex;

    fn collect_disk_files(root: &Path, path: &Path, files: &mut BTreeSet<String>) {
        for entry in fs::read_dir(path).expect("read checked-in protocol directory") {
            let entry = entry.expect("read checked-in protocol entry");
            if entry.file_type().expect("protocol entry type").is_dir() {
                collect_disk_files(root, &entry.path(), files);
            } else {
                files.insert(
                    entry
                        .path()
                        .strip_prefix(root)
                        .expect("protocol path below root")
                        .to_string_lossy()
                        .replace(std::path::MAIN_SEPARATOR, "/"),
                );
            }
        }
    }

    #[test]
    fn embedded_manifest_exactly_matches_the_checked_in_protocol_tree() {
        let plugin = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/ostrom");
        let mut disk = BTreeSet::new();
        for directory in ["hooks", "rules", "skills"] {
            collect_disk_files(&plugin, &plugin.join(directory), &mut disk);
        }
        let embedded = EMBEDDED_ASSETS
            .iter()
            .map(|asset| asset.path.to_owned())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            EMBEDDED_ASSETS.len(),
            embedded.len(),
            "embedded protocol paths must be unique"
        );
        assert!(
            EMBEDDED_ASSETS
                .windows(2)
                .all(|assets| assets[0].path < assets[1].path),
            "embedded protocol paths must stay sorted"
        );
        assert_eq!(
            embedded, disk,
            "update EMBEDDED_ASSETS when protocol files change"
        );
        for asset in EMBEDDED_ASSETS {
            let disk_contents = fs::read(plugin.join(asset.path)).expect("read embedded asset");
            assert_eq!(asset.contents.as_bytes(), disk_contents, "{}", asset.path);
            assert_eq!(asset.sha256, sha256_hex(&disk_contents), "{}", asset.path);
        }
    }

    #[test]
    fn install_is_idempotent_and_verification_names_drift() {
        let root = tempdir().expect("protocol install root");
        let missing = verify(root.path()).expect("verify empty root");
        assert_eq!(missing.entries.len(), EMBEDDED_ASSETS.len());
        assert!(
            missing
                .entries
                .iter()
                .all(|entry| entry.status == VerificationStatus::Missing)
        );
        let first = install(root.path()).expect("first install");
        assert_eq!(first.changed_count(), EMBEDDED_ASSETS.len());
        assert!(
            first
                .entries
                .iter()
                .all(|entry| entry.status == InstallStatus::Written)
        );

        let snapshots = EMBEDDED_ASSETS
            .iter()
            .map(|asset| (asset.path, fs::read(root.path().join(asset.path)).unwrap()))
            .collect::<Vec<_>>();
        let second = install(root.path()).expect("second install");
        assert_eq!(second.changed_count(), 0);
        assert!(
            second
                .entries
                .iter()
                .all(|entry| entry.status == InstallStatus::Unchanged)
        );
        for (path, contents) in snapshots {
            assert_eq!(
                fs::read(root.path().join(path)).unwrap(),
                contents,
                "{path}"
            );
        }

        fs::write(root.path().join("skills/brief/SKILL.md"), "operator edit\n").unwrap();
        fs::write(root.path().join("skills/extra.md"), "extra\n").unwrap();
        let report = verify(root.path()).expect("verify drift");
        assert!(report.entries.iter().any(|entry| {
            entry.path == "skills/brief/SKILL.md" && entry.status == VerificationStatus::Modified
        }));
        assert!(report.entries.iter().any(|entry| {
            entry.path == "skills/extra.md" && entry.status == VerificationStatus::UnexpectedExtra
        }));

        let repaired = install(root.path()).expect("repair drift");
        assert!(repaired.entries.iter().any(|entry| {
            entry.path == "skills/brief/SKILL.md" && entry.status == InstallStatus::Changed
        }));
        assert!(repaired.entries.iter().any(|entry| {
            entry.path == "skills/extra.md" && entry.status == InstallStatus::UnexpectedExtra
        }));
        assert_eq!(
            fs::read(root.path().join("skills/brief/SKILL.md")).unwrap(),
            EMBEDDED_ASSETS
                .iter()
                .find(|asset| asset.path == "skills/brief/SKILL.md")
                .unwrap()
                .contents
                .as_bytes()
        );
    }
}
