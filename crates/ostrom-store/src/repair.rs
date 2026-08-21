use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::SystemTime,
};

use chrono::{DateTime, Utc};
use regex::Regex;
use serde_json::{Map, Value, json};
use thiserror::Error;

use crate::{
    OstromPaths, TraceAppend,
    app_token::{
        AuthenticatedCommandError, GitHubInstallationTokenMinter, InstallationTokenMinter,
        ScopedAppTokenRequest, authenticated_output,
    },
    append_trace, load_config_or_defaults,
};

const REPAIR_CAP: usize = 3;
const QUERY_LIMIT: usize = 1_000;
const LIST_PERMISSIONS: &str = "metadata:read,pull_requests:read";
const CHECK_PERMISSIONS: &str = "metadata:read,checks:read,statuses:read";
const FETCH_PERMISSIONS: &str = "metadata:read,contents:read";
const PUSH_PERMISSIONS: &str = "metadata:read,contents:write";

#[derive(Debug, Clone)]
pub struct RepairOptions {
    pub paths: OstromPaths,
    pub working_directory: PathBuf,
    pub lease_owner: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

#[derive(Debug, Error)]
pub enum RepairError {
    #[error("{0}")]
    Config(String),
    #[error("mandate repair: could not create temporary repository: {0}")]
    Temporary(String),
    #[error("mandate repair: could not append pr-repair trace")]
    Trace,
    #[error("mandate repair: could not serialize summary")]
    Serialize,
}

impl RepairError {
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::Config(_) => 2,
            Self::Temporary(_) | Self::Trace | Self::Serialize => 1,
        }
    }
}

#[derive(Debug, Clone)]
struct Candidate {
    repository: String,
    number: String,
    head_branch: String,
    base_branch: String,
    listed_head_sha: String,
}

#[derive(Debug, Default)]
struct Summary {
    attempted: usize,
    repaired: usize,
    conflicted: usize,
    skipped: usize,
    failed: usize,
    repositories: usize,
    scanned_repositories: usize,
    repository_failures: usize,
}

struct RepairContext<'a> {
    options: &'a RepairOptions,
    minter: &'a mut dyn InstallationTokenMinter,
    stderr: String,
    stdout_prefix: String,
}

pub fn run_repair_prs(options: &RepairOptions) -> Result<RepairOutput, RepairError> {
    let mut minter = GitHubInstallationTokenMinter;
    run_repair_prs_with_minter(options, &mut minter)
}

fn run_repair_prs_with_minter(
    options: &RepairOptions,
    minter: &mut dyn InstallationTokenMinter,
) -> Result<RepairOutput, RepairError> {
    if !command_exists("git") {
        return Ok(RepairOutput {
            stdout: String::new(),
            stderr: "mandate repair: git is required\n".to_owned(),
            exit_code: 1,
        });
    }
    let config = load_config_or_defaults(&options.paths, &options.working_directory)
        .map_err(|error| RepairError::Config(error.to_string()))?;
    let temporary = tempfile::Builder::new()
        .prefix("ostrom-pr-repair.")
        .tempdir()
        .map_err(|error| RepairError::Temporary(error.to_string()))?;
    let mut context = RepairContext {
        options,
        minter,
        stderr: String::new(),
        stdout_prefix: String::new(),
    };
    let mut summary = Summary::default();
    let mut candidates = Vec::new();

    for project in config.projects {
        let repository = project.repo.as_str();
        summary.repositories += 1;
        let listing = context.authenticated(
            repository,
            LIST_PERMISSIONS,
            &[
                "gh",
                "pr",
                "list",
                "--repo",
                repository,
                "--state",
                "open",
                "--limit",
                &QUERY_LIMIT.to_string(),
                "--json",
                "number,body,author,mergeable,headRefName,baseRefName,headRefOid,isCrossRepository",
            ],
        );
        if listing.code != 0 {
            summary.repository_failures += 1;
            context.stderr.push_str(&format!(
                "mandate repair: failed to enumerate open pull requests for {repository} (rc={})\n",
                listing.code
            ));
            context.trace(
                repository,
                None,
                "",
                "",
                "enumeration-failed",
                "",
                "",
                &[],
                Some(listing.code),
                json!({"reason": "open pull requests could not be enumerated"}),
            )?;
            continue;
        }
        let Ok(listing) = serde_json::from_slice::<Value>(&listing.stdout) else {
            enumeration_malformed(&mut context, &mut summary, repository)?;
            continue;
        };
        let Some(pull_requests) = listing.as_array() else {
            enumeration_malformed(&mut context, &mut summary, repository)?;
            continue;
        };
        if pull_requests.len() == QUERY_LIMIT {
            summary.repository_failures += 1;
            context.stderr.push_str(&format!(
                "mandate repair: pull-request listing for {repository} reached query limit {QUERY_LIMIT}; refusing a truncated scan\n"
            ));
            context.trace(
                repository,
                None,
                "",
                "",
                "enumeration-truncated",
                "",
                "",
                &[],
                Some(6),
                json!({"reason": "open pull-request listing reached the query limit"}),
            )?;
            continue;
        }
        if !pull_requests.iter().all(listing_row_is_filterable) {
            enumeration_filter_malformed(&mut context, &mut summary, repository)?;
            continue;
        }
        summary.scanned_repositories += 1;
        collect_candidates(
            &mut context,
            repository,
            pull_requests,
            &mut candidates,
            &mut summary,
        )?;
    }

    for candidate in candidates {
        if summary.attempted >= REPAIR_CAP {
            summary.skipped += 1;
            context.trace_candidate(
                &candidate,
                "skipped-cap",
                &candidate.listed_head_sha,
                "",
                &[],
                None,
                json!({"reason": "per-pass repair cap reached"}),
            )?;
            continue;
        }
        summary.attempted += 1;
        repair_candidate(
            &mut context,
            &candidate,
            &temporary
                .path()
                .join(format!("candidate-{}", summary.attempted)),
            &mut summary,
        )?;
    }

    let encoded = serde_json::to_string(&json!({
        "cap": REPAIR_CAP,
        "attempted": summary.attempted,
        "repaired": summary.repaired,
        "conflicted": summary.conflicted,
        "skipped": summary.skipped,
        "failed": summary.failed,
        "repositories": summary.repositories,
        "scanned_repositories": summary.scanned_repositories,
        "repository_failures": summary.repository_failures,
    }))
    .map_err(|_| RepairError::Serialize)?;
    context.stdout_prefix.push_str(&encoded);
    context.stdout_prefix.push('\n');
    Ok(RepairOutput {
        stdout: context.stdout_prefix,
        stderr: context.stderr,
        exit_code: i32::from(summary.repositories > 0 && summary.scanned_repositories == 0),
    })
}

fn collect_candidates(
    context: &mut RepairContext<'_>,
    repository: &str,
    pull_requests: &[Value],
    candidates: &mut Vec<Candidate>,
    summary: &mut Summary,
) -> Result<(), RepairError> {
    let role_line = Regex::new(r"(^|\n)Ostrom-Role: builder(\r?\n|$)")
        .expect("builder role marker regex is valid");
    for pull_request in pull_requests {
        if pull_request.get("mergeable").and_then(Value::as_str) != Some("CONFLICTING")
            || !machine_authored(pull_request)
            || !pull_request
                .get("body")
                .and_then(Value::as_str)
                .is_some_and(|body| role_line.is_match(body))
            || pull_request
                .get("isCrossRepository")
                .and_then(Value::as_bool)
                == Some(true)
        {
            continue;
        }
        let candidate = Candidate {
            repository: repository.to_owned(),
            number: jq_text(pull_request.get("number")),
            head_branch: jq_text(pull_request.get("headRefName")),
            base_branch: jq_text(pull_request.get("baseRefName")),
            listed_head_sha: jq_default_text(pull_request.get("headRefOid")),
        };
        let check_runs_endpoint = format!(
            "repos/{repository}/commits/{}/check-runs",
            candidate.listed_head_sha
        );
        let checks = context.authenticated(
            repository,
            CHECK_PERMISSIONS,
            &["gh", "api", &check_runs_endpoint],
        );
        if checks.code != 0 {
            check_fetch_failed(context, summary, &candidate, checks.code)?;
            continue;
        }
        let parsed = serde_json::from_slice::<Value>(&checks.stdout).ok();
        let check_runs = parsed
            .as_ref()
            .and_then(Value::as_object)
            .filter(|object| object.get("total_count").and_then(Value::as_u64).is_some())
            .and_then(|object| object.get("check_runs"))
            .and_then(Value::as_array);
        let Some(check_runs) = check_runs else {
            check_fetch_malformed(context, summary, &candidate)?;
            continue;
        };

        let status_endpoint = format!(
            "repos/{repository}/commits/{}/status",
            candidate.listed_head_sha
        );
        let statuses = context.authenticated(
            repository,
            CHECK_PERMISSIONS,
            &["gh", "api", &status_endpoint],
        );
        if statuses.code != 0 {
            check_fetch_failed(context, summary, &candidate, statuses.code)?;
            continue;
        }
        let parsed_statuses = serde_json::from_slice::<Value>(&statuses.stdout).ok();
        let status_count = parsed_statuses
            .as_ref()
            .and_then(Value::as_object)
            .and_then(|object| object.get("total_count"))
            .and_then(Value::as_u64);
        let Some(status_count) = status_count else {
            check_fetch_malformed(context, summary, &candidate)?;
            continue;
        };
        let status_state = parsed_statuses
            .as_ref()
            .and_then(Value::as_object)
            .and_then(|object| object.get("state"))
            .and_then(Value::as_str);
        if status_count > 0 && status_state.is_none() {
            check_fetch_malformed(context, summary, &candidate)?;
            continue;
        }

        if candidate_checks_green(check_runs, status_count, status_state) {
            candidates.push(candidate);
        }
    }
    Ok(())
}

fn check_fetch_failed(
    context: &mut RepairContext<'_>,
    summary: &mut Summary,
    candidate: &Candidate,
    code: i32,
) -> Result<(), RepairError> {
    summary.skipped += 1;
    context.stderr.push_str(&format!(
        "mandate repair: failed to read checks for {}#{} (rc={code})\n",
        candidate.repository, candidate.number
    ));
    context.trace_candidate(
        candidate,
        "check-fetch-failed",
        &candidate.listed_head_sha,
        "",
        &[],
        Some(code),
        json!({"reason": "candidate check state could not be read"}),
    )
}

fn check_fetch_malformed(
    context: &mut RepairContext<'_>,
    summary: &mut Summary,
    candidate: &Candidate,
) -> Result<(), RepairError> {
    summary.skipped += 1;
    context.stderr.push_str(&format!(
        "mandate repair: check state for {}#{} was malformed\n",
        candidate.repository, candidate.number
    ));
    context.trace_candidate(
        candidate,
        "check-fetch-malformed",
        &candidate.listed_head_sha,
        "",
        &[],
        Some(1),
        json!({"reason": "candidate check state was malformed"}),
    )
}

fn repair_candidate(
    context: &mut RepairContext<'_>,
    candidate: &Candidate,
    work: &Path,
    summary: &mut Summary,
) -> Result<(), RepairError> {
    if fs::create_dir_all(work).is_err() {
        summary.failed += 1;
        context.trace_candidate(
            candidate,
            "local-setup-failed",
            &candidate.listed_head_sha,
            "",
            &[],
            Some(1),
            json!({"reason": "temporary repository initialization failed"}),
        )?;
        return Ok(());
    }
    let init = local_git(work, &["init", "--quiet"]);
    context.capture_diagnostics(&init, false);
    if !init.status.success() {
        summary.failed += 1;
        context.trace_candidate(
            candidate,
            "local-setup-failed",
            &candidate.listed_head_sha,
            "",
            &[],
            Some(exit_code(&init)),
            json!({"reason": "temporary repository initialization failed"}),
        )?;
        return Ok(());
    }
    let _ = local_git(work, &["config", "user.name", "Ostrom Builder"]);
    let _ = local_git(
        work,
        &[
            "config",
            "user.email",
            "ostrom-builder@users.noreply.github.com",
        ],
    );
    let remote_url = format!("https://github.com/{}.git", candidate.repository);
    let fetch = context.authenticated_os(
        &candidate.repository,
        FETCH_PERMISSIONS,
        &[
            "git",
            "-C",
            &work.to_string_lossy(),
            "fetch",
            "--no-tags",
            &remote_url,
            &format!(
                "refs/heads/{}:refs/remotes/repair/head",
                candidate.head_branch
            ),
            &format!(
                "refs/heads/{}:refs/remotes/repair/base",
                candidate.base_branch
            ),
        ],
    );
    if fetch.code != 0 {
        summary.failed += 1;
        context.trace_candidate(
            candidate,
            "fetch-failed",
            &candidate.listed_head_sha,
            "",
            &[],
            Some(fetch.code),
            json!({"reason": "published branches could not be fetched"}),
        )?;
        return Ok(());
    }
    let Some(head_sha) = rev_parse(work, "refs/remotes/repair/head") else {
        summary.failed += 1;
        context.trace_candidate(
            candidate,
            "fetch-failed",
            &candidate.listed_head_sha,
            "",
            &[],
            Some(1),
            json!({"reason": "fetched head could not be resolved"}),
        )?;
        return Ok(());
    };
    let Some(base_sha) = rev_parse(work, "refs/remotes/repair/base") else {
        summary.failed += 1;
        context.trace_candidate(
            candidate,
            "fetch-failed",
            &head_sha,
            "",
            &[],
            Some(1),
            json!({"reason": "fetched base could not be resolved"}),
        )?;
        return Ok(());
    };
    if !candidate.listed_head_sha.is_empty() && candidate.listed_head_sha != head_sha {
        summary.failed += 1;
        context.trace_candidate(
            candidate,
            "head-moved",
            &head_sha,
            &base_sha,
            &[],
            None,
            json!({"reason": "published head changed after enumeration"}),
        )?;
        return Ok(());
    }
    let checkout = local_git(work, &["switch", "--detach", &head_sha]);
    context.capture_diagnostics(&checkout, true);
    if !checkout.status.success() {
        summary.failed += 1;
        context.trace_candidate(
            candidate,
            "local-setup-failed",
            &head_sha,
            &base_sha,
            &[],
            Some(exit_code(&checkout)),
            json!({"reason": "fetched head could not be checked out"}),
        )?;
        return Ok(());
    }
    let message = format!(
        "Merge {} into {}\n\nOstrom-Role: builder",
        candidate.base_branch, candidate.head_branch
    );
    let merge = local_git(work, &["merge", "--no-ff", "-m", &message, &base_sha]);
    context.capture_diagnostics(&merge, true);
    if !merge.status.success() {
        let conflicted_paths = conflict_paths(work);
        if !conflicted_paths.is_empty() {
            let _ = local_git(work, &["merge", "--abort"]);
            summary.conflicted += 1;
            context.trace_candidate(
                candidate,
                "conflicted",
                &head_sha,
                &base_sha,
                &conflicted_paths,
                Some(exit_code(&merge)),
                json!({"reason": "base-forward merge has content conflicts"}),
            )?;
        } else {
            summary.failed += 1;
            context.trace_candidate(
                candidate,
                "merge-failed",
                &head_sha,
                &base_sha,
                &[],
                Some(exit_code(&merge)),
                json!({"reason": "base-forward merge did not complete"}),
            )?;
        }
        return Ok(());
    }
    let parents = local_git(work, &["rev-list", "--parents", "-n", "1", "HEAD"]);
    let parent_text = String::from_utf8_lossy(&parents.stdout);
    let mut fields = parent_text.split_whitespace();
    let parents_match = fields.next().is_some()
        && fields.next() == Some(head_sha.as_str())
        && fields.next() == Some(base_sha.as_str());
    if !parents.status.success() || !parents_match {
        summary.failed += 1;
        context.trace_candidate(
            candidate,
            "merge-failed",
            &head_sha,
            &base_sha,
            &[],
            None,
            json!({"reason": "merge commit did not preserve the fetched parents"}),
        )?;
        return Ok(());
    }
    let push = context.authenticated_os(
        &candidate.repository,
        PUSH_PERMISSIONS,
        &[
            "git",
            "-C",
            &work.to_string_lossy(),
            "push",
            &remote_url,
            &format!("HEAD:refs/heads/{}", candidate.head_branch),
        ],
    );
    if push.code == 0 {
        summary.repaired += 1;
        context.trace_candidate(
            candidate,
            "repaired",
            &head_sha,
            &base_sha,
            &[],
            Some(0),
            json!({}),
        )?;
    } else {
        summary.failed += 1;
        context.trace_candidate(
            candidate,
            "push-failed",
            &head_sha,
            &base_sha,
            &[],
            Some(push.code),
            json!({"reason": "ordinary push was rejected"}),
        )?;
    }
    Ok(())
}

fn enumeration_malformed(
    context: &mut RepairContext<'_>,
    summary: &mut Summary,
    repository: &str,
) -> Result<(), RepairError> {
    summary.repository_failures += 1;
    context.stderr.push_str(&format!(
        "mandate repair: pull-request listing for {repository} was malformed\n"
    ));
    context.trace(
        repository,
        None,
        "",
        "",
        "enumeration-malformed",
        "",
        "",
        &[],
        Some(1),
        json!({"reason": "open pull-request listing was not an array"}),
    )
}

fn enumeration_filter_malformed(
    context: &mut RepairContext<'_>,
    summary: &mut Summary,
    repository: &str,
) -> Result<(), RepairError> {
    summary.repository_failures += 1;
    context.stderr.push_str(&format!(
        "mandate repair: pull-request listing for {repository} was malformed\n"
    ));
    context.trace(
        repository,
        None,
        "",
        "",
        "enumeration-malformed",
        "",
        "",
        &[],
        Some(1),
        json!({"reason": "open pull-request listing could not be filtered"}),
    )
}

impl RepairContext<'_> {
    fn authenticated(&mut self, repository: &str, permissions: &str, command: &[&str]) -> Run {
        self.authenticated_os(repository, permissions, command)
    }

    fn authenticated_os<S: AsRef<std::ffi::OsStr>>(
        &mut self,
        repository: &str,
        permissions: &str,
        command: &[S],
    ) -> Run {
        match authenticated_output(
            &self.options.paths,
            ScopedAppTokenRequest::new("builder", repository, repository, permissions),
            command,
            self.minter,
        ) {
            Ok(output) => {
                self.stderr
                    .push_str(&String::from_utf8_lossy(&output.stderr));
                Run {
                    code: exit_code(&output),
                    stdout: output.stdout,
                }
            }
            Err(error) => {
                self.stderr
                    .push_str(&format!("ostrom credential: {error}\n"));
                Run {
                    code: authentication_exit_code(&error),
                    stdout: Vec::new(),
                }
            }
        }
    }

    fn capture_diagnostics(&mut self, output: &Output, suppress_stdout: bool) {
        if !suppress_stdout {
            self.stdout_prefix
                .push_str(&String::from_utf8_lossy(&output.stdout));
        }
        self.stderr
            .push_str(&String::from_utf8_lossy(&output.stderr));
    }

    #[allow(clippy::too_many_arguments)]
    fn trace(
        &self,
        repository: &str,
        number: Option<&str>,
        head_branch: &str,
        base_branch: &str,
        outcome: &str,
        head_sha: &str,
        base_sha: &str,
        conflicted_paths: &[String],
        exit_code: Option<i32>,
        narration: Value,
    ) -> Result<(), RepairError> {
        let mut fact = Map::new();
        fact.insert("role".to_owned(), json!("builder"));
        fact.insert("owner".to_owned(), json!(self.options.lease_owner));
        fact.insert("repo".to_owned(), json!(repository));
        fact.insert(
            "ref".to_owned(),
            number.map_or(Value::Null, |number| json!(format!("#{number}"))),
        );
        fact.insert("action".to_owned(), json!("merge-base-forward"));
        fact.insert("outcome".to_owned(), json!(outcome));
        fact.insert("head_branch".to_owned(), json!(head_branch));
        fact.insert("base_branch".to_owned(), json!(base_branch));
        fact.insert(
            "head_sha".to_owned(),
            if head_sha.is_empty() {
                Value::Null
            } else {
                json!(head_sha)
            },
        );
        fact.insert(
            "base_sha".to_owned(),
            if base_sha.is_empty() {
                Value::Null
            } else {
                json!(base_sha)
            },
        );
        fact.insert("conflicted_paths".to_owned(), json!(conflicted_paths));
        fact.insert("cap".to_owned(), json!(REPAIR_CAP));
        if let Some(code) = exit_code {
            fact.insert("exit_code".to_owned(), json!(code));
        }
        append_trace(
            &self.options.paths.trace_file(),
            &TraceAppend {
                ts: trace_time(),
                kind: "pr-repair".to_owned(),
                fact,
                narration: narration.as_object().cloned().unwrap_or_default(),
            },
        )
        .map(|_| ())
        .map_err(|_| RepairError::Trace)
    }

    #[allow(clippy::too_many_arguments)]
    fn trace_candidate(
        &self,
        candidate: &Candidate,
        outcome: &str,
        head_sha: &str,
        base_sha: &str,
        conflicted_paths: &[String],
        exit_code: Option<i32>,
        narration: Value,
    ) -> Result<(), RepairError> {
        self.trace(
            &candidate.repository,
            Some(&candidate.number),
            &candidate.head_branch,
            &candidate.base_branch,
            outcome,
            head_sha,
            base_sha,
            conflicted_paths,
            exit_code,
            narration,
        )
    }
}

struct Run {
    code: i32,
    stdout: Vec<u8>,
}

fn machine_authored(pull_request: &Value) -> bool {
    let author = pull_request.get("author");
    author
        .and_then(|value| value.get("is_bot"))
        .and_then(Value::as_bool)
        == Some(true)
        || author
            .and_then(|value| value.get("login"))
            .and_then(Value::as_str)
            .is_some_and(|login| login.ends_with("[bot]"))
}

fn listing_row_is_filterable(pull_request: &Value) -> bool {
    let Some(pull_request) = pull_request.as_object() else {
        return false;
    };
    if pull_request
        .get("body")
        .is_some_and(|body| !body.is_null() && body != &Value::Bool(false) && !body.is_string())
    {
        return false;
    }
    let Some(author) = pull_request.get("author") else {
        return true;
    };
    if author.is_null() {
        return true;
    }
    let Some(author) = author.as_object() else {
        return false;
    };
    author
        .get("login")
        .is_none_or(|login| login.is_null() || login == &Value::Bool(false) || login.is_string())
}

fn completed_green(checks: &[Value]) -> bool {
    !checks.is_empty()
        && checks.iter().all(|check| {
            if check.get("status").and_then(Value::as_str) != Some("completed") {
                return false;
            }
            let conclusion = check.get("conclusion");
            let state = match conclusion {
                Some(Value::Null | Value::Bool(false)) | None => check.get("state"),
                value => value,
            };
            let state = state
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_ascii_uppercase();
            matches!(state.as_str(), "SUCCESS" | "NEUTRAL" | "SKIPPED")
        })
}

fn candidate_checks_green(
    check_runs: &[Value],
    status_count: u64,
    status_state: Option<&str>,
) -> bool {
    let has_check_runs = !check_runs.is_empty();
    let has_statuses = status_count > 0;
    let check_runs_green = !has_check_runs || completed_green(check_runs);
    let statuses_green = !has_statuses || status_state == Some("success");

    (has_check_runs || has_statuses) && check_runs_green && statuses_green
}

fn jq_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(value)) => value.clone(),
        Some(Value::Null) | None => "null".to_owned(),
        Some(value) => value.to_string(),
    }
}

fn jq_default_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(value)) => value.clone(),
        Some(value) if !value.is_null() && value != &Value::Bool(false) => value.to_string(),
        _ => String::new(),
    }
}

fn local_git(cwd: &Path, arguments: &[&str]) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(arguments)
        .output()
        .unwrap_or_else(synthetic_output)
}

fn rev_parse(cwd: &Path, reference: &str) -> Option<String> {
    let output = local_git(cwd, &["rev-parse", reference]);
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn conflict_paths(cwd: &Path) -> Vec<String> {
    let output = local_git(cwd, &["diff", "--name-only", "-z", "--diff-filter=U"]);
    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect()
}

fn command_exists(name: &str) -> bool {
    env::var_os("PATH").is_some_and(|path| {
        env::split_paths(&path).any(|directory| {
            let candidate = directory.join(name);
            candidate.is_file()
        })
    })
}

fn trace_time() -> String {
    env::var("MANDATE_TRACE_TIME")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| {
            DateTime::<Utc>::from(SystemTime::now())
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string()
        })
}

fn exit_code(output: &Output) -> i32 {
    output.status.code().unwrap_or(1)
}

fn authentication_exit_code(error: &AuthenticatedCommandError) -> i32 {
    match error {
        AuthenticatedCommandError::Authentication(_) | AuthenticatedCommandError::Transport(_) => {
            111
        }
    }
}

fn synthetic_output(error: std::io::Error) -> Output {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        Output {
            status: std::process::ExitStatus::from_raw(1 << 8),
            stdout: Vec::new(),
            stderr: error.to_string().into_bytes(),
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        Output {
            status: std::process::ExitStatus::from_raw(1),
            stdout: Vec::new(),
            stderr: error.to_string().into_bytes(),
        }
    }
}
