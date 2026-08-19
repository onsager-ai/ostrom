use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::Value;
use thiserror::Error;

const MISSING_VERSION: &str = "<missing>";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionBumpViolation {
    pub plugin: String,
    pub shipped_path: PathBuf,
    pub manifest: PathBuf,
    pub version: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SkillVersionBumpReport {
    pub violations: Vec<VersionBumpViolation>,
}

impl SkillVersionBumpReport {
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.violations.is_empty()
    }
}

#[derive(Debug, Error)]
pub enum SkillVersionBumpError {
    #[error("skill version check: cannot {operation}: {detail}")]
    Git {
        operation: &'static str,
        detail: String,
    },
    #[error(
        "skill version check: {manifest} at {reference} has no non-empty string version: {detail}"
    )]
    InvalidManifest {
        manifest: String,
        reference: String,
        detail: String,
    },
    #[error("skill version check: git returned a non-UTF-8 path")]
    NonUtf8Path,
}

/// Check that every plugin whose shipped tree changed also changed its version.
///
/// A plugin installation caches the complete plugin tree by the version in its
/// manifest. Repository-only tests are excluded; every other path below
/// `plugins/<name>/` is treated as shipped content so metadata changes and
/// newly introduced surfaces are protected without extending an allowlist.
pub fn check_skill_version_bump(
    repository: &Path,
    base_ref: &str,
    head_ref: &str,
) -> Result<SkillVersionBumpReport, SkillVersionBumpError> {
    let merge_base = git_stdout(
        repository,
        ["merge-base", base_ref, head_ref],
        "find the merge base",
    )?;
    let merge_base = merge_base.trim();
    let diff = git_output(
        repository,
        [
            "diff",
            "--name-only",
            "-z",
            "--diff-filter=ACMRD",
            merge_base,
            head_ref,
            "--",
            "plugins/",
        ],
        "inspect changed plugin files",
    )?;

    let mut changed = diff
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            String::from_utf8(path.to_vec()).map_err(|_| SkillVersionBumpError::NonUtf8Path)
        })
        .collect::<Result<Vec<_>, _>>()?;
    changed.sort();

    let mut versions = BTreeMap::<String, (String, String)>::new();
    let mut violations = Vec::new();
    for shipped_path in changed {
        let Some(plugin) = shipped_plugin(&shipped_path) else {
            continue;
        };
        let manifest = format!("plugins/{plugin}/.claude-plugin/plugin.json");
        let (base_version, head_version) = if let Some(versions) = versions.get(plugin) {
            versions.clone()
        } else {
            let pair = (
                version_at(repository, merge_base, &manifest)?,
                version_at(repository, head_ref, &manifest)?,
            );
            versions.insert(plugin.to_owned(), pair.clone());
            pair
        };

        if base_version == head_version {
            violations.push(VersionBumpViolation {
                plugin: plugin.to_owned(),
                shipped_path: shipped_path.into(),
                manifest: manifest.into(),
                version: head_version,
            });
        }
    }

    Ok(SkillVersionBumpReport { violations })
}

fn shipped_plugin(path: &str) -> Option<&str> {
    let mut components = path.split('/');
    if components.next()? != "plugins" {
        return None;
    }
    let plugin = components.next()?;
    let surface = components.next()?;
    if plugin.is_empty() || surface == "tests" {
        return None;
    }
    Some(plugin)
}

fn version_at(
    repository: &Path,
    reference: &str,
    manifest: &str,
) -> Result<String, SkillVersionBumpError> {
    let object = format!("{reference}:{manifest}");
    let exists = Command::new("git")
        .current_dir(repository)
        .args(["cat-file", "-e", &object])
        .output()
        .map_err(|error| SkillVersionBumpError::Git {
            operation: "inspect the plugin manifest",
            detail: error.to_string(),
        })?;
    if !exists.status.success() {
        return Ok(MISSING_VERSION.to_owned());
    }

    let source = git_stdout(
        repository,
        ["show", object.as_str()],
        "read the plugin manifest",
    )?;
    let value: Value =
        serde_json::from_str(&source).map_err(|error| SkillVersionBumpError::InvalidManifest {
            manifest: manifest.to_owned(),
            reference: reference.to_owned(),
            detail: error.to_string(),
        })?;
    value
        .get("version")
        .and_then(Value::as_str)
        .filter(|version| !version.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| SkillVersionBumpError::InvalidManifest {
            manifest: manifest.to_owned(),
            reference: reference.to_owned(),
            detail: "missing version field".to_owned(),
        })
}

fn git_stdout<const N: usize>(
    repository: &Path,
    arguments: [&str; N],
    operation: &'static str,
) -> Result<String, SkillVersionBumpError> {
    let output = git_output(repository, arguments, operation)?;
    String::from_utf8(output.stdout).map_err(|error| SkillVersionBumpError::Git {
        operation,
        detail: error.to_string(),
    })
}

fn git_output<I, S>(
    repository: &Path,
    arguments: I,
    operation: &'static str,
) -> Result<Output, SkillVersionBumpError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let output = Command::new("git")
        .current_dir(repository)
        .args(arguments)
        .output()
        .map_err(|error| SkillVersionBumpError::Git {
            operation,
            detail: error.to_string(),
        })?;
    if output.status.success() {
        return Ok(output);
    }
    Err(SkillVersionBumpError::Git {
        operation,
        detail: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, process::Command};

    use tempfile::TempDir;

    use super::check_skill_version_bump;

    struct FixtureRepo {
        temporary: TempDir,
    }

    impl FixtureRepo {
        fn new() -> Self {
            let temporary = tempfile::tempdir().expect("fixture directory");
            let root = temporary.path();
            write(root, "plugins/alpha/skills/work/SKILL.md", "# alpha\n");
            write(
                root,
                "plugins/alpha/.claude-plugin/plugin.json",
                r#"{"name":"alpha","version":"1.0.0"}"#,
            );
            write(
                root,
                "plugins/beta/.claude-plugin/plugin.json",
                r#"{"name":"beta","version":"2.0.0"}"#,
            );
            write(root, "plugins/alpha/tests/test.sh", "# fixture\n");
            write(root, "README.md", "# fixture\n");
            git(root, ["init", "--quiet", "--initial-branch=main"]);
            git(root, ["config", "user.name", "Ostrom Test"]);
            git(root, ["config", "user.email", "ostrom@example.test"]);
            git(root, ["add", "."]);
            git(root, ["commit", "--quiet", "-m", "base"]);
            git(root, ["branch", "base"]);
            Self { temporary }
        }

        fn root(&self) -> &Path {
            self.temporary.path()
        }

        fn commit(&self) {
            git(self.root(), ["add", "."]);
            git(self.root(), ["commit", "--quiet", "-m", "change"]);
        }
    }

    #[test]
    fn rejects_changed_shipped_content_without_a_version_bump() {
        for path in [
            "plugins/alpha/skills/work/SKILL.md",
            "plugins/alpha/runtime/runner",
            "plugins/alpha/future-surface/template.txt",
        ] {
            let repo = FixtureRepo::new();
            write(repo.root(), path, "changed shipped content\n");
            repo.commit();

            let report = check_skill_version_bump(repo.root(), "base", "HEAD").expect("check");

            assert_eq!(report.violations.len(), 1, "did not guard {path}");
            assert_eq!(report.violations[0].plugin, "alpha");
            assert_eq!(report.violations[0].shipped_path, Path::new(path));
            assert_eq!(report.violations[0].version, "1.0.0");
        }
    }

    #[test]
    fn rejects_deleted_shipped_content_without_a_version_bump() {
        let repo = FixtureRepo::new();
        fs::remove_file(repo.root().join("plugins/alpha/skills/work/SKILL.md"))
            .expect("delete shipped fixture");
        repo.commit();

        let report = check_skill_version_bump(repo.root(), "base", "HEAD").expect("check");

        assert_eq!(report.violations.len(), 1);
        assert_eq!(
            report.violations[0].shipped_path,
            Path::new("plugins/alpha/skills/work/SKILL.md")
        );
    }

    #[test]
    fn rejects_manifest_metadata_changes_without_a_version_bump() {
        let repo = FixtureRepo::new();
        write(
            repo.root(),
            "plugins/alpha/.claude-plugin/plugin.json",
            r#"{"name":"alpha","description":"changed","version":"1.0.0"}"#,
        );
        repo.commit();

        let report = check_skill_version_bump(repo.root(), "base", "HEAD").expect("check");

        assert_eq!(report.violations.len(), 1);
        assert_eq!(
            report.violations[0].shipped_path,
            Path::new("plugins/alpha/.claude-plugin/plugin.json")
        );
    }

    #[test]
    fn accepts_a_matching_version_bump() {
        let repo = FixtureRepo::new();
        write(
            repo.root(),
            "plugins/alpha/skills/work/SKILL.md",
            "changed shipped content\n",
        );
        write(
            repo.root(),
            "plugins/alpha/.claude-plugin/plugin.json",
            r#"{"name":"alpha","version":"1.0.1"}"#,
        );
        repo.commit();

        let report = check_skill_version_bump(repo.root(), "base", "HEAD").expect("check");

        assert!(report.is_clean());
    }

    #[test]
    fn ignores_repository_only_changes_and_other_plugin_bumps() {
        let repo = FixtureRepo::new();
        write(repo.root(), "README.md", "documentation\n");
        write(
            repo.root(),
            "plugins/alpha/tests/test.sh",
            "changed fixture\n",
        );
        write(
            repo.root(),
            "plugins/beta/.claude-plugin/plugin.json",
            r#"{"name":"beta","version":"2.0.1"}"#,
        );
        repo.commit();

        let report = check_skill_version_bump(repo.root(), "base", "HEAD").expect("check");

        assert!(report.is_clean());
    }

    #[test]
    fn bumping_the_wrong_plugin_does_not_mask_a_violation() {
        let repo = FixtureRepo::new();
        write(
            repo.root(),
            "plugins/alpha/skills/work/SKILL.md",
            "changed shipped content\n",
        );
        write(
            repo.root(),
            "plugins/beta/.claude-plugin/plugin.json",
            r#"{"name":"beta","version":"2.0.1"}"#,
        );
        repo.commit();

        let report = check_skill_version_bump(repo.root(), "base", "HEAD").expect("check");

        assert_eq!(report.violations.len(), 1);
        assert_eq!(report.violations[0].plugin, "alpha");
    }

    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("fixture parent")).expect("create fixture parent");
        fs::write(path, contents).expect("write fixture");
    }

    fn git<const N: usize>(root: &Path, arguments: [&str; N]) {
        let output = Command::new("git")
            .current_dir(root)
            .args(arguments)
            .output()
            .expect("run fixture git");
        assert!(
            output.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
