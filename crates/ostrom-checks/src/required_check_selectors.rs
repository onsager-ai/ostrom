use std::{collections::BTreeSet, fmt, fs, io, path::Path, process::Command};

use ostrom_core::{GateProject, RepositoryName};
use regex::Regex;
use serde_yaml::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequiredCheckSelectorViolation {
    pub selector: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct RequiredCheckSelectorReport {
    pub job_names: Vec<String>,
    pub violations: Vec<RequiredCheckSelectorViolation>,
}

impl RequiredCheckSelectorReport {
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.violations.is_empty()
    }
}

impl fmt::Display for RequiredCheckSelectorReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let declared = if self.job_names.is_empty() {
            "none".to_owned()
        } else {
            self.job_names.join(", ")
        };
        for violation in &self.violations {
            writeln!(
                formatter,
                "required check selectors: '{}' matches no job name declared in .github/workflows/ (declared: {declared})",
                violation.selector
            )?;
        }
        Ok(())
    }
}

/// Cross-reference one gate project's required-check selectors against the
/// check names its GitHub Actions jobs declare.
pub fn check_required_check_selectors(
    repository: &Path,
    project: &GateProject,
) -> io::Result<RequiredCheckSelectorReport> {
    let job_names = workflow_job_names(repository)?;
    let violations = project
        .required_checks
        .iter()
        .filter(|selector| {
            !job_names
                .iter()
                .any(|job_name| glob_match(job_name, selector))
        })
        .map(|selector| RequiredCheckSelectorViolation {
            selector: selector.clone(),
        })
        .collect();
    Ok(RequiredCheckSelectorReport {
        job_names,
        violations,
    })
}

/// Resolve the repository selected by CI, falling back to the checkout's
/// local origin URL. Neither path performs a network operation.
pub fn resolve_repository_name(
    repository: &Path,
    ci_repository: Option<&str>,
) -> io::Result<Option<String>> {
    if let Some(value) = ci_repository.filter(|value| !value.trim().is_empty()) {
        return validated_repository(value).map(Some);
    }

    let output = Command::new("git")
        .current_dir(repository)
        .args(["remote", "get-url", "origin"])
        .output()
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("could not inspect the local origin URL: {error}"),
            )
        })?;
    if !output.status.success() {
        return Ok(None);
    }
    let remote = String::from_utf8(output.stdout).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("local origin URL is not UTF-8: {error}"),
        )
    })?;
    parse_github_repository(remote.trim()).transpose()
}

fn workflow_job_names(repository: &Path) -> io::Result<Vec<String>> {
    let workflow_directory = repository.join(".github/workflows");
    let entries = fs::read_dir(&workflow_directory).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "could not read workflow directory {}: {error}",
                workflow_directory.display()
            ),
        )
    })?;
    let mut workflows = entries
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<io::Result<Vec<_>>>()?;
    workflows.retain(|path| {
        path.is_file()
            && path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| matches!(extension, "yml" | "yaml"))
    });
    workflows.sort();

    let mut names = BTreeSet::new();
    for workflow in workflows {
        collect_workflow_job_names(&workflow, &mut names)?;
    }
    Ok(names.into_iter().collect())
}

fn collect_workflow_job_names(path: &Path, names: &mut BTreeSet<String>) -> io::Result<()> {
    let source = fs::read(path).map_err(|error| workflow_error(path, error.to_string()))?;
    let document = serde_yaml::from_slice::<Value>(&source)
        .map_err(|error| workflow_error(path, format!("invalid YAML: {error}")))?;
    let Some(jobs) = mapping_value(&document, "jobs").and_then(Value::as_mapping) else {
        return Ok(());
    };
    for (identifier, job) in jobs {
        let Some(identifier) = identifier.as_str().filter(|value| !value.is_empty()) else {
            continue;
        };
        let name = mapping_value(job, "name")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or(identifier);
        names.insert(name.to_owned());
    }
    Ok(())
}

fn mapping_value<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.as_mapping()?.get(Value::String(key.to_owned()))
}

fn workflow_error(path: &Path, detail: String) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("could not inspect workflow {}: {detail}", path.display()),
    )
}

fn glob_match(value: &str, glob: &str) -> bool {
    let body = glob
        .chars()
        .map(|character| {
            if character == '*' {
                ".*".to_owned()
            } else {
                regex::escape(&character.to_string())
            }
        })
        .collect::<String>();
    Regex::new(&format!("(?i:^{body}$)")).is_ok_and(|regex| regex.is_match(value))
}

fn parse_github_repository(remote: &str) -> Option<io::Result<String>> {
    let path = [
        "git@github.com:",
        "ssh://git@github.com/",
        "https://github.com/",
        "http://github.com/",
    ]
    .iter()
    .find_map(|prefix| remote.strip_prefix(prefix))?;
    let path = path
        .strip_suffix(".git")
        .unwrap_or(path)
        .trim_end_matches('/');
    Some(validated_repository(path))
}

fn validated_repository(value: &str) -> io::Result<String> {
    RepositoryName::new(value.to_owned())
        .map(|repository| repository.as_str().to_owned())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use ostrom_core::{GateProject, RepositoryName};

    use super::{check_required_check_selectors, parse_github_repository, resolve_repository_name};

    #[test]
    fn passes_when_every_selector_matches_a_declared_job_name() {
        let fixture = tempfile::tempdir().expect("workflow fixture");
        write_workflow(
            fixture.path(),
            r#"jobs:
  rust:
    runs-on: ubuntu-latest
    steps: []
  integration:
    name: Plugin integration
    runs-on: ubuntu-latest
    steps: []
"#,
        );

        let report = check_required_check_selectors(fixture.path(), &project(&["rust", "plugin*"]))
            .expect("selector check");

        assert!(report.is_clean());
        assert_eq!(report.job_names, ["Plugin integration", "rust"]);
    }

    #[test]
    fn fails_and_names_a_selector_that_matches_no_declared_job() {
        let fixture = tempfile::tempdir().expect("workflow fixture");
        write_workflow(
            fixture.path(),
            "jobs:\n  rust:\n    runs-on: ubuntu-latest\n    steps: []\n",
        );

        let report =
            check_required_check_selectors(fixture.path(), &project(&["rust", "removed-tools"]))
                .expect("selector check");

        assert!(!report.is_clean());
        assert_eq!(report.violations.len(), 1);
        assert_eq!(report.violations[0].selector, "removed-tools");
        assert!(report.to_string().contains("matches no job name"));
    }

    #[test]
    fn malformed_workflow_is_an_explicit_fault() {
        let fixture = tempfile::tempdir().expect("workflow fixture");
        write_workflow(fixture.path(), "jobs: [unterminated\n");

        let error = check_required_check_selectors(fixture.path(), &project(&["rust"]))
            .expect_err("malformed workflow must fault");

        assert!(error.to_string().contains("invalid YAML"));
        assert!(error.to_string().contains("test.yml"));
    }

    #[test]
    fn resolves_ci_and_common_github_origin_names_without_network() {
        let fixture = tempfile::tempdir().expect("repository fixture");
        assert_eq!(
            resolve_repository_name(fixture.path(), Some("onsager-ai/ostrom"))
                .expect("CI repository"),
            Some("onsager-ai/ostrom".to_owned())
        );
        for remote in [
            "git@github.com:onsager-ai/ostrom.git",
            "ssh://git@github.com/onsager-ai/ostrom.git",
            "https://github.com/onsager-ai/ostrom.git",
        ] {
            assert_eq!(
                parse_github_repository(remote)
                    .expect("GitHub remote")
                    .unwrap(),
                "onsager-ai/ostrom"
            );
        }
    }

    fn project(selectors: &[&str]) -> GateProject {
        GateProject {
            repo: RepositoryName::new("onsager-ai/ostrom").expect("fixture repository"),
            required_checks: selectors
                .iter()
                .map(|selector| (*selector).to_owned())
                .collect(),
            bounce: Vec::new(),
            reserved: Vec::new(),
        }
    }

    fn write_workflow(root: &Path, source: &str) {
        let directory = root.join(".github/workflows");
        fs::create_dir_all(&directory).expect("workflow directory");
        fs::write(directory.join("test.yml"), source).expect("workflow fixture");
    }
}
