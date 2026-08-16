use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use ostrom_core::{DefaultDisposition, MandateConfig, ProjectMandate, RepositoryName, Selector};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use thiserror::Error;

use crate::{OstromPaths, QueueDocument, StoreError, io_error, read_queue, write_queue};

const QUERY_LIMIT: usize = 200;
const FULL_RECONCILIATION_HOURS: i64 = 24;
const SHIPPED_DEFAULTS: &str = include_str!("../../../plugins/ostrom/config/mandate-defaults.yaml");

#[derive(Debug, Error)]
pub enum SweepError {
    #[error("no mandates.yaml found at {0}")]
    NotConfigured(String),
    #[error("could not parse sweep configuration: {0}")]
    Config(String),
    #[error("could not read sweep state: {0}")]
    State(String),
    #[error("GitHub acquisition failed: {0}")]
    Acquisition(String),
    #[error("sweep fixture is malformed: {0}")]
    Fixture(String),
    #[error("publish failed: {0}")]
    Publish(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishTarget {
    Disabled,
    Repository(RepositoryName),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SweepMode {
    Auto,
    Full,
    Incremental,
}

#[derive(Debug, Clone)]
pub struct SweepOptions {
    pub paths: OstromPaths,
    pub working_directory: PathBuf,
    pub executable: PathBuf,
    pub plugin_root: PathBuf,
    pub started_at: DateTime<Utc>,
    pub requested_mode: SweepMode,
    pub fixture: Option<PathBuf>,
    pub publish: PublishTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepOutcome {
    pub project_count: usize,
    pub queue_changes: usize,
    pub mode: SweepMode,
    pub faults: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SweepFixture {
    #[serde(default)]
    pub repositories: Vec<RepositorySnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositorySnapshot {
    pub repo: RepositoryName,
    #[serde(default)]
    pub issues: Vec<Value>,
    #[serde(default)]
    pub issue_etag: Option<String>,
    #[serde(default)]
    pub issue_not_modified: bool,
    #[serde(default)]
    pub open_prs: Vec<Value>,
    #[serde(default)]
    pub merged_prs: Vec<Value>,
    #[serde(default)]
    pub default_branch: Option<String>,
    #[serde(default)]
    pub ci_runs: Vec<Value>,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrgSnapshots {
    repositories: Vec<RepositorySnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NormalizedItem {
    id: String,
    repo: String,
    number: u64,
    #[serde(rename = "ref")]
    reference: String,
    #[serde(rename = "type")]
    item_type: String,
    title: String,
    blocked_by: Vec<String>,
    labels: Vec<String>,
    refs: Vec<u64>,
    closing_refs: Vec<u64>,
    files: Vec<String>,
    opened: String,
    updated: String,
    ci: String,
    ready: bool,
    review: String,
    fingerprint: String,
}

#[derive(Debug, Clone)]
struct Classification {
    terminal: String,
    source: String,
    selector: String,
}

#[derive(Debug, Clone)]
struct ClassifiedItem {
    item: NormalizedItem,
    classification: Classification,
    hold_glob: Option<String>,
    first_seen: String,
    age_days: u64,
    movement_stuck: bool,
    old: Option<Value>,
}

#[derive(Debug, Clone)]
struct RepoAnalysis {
    generated: Vec<Value>,
    active_ids: BTreeSet<String>,
    current: BTreeMap<String, Value>,
    state: Value,
}

pub fn run_sweep(options: &SweepOptions) -> Result<SweepOutcome, SweepError> {
    let config = load_config(&options.paths, &options.working_directory)?;
    if config.projects.is_empty() {
        return Err(SweepError::Config(
            "mandates.yaml contains no projects".to_owned(),
        ));
    }
    let existing = read_queue(&options.paths.queue_file())?;
    let state_path = options.paths.state.join("state.json");
    let old_state = read_state(&state_path)?;
    let mode = effective_mode(
        options.requested_mode,
        &config,
        &old_state,
        options.started_at,
    );

    let (snapshots, mut faults) = if let Some(path) = &options.fixture {
        let bytes = fs::read(path).map_err(|error| SweepError::Fixture(error.to_string()))?;
        let fixture: SweepFixture = serde_json::from_slice(&bytes)
            .map_err(|error| SweepError::Fixture(error.to_string()))?;
        validate_fixture(&config, &fixture)?;
        (fixture.repositories, Vec::new())
    } else {
        acquire_by_organization(options, &config, &old_state, mode)?
    };

    let mut snapshots_by_repo = snapshots
        .into_iter()
        .map(|snapshot| (snapshot.repo.as_str().to_owned(), snapshot))
        .collect::<BTreeMap<_, _>>();
    let mut generated = Vec::new();
    let mut active_ids = BTreeSet::new();
    let mut current = BTreeMap::new();
    let mut new_state = old_state.clone();
    ensure_state_shape(&mut new_state);

    for project in &config.projects {
        let repo = project.repo.as_str();
        let Some(snapshot) = snapshots_by_repo.remove(repo) else {
            let reason = format!(
                "authentication or GitHub query failed; repository acquisition produced no result for {repo}"
            );
            faults.push(reason.clone());
            let row = fault_row(repo, &reason, options.started_at);
            active_ids.insert(row["id"].as_str().unwrap_or_default().to_owned());
            generated.push(row);
            continue;
        };
        for warning in &snapshot.warnings {
            faults.push(format!("{repo}: {warning}"));
            let row = fault_row(repo, warning, options.started_at);
            active_ids.insert(row["id"].as_str().unwrap_or_default().to_owned());
            generated.push(row);
        }
        let previous = old_state
            .get("repos")
            .and_then(|repos| repos.get(repo))
            .cloned()
            .unwrap_or_else(|| json!({}));
        let analysis = analyze_repository(
            &config,
            project,
            &snapshot,
            &previous,
            mode,
            options.started_at,
            &options.paths,
        )?;
        generated.extend(analysis.generated);
        active_ids.extend(analysis.active_ids);
        current.extend(analysis.current);
        new_state["repos"][repo] = analysis.state;
    }

    new_state["version"] = json!(2);
    new_state["sweep_mode"] = json!(mode_name(mode));
    if mode == SweepMode::Full {
        new_state["last_full_reconciliation"] = json!(format_time(options.started_at));
    }
    let configured = config
        .projects
        .iter()
        .map(|project| project.repo.as_str())
        .collect::<BTreeSet<_>>();
    if let Some(repos) = new_state.get_mut("repos").and_then(Value::as_object_mut) {
        repos.retain(|repo, _| configured.contains(repo.as_str()));
    }

    let final_rows = reconcile_queue(existing, generated, &active_ids, &current, &configured)?;
    let before = read_queue(&options.paths.queue_file())?;
    let queue_changes = symmetric_queue_changes(&before, &final_rows);
    write_queue(&options.paths.queue_file(), &final_rows)?;
    write_json_private(&state_path, &new_state)?;

    if let PublishTarget::Repository(repository) = &options.publish {
        if let Err(error) = publish(options, repository) {
            faults.push(format!(
                "publish failed; local records remain authoritative: {error}"
            ));
        }
    }

    Ok(SweepOutcome {
        project_count: config.projects.len(),
        queue_changes,
        mode,
        faults,
    })
}

pub fn acquire_org_from_github(
    paths: &OstromPaths,
    working_directory: &Path,
    org: &str,
    started_at: DateTime<Utc>,
    mode: SweepMode,
) -> Result<Vec<RepositorySnapshot>, SweepError> {
    let config = load_config(paths, working_directory)?;
    let state = read_state(&paths.state.join("state.json"))?;
    let mut repositories = Vec::new();
    let gh_host = std::env::var("GH_HOST").unwrap_or_else(|_| "github.com".to_owned());
    gh(&["auth", "status", "--hostname", &gh_host])?;
    for project in config
        .projects
        .iter()
        .filter(|project| owner(project.repo.as_str()) == org)
    {
        let previous = state
            .get("repos")
            .and_then(|repos| repos.get(project.repo.as_str()))
            .cloned()
            .unwrap_or_else(|| json!({}));
        repositories.push(acquire_repository(
            project.repo.clone(),
            &previous,
            &config,
            started_at,
            mode,
        )?);
    }
    Ok(repositories)
}

fn acquire_by_organization(
    options: &SweepOptions,
    config: &MandateConfig,
    _old_state: &Value,
    mode: SweepMode,
) -> Result<(Vec<RepositorySnapshot>, Vec<String>), SweepError> {
    let mut organizations = BTreeMap::<String, String>::new();
    for project in &config.projects {
        organizations
            .entry(owner(project.repo.as_str()).to_owned())
            .or_insert_with(|| project.repo.as_str().to_owned());
    }
    let mut snapshots = Vec::new();
    let mut faults = Vec::new();
    for (org, anchor) in organizations {
        let output = Command::new("bash")
            .arg(options.plugin_root.join("scripts/gh-as.sh"))
            .args(["gatekeeper", &anchor])
            .arg(&options.executable)
            .args([
                "sweep",
                "--inner-org",
                &org,
                "--started-at",
                &format_time(options.started_at),
                "--mode",
                mode_name(mode),
            ])
            .current_dir(&options.working_directory)
            .output()
            .map_err(|error| SweepError::Acquisition(error.to_string()))?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            faults.push(format!(
                "authentication or GitHub query failed for organization {org}{}",
                if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                }
            ));
            continue;
        }
        let result: OrgSnapshots = serde_json::from_slice(&output.stdout).map_err(|error| {
            SweepError::Acquisition(format!(
                "organization {org} returned malformed data: {error}"
            ))
        })?;
        snapshots.extend(result.repositories);
    }
    Ok((snapshots, faults))
}

fn acquire_repository(
    repo: RepositoryName,
    previous: &Value,
    config: &MandateConfig,
    started_at: DateTime<Utc>,
    mode: SweepMode,
) -> Result<RepositorySnapshot, SweepError> {
    let repo_name = repo.as_str();
    let previous_cursor = previous.get("cursor").and_then(Value::as_str).unwrap_or("");
    let previous_etag = previous.get("etag").and_then(Value::as_str).unwrap_or("");
    let issue_since = if mode == SweepMode::Incremental {
        previous_cursor
    } else {
        ""
    };
    let closed_delta = if mode == SweepMode::Full && !previous_cursor.is_empty() {
        let (delta, _, _) = fetch_issues(repo_name, previous_cursor, "")?;
        delta
            .into_iter()
            .filter(|issue| {
                issue.get("pull_request").is_none()
                    && string_field(issue, &["state"]).eq_ignore_ascii_case("closed")
            })
            .collect()
    } else {
        Vec::new()
    };
    let (mut issues, issue_etag, issue_not_modified) =
        fetch_issues(repo_name, issue_since, previous_etag)?;
    issues.extend(closed_delta);

    let open_prs = gh_json(&[
        "pr",
        "list",
        "--repo",
        repo_name,
        "--state",
        "open",
        "--limit",
        "200",
        "--json",
        "number,title,body,labels,createdAt,updatedAt,url,isDraft,reviewDecision,statusCheckRollup,closingIssuesReferences,files,state,mergedAt,headRefOid,mergeable",
    ])?;
    let open_prs = exhaustive_array(open_prs, repo_name, "open pull-request query")?;

    let lookback_days = i64::try_from(
        30_u64
            .max(config.stuck_after_days.saturating_add(7))
            .max((config.cadence_hours.saturating_add(7 * 24)).div_ceil(24)),
    )
    .unwrap_or(i64::MAX);
    let cutoff = (started_at - Duration::days(lookback_days))
        .format("%Y-%m-%d")
        .to_string();
    let search = format!("merged:>={cutoff}");
    let merged_prs = gh_json(&[
        "pr",
        "list",
        "--repo",
        repo_name,
        "--state",
        "merged",
        "--search",
        &search,
        "--limit",
        "200",
        "--json",
        "number,title,author,closingIssuesReferences,createdAt,mergedAt,headRefOid,state",
    ])?;
    let merged_prs = exhaustive_array(merged_prs, repo_name, "recent merged pull-request query")?;

    let mut warnings = Vec::new();
    let (default_branch, ci_runs) = match gh_json(&[
        "repo",
        "view",
        repo_name,
        "--json",
        "defaultBranchRef",
    ]) {
        Ok(value) => {
            let branch = value
                .pointer("/defaultBranchRef/name")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let runs = if let Some(branch) = &branch {
                match gh_json(&[
                    "run",
                    "list",
                    "--repo",
                    repo_name,
                    "--branch",
                    branch,
                    "--limit",
                    "200",
                    "--json",
                    "databaseId,workflowDatabaseId,workflowName,name,headSha,conclusion,status,createdAt,url",
                ]) {
                    Ok(value) => exhaustive_array(value, repo_name, "default-branch CI query")?,
                    Err(error) => {
                        warnings.push(format!("default-branch CI query failed: {error}"));
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            };
            (branch, runs)
        }
        Err(error) => {
            warnings.push(format!("default-branch lookup failed: {error}"));
            (None, Vec::new())
        }
    };

    Ok(RepositorySnapshot {
        repo,
        issues,
        issue_etag,
        issue_not_modified,
        open_prs,
        merged_prs,
        default_branch,
        ci_runs,
        warnings,
    })
}

fn fetch_issues(
    repo: &str,
    since: &str,
    etag: &str,
) -> Result<(Vec<Value>, Option<String>, bool), SweepError> {
    let mut all = Vec::new();
    let mut next_etag = None;
    for page in 1..=2 {
        let state = if since.is_empty() { "open" } else { "all" };
        let mut endpoint = format!(
            "repos/{repo}/issues?state={state}&sort=updated&direction=asc&per_page=100&page={page}"
        );
        if !since.is_empty() {
            endpoint.push_str("&since=");
            endpoint.push_str(since);
        }
        let mut args = vec![
            "api".to_owned(),
            "-X".to_owned(),
            "GET".to_owned(),
            "--include".to_owned(),
        ];
        if page == 1 && !etag.is_empty() {
            args.extend(["-H".to_owned(), format!("If-None-Match: {etag}")]);
        }
        args.push(endpoint);
        let output = Command::new("gh")
            .args(&args)
            .output()
            .map_err(|error| SweepError::Acquisition(error.to_string()))?;
        let response = parse_http_response(&output.stdout).map_err(|parse_error| {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            if detail.is_empty() {
                parse_error
            } else {
                SweepError::Acquisition(detail)
            }
        })?;
        if !output.status.success() && response.status != 304 {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(SweepError::Acquisition(if detail.is_empty() {
                format!(
                    "issues change feed for {repo} returned HTTP {}",
                    response.status
                )
            } else {
                detail
            }));
        }
        if response.status == 304 {
            return Ok((Vec::new(), response.etag, true));
        }
        if !(200..300).contains(&response.status) {
            return Err(SweepError::Acquisition(format!(
                "issues change feed for {repo} returned HTTP {}",
                response.status
            )));
        }
        if page == 1 {
            next_etag = response.etag;
        }
        let values = exhaustive_array(response.body, repo, "issues change feed page")?;
        let count = values.len();
        all.extend(values);
        if count < 100 {
            return Ok((all, next_etag, false));
        }
        next_etag = None;
    }
    Err(SweepError::Acquisition(format!(
        "issues change feed for {repo} reached query_limit {QUERY_LIMIT}; refusing a truncated sweep"
    )))
}

struct HttpResponse {
    status: u16,
    etag: Option<String>,
    body: Value,
}

fn parse_http_response(output: &[u8]) -> Result<HttpResponse, SweepError> {
    let text = String::from_utf8_lossy(output).replace("\r\n", "\n");
    let (headers, body) = text.split_once("\n\n").ok_or_else(|| {
        SweepError::Acquisition("GitHub response had no header boundary".to_owned())
    })?;
    let status = headers
        .lines()
        .filter_map(|line| {
            line.strip_prefix("HTTP/")
                .and_then(|rest| rest.split_whitespace().nth(1))
                .and_then(|code| code.parse::<u16>().ok())
        })
        .next_back()
        .ok_or_else(|| SweepError::Acquisition("GitHub response had no HTTP status".to_owned()))?;
    let etag = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("etag")
            .then(|| value.trim().to_owned())
    });
    let body = if status == 304 {
        json!([])
    } else {
        serde_json::from_str(body).map_err(|error| {
            SweepError::Acquisition(format!("GitHub returned invalid JSON: {error}"))
        })?
    };
    Ok(HttpResponse { status, etag, body })
}

fn gh(args: &[&str]) -> Result<(), SweepError> {
    let output = Command::new("gh")
        .args(args)
        .output()
        .map_err(|error| SweepError::Acquisition(error.to_string()))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(SweepError::Acquisition(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ))
    }
}

fn gh_raw(args: &[&str]) -> Result<Vec<u8>, SweepError> {
    let output = Command::new("gh")
        .args(args)
        .output()
        .map_err(|error| SweepError::Acquisition(error.to_string()))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(SweepError::Acquisition(if detail.is_empty() {
            "gh command failed".to_owned()
        } else {
            detail
        }))
    }
}

fn gh_json(args: &[&str]) -> Result<Value, SweepError> {
    serde_json::from_slice(&gh_raw(args)?).map_err(|error| {
        SweepError::Acquisition(format!("gh command returned malformed JSON: {error}"))
    })
}

fn exhaustive_array(value: Value, repo: &str, query: &str) -> Result<Vec<Value>, SweepError> {
    let array = value.as_array().cloned().ok_or_else(|| {
        SweepError::Acquisition(format!("{query} for {repo} returned a non-array body"))
    })?;
    if array.len() >= QUERY_LIMIT {
        return Err(SweepError::Acquisition(format!(
            "{query} for {repo} reached query_limit {QUERY_LIMIT}; refusing a truncated sweep"
        )));
    }
    Ok(array)
}

fn analyze_repository(
    config: &MandateConfig,
    project: &ProjectMandate,
    snapshot: &RepositorySnapshot,
    previous: &Value,
    mode: SweepMode,
    started_at: DateTime<Utc>,
    paths: &OstromPaths,
) -> Result<RepoAnalysis, SweepError> {
    let repo = project.repo.as_str();
    let previous_cursor = previous.get("cursor").and_then(Value::as_str);
    let initial = previous_cursor.is_none();
    let closed_ids = snapshot
        .issues
        .iter()
        .filter(|issue| issue.get("pull_request").is_none())
        .filter(|issue| string_field(issue, &["state"]).eq_ignore_ascii_case("closed"))
        .filter_map(|issue| number_field(issue, &["number"]))
        .map(|number| format!("{repo}#{number}"))
        .collect::<BTreeSet<_>>();

    let mut fresh = snapshot
        .issues
        .iter()
        .filter(|issue| issue.get("pull_request").is_none())
        .filter(|issue| !string_field(issue, &["state"]).eq_ignore_ascii_case("closed"))
        .filter_map(|issue| normalize_item(repo, issue, "issue").ok())
        .chain(
            snapshot
                .open_prs
                .iter()
                .filter(|pull| {
                    string_field(pull, &["state"]).is_empty()
                        || string_field(pull, &["state"]).eq_ignore_ascii_case("open")
                })
                .filter_map(|pull| normalize_item(repo, pull, "pr").ok()),
        )
        .collect::<Vec<_>>();
    fresh.sort_by(|left, right| left.id.cmp(&right.id));

    let mut items = if previous_cursor.is_none() && !snapshot.issue_not_modified {
        fresh.clone()
    } else {
        previous
            .get("records")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|records| records.values())
            .filter_map(|value| serde_json::from_value::<NormalizedItem>(value.clone()).ok())
            .filter(|item| item.item_type != "pr" && !closed_ids.contains(&item.id))
            .collect::<Vec<_>>()
    };
    for item in fresh {
        items.retain(|old| old.id != item.id);
        items.push(item);
    }
    items.sort_by(|left, right| left.id.cmp(&right.id));

    let selector_hash = selector_hash(config, project)?;
    let policy_changed = previous
        .get("selector_hash")
        .and_then(Value::as_str)
        .is_some_and(|old| old != selector_hash);
    let classified = items
        .iter()
        .map(|item| {
            classify_item(
                config,
                project,
                item.clone(),
                previous,
                initial,
                policy_changed,
                started_at,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;

    let active_items = classified
        .iter()
        .filter(|item| {
            is_safety(item)
                || item.hold_glob.is_some()
                || (!initial
                    && !policy_changed
                    && !project.paused
                    && item.classification.terminal == "delegated")
        })
        .collect::<Vec<_>>();
    let shadowed = classified
        .iter()
        .filter(|item| {
            item.item.item_type == "issue"
                && active_items.iter().any(|active| {
                    active.item.item_type == "pr"
                        && active.item.closing_refs.contains(&item.item.number)
                })
        })
        .map(|item| item.item.id.as_str())
        .collect::<BTreeSet<_>>();

    let mut generated = Vec::new();
    for item in &classified {
        let event = item.old.is_none()
            || item
                .old
                .as_ref()
                .and_then(|old| old.get("fingerprint"))
                .and_then(Value::as_str)
                != Some(item.item.fingerprint.as_str())
            || previous_cursor.is_some_and(|cursor| item.item.updated.as_str() > cursor)
            || (item.movement_stuck
                && !item
                    .old
                    .as_ref()
                    .and_then(|old| old.get("stuck"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false));
        let eligible = if initial || policy_changed {
            is_safety(item) || item.hold_glob.is_some()
        } else {
            event
                && (is_safety(item)
                    || item.hold_glob.is_some()
                    || (!project.paused
                        && matches!(
                            item.classification.terminal.as_str(),
                            "delegated" | "unclassified"
                        )))
        };
        if eligible && !shadowed.contains(item.item.id.as_str()) {
            generated.push(queue_row(item, &classified, config.stuck_after_days));
        }
    }

    let mut active_ids = active_items
        .iter()
        .filter(|item| !shadowed.contains(item.item.id.as_str()))
        .map(|item| item.item.id.clone())
        .collect::<BTreeSet<_>>();
    let mut current = classified
        .iter()
        .map(|item| {
            (
                item.item.id.clone(),
                json!({
                    "id": item.item.id,
                    "title": item.item.title,
                    "closing_suffix": closing_suffix(item, &classified),
                    "age_days": item.age_days,
                    "aged_out": item.age_days >= config.stuck_after_days,
                    "blocked_by": item.item.blocked_by,
                }),
            )
        })
        .collect::<BTreeMap<_, _>>();

    let ci = analyze_default_branch_ci(
        repo,
        &snapshot.ci_runs,
        previous,
        started_at,
        config.stuck_after_days,
    );
    generated.extend(ci.generated);
    active_ids.extend(ci.active_ids);
    current.extend(ci.current);

    let gate = analyze_merge_gate(
        repo,
        &snapshot.merged_prs,
        previous,
        paths,
        started_at,
        config.stuck_after_days,
    );
    generated.extend(gate.generated);
    active_ids.extend(gate.active_ids);
    current.extend(gate.current);

    let issue_cursor = snapshot
        .issues
        .iter()
        .filter_map(|issue| nonempty_string(issue, &["updatedAt", "updated_at"]).map(str::to_owned))
        .max();
    let cursor = if initial {
        format_time(started_at)
    } else if policy_changed {
        previous_cursor.unwrap_or_default().to_owned()
    } else if mode == SweepMode::Full {
        format_time(started_at)
    } else {
        [previous_cursor.map(str::to_owned), issue_cursor]
            .into_iter()
            .flatten()
            .map(|candidate| candidate.min(format_time(started_at)))
            .max()
            .unwrap_or_else(|| format_time(started_at))
    };
    let item_states = classified
        .iter()
        .map(|item| {
            let mut value = json!({
                "updated": item.item.updated,
                "fingerprint": item.item.fingerprint,
                "first_seen": item.first_seen,
                "classification": item.classification.terminal,
                "matched_selector": item.classification.selector,
                "stuck": item.movement_stuck,
            });
            if item.hold_glob.is_some() {
                value["parked"] = json!(true);
            }
            (item.item.id.clone(), value)
        })
        .collect::<Map<_, _>>();
    let records = items
        .iter()
        .map(|item| {
            (
                item.id.clone(),
                serde_json::to_value(item).expect("normalized item serializes"),
            )
        })
        .collect::<Map<_, _>>();
    let unclassified = classified
        .iter()
        .filter(|item| item.classification.terminal == "unclassified")
        .count();
    let mut state = json!({
        "cursor": cursor,
        "previous_cursor": previous_cursor.unwrap_or("initial"),
        "selector_hash": selector_hash,
        "unclassified": unclassified,
        "items": item_states,
        "records": records,
        "etag": snapshot.issue_etag,
        "ci_drift": ci.extra_state,
        "merge_gate_merges": gate.extra_state["merges"],
        "merge_gate_floor": gate.extra_state["floor"],
        "merge_gate_faults": gate.extra_state["faults"],
        "merge_gate_fault_count": gate.extra_state["fault_count"],
    });
    if let Some(old) = previous.get("notice") {
        state["notice"] = old.clone();
    }
    Ok(RepoAnalysis {
        generated,
        active_ids,
        current,
        state,
    })
}

fn normalize_item(
    repo: &str,
    source: &Value,
    item_type: &str,
) -> Result<NormalizedItem, SweepError> {
    let number = number_field(source, &["number"])
        .ok_or_else(|| SweepError::Acquisition(format!("{repo} item has no numeric number")))?;
    let id = format!("{repo}#{number}");
    let title = nonempty_string(source, &["title"])
        .unwrap_or("(title unavailable)")
        .to_owned();
    let mut labels = label_names(source.get("labels"));
    let linked = linked_issues(source);
    for issue in &linked {
        labels.extend(label_names(issue.get("labels")));
    }
    labels.sort();
    labels.dedup();
    let mut refs = vec![number];
    refs.extend(
        linked
            .iter()
            .filter_map(|issue| number_field(issue, &["number"])),
    );
    refs.sort_unstable();
    refs.dedup();
    let mut closing_refs = if item_type == "pr" {
        linked
            .iter()
            .filter_map(|issue| number_field(issue, &["number"]))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    closing_refs.sort_unstable();
    closing_refs.dedup();
    let mut files = if item_type == "pr" {
        source
            .get("files")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|file| nonempty_string(file, &["path"]).map(str::to_owned))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    files.sort();
    files.dedup();
    let blocked_by = dependency_refs(repo, string_field(source, &["body"]))?;
    let ci = if item_type == "pr" {
        pr_ci_state(source)
    } else {
        "none".to_owned()
    };
    let mergeable = string_field(source, &["mergeable"]);
    let ready = item_type == "pr"
        && !source
            .get("isDraft")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        && ci == "passing"
        && mergeable != "CONFLICTING";
    let review = string_field(source, &["reviewDecision"]).to_owned();
    let fingerprint = [
        title.clone(),
        labels.join(","),
        refs.iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(","),
        files.join(","),
        ci.clone(),
        mergeable.to_owned(),
        ready.to_string(),
        review.clone(),
    ]
    .join("|");
    Ok(NormalizedItem {
        id,
        repo: repo.to_owned(),
        number,
        reference: format!("#{number}"),
        item_type: item_type.to_owned(),
        title,
        blocked_by,
        labels,
        refs,
        closing_refs,
        files,
        opened: string_field(source, &["createdAt", "created_at"]).to_owned(),
        updated: string_field(source, &["updatedAt", "updated_at"]).to_owned(),
        ci,
        ready,
        review,
        fingerprint,
    })
}

fn classify_item(
    config: &MandateConfig,
    project: &ProjectMandate,
    item: NormalizedItem,
    previous: &Value,
    initial: bool,
    policy_changed: bool,
    started_at: DateTime<Utc>,
) -> Result<ClassifiedItem, SweepError> {
    let classification = classify(config, project, &item)?;
    let hold_glob = if classification.terminal == "delegated" {
        config
            .hold_labels
            .iter()
            .find(|glob| {
                item.labels
                    .iter()
                    .any(|label| glob_match(label, glob, false))
            })
            .cloned()
    } else {
        None
    };
    let old = previous
        .get("items")
        .and_then(|items| items.get(&item.id))
        .cloned();
    let first_seen = if initial || policy_changed {
        format_time(started_at)
    } else {
        old.as_ref()
            .and_then(|old| old.get("first_seen"))
            .and_then(Value::as_str)
            .unwrap_or(&format_time(started_at))
            .to_owned()
    };
    let movement_clock = parse_time(&first_seen)
        .into_iter()
        .chain(parse_time(&item.updated))
        .max()
        .unwrap_or(started_at);
    let age_days = days_since(started_at, &item.opened);
    let movement_stuck = !initial
        && !policy_changed
        && classification.terminal == "delegated"
        && hold_glob.is_none()
        && started_at.signed_duration_since(movement_clock)
            >= Duration::days(i64::try_from(config.stuck_after_days).unwrap_or(i64::MAX));
    Ok(ClassifiedItem {
        item,
        classification,
        hold_glob,
        first_seen,
        age_days,
        movement_stuck,
        old,
    })
}

fn classify(
    config: &MandateConfig,
    project: &ProjectMandate,
    item: &NormalizedItem,
) -> Result<Classification, SweepError> {
    if let Some(number) = project
        .reserved
        .iter()
        .find(|number| item.refs.contains(number))
    {
        return Ok(Classification {
            terminal: "reserved".to_owned(),
            source: "reserved".to_owned(),
            selector: format!("ref:#{number}"),
        });
    }
    for (selectors, source, terminal) in [
        (&config.bounce_all, "bounce_all", "tripwire"),
        (&project.bounce, "project bounce", "tripwire"),
        (&project.excluded, "excluded", "excluded"),
        (&project.delegated, "delegated", "delegated"),
    ] {
        for selector in selectors {
            if selector_match(item, selector)? {
                return Ok(Classification {
                    terminal: terminal.to_owned(),
                    source: source.to_owned(),
                    selector: selector.as_str().to_owned(),
                });
            }
        }
    }
    let terminal = match project.default {
        DefaultDisposition::Delegated => "delegated",
        DefaultDisposition::Excluded => "excluded",
        DefaultDisposition::Unclassified => "unclassified",
    };
    Ok(Classification {
        terminal: terminal.to_owned(),
        source: "default".to_owned(),
        selector: format!("default:{terminal}"),
    })
}

fn selector_match(item: &NormalizedItem, selector: &Selector) -> Result<bool, SweepError> {
    let (prefix, glob) = selector
        .as_str()
        .split_once(':')
        .ok_or_else(|| SweepError::Config("selector has no prefix".to_owned()))?;
    let (item_type, scopes) = conventional(&item.title);
    Ok(match prefix {
        "label" => item
            .labels
            .iter()
            .any(|value| glob_match(value, glob, false)),
        "scope" => scopes.iter().any(|value| glob_match(value, glob, false)),
        "type" => glob_match(&item_type, glob, false),
        "path" => {
            item.item_type == "pr" && item.files.iter().any(|value| glob_match(value, glob, true))
        }
        "ref" => item.refs.iter().any(|number| format!("#{number}") == glob),
        "title" => glob_match(&item.title, glob, false),
        _ => false,
    })
}

fn queue_row(item: &ClassifiedItem, all: &[ClassifiedItem], stuck_after_days: u64) -> Value {
    let match_reason = if item.classification.source == "default" {
        item.classification.selector.clone()
    } else {
        format!(
            "{} {}",
            item.classification.source, item.classification.selector
        )
    };
    let (kind, mut reason) = if item.classification.terminal == "reserved" {
        (
            "decision",
            format!("reserved {}", item.classification.selector),
        )
    } else if item.classification.terminal == "tripwire" {
        ("tripwire", format!("tripwire: {match_reason}"))
    } else if item.classification.terminal == "unclassified" {
        (
            "decision",
            format!("no selector matched ({match_reason}); classification needed"),
        )
    } else if item.item.item_type == "pr" && item.item.ci == "failing" {
        ("drift", format!("CI is failing; {match_reason}"))
    } else if let Some(glob) = &item.hold_glob {
        ("parked", format!("hold label {glob}"))
    } else if item.movement_stuck {
        (
            "stuck",
            format!("{match_reason}; no movement for {stuck_after_days} days"),
        )
    } else if item.item.ready {
        ("decision", format!("{match_reason}; open PR passed CI"))
    } else {
        (
            "moved",
            format!("{match_reason}; updated since the read cursor"),
        )
    };
    reason.push_str(&closing_suffix(item, all));
    let mandate = if kind == "tripwire" {
        json!({
            "reason": reason,
            "dossier": {
                "question": format!("May {}{} cross the matched mandate tripwire?", item.item.repo, item.item.reference),
                "options_ruled_out": ["Auto-proceed — a tripwire requires human judgment."],
                "recommended_action": format!("Review {}{}, then approve, reject, or defer it in /ostrom:desk.", item.item.repo, item.item.reference),
                "blast_radius": format!("{}{} only.", item.item.repo, item.item.reference),
            }
        })
    } else {
        json!({"reason": reason})
    };
    json!({
        "id": item.item.id,
        "repo": item.item.repo,
        "ref": item.item.reference,
        "title": item.item.title,
        "kind": kind,
        "mandate": mandate,
        "state": "pending",
        "opened": item.item.opened,
        "age_days": item.age_days,
        "aged_out": item.age_days >= stuck_after_days,
        "needs_judgment": matches!(kind, "tripwire" | "decision"),
        "blocked_by": item.item.blocked_by,
    })
}

fn closing_suffix(item: &ClassifiedItem, all: &[ClassifiedItem]) -> String {
    let mut refs = item
        .item
        .closing_refs
        .iter()
        .filter(|number| {
            all.iter().any(|candidate| {
                candidate.item.item_type == "issue" && candidate.item.number == **number
            })
        })
        .copied()
        .collect::<Vec<_>>();
    refs.sort_unstable();
    refs.dedup();
    if refs.is_empty() {
        String::new()
    } else {
        format!(
            " (closes {})",
            refs.iter()
                .map(|number| format!("#{number}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

fn is_safety(item: &ClassifiedItem) -> bool {
    matches!(
        item.classification.terminal.as_str(),
        "reserved" | "tripwire"
    ) || (item.item.item_type == "pr" && item.item.ci == "failing")
}

struct SupplementalAnalysis {
    generated: Vec<Value>,
    active_ids: BTreeSet<String>,
    current: BTreeMap<String, Value>,
    extra_state: Value,
}

fn analyze_default_branch_ci(
    repo: &str,
    runs: &[Value],
    previous: &Value,
    started_at: DateTime<Utc>,
    stuck_after_days: u64,
) -> SupplementalAnalysis {
    let mut grouped = BTreeMap::<String, Vec<&Value>>::new();
    for run in runs {
        let key = run
            .get("workflowDatabaseId")
            .and_then(Value::as_u64)
            .map(|value| value.to_string())
            .or_else(|| nonempty_string(run, &["name"]).map(str::to_owned))
            .unwrap_or_default();
        grouped.entry(key).or_default().push(run);
    }
    let previous_ci = previous
        .get("ci_drift")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let initial = previous.get("cursor").is_none();
    let mut generated = Vec::new();
    let mut active_ids = BTreeSet::new();
    let mut current = BTreeMap::new();
    let mut state = Map::new();
    for history in grouped.values_mut() {
        history.sort_by_key(|run| string_field(run, &["createdAt"]));
        history.reverse();
        let Some(latest) = history.first() else {
            continue;
        };
        if string_field(latest, &["status"]) != "completed"
            || !failure_conclusion(string_field(latest, &["conclusion"]))
        {
            continue;
        }
        let Some(workflow_id) = number_field(latest, &["workflowDatabaseId"]) else {
            continue;
        };
        let key = workflow_id.to_string();
        let mut red_since = string_field(latest, &["createdAt"]).to_owned();
        for run in history.iter() {
            if string_field(run, &["status"]) != "completed" {
                continue;
            }
            if failure_conclusion(string_field(run, &["conclusion"])) {
                red_since = string_field(run, &["createdAt"]).to_owned();
            } else {
                break;
            }
        }
        let old = previous_ci.get(&key);
        if let Some(old_red) = old
            .and_then(|old| old.get("red_since"))
            .and_then(Value::as_str)
            .filter(|old_red| *old_red < red_since.as_str())
        {
            red_since = old_red.to_owned();
        }
        let run_id = number_field(latest, &["databaseId"]).unwrap_or_default();
        let event = initial
            || old.is_none()
            || old
                .and_then(|old| old.get("run_id"))
                .and_then(Value::as_u64)
                != Some(run_id);
        let id = format!("{repo}#{workflow_id}");
        let age_days = days_since(started_at, &red_since);
        active_ids.insert(id.clone());
        current.insert(
            id.clone(),
            json!({"id": id, "age_days": age_days, "aged_out": age_days >= stuck_after_days}),
        );
        if event {
            let name = nonempty_string(latest, &["workflowName", "name"])
                .unwrap_or("(workflow name unavailable)");
            let sha = string_field(latest, &["headSha"]);
            generated.push(json!({
                "id": id,
                "repo": repo,
                "ref": format!("#{workflow_id}"),
                "title": format!("CI failing on default branch: {name}"),
                "kind": "drift",
                "mandate": {"reason": format!("default branch CI failing: {name}; run {run_id} at {}; red since {red_since}", &sha[..sha.len().min(8)])},
                "state": "pending",
                "opened": red_since,
                "age_days": age_days,
                "aged_out": age_days >= stuck_after_days,
                "needs_judgment": false,
                "blocked_by": [],
            }));
        }
        state.insert(key, json!({"run_id": run_id, "red_since": red_since}));
    }
    SupplementalAnalysis {
        generated,
        active_ids,
        current,
        extra_state: Value::Object(state),
    }
}

fn analyze_merge_gate(
    repo: &str,
    merged: &[Value],
    previous: &Value,
    paths: &OstromPaths,
    started_at: DateTime<Utc>,
    stuck_after_days: u64,
) -> SupplementalAnalysis {
    let gate_read = read_jsonl_values(&paths.state.join("gate.jsonl"));
    let gate_degraded = gate_read.is_err();
    let gate_records = gate_read.unwrap_or_default();
    let exception_records =
        read_jsonl_values(&paths.state.join("exceptions.jsonl")).unwrap_or_default();
    let recorded_floor = gate_records
        .iter()
        .filter(|record| {
            string_field(record, &["pr"]).starts_with(&format!("{repo}#"))
                && matches!(
                    string_field(record, &["verdict"]),
                    "pass" | "fail" | "inconclusive"
                )
        })
        .filter_map(|record| {
            let timestamp = nonempty_string(record, &["ts"])?;
            parse_time(timestamp).map(|parsed| (parsed, timestamp.to_owned()))
        })
        .min_by_key(|(parsed, _)| *parsed)
        .map(|(_, timestamp)| timestamp);
    let floor = if gate_degraded {
        recorded_floor.or_else(|| {
            previous
                .get("merge_gate_floor")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
    } else {
        recorded_floor
    };
    let mut known = previous
        .get("merge_gate_merges")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for pull in merged {
        let Some(number) = number_field(pull, &["number"]) else {
            continue;
        };
        let author_login = nonempty_string(pull, &["author", "login"])
            .or_else(|| pull.pointer("/author/login").and_then(Value::as_str))
            .unwrap_or("");
        let is_bot = pull
            .pointer("/author/is_bot")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || pull
                .pointer("/author/isBot")
                .and_then(Value::as_bool)
                .unwrap_or(false);
        let machine_authored = is_bot || author_login.ends_with("[bot]");
        let work_order_refs = linked_issues(pull)
            .iter()
            .filter_map(|issue| number_field(issue, &["number"]))
            .map(|issue| format!("{repo}#{issue}"))
            .collect::<Vec<_>>();
        known.insert(
            format!("{repo}#{number}"),
            json!({
                "id": format!("{repo}#{number}"),
                "number": number,
                "title": nonempty_string(pull, &["title"]).unwrap_or("(title unavailable)"),
                "created_at": string_field(pull, &["createdAt", "mergedAt"]),
                "merged_at": string_field(pull, &["mergedAt"]),
                "head_sha": string_field(pull, &["headRefOid"]),
                "machine_authored": machine_authored,
                "machine_author": if machine_authored { json!({"login": author_login, "is_bot": is_bot}) } else { Value::Null },
                "work_order_refs": work_order_refs,
            }),
        );
    }
    let gate_by_sha =
        gate_records
            .iter()
            .fold(BTreeMap::<String, Vec<&Value>>::new(), |mut map, record| {
                if let Some(sha) = nonempty_string(record, &["head_sha"]) {
                    map.entry(sha.to_owned()).or_default().push(record);
                }
                map
            });
    let old_faults = previous
        .get("merge_gate_faults")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut generated = Vec::new();
    let mut active_ids = BTreeSet::new();
    let mut current = BTreeMap::new();
    let mut faults = Map::new();
    for merge in known.values() {
        let merged_at = string_field(merge, &["merged_at"]);
        let in_scope = merge
            .get("machine_authored")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || merge
                .get("work_order_refs")
                .and_then(Value::as_array)
                .is_some_and(|refs| !refs.is_empty());
        let Some(merged_time) = parse_time(merged_at) else {
            continue;
        };
        let predates_floor = floor
            .as_deref()
            .and_then(parse_time)
            .is_some_and(|floor_time| merged_time < floor_time);
        if !in_scope || predates_floor || (floor.is_none() && !gate_degraded) {
            continue;
        }
        let sha = string_field(merge, &["head_sha"]);
        let records = gate_by_sha.get(sha).cloned().unwrap_or_default();
        let timely_pass = records.iter().any(|record| {
            string_field(record, &["verdict"]) == "pass"
                && parse_time(string_field(record, &["ts"]))
                    .is_some_and(|gate_time| gate_time < merged_time)
        });
        if timely_pass {
            continue;
        }
        let (shape, verdict, gate_ts, reason) = if records.is_empty() {
            (
                "no_verdict",
                "none",
                "",
                format!("merge gate fault: no verdict for merged head {sha}"),
            )
        } else if let Some(pass) = records
            .iter()
            .find(|record| string_field(record, &["verdict"]) == "pass")
        {
            (
                "pass_after_merge",
                "pass",
                string_field(pass, &["ts"]),
                format!("merge gate fault: pass recorded after merge for head {sha}"),
            )
        } else {
            let last = records.last().copied().unwrap_or(&Value::Null);
            let verdict = string_field(last, &["verdict"]);
            (
                "non_pass",
                verdict,
                string_field(last, &["ts"]),
                format!("merge gate fault: {verdict} verdict for merged head {sha}"),
            )
        };
        let id = string_field(merge, &["id"]).to_owned();
        let merge_number = number_field(merge, &["number"]).unwrap_or_default();
        let excused = exception_records.iter().any(|exception| {
            string_field(exception, &["repo"]) == repo
                && number_field(exception, &["pr"]) == Some(merge_number)
                && string_field(exception, &["head_sha"]) == sha
                && string_field(exception, &["condition"]) == "merge_protocol"
                && nonempty_string(exception, &["reason"]).is_some()
        });
        if excused {
            continue;
        }
        let machine_authored = merge
            .get("machine_authored")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let work_order_refs = merge
            .get("work_order_refs")
            .cloned()
            .unwrap_or_else(|| json!([]));
        let mut basis = Vec::new();
        if machine_authored {
            basis.push("machine_authorship");
        }
        if work_order_refs
            .as_array()
            .is_some_and(|refs| !refs.is_empty())
        {
            basis.push("work_order");
        }
        let fingerprint = format!(
            "scope-v1|{shape}|{sha}|{verdict}|{gate_ts}|{machine_authored}|{}",
            work_order_refs
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(",")
        );
        let age_days = days_since(started_at, merged_at);
        active_ids.insert(id.clone());
        current.insert(
            id.clone(),
            json!({"id": id, "title": merge["title"], "age_days": age_days, "aged_out": age_days >= stuck_after_days}),
        );
        if old_faults
            .get(&id)
            .and_then(|fault| fault.get("fingerprint"))
            .and_then(Value::as_str)
            != Some(fingerprint.as_str())
        {
            generated.push(json!({
                "id": id,
                "repo": repo,
                "ref": format!("#{merge_number}"),
                "title": merge["title"],
                "kind": "merge-gate-fault",
                "mandate": {
                    "reason": reason,
                    "scope_evidence": {
                        "basis": basis,
                        "machine_author": merge["machine_author"],
                        "work_order_refs": work_order_refs,
                    }
                },
                "state": "pending",
                "opened": merged_at,
                "age_days": age_days,
                "aged_out": age_days >= stuck_after_days,
                "needs_judgment": false,
                "blocked_by": [],
            }));
        }
        faults.insert(
            id,
            json!({"shape": shape, "head_sha": sha, "verdict": verdict, "gate_ts": if gate_ts.is_empty() { Value::Null } else { json!(gate_ts) }, "fingerprint": fingerprint}),
        );
    }
    SupplementalAnalysis {
        generated,
        active_ids,
        current,
        extra_state: json!({
            "merges": known,
            "floor": floor,
            "faults": faults,
            "fault_count": faults.len(),
        }),
    }
}

fn reconcile_queue(
    existing: Vec<QueueDocument>,
    generated: Vec<Value>,
    active_ids: &BTreeSet<String>,
    current: &BTreeMap<String, Value>,
    configured: &BTreeSet<&str>,
) -> Result<Vec<QueueDocument>, SweepError> {
    let existing_values = existing
        .iter()
        .map(|row| row.value().clone())
        .collect::<Vec<_>>();
    let mut result = existing_values
        .iter()
        .filter(|row| {
            let id = string_field(row, &["id"]);
            let repo = string_field(row, &["repo"]);
            active_ids.contains(id)
                || string_field(row, &["state"]) == "approved"
                || (!configured.contains(repo) && string_field(row, &["kind"]) == "drift")
        })
        .cloned()
        .collect::<Vec<_>>();
    for mut row in generated {
        let id = string_field(&row, &["id"]).to_owned();
        if let Some(old) = existing_values
            .iter()
            .find(|candidate| string_field(candidate, &["id"]) == id)
        {
            if let Some(state) = old.get("state") {
                row["state"] = state.clone();
            }
        }
        result.retain(|candidate| string_field(candidate, &["id"]) != id);
        result.push(row);
    }
    for row in &mut result {
        if let Some(item) = current.get(string_field(row, &["id"])) {
            for field in ["title", "age_days", "aged_out", "blocked_by"] {
                if let Some(value) = item.get(field) {
                    row[field] = value.clone();
                }
            }
        }
        let kind = string_field(row, &["kind"]);
        row["needs_judgment"] = json!(matches!(kind, "tripwire" | "decision"));
    }
    result.sort_by(|left, right| {
        (string_field(left, &["opened"]), string_field(left, &["id"])).cmp(&(
            string_field(right, &["opened"]),
            string_field(right, &["id"]),
        ))
    });
    result
        .into_iter()
        .map(QueueDocument::from_value)
        .collect::<Result<Vec<_>, _>>()
        .map_err(SweepError::Store)
}

fn fault_row(repo: &str, reason: &str, started_at: DateTime<Utc>) -> Value {
    json!({
        "id": format!("{repo}#0"),
        "repo": repo,
        "ref": "#0",
        "title": "Sweep fault: portfolio data is incomplete",
        "kind": "drift",
        "mandate": {"reason": format!("sweep fault: {reason}")},
        "state": "pending",
        "opened": format_time(started_at),
        "age_days": 0,
        "aged_out": false,
        "needs_judgment": false,
        "blocked_by": [],
    })
}

fn load_config(paths: &OstromPaths, cwd: &Path) -> Result<MandateConfig, SweepError> {
    let user_path = paths.config.join("mandates.yaml");
    let repo_path = cwd.join(".ostrom/mandates.yaml");
    if !user_path.exists() && !repo_path.exists() {
        return Err(SweepError::NotConfigured(user_path.display().to_string()));
    }
    let mut merged: serde_yaml::Value = serde_yaml::from_str(SHIPPED_DEFAULTS)
        .map_err(|error| SweepError::Config(error.to_string()))?;
    for path in [&user_path, &repo_path] {
        if path.exists() {
            let text = fs::read_to_string(path)
                .map_err(|error| SweepError::Config(format!("{}: {error}", path.display())))?;
            let overlay = serde_yaml::from_str(&text)
                .map_err(|error| SweepError::Config(format!("{}: {error}", path.display())))?;
            merge_yaml(&mut merged, overlay);
        }
    }
    let serialized =
        serde_yaml::to_string(&merged).map_err(|error| SweepError::Config(error.to_string()))?;
    MandateConfig::from_yaml(&serialized).map_err(|error| SweepError::Config(error.to_string()))
}

fn merge_yaml(base: &mut serde_yaml::Value, overlay: serde_yaml::Value) {
    match (base, overlay) {
        (serde_yaml::Value::Mapping(base), serde_yaml::Value::Mapping(overlay)) => {
            for (key, value) in overlay {
                if let Some(existing) = base.get_mut(&key) {
                    merge_yaml(existing, value);
                } else {
                    base.insert(key, value);
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

fn effective_mode(
    requested: SweepMode,
    config: &MandateConfig,
    state: &Value,
    now: DateTime<Utc>,
) -> SweepMode {
    let supports_incremental = config.projects.iter().all(|project| {
        state
            .get("repos")
            .and_then(|repos| repos.get(project.repo.as_str()))
            .is_some_and(|repo| {
                repo.get("cursor").is_some_and(Value::is_string)
                    && repo.get("records").is_some_and(Value::is_object)
            })
    });
    let full_due = state
        .get("last_full_reconciliation")
        .and_then(Value::as_str)
        .and_then(parse_time)
        .is_none_or(|last| {
            let age = now.signed_duration_since(last);
            age < Duration::zero() || age >= Duration::hours(FULL_RECONCILIATION_HOURS)
        });
    if requested == SweepMode::Full
        || !supports_incremental
        || (requested == SweepMode::Auto && full_due)
    {
        SweepMode::Full
    } else {
        SweepMode::Incremental
    }
}

fn validate_fixture(config: &MandateConfig, fixture: &SweepFixture) -> Result<(), SweepError> {
    let expected = config
        .projects
        .iter()
        .map(|project| project.repo.as_str())
        .collect::<BTreeSet<_>>();
    let actual = fixture
        .repositories
        .iter()
        .map(|snapshot| snapshot.repo.as_str())
        .collect::<BTreeSet<_>>();
    if expected != actual || actual.len() != fixture.repositories.len() {
        return Err(SweepError::Fixture(
            "fixture repositories must match the configured roster exactly".to_owned(),
        ));
    }
    for snapshot in &fixture.repositories {
        for (name, count) in [
            ("open pull-request query", snapshot.open_prs.len()),
            (
                "recent merged pull-request query",
                snapshot.merged_prs.len(),
            ),
            ("issues change feed", snapshot.issues.len()),
            ("default-branch CI query", snapshot.ci_runs.len()),
        ] {
            if count >= QUERY_LIMIT {
                return Err(SweepError::Fixture(format!(
                    "{name} for {} reached query_limit {QUERY_LIMIT}; refusing a truncated sweep",
                    snapshot.repo
                )));
            }
        }
    }
    Ok(())
}

fn read_state(path: &Path) -> Result<Value, SweepError> {
    if !path.exists() {
        return Ok(json!({"version": 2, "repos": {}}));
    }
    let text = fs::read_to_string(path).map_err(|error| SweepError::State(error.to_string()))?;
    let value: Value =
        serde_json::from_str(&text).map_err(|error| SweepError::State(error.to_string()))?;
    if !value.is_object() {
        return Err(SweepError::State("state is not an object".to_owned()));
    }
    Ok(value)
}

fn ensure_state_shape(state: &mut Value) {
    if !state.is_object() {
        *state = json!({});
    }
    if !state.get("repos").is_some_and(Value::is_object) {
        state["repos"] = json!({});
    }
}

fn write_json_private(path: &Path, value: &Value) -> Result<(), SweepError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| io_error("create directory", parent, error))?;
    let temporary = path.with_extension(format!("tmp.{}", std::process::id()));
    let bytes = serde_json::to_vec_pretty(value).expect("JSON value serializes");
    fs::write(&temporary, bytes)
        .map_err(|error| io_error("write sweep state", &temporary, error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
            .map_err(|error| io_error("set private sweep state mode", &temporary, error))?;
    }
    fs::rename(&temporary, path).map_err(|error| io_error("install sweep state", path, error))?;
    Ok(())
}

fn publish(options: &SweepOptions, repository: &RepositoryName) -> Result<(), SweepError> {
    let compatibility =
        tempfile::tempdir().map_err(|error| SweepError::Publish(error.to_string()))?;
    let data = compatibility.path().join("ostrom");
    fs::create_dir(&data).map_err(|error| SweepError::Publish(error.to_string()))?;
    for (source, name) in [
        (options.paths.queue_file(), "queue.jsonl"),
        (options.paths.state.join("state.json"), "state.json"),
        (options.paths.state.join("gate.jsonl"), "gate.jsonl"),
    ] {
        if source.exists() {
            fs::copy(&source, data.join(name)).map_err(|error| {
                SweepError::Publish(format!("could not stage {}: {error}", source.display()))
            })?;
        }
    }
    let status = Command::new("bash")
        .arg(options.plugin_root.join("scripts/publish.sh"))
        .current_dir(&options.working_directory)
        .env("CLAUDE_CONFIG_DIR", compatibility.path())
        .env("MANDATE_PUBLISH_REMOTE", repository.as_str())
        .env("MANDATE_PUBLISH_DIR", options.paths.state.join("publish"))
        .env("MANDATE_SWEEP_TIME", format_time(options.started_at))
        .status()
        .map_err(|error| SweepError::Publish(error.to_string()))?;
    if status.success() {
        Ok(())
    } else {
        Err(SweepError::Publish(format!(
            "publisher exited with status {status}"
        )))
    }
}

fn symmetric_queue_changes(before: &[QueueDocument], after: &[QueueDocument]) -> usize {
    let before = before
        .iter()
        .map(|row| serde_json::to_string(row.value()).expect("queue value serializes"))
        .collect::<BTreeSet<_>>();
    let after = after
        .iter()
        .map(|row| serde_json::to_string(row.value()).expect("queue value serializes"))
        .collect::<BTreeSet<_>>();
    before.symmetric_difference(&after).count()
}

fn selector_hash(config: &MandateConfig, project: &ProjectMandate) -> Result<String, SweepError> {
    let mut policy = Map::new();
    policy.insert("delegated".to_owned(), selectors_value(&project.delegated));
    policy.insert("excluded".to_owned(), selectors_value(&project.excluded));
    policy.insert("reserved".to_owned(), json!(project.reserved));
    policy.insert("bounce".to_owned(), selectors_value(&project.bounce));
    policy.insert("bounce_all".to_owned(), selectors_value(&config.bounce_all));
    policy.insert(
        "default".to_owned(),
        json!(match project.default {
            DefaultDisposition::Delegated => "delegated",
            DefaultDisposition::Excluded => "excluded",
            DefaultDisposition::Unclassified => "unclassified",
        }),
    );
    if !config.hold_labels.is_empty() {
        policy.insert("hold_labels".to_owned(), json!(config.hold_labels));
    }
    let encoded =
        serde_json::to_string(&policy).map_err(|error| SweepError::Config(error.to_string()))?;
    let hash = encoded.bytes().fold(0_u64, |hash, byte| {
        (hash * 31 + u64::from(byte)) % 2_147_483_647
    });
    Ok(hash.to_string())
}

fn selectors_value(selectors: &[Selector]) -> Value {
    Value::Array(
        selectors
            .iter()
            .map(|selector| json!(selector.as_str()))
            .collect(),
    )
}

fn conventional(title: &str) -> (String, Vec<String>) {
    let Ok(regex) = Regex::new(r"^([^(:\s]+)(?:\(([^)]*)\))?:") else {
        return (String::new(), Vec::new());
    };
    let Some(captures) = regex.captures(title) else {
        return (String::new(), Vec::new());
    };
    let item_type = captures
        .get(1)
        .map_or("", |value| value.as_str())
        .to_owned();
    let scopes = captures
        .get(2)
        .map_or("", |value| value.as_str())
        .split(',')
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .map(str::to_owned)
        .collect();
    (item_type, scopes)
}

fn glob_match(value: &str, glob: &str, path: bool) -> bool {
    let mut body = String::from("^");
    let chars = glob.chars().collect::<Vec<_>>();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '*' {
            if path && chars.get(index + 1) == Some(&'*') {
                if chars.get(index + 2) == Some(&'/') {
                    body.push_str("(?:.*/)?");
                    index += 3;
                } else {
                    body.push_str(".*");
                    index += 2;
                }
            } else {
                body.push_str(if path { "[^/]*" } else { ".*" });
                index += 1;
            }
        } else {
            body.push_str(&regex::escape(&chars[index].to_string()));
            index += 1;
        }
    }
    body.push('$');
    Regex::new(&format!("(?i:{body})")).is_ok_and(|regex| regex.is_match(value))
}

fn dependency_refs(repo: &str, body: &str) -> Result<Vec<String>, SweepError> {
    let regex = Regex::new(
        r"(?i)(?:depends\s+on|blocked\s+by|gate\s+for)\s+((?:[[:alnum:]_.-]+/[[:alnum:]_.-]+)?#[1-9][0-9]*)",
    )
    .map_err(|error| SweepError::Config(error.to_string()))?;
    let mut refs = regex
        .captures_iter(body)
        .filter_map(|captures| captures.get(1).map(|value| value.as_str()))
        .map(|value| {
            if value.starts_with('#') {
                format!("{repo}{value}")
            } else {
                value.to_owned()
            }
        })
        .collect::<Vec<_>>();
    refs.sort();
    refs.dedup();
    Ok(refs)
}

fn linked_issues(source: &Value) -> Vec<&Value> {
    match source.get("closingIssuesReferences") {
        Some(Value::Array(values)) => values.iter().collect(),
        Some(Value::Object(object)) => object
            .get("nodes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .collect(),
        _ => Vec::new(),
    }
}

fn label_names(labels: Option<&Value>) -> Vec<String> {
    match labels {
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(|value| nonempty_string(value, &["name"]).map(str::to_owned))
            .collect(),
        Some(Value::Object(object)) => object
            .get("nodes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|value| nonempty_string(value, &["name"]).map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

fn pr_ci_state(source: &Value) -> String {
    let checks = source
        .get("statusCheckRollup")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    if checks
        .iter()
        .any(|check| failure_conclusion(string_field(check, &["conclusion", "state"])))
    {
        "failing".to_owned()
    } else if !checks.is_empty()
        && checks
            .iter()
            .all(|check| success_conclusion(string_field(check, &["conclusion", "state"])))
    {
        "passing".to_owned()
    } else {
        "pending".to_owned()
    }
}

fn failure_conclusion(value: &str) -> bool {
    matches!(
        value.to_ascii_uppercase().as_str(),
        "FAILURE"
            | "ERROR"
            | "CANCELLED"
            | "TIMED_OUT"
            | "ACTION_REQUIRED"
            | "STALE"
            | "STARTUP_FAILURE"
    )
}

fn success_conclusion(value: &str) -> bool {
    matches!(
        value.to_ascii_uppercase().as_str(),
        "SUCCESS" | "NEUTRAL" | "SKIPPED"
    )
}

fn string_field<'a>(value: &'a Value, fields: &[&str]) -> &'a str {
    nonempty_string(value, fields).unwrap_or("")
}

fn nonempty_string<'a>(value: &'a Value, fields: &[&str]) -> Option<&'a str> {
    fields
        .iter()
        .find_map(|field| value.get(field).and_then(Value::as_str))
        .filter(|value| !value.is_empty())
}

fn number_field(value: &Value, fields: &[&str]) -> Option<u64> {
    fields
        .iter()
        .find_map(|field| value.get(field).and_then(Value::as_u64))
}

fn parse_time(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

fn format_time(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn days_since(now: DateTime<Utc>, opened: &str) -> u64 {
    parse_time(opened)
        .map(|opened| now.signed_duration_since(opened).num_days().max(0))
        .and_then(|days| u64::try_from(days).ok())
        .unwrap_or(0)
}

fn owner(repo: &str) -> &str {
    repo.split_once('/').map_or(repo, |(owner, _)| owner)
}

fn mode_name(mode: SweepMode) -> &'static str {
    match mode {
        SweepMode::Auto => "auto",
        SweepMode::Full => "full",
        SweepMode::Incremental => "incremental",
    }
}

fn read_jsonl_values(path: &Path) -> Result<Vec<Value>, SweepError> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    fs::read_to_string(path)
        .map_err(|error| SweepError::State(error.to_string()))?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line).map_err(|error| SweepError::State(error.to_string()))
        })
        .collect()
}

pub fn encode_org_snapshots(repositories: Vec<RepositorySnapshot>) -> Result<Vec<u8>, SweepError> {
    serde_json::to_vec(&OrgSnapshots { repositories })
        .map_err(|error| SweepError::Acquisition(error.to_string()))
}
