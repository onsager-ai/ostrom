use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use ostrom_core::{ActionDefinition, glob_matches};
use serde_json::{Value, json};

use crate::{
    ActionFault, ActionOutcome, ActionProvider, PreparedAction,
    process::{ProcessResult, exact_keys, invalid_parameters, run_bounded},
};

const GH_TIMEOUT: Duration = Duration::from_secs(30);
const PERMISSIONS: &[&str] = &[
    "actions",
    "administration",
    "checks",
    "contents",
    "deployments",
    "discussions",
    "environments",
    "issues",
    "merge_queues",
    "metadata",
    "packages",
    "pages",
    "pull_requests",
    "repository_hooks",
    "repository_projects",
    "secret_scanning_alerts",
    "security_events",
    "statuses",
    "vulnerability_alerts",
    "workflows",
];

pub struct GitHubProvider {
    working_directory: PathBuf,
    executable: PathBuf,
}

impl GitHubProvider {
    #[must_use]
    pub fn new(working_directory: impl Into<PathBuf>) -> Self {
        Self {
            working_directory: working_directory.into(),
            executable: PathBuf::from("gh"),
        }
    }

    #[cfg(test)]
    fn with_executable(
        working_directory: impl Into<PathBuf>,
        executable: impl Into<PathBuf>,
    ) -> Self {
        Self {
            working_directory: working_directory.into(),
            executable: executable.into(),
        }
    }
}

impl ActionProvider for GitHubProvider {
    fn domain(&self) -> &'static str {
        "gh"
    }

    fn verbs(&self) -> &'static [&'static str] {
        &["check-run", "token-scope"]
    }

    fn action_definition(&self, verb: &str) -> Option<ActionDefinition> {
        let (definition, revision) = match verb {
            "check-run" => (json!({"parameters": ["name"]}), "gh-check-run-v1"),
            "token-scope" => (json!({"parameters": ["scopes"]}), "gh-token-scope-v1"),
            _ => return None,
        };
        Some(ActionDefinition {
            uses: format!("gh/{verb}"),
            producer: "ostrom-gh".to_owned(),
            default_fresh_for_seconds: 60,
            definition,
            source_revision: revision.to_owned(),
        })
    }

    fn prepare(
        &self,
        verb: &str,
        parameters: &BTreeMap<String, Value>,
    ) -> Result<Box<dyn PreparedAction>, ActionFault> {
        match verb {
            "check-run" if exact_keys(parameters, &["name"]) => {
                let name = parameters
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .ok_or_else(invalid_parameters)?;
                let jobs = workflow_job_names(&self.working_directory)?;
                if !jobs.iter().any(|job| glob_matches(job, name, false)) {
                    return Err(ActionFault::new("gh_unknown_check_run", None));
                }
                Ok(Box::new(CheckRunAction {
                    executable: self.executable.clone(),
                    working_directory: self.working_directory.clone(),
                    name: name.to_owned(),
                }))
            }
            "token-scope" if exact_keys(parameters, &["scopes"]) => {
                let scopes = parameters
                    .get("scopes")
                    .cloned()
                    .and_then(|value| serde_json::from_value::<Vec<String>>(value).ok())
                    .filter(|scopes| !scopes.is_empty())
                    .ok_or_else(invalid_parameters)?;
                let mut unique = BTreeSet::new();
                if scopes
                    .iter()
                    .any(|scope| !valid_scope(scope) || !unique.insert(scope.clone()))
                {
                    return Err(invalid_parameters());
                }
                Ok(Box::new(TokenScopeAction {
                    executable: self.executable.clone(),
                    working_directory: self.working_directory.clone(),
                    scopes: unique,
                }))
            }
            _ => Err(invalid_parameters()),
        }
    }
}

struct CheckRunAction {
    executable: PathBuf,
    working_directory: PathBuf,
    name: String,
}

impl PreparedAction for CheckRunAction {
    fn execute(&self) -> ActionOutcome {
        let mut command = Command::new(&self.executable);
        command.current_dir(&self.working_directory).args([
            "pr",
            "checks",
            "--json",
            "name,bucket",
        ]);
        match run_bounded(&mut command, GH_TIMEOUT) {
            ProcessResult::Completed { status, stdout, .. } if status.success() => {
                let Ok(checks) = serde_json::from_slice::<Vec<Value>>(&stdout) else {
                    return inconclusive("gh_check_run_response");
                };
                let selected = checks
                    .iter()
                    .filter(|check| {
                        check
                            .get("name")
                            .and_then(Value::as_str)
                            .is_some_and(|name| glob_matches(name, &self.name, false))
                    })
                    .collect::<Vec<_>>();
                if selected.is_empty() {
                    return ActionOutcome::Fail;
                }
                if selected.iter().any(|check| {
                    matches!(
                        check
                            .get("bucket")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_ascii_lowercase()
                            .as_str(),
                        "fail" | "cancel" | "pending"
                    )
                }) {
                    return ActionOutcome::Fail;
                }
                if selected.iter().all(|check| {
                    matches!(
                        check
                            .get("bucket")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_ascii_lowercase()
                            .as_str(),
                        "pass" | "skipping"
                    )
                }) {
                    ActionOutcome::Pass
                } else {
                    inconclusive("gh_check_run_pending")
                }
            }
            ProcessResult::TimedOut => inconclusive("gh_timeout"),
            ProcessResult::Completed { .. }
            | ProcessResult::SpawnFailed
            | ProcessResult::WaitFailed => inconclusive("gh_unavailable"),
        }
    }
}

struct TokenScopeAction {
    executable: PathBuf,
    working_directory: PathBuf,
    scopes: BTreeSet<String>,
}

impl PreparedAction for TokenScopeAction {
    fn execute(&self) -> ActionOutcome {
        let mut command = Command::new(&self.executable);
        command
            .current_dir(&self.working_directory)
            .args(["auth", "status", "--active", "--json", "hosts"]);
        match run_bounded(&mut command, GH_TIMEOUT) {
            ProcessResult::Completed { status, stdout, .. } if status.success() => {
                let Ok(document) = serde_json::from_slice::<Value>(&stdout) else {
                    return inconclusive("gh_token_scope_response");
                };
                let mut observed = BTreeSet::new();
                collect_scopes(&document, &mut observed);
                if observed.is_empty() {
                    inconclusive("gh_token_scope_unobservable")
                } else if self.scopes.is_subset(&observed) {
                    ActionOutcome::Pass
                } else {
                    ActionOutcome::Fail
                }
            }
            ProcessResult::TimedOut => inconclusive("gh_timeout"),
            ProcessResult::Completed { .. }
            | ProcessResult::SpawnFailed
            | ProcessResult::WaitFailed => inconclusive("gh_unavailable"),
        }
    }
}

fn workflow_job_names(root: &Path) -> Result<BTreeSet<String>, ActionFault> {
    let directory = root.join(".github/workflows");
    let entries =
        fs::read_dir(directory).map_err(|_| ActionFault::new("gh_workflows_unavailable", None))?;
    let mut names = BTreeSet::new();
    for entry in entries {
        let entry = entry.map_err(|_| ActionFault::new("gh_workflows_unavailable", None))?;
        let path = entry.path();
        if !matches!(
            path.extension().and_then(|value| value.to_str()),
            Some("yml" | "yaml")
        ) {
            continue;
        }
        let source = fs::read_to_string(path)
            .map_err(|_| ActionFault::new("gh_workflows_unavailable", None))?;
        let workflow: serde_yaml::Value = serde_yaml::from_str(&source)
            .map_err(|_| ActionFault::new("gh_workflow_invalid", None))?;
        let Some(jobs) = workflow.get("jobs").and_then(serde_yaml::Value::as_mapping) else {
            continue;
        };
        for (id, job) in jobs {
            if let Some(id) = id.as_str().filter(|id| !id.is_empty()) {
                names.insert(id.to_owned());
            }
            if let Some(name) = job
                .get("name")
                .and_then(serde_yaml::Value::as_str)
                .filter(|name| !name.is_empty())
            {
                names.insert(name.to_owned());
            }
        }
    }
    Ok(names)
}

fn valid_scope(scope: &str) -> bool {
    let Some((permission, level)) = scope.split_once(':') else {
        return false;
    };
    PERMISSIONS.contains(&permission)
        && matches!(level, "read" | "write")
        && (permission != "metadata" || level == "read")
}

fn collect_scopes(value: &Value, scopes: &mut BTreeSet<String>) {
    match value {
        Value::Array(values) => values
            .iter()
            .for_each(|value| collect_scopes(value, scopes)),
        Value::Object(values) => {
            for (key, value) in values {
                if key == "scopes" {
                    match value {
                        Value::String(value) => {
                            scopes.extend(
                                value
                                    .split(',')
                                    .map(str::trim)
                                    .filter(|v| !v.is_empty())
                                    .map(str::to_owned),
                            );
                        }
                        Value::Array(values) => scopes
                            .extend(values.iter().filter_map(Value::as_str).map(str::to_owned)),
                        _ => {}
                    }
                } else {
                    collect_scopes(value, scopes);
                }
            }
        }
        _ => {}
    }
}

fn inconclusive(name: &'static str) -> ActionOutcome {
    ActionOutcome::Inconclusive(ActionFault::new(name, None))
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt};

    use ostrom_core::{Catalogue, CatalogueEnumeration, CheckDocument, CheckVerdict};
    use tempfile::tempdir;

    use super::*;
    use crate::ActionRegistry;

    fn fixture(script: &str) -> (tempfile::TempDir, GitHubProvider) {
        let root = tempdir().expect("GitHub action fixture");
        fs::create_dir_all(root.path().join(".github/workflows")).expect("workflow directory");
        fs::write(
            root.path().join(".github/workflows/ci.yml"),
            "name: CI\njobs:\n  rust:\n    name: Rust workspace\n    runs-on: ubuntu-latest\n    steps: []\n",
        )
        .expect("workflow fixture");
        let gh = root.path().join("gh-fixture");
        fs::write(&gh, script).expect("gh fixture");
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o700)).expect("gh fixture mode");
        let provider = GitHubProvider::with_executable(root.path(), &gh);
        (root, provider)
    }

    fn catalogue(uses: &str, parameters: &str) -> CatalogueEnumeration {
        let source = format!(
            "checks_version: 1\nchecks:\n  github:\n    uses: {uses}\n    with: {parameters}\n"
        );
        CatalogueEnumeration {
            catalogues: vec![Catalogue {
                document: CheckDocument::from_yaml(&source).expect("GitHub check catalogue"),
            }],
            complete: true,
        }
    }

    #[test]
    fn undefined_workflow_job_is_rejected_during_validation() {
        let (_root, provider) = fixture("#!/bin/sh\nexit 1\n");
        let mut registry = ActionRegistry::new();
        registry.register(provider).expect("GitHub provider");
        let error = registry
            .prepare("github", &catalogue("gh/check-run", "{name: absent-job}"))
            .err()
            .expect("undefined workflow job must fail validation");
        assert_eq!(error.name(), "gh_unknown_check_run");
    }

    #[test]
    fn check_run_reads_the_exact_named_job() {
        let (_root, provider) = fixture(
            "#!/bin/sh\nprintf '%s\\n' '[{\"name\":\"Rust workspace\",\"bucket\":\"pass\"}]'\n",
        );
        let mut registry = ActionRegistry::new();
        registry.register(provider).expect("GitHub provider");
        let receipt = registry
            .prepare(
                "github",
                &catalogue("gh/check-run", "{name: Rust workspace}"),
            )
            .expect("defined job")
            .execute("gh-check-run");
        assert_eq!(receipt.verdict, Some(CheckVerdict::Pass));
    }

    #[test]
    fn check_run_glob_requires_every_matching_job_to_be_green() {
        let (_root, provider) = fixture(
            "#!/bin/sh\nprintf '%s\\n' '[{\"name\":\"Rust workspace\",\"bucket\":\"pass\"},{\"name\":\"rust\",\"bucket\":\"fail\"}]'\n",
        );
        let mut registry = ActionRegistry::new();
        registry.register(provider).expect("GitHub provider");
        let receipt = registry
            .prepare("github", &catalogue("gh/check-run", "{name: 'rust*'}"))
            .expect("glob matches workflow jobs")
            .execute("gh-check-run-glob");
        assert_eq!(receipt.verdict, Some(CheckVerdict::Fail));
    }

    #[test]
    fn token_scope_compares_exact_enumerated_scopes() {
        let (_root, provider) = fixture(
            "#!/bin/sh\nprintf '%s\\n' '{\"hosts\":{\"github.com\":[{\"scopes\":[\"contents:write\",\"pull_requests:write\"]}]}}'\n",
        );
        let mut registry = ActionRegistry::new();
        registry.register(provider).expect("GitHub provider");
        let receipt = registry
            .prepare(
                "github",
                &catalogue(
                    "gh/token-scope",
                    "{scopes: [contents:write, pull_requests:write]}",
                ),
            )
            .expect("valid scopes")
            .execute("gh-token-scope");
        assert_eq!(receipt.verdict, Some(CheckVerdict::Pass));
    }
}
