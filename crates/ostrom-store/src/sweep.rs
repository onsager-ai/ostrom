use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use ostrom_core::{
    DefaultDisposition, MandateConfig, ProjectMandate, RepositoryName, Selector, WorkNodeInput,
    build_work_graph,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use thiserror::Error;

use crate::{
    AppTokenError, OstromPaths, QueueDocument, StoreError,
    app_token::{AppTokenRequest, mint_installation_token},
    io_error, read_queue, set_private_file_mode, write_queue,
};

const QUERY_LIMIT: usize = 200;
const FULL_RECONCILIATION_HOURS: i64 = 24;
/// A partially reachable portfolio remains useful when failed repositories are
/// retained from the previous generation. Zero acquired repositories cannot
/// distinguish a quiet portfolio from the 2026-08-18 authentication outage,
/// so that incident-shaped result must never reach persistence.
const MIN_ACQUIRED_REPOSITORIES_TO_WRITE: usize = 1;
/// Read-only scope for an acquisition token, kept identical to the set
/// `scripts/sweep.sh` requests so both paths mint the same grant. A sweep
/// reads; it never writes, so no write permission belongs here.
const SWEEP_TOKEN_PERMISSIONS: &str = "metadata:read,issues:read,pull_requests:read,checks:read,statuses:read,actions:read,contents:read";
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
    #[error(
        "refusing to overwrite queue and state: repository acquisition succeeded for {acquired} of {configured} configured repositories; at least {minimum} must succeed; acquisition faults: {details}"
    )]
    AcquisitionRefused {
        acquired: usize,
        configured: usize,
        minimum: usize,
        details: String,
    },
    #[error(transparent)]
    AppToken(#[from] AppTokenError),
    #[error("{0}")]
    BranchListingTruncated(String),
    #[error("sweep fixture is malformed: {0}")]
    Fixture(String),
    #[error("could not read local work orders: {0}")]
    WorkOrders(String),
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
    pub branches: Vec<Value>,
    #[serde(default)]
    pub branch_read_degraded: bool,
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
    #[serde(default)]
    parent: Option<String>,
    #[serde(default)]
    children: Vec<String>,
    #[serde(default)]
    closes: Vec<String>,
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

#[derive(Debug, Clone, Deserialize)]
struct WorkOrderEvidence {
    repository: String,
    branch_name: String,
    item_id: String,
    order_id: String,
}

struct RepositoryEvidence<'a> {
    paths: &'a OstromPaths,
    work_orders: &'a [WorkOrderEvidence],
    queued_kinds: &'a BTreeMap<String, String>,
}

struct SweepToken(String);

trait InstallationTokenMinter {
    fn mint(
        &mut self,
        paths: &OstromPaths,
        request: AppTokenRequest<'_>,
    ) -> Result<SweepToken, AppTokenError>;
}

struct GitHubInstallationTokenMinter;

impl InstallationTokenMinter for GitHubInstallationTokenMinter {
    fn mint(
        &mut self,
        paths: &OstromPaths,
        request: AppTokenRequest<'_>,
    ) -> Result<SweepToken, AppTokenError> {
        mint_installation_token(paths, request).map(|token| SweepToken(token.expose().to_owned()))
    }
}

pub fn run_sweep(options: &SweepOptions) -> Result<SweepOutcome, SweepError> {
    run_sweep_with_mirror(options).map(|(outcome, _mirror)| outcome)
}

pub(crate) fn run_sweep_with_mirror(
    options: &SweepOptions,
) -> Result<(SweepOutcome, Vec<RepositorySnapshot>), SweepError> {
    let mut minter = GitHubInstallationTokenMinter;
    run_sweep_with_minter(options, &mut minter)
}

fn run_sweep_with_minter(
    options: &SweepOptions,
    minter: &mut dyn InstallationTokenMinter,
) -> Result<(SweepOutcome, Vec<RepositorySnapshot>), SweepError> {
    let config = load_config(&options.paths, &options.working_directory)?;
    if config.projects.is_empty() {
        return Err(SweepError::Config(
            "mandates.yaml contains no projects".to_owned(),
        ));
    }
    let existing = read_queue(&options.paths.queue_file())?;
    let mut queued_kinds = BTreeMap::new();
    for row in &existing {
        queued_kinds
            .entry(string_field(row.value(), &["id"]).to_owned())
            .or_insert_with(|| string_field(row.value(), &["kind"]).to_owned());
    }
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
        acquire_by_organization(options, &config, &old_state, mode, minter)?
    };
    let configured_repositories = config
        .projects
        .iter()
        .map(|project| project.repo.as_str().to_owned())
        .collect::<BTreeSet<_>>();
    let acquired_repositories = snapshots
        .iter()
        .map(|snapshot| snapshot.repo.as_str().to_owned())
        .filter(|repo| configured_repositories.contains(repo.as_str()))
        .collect::<BTreeSet<_>>();
    if acquired_repositories.len() < MIN_ACQUIRED_REPOSITORIES_TO_WRITE {
        let details = if faults.is_empty() {
            "acquisition returned no configured repository snapshots".to_owned()
        } else {
            faults.join("; ")
        };
        return Err(SweepError::AcquisitionRefused {
            acquired: acquired_repositories.len(),
            configured: configured_repositories.len(),
            minimum: MIN_ACQUIRED_REPOSITORIES_TO_WRITE,
            details,
        });
    }
    let unacquired_repositories = configured_repositories
        .difference(&acquired_repositories)
        .cloned()
        .collect::<BTreeSet<_>>();
    let (work_orders, work_order_warnings) = load_work_orders(&options.paths)?;
    faults.extend(work_order_warnings);

    let mirror = snapshots.clone();
    let mut snapshots_by_repo = snapshots
        .into_iter()
        .map(|snapshot| (snapshot.repo.as_str().to_owned(), snapshot))
        .collect::<BTreeMap<_, _>>();
    let mut generated = Vec::new();
    let mut active_ids = BTreeSet::new();
    let mut current = BTreeMap::new();
    let mut verified_repositories = BTreeSet::new();
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
        verified_repositories.insert(repo.to_owned());
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
        let evidence = RepositoryEvidence {
            paths: &options.paths,
            work_orders: &work_orders,
            queued_kinds: &queued_kinds,
        };
        let analysis = analyze_repository(
            &config,
            project,
            &snapshot,
            &previous,
            mode,
            options.started_at,
            &evidence,
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
    let configured = configured_repositories;
    if let Some(repos) = new_state.get_mut("repos").and_then(Value::as_object_mut) {
        repos.retain(|repo, _| configured.contains(repo.as_str()));
    }

    let mut ranking_faults = Vec::new();
    for item in &config.work_ranking {
        let Some((repo, reference)) = item.rsplit_once('#') else {
            continue;
        };
        if !verified_repositories.contains(repo) {
            continue;
        }
        let exists = new_state
            .get("repos")
            .and_then(|repos| repos.get(repo))
            .and_then(|state| state.get("records"))
            .and_then(Value::as_object)
            .is_some_and(|records| records.contains_key(item));
        if exists {
            continue;
        }
        let reason = format!("work_ranking item no longer exists: {item}");
        let opened = existing
            .iter()
            .find(|row| {
                string_field(row.value(), &["id"]) == item
                    && string_field(row.value(), &["kind"]) == "drift"
                    && row
                        .value()
                        .get("mandate")
                        .and_then(|mandate| mandate.get("reason"))
                        .and_then(Value::as_str)
                        == Some(reason.as_str())
            })
            .map_or_else(
                || format_time(options.started_at),
                |row| string_field(row.value(), &["opened"]).to_owned(),
            );
        generated.retain(|row| string_field(row, &["id"]) != item);
        generated.push(json!({
            "id": item,
            "repo": repo,
            "ref": format!("#{reference}"),
            "title": "Ranking fault: recorded item no longer exists",
            "kind": "drift",
            "mandate": {"reason": reason},
            "state": "pending",
            "opened": opened,
            "age_days": 0,
            "aged_out": false,
            "needs_judgment": false,
            "blocked_by": [],
        }));
        ranking_faults.push(item.clone());
        faults.push(format!(
            "recorded work_ranking item no longer exists: {item}"
        ));
    }
    new_state["work_ranking"] = json!(&config.work_ranking);
    new_state["work_ranking_faults"] = json!(ranking_faults);

    let final_rows = reconcile_queue(
        existing,
        generated,
        &active_ids,
        &current,
        &configured,
        &unacquired_repositories,
    )?;
    let before = read_queue(&options.paths.queue_file())?;
    let queue_changes = symmetric_queue_changes(&before, &final_rows);
    let graph = graph_from_state(&new_state, &final_rows, &configured);
    for fault in &graph.faults {
        faults.push(format!("{}: {}", fault.name, fault.nodes.join(", ")));
    }
    new_state["dependency_graph"] =
        serde_json::to_value(graph).expect("work dependency graph serializes");
    backup_previous_sweep(&options.paths)?;
    write_queue(&options.paths.queue_file(), &final_rows)?;
    write_json_private(&state_path, &new_state)?;

    if let PublishTarget::Repository(repository) = &options.publish {
        if let Err(error) = publish(options, repository) {
            faults.push(format!(
                "publish failed; local records remain authoritative: {error}"
            ));
        }
    }

    Ok((
        SweepOutcome {
            project_count: config.projects.len(),
            queue_changes,
            mode,
            faults,
        },
        mirror,
    ))
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

/// The credential request for one organization's acquisition worker.
struct OrganizationScope {
    /// Any roster repository under the organization. The App lookup resolves
    /// the installation from it and nothing else, so which one is immaterial.
    anchor: String,
    /// Every roster repository under the organization. The inner worker reads
    /// all of them, so all of them must be in the grant.
    repositories: Vec<String>,
}

/// Group the roster by organization, one credential request per group.
fn organization_scopes(config: &MandateConfig) -> BTreeMap<String, OrganizationScope> {
    let mut organizations = BTreeMap::<String, OrganizationScope>::new();
    for project in &config.projects {
        let repo = project.repo.as_str().to_owned();
        organizations
            .entry(owner(project.repo.as_str()).to_owned())
            .or_insert_with(|| OrganizationScope {
                anchor: repo.clone(),
                repositories: Vec::new(),
            })
            .repositories
            .push(repo);
    }
    for scope in organizations.values_mut() {
        scope.repositories.sort();
        scope.repositories.dedup();
    }
    organizations
}

/// The credential request for one organization's acquisition worker.
///
/// Scope is mandatory on both halves. The minter rejects a request naming
/// neither repositories nor permissions rather than falling back to the
/// installation's full grant, so omitting either leaves the sweep unable to
/// authenticate at all — which is what happened while this call was unscoped.
fn organization_token_request<'a>(
    scope: &'a OrganizationScope,
    repositories: &'a str,
) -> AppTokenRequest<'a> {
    AppTokenRequest {
        role: "gatekeeper",
        anchor_repository: &scope.anchor,
        repositories: Some(repositories),
        permissions: Some(SWEEP_TOKEN_PERMISSIONS),
    }
}

fn acquire_by_organization(
    options: &SweepOptions,
    config: &MandateConfig,
    _old_state: &Value,
    mode: SweepMode,
    minter: &mut dyn InstallationTokenMinter,
) -> Result<(Vec<RepositorySnapshot>, Vec<String>), SweepError> {
    let mut snapshots = Vec::new();
    let mut faults = Vec::new();
    for (org, scope) in organization_scopes(config) {
        let repositories = scope.repositories.join(",");
        let request = organization_token_request(&scope, &repositories);
        let token = match minter.mint(&options.paths, request) {
            Ok(token) => token,
            Err(error) => {
                faults.push(format!(
                    "authentication or GitHub query failed for organization {org}: {error}"
                ));
                continue;
            }
        };
        let output = Command::new(&options.executable)
            .args([
                "sweep",
                "--inner-org",
                &org,
                "--started-at",
                &format_time(options.started_at),
                "--mode",
                mode_name(mode),
            ])
            .env("GH_TOKEN", &token.0)
            .env("GITHUB_TOKEN", &token.0)
            .current_dir(&options.working_directory)
            .output()
            .map_err(|error| SweepError::Acquisition(error.to_string()))?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            if output.status.code() == Some(6) {
                return Err(SweepError::BranchListingTruncated(detail));
            }
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
    enrich_issue_relationships(repo_name, &mut issues)?;

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
    let (default_branch, branches, branch_read_degraded, ci_runs) = match gh_json(&[
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
            let (branches, branch_read_degraded) = if branch.is_some() {
                (fetch_branches(repo_name)?, false)
            } else {
                warnings.push(
                    "default-branch lookup returned no branch; skipping pushed-branch checks this sweep"
                        .to_owned(),
                );
                (Vec::new(), true)
            };
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
                    // Deliberately not exhaustive_array. A change feed that
                    // hits the limit may have silently dropped items, so it
                    // must refuse. This query is different in kind: `gh run
                    // list` returns newest-first and the result is judged
                    // only against the LATEST run per workflow on this ref,
                    // so anything past the limit is older than something we
                    // already hold and cannot change a verdict. Refusing here
                    // takes the whole sweep down on any repository with 200+
                    // default-branch runs — crawlab-pro and duhem both do.
                    Ok(value) => value.as_array().cloned().unwrap_or_default(),
                    Err(error) => {
                        warnings.push(format!("default-branch CI query failed: {error}"));
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            };
            (branch, branches, branch_read_degraded, runs)
        }
        Err(error) => {
            warnings.push(format!(
                "default-branch lookup failed: {error}; skipping pushed-branch checks this sweep"
            ));
            (None, Vec::new(), true, Vec::new())
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
        branches,
        branch_read_degraded,
        ci_runs,
        warnings,
    })
}

fn fetch_branches(repo: &str) -> Result<Vec<Value>, SweepError> {
    let mut branches = Vec::new();
    for page in 1..=2 {
        let endpoint = format!("repos/{repo}/branches?per_page=100&page={page}");
        let value = gh_json(&["api", "-X", "GET", &endpoint])?;
        let page_values = value.as_array().cloned().ok_or_else(|| {
            SweepError::Acquisition(format!("branch query for {repo} returned a non-array body"))
        })?;
        let count = page_values.len();
        branches.extend(page_values);
        if count < 100 {
            return Ok(branches);
        }
    }
    Err(SweepError::BranchListingTruncated(format!(
        "branch query for {repo} reached query_limit {QUERY_LIMIT}; refusing a truncated sweep"
    )))
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
    evidence: &RepositoryEvidence<'_>,
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
        evidence.paths,
        started_at,
        config.stuck_after_days,
        evidence.queued_kinds,
    );
    generated.extend(gate.generated);
    active_ids.extend(gate.active_ids);
    current.extend(gate.current);

    let branch_writes = analyze_branch_writes(
        repo,
        snapshot,
        previous,
        evidence.work_orders,
        evidence.queued_kinds,
        started_at,
    );
    generated.extend(branch_writes.generated);
    active_ids.extend(branch_writes.active_ids);
    current.extend(branch_writes.current);

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
        "unexplained_branch_writes": branch_writes.extra_state["writes"],
        "unexplained_write_count": gate.extra_state["anomaly_count"].as_u64().unwrap_or_default()
            + branch_writes.extra_state["count"].as_u64().unwrap_or_default(),
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
    let parent = issue_reference(
        repo,
        source.get("parent").or_else(|| source.get("parentIssue")),
    );
    let mut children = source
        .get("subIssues")
        .and_then(|value| value.get("nodes").or(Some(value)))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|child| issue_reference(repo, Some(child)))
        .collect::<Vec<_>>();
    children.sort();
    children.dedup();
    let mut closes = if item_type == "pr" {
        linked
            .iter()
            .filter_map(|issue| issue_reference(repo, Some(issue)))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    closes.sort();
    closes.dedup();
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
        parent,
        children,
        closes,
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
    queued_kinds: &BTreeMap<String, String>,
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
        // Deduplicate and order by issue *number*, not by the rendered string.
        // These references reach the fingerprint through a join, so ordering
        // them lexicographically would place #10 before #9 and re-emit the
        // fault on a sweep where nothing about the merge changed.
        let mut work_order_numbers = linked_issues(pull)
            .iter()
            .filter_map(|issue| number_field(issue, &["number"]))
            .filter(|issue| *issue > 0)
            .collect::<Vec<_>>();
        work_order_numbers.sort_unstable();
        work_order_numbers.dedup();
        let work_order_refs = work_order_numbers
            .into_iter()
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
    let mut ordinary_fault_count = 0_u64;
    let mut anomaly_count = 0_u64;
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
        } else {
            basis.push("human_authorship");
        }
        if work_order_refs
            .as_array()
            .is_some_and(|refs| !refs.is_empty())
        {
            basis.push("work_order");
        }
        if !records.is_empty() {
            basis.push("gate_verdict");
        }
        // A cited order explains requested loop work even when gate execution
        // produced no verdict; the surviving fault is still actionable.
        let classification = if !machine_authored
            || work_order_refs
                .as_array()
                .is_some_and(|refs| !refs.is_empty())
        {
            "explained"
        } else {
            "unexplained"
        };
        let kind = if classification == "unexplained" {
            "unexplained-write"
        } else {
            "merge-gate-fault"
        };
        let scope_evidence = json!({
            "basis": basis,
            "machine_author": merge["machine_author"],
            "work_order_refs": work_order_refs,
            "gate_verdict": if records.is_empty() {
                Value::Null
            } else {
                json!({
                    "verdict": verdict,
                    "ts": if gate_ts.is_empty() { Value::Null } else { json!(gate_ts) },
                })
            },
            "classification": classification,
        });
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
            || queued_kinds.get(&id).map(String::as_str) != Some(kind)
        {
            let reason = if kind == "unexplained-write" {
                format!(
                    "unexplained write: machine-authored merge has no matching work order; {reason}"
                )
            } else {
                reason
            };
            generated.push(json!({
                "id": id,
                "repo": repo,
                "ref": format!("#{merge_number}"),
                "title": merge["title"],
                "kind": kind,
                "mandate": {
                    "reason": reason,
                    "scope_evidence": scope_evidence,
                },
                "state": "pending",
                "opened": merged_at,
                "age_days": age_days,
                "aged_out": age_days >= stuck_after_days,
                "needs_judgment": false,
                "blocked_by": [],
            }));
        }
        if classification == "unexplained" {
            anomaly_count += 1;
        } else {
            ordinary_fault_count += 1;
        }
        faults.insert(
            id,
            json!({
                "shape": shape,
                "head_sha": sha,
                "verdict": verdict,
                "gate_ts": if gate_ts.is_empty() { Value::Null } else { json!(gate_ts) },
                "scope_evidence": scope_evidence,
                "fingerprint": fingerprint,
            }),
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
            "fault_count": ordinary_fault_count,
            "anomaly_count": anomaly_count,
        }),
    }
}

fn analyze_branch_writes(
    repo: &str,
    snapshot: &RepositorySnapshot,
    previous: &Value,
    work_orders: &[WorkOrderEvidence],
    queued_kinds: &BTreeMap<String, String>,
    started_at: DateTime<Utc>,
) -> SupplementalAnalysis {
    let mut generated = Vec::new();
    let mut active_ids = BTreeSet::new();
    let mut current = BTreeMap::new();
    let mut writes = Map::new();
    if snapshot.branch_read_degraded || snapshot.default_branch.is_none() {
        return SupplementalAnalysis {
            generated,
            active_ids,
            current,
            extra_state: json!({"writes": writes, "count": 0}),
        };
    }

    let default_branch = snapshot.default_branch.as_deref().unwrap_or_default();
    let old_writes = previous
        .get("unexplained_branch_writes")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for branch in &snapshot.branches {
        let Some(branch_name) = branch.get("name").and_then(Value::as_str) else {
            continue;
        };
        if branch_name == default_branch || !branch_name.starts_with("ostrom/") {
            continue;
        }
        let matching_work_orders = work_orders
            .iter()
            .filter(|order| order.repository == repo && order.branch_name == branch_name)
            .map(|order| {
                json!({
                    "item_id": order.item_id,
                    "order_id": order.order_id,
                    "branch_name": order.branch_name,
                })
            })
            .collect::<Vec<_>>();
        if !matching_work_orders.is_empty() {
            continue;
        }
        let branch_sha = branch
            .pointer("/commit/sha")
            .and_then(Value::as_str)
            .unwrap_or("");
        let id = format!("{repo}@refs/heads/{branch_name}");
        let title = format!("Pushed branch {branch_name}");
        let fingerprint = format!("branch-v1|{branch_name}|{branch_sha}");
        let scope_evidence = json!({
            "basis": [],
            "machine_author": null,
            "work_order_refs": [],
            "gate_verdict": null,
            "classification": "unexplained",
            "branch_name": branch_name,
            "branch_sha": branch_sha,
            "matching_work_orders": [],
        });
        active_ids.insert(id.clone());
        current.insert(
            id.clone(),
            json!({"id": id, "title": title, "age_days": 0, "aged_out": false}),
        );
        if old_writes
            .get(&id)
            .and_then(|write| write.get("fingerprint"))
            .and_then(Value::as_str)
            != Some(fingerprint.as_str())
            || queued_kinds.get(&id).map(String::as_str) != Some("unexplained-write")
        {
            generated.push(json!({
                "id": id,
                "repo": repo,
                "ref": format!("@{branch_name}"),
                "title": title,
                "kind": "unexplained-write",
                "mandate": {
                    "reason": format!(
                        "unexplained write: pushed branch {branch_name} has no matching work order"
                    ),
                    "scope_evidence": scope_evidence,
                },
                "state": "pending",
                "opened": format_time(started_at),
                "age_days": 0,
                "aged_out": false,
                "needs_judgment": false,
                "blocked_by": [],
            }));
        }
        writes.insert(
            id,
            json!({
                "branch_name": branch_name,
                "branch_sha": branch_sha,
                "scope_evidence": scope_evidence,
                "fingerprint": fingerprint,
            }),
        );
    }
    let count = writes.len();
    SupplementalAnalysis {
        generated,
        active_ids,
        current,
        extra_state: json!({"writes": writes, "count": count}),
    }
}

fn reconcile_queue(
    existing: Vec<QueueDocument>,
    generated: Vec<Value>,
    active_ids: &BTreeSet<String>,
    current: &BTreeMap<String, Value>,
    configured: &BTreeSet<String>,
    unacquired_repositories: &BTreeSet<String>,
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
                || unacquired_repositories.contains(repo)
                || (!configured.contains(repo) && string_field(row, &["kind"]) == "drift")
        })
        .cloned()
        .collect::<Vec<_>>();
    for mut row in generated {
        let id = string_field(&row, &["id"]).to_owned();
        let ranking_fault = string_field(&row, &["kind"]) == "drift"
            && row
                .get("mandate")
                .and_then(|mandate| mandate.get("reason"))
                .and_then(Value::as_str)
                .is_some_and(|reason| reason.starts_with("work_ranking item no longer exists: "));
        if !ranking_fault {
            if let Some(old) = existing_values
                .iter()
                .find(|candidate| string_field(candidate, &["id"]) == id)
            {
                if let Some(state) = old.get("state") {
                    row["state"] = state.clone();
                }
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

fn load_work_orders(
    paths: &OstromPaths,
) -> Result<(Vec<WorkOrderEvidence>, Vec<String>), SweepError> {
    let directory = paths.state.join("work-orders");
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), Vec::new()));
        }
        Err(error) => return Err(SweepError::WorkOrders(error.to_string())),
    };
    let mut files = entries
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|error| SweepError::WorkOrders(error.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    files.sort();

    let mut orders = Vec::new();
    let mut warnings = Vec::new();
    for path in files {
        if !path.is_file() || path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let order = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<WorkOrderEvidence>(&bytes).ok());
        if let Some(order) = order {
            orders.push(order);
        } else {
            warnings
                .push("ignoring malformed work order while classifying pushed branches".to_owned());
        }
    }
    Ok((orders, warnings))
}

pub fn load_config(paths: &OstromPaths, cwd: &Path) -> Result<MandateConfig, SweepError> {
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

pub fn load_config_or_defaults(
    paths: &OstromPaths,
    cwd: &Path,
) -> Result<MandateConfig, SweepError> {
    let user_path = paths.config.join("mandates.yaml");
    let repo_path = cwd.join(".ostrom/mandates.yaml");
    if user_path.exists() || repo_path.exists() {
        load_config(paths, cwd)
    } else {
        MandateConfig::from_yaml(SHIPPED_DEFAULTS)
            .map_err(|error| SweepError::Config(error.to_string()))
    }
}

pub(crate) fn merge_yaml(base: &mut serde_yaml::Value, overlay: serde_yaml::Value) {
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
            ("branch query", snapshot.branches.len()),
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

fn backup_previous_sweep(paths: &OstromPaths) -> Result<(), SweepError> {
    let previous = paths.previous_sweep_dir();
    for (source, name) in [
        (paths.queue_file(), "queue.jsonl"),
        (paths.sweep_state_file(), "state.json"),
    ] {
        if !source.exists() {
            continue;
        }
        fs::create_dir_all(&previous)
            .map_err(|error| io_error("create previous sweep directory", &previous, error))?;
        let destination = previous.join(name);
        let temporary = destination.with_extension(format!("tmp.{}", std::process::id()));
        fs::copy(&source, &temporary)
            .map_err(|error| io_error("copy previous sweep file", &temporary, error))?;
        set_private_file_mode(&temporary)?;
        fs::rename(&temporary, &destination)
            .map_err(|error| io_error("install previous sweep file", &destination, error))?;
    }
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

fn issue_reference(default_repo: &str, issue: Option<&Value>) -> Option<String> {
    let issue = issue?;
    let number = number_field(issue, &["number"])?;
    let repo = issue
        .get("repository")
        .and_then(|repository| nonempty_string(repository, &["nameWithOwner", "name_with_owner"]))
        .unwrap_or(default_repo);
    Some(format!("{repo}#{number}"))
}

fn enrich_issue_relationships(repo: &str, issues: &mut [Value]) -> Result<(), SweepError> {
    let Some((owner, name)) = repo.split_once('/') else {
        return Ok(());
    };
    let query = r#"query OstromDependencyGraph($owner:String!,$name:String!,$cursor:String){repository(owner:$owner,name:$name){issues(first:100,after:$cursor,states:OPEN){nodes{number parent{number repository{nameWithOwner}} subIssues(first:100){nodes{number state repository{nameWithOwner}} pageInfo{hasNextPage}}} pageInfo{hasNextPage endCursor}}}}"#;
    let query_field = format!("query={query}");
    let owner_field = format!("owner={owner}");
    let name_field = format!("name={name}");
    let first = gh_json(&[
        "api",
        "graphql",
        "-f",
        &query_field,
        "-F",
        &owner_field,
        "-F",
        &name_field,
    ])?;
    let first_connection = dependency_connection(repo, &first)?;
    reject_truncated_children(repo, first_connection)?;
    let mut relationships = first_connection["nodes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if first_connection["pageInfo"]["hasNextPage"].as_bool() == Some(true) {
        let cursor = first_connection["pageInfo"]["endCursor"]
            .as_str()
            .filter(|cursor| !cursor.is_empty())
            .ok_or_else(|| {
                SweepError::Acquisition(format!(
                    "sub-issue query for {repo} returned no second-page cursor"
                ))
            })?;
        let cursor_field = format!("cursor={cursor}");
        let second = gh_json(&[
            "api",
            "graphql",
            "-f",
            &query_field,
            "-F",
            &owner_field,
            "-F",
            &name_field,
            "-F",
            &cursor_field,
        ])?;
        let second_connection = dependency_connection(repo, &second)?;
        reject_truncated_children(repo, second_connection)?;
        if second_connection["pageInfo"]["hasNextPage"].as_bool() == Some(true) {
            return Err(SweepError::Acquisition(format!(
                "sub-issue query for {repo} reached query_limit 200; refusing a truncated dependency graph"
            )));
        }
        relationships.extend(
            second_connection["nodes"]
                .as_array()
                .cloned()
                .unwrap_or_default(),
        );
    }
    for issue in issues {
        let Some(number) = number_field(issue, &["number"]) else {
            continue;
        };
        let Some(relationship) = relationships
            .iter()
            .find(|relationship| number_field(relationship, &["number"]) == Some(number))
        else {
            continue;
        };
        if let Some(object) = issue.as_object_mut() {
            if let Some(parent) = relationship.get("parent") {
                object.insert("parent".to_owned(), parent.clone());
            }
            if let Some(children) = relationship.get("subIssues") {
                object.insert("subIssues".to_owned(), children.clone());
            }
        }
    }
    Ok(())
}

fn dependency_connection<'a>(repo: &str, response: &'a Value) -> Result<&'a Value, SweepError> {
    response.pointer("/data/repository/issues").ok_or_else(|| {
        SweepError::Acquisition(format!(
            "sub-issue query for {repo} returned no issue connection"
        ))
    })
}

fn reject_truncated_children(repo: &str, connection: &Value) -> Result<(), SweepError> {
    if connection
        .get("nodes")
        .and_then(Value::as_array)
        .is_none_or(|nodes| {
            nodes.iter().any(|item| {
                item.pointer("/subIssues/pageInfo/hasNextPage")
                    .and_then(Value::as_bool)
                    != Some(false)
            })
        })
    {
        return Err(SweepError::Acquisition(format!(
            "sub-issue query for {repo} was malformed or a parent reached 100 children; refusing a truncated dependency graph"
        )));
    }
    Ok(())
}

fn graph_from_state(
    state: &Value,
    queue: &[QueueDocument],
    configured: &BTreeSet<String>,
) -> ostrom_core::WorkGraph {
    let mut inputs = state
        .get("repos")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|repos| repos.values())
        .filter_map(|repo| repo.get("records").and_then(Value::as_object))
        .flat_map(|records| records.values())
        .filter_map(|record| serde_json::from_value::<NormalizedItem>(record.clone()).ok())
        .map(|item| WorkNodeInput {
            id: item.id,
            open: true,
            body_dependencies: item.blocked_by,
            parent: item.parent,
            children: item.children,
            closes: item.closes,
        })
        .collect::<Vec<_>>();
    for document in queue {
        let row = document.value();
        let Some(id) = row["id"].as_str() else {
            continue;
        };
        let dependencies = row["blocked_by"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if let Some(input) = inputs.iter_mut().find(|input| input.id == id) {
            input.body_dependencies.extend(dependencies);
            input.body_dependencies.sort();
            input.body_dependencies.dedup();
            continue;
        }
        inputs.push(WorkNodeInput {
            id: id.to_owned(),
            open: true,
            body_dependencies: dependencies,
            parent: None,
            children: Vec::new(),
            closes: Vec::new(),
        });
    }
    build_work_graph(&inputs, configured)
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

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::{env, os::unix::fs::PermissionsExt, process::Output};

    use tempfile::tempdir;

    use super::*;

    fn roster(repositories: &[&str]) -> MandateConfig {
        let projects = repositories
            .iter()
            .map(|repo| format!("  - repo: {repo}\n"))
            .collect::<String>();
        MandateConfig::from_yaml(&format!(
            "cadence_hours: 1\nstuck_after_days: 7\nprojects:\n{projects}"
        ))
        .expect("the fixture roster is valid")
    }

    #[test]
    fn every_roster_repository_under_an_organization_enters_its_scope() {
        let scopes = roster(&["placeholder-org/alpha", "placeholder-org/beta"]);
        let scopes = organization_scopes(&scopes);
        let scope = &scopes["placeholder-org"];
        assert_eq!(
            scope.repositories,
            vec!["placeholder-org/alpha", "placeholder-org/beta"]
        );
    }

    #[test]
    fn each_organization_is_scoped_to_its_own_repositories_only() {
        let scopes = organization_scopes(&roster(&[
            "placeholder-org/alpha",
            "other-placeholder-org/gamma",
        ]));
        assert_eq!(
            scopes["placeholder-org"].repositories,
            vec!["placeholder-org/alpha"]
        );
        assert_eq!(
            scopes["other-placeholder-org"].repositories,
            vec!["other-placeholder-org/gamma"]
        );
    }

    #[test]
    fn the_credential_request_names_both_halves_of_the_scope() {
        // gh-as.sh refuses an unscoped request rather than falling back to the
        // installation's full grant, so dropping either half leaves the sweep
        // unable to authenticate at all.
        let scopes =
            organization_scopes(&roster(&["placeholder-org/alpha", "placeholder-org/beta"]));
        let repositories = scopes["placeholder-org"].repositories.join(",");
        let request = organization_token_request(&scopes["placeholder-org"], &repositories);
        assert_eq!(request.role, "gatekeeper");
        assert_eq!(request.anchor_repository, "placeholder-org/alpha");
        assert_eq!(request.repositories, Some(repositories.as_str()));
        assert_eq!(request.permissions, Some(SWEEP_TOKEN_PERMISSIONS));
    }

    #[test]
    fn the_acquisition_grant_requests_no_write_permission() {
        assert!(
            SWEEP_TOKEN_PERMISSIONS
                .split(',')
                .all(|permission| permission.ends_with(":read")),
            "a sweep reads; a write permission in the grant is a defect"
        );
    }

    #[test]
    fn local_work_order_snapshot_tolerates_absence_and_malformed_files() {
        let home = tempdir().expect("temporary Ostrom paths");
        let paths = OstromPaths {
            config: home.path().to_path_buf(),
            state: home.path().to_path_buf(),
        };
        let (orders, warnings) = load_work_orders(&paths).expect("absent directory is empty");
        assert!(orders.is_empty());
        assert!(warnings.is_empty());

        let directory = home.path().join("work-orders");
        fs::create_dir(&directory).expect("create work-order directory");
        fs::write(directory.join("malformed.json"), "not JSON")
            .expect("write malformed work order");
        fs::write(
            directory.join("valid.json"),
            r##"{"repository":"placeholder-org/alpha","branch_name":"ostrom/item","item_id":"placeholder-org/alpha#1","order_id":"placeholder-order"}"##,
        )
        .expect("write valid work order");
        let (orders, warnings) = load_work_orders(&paths).expect("read work-order snapshot");
        assert_eq!(orders.len(), 1);
        assert_eq!(warnings.len(), 1);
    }

    #[cfg(unix)]
    const ACQUISITION_GUARD_CHILD: &str = "OSTROM_TEST_ACQUISITION_GUARD_CHILD";

    #[cfg(unix)]
    struct FixtureMinter {
        successful_anchors: BTreeSet<String>,
    }

    #[cfg(unix)]
    impl InstallationTokenMinter for FixtureMinter {
        fn mint(
            &mut self,
            _paths: &OstromPaths,
            request: AppTokenRequest<'_>,
        ) -> Result<SweepToken, AppTokenError> {
            if self.successful_anchors.contains(request.anchor_repository) {
                Ok(SweepToken("placeholder-installation-token".to_owned()))
            } else {
                Err(AppTokenError::LookupHttp(403))
            }
        }
    }

    #[cfg(unix)]
    fn acquisition_guard_options() -> SweepOptions {
        let paths = OstromPaths::resolve().expect("resolve scratch OSTROM_HOME");
        SweepOptions {
            working_directory: paths.state.clone(),
            executable: paths.state.join("fixture-github-worker.sh"),
            plugin_root: paths.state.clone(),
            paths,
            started_at: "2026-08-18T15:04:00Z"
                .parse()
                .expect("valid fixture timestamp"),
            requested_mode: SweepMode::Full,
            fixture: None,
            publish: PublishTarget::Disabled,
        }
    }

    #[cfg(unix)]
    fn run_acquisition_guard_child(home: &Path, test_name: &str, scenario: &str) -> Output {
        Command::new(env::current_exe().expect("current test executable"))
            .args(["--exact", test_name, "--nocapture"])
            .env("OSTROM_HOME", home)
            .env(ACQUISITION_GUARD_CHILD, scenario)
            .current_dir(home)
            .output()
            .expect("run isolated acquisition fixture")
    }

    #[cfg(unix)]
    fn write_acquisition_guard_roster(home: &Path) {
        fs::write(
            home.join("mandates.yaml"),
            concat!(
                "cadence_hours: 1\n",
                "stuck_after_days: 7\n",
                "projects:\n",
                "  - repo: placeholder-org/alpha\n",
                "  - repo: other-placeholder-org/beta\n",
            ),
        )
        .expect("write placeholder roster");
    }

    #[cfg(unix)]
    fn write_fixture_github_worker(home: &Path) {
        let worker = home.join("fixture-github-worker.sh");
        fs::write(
            &worker,
            r#"#!/usr/bin/env bash
set -eu
org=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--inner-org" ]; then
    org="$2"
    break
  fi
  shift
done
case "$org" in
  placeholder-org) repo=placeholder-org/alpha ;;
  other-placeholder-org) repo=other-placeholder-org/beta ;;
  *) exit 9 ;;
esac
printf '{"repositories":[{"repo":"%s","issues":[],"open_prs":[],"merged_prs":[],"default_branch":"main","branches":[],"branch_read_degraded":false,"ci_runs":[]}]}\n' "$repo"
"#,
        )
        .expect("write fixture GitHub worker");
        fs::set_permissions(&worker, fs::Permissions::from_mode(0o700))
            .expect("make fixture GitHub worker executable");
    }

    #[cfg(unix)]
    fn prior_queue_bytes() -> Vec<u8> {
        concat!(
            r##"{"id":"placeholder-org/alpha#1","repo":"placeholder-org/alpha","ref":"#1","title":"Placeholder alpha decision","kind":"decision","mandate":{"reason":"placeholder"},"state":"deferred","opened":"2026-08-01T00:00:00Z","age_days":17,"aged_out":true,"needs_judgment":true,"blocked_by":[]}"##,
            "\n",
            r##"{"id":"other-placeholder-org/beta#2","repo":"other-placeholder-org/beta","ref":"#2","title":"Placeholder beta decision","kind":"decision","mandate":{"reason":"placeholder"},"state":"pending","opened":"2026-08-02T00:00:00Z","age_days":16,"aged_out":true,"needs_judgment":true,"blocked_by":[]}"##,
            "\n",
        )
        .as_bytes()
        .to_vec()
    }

    #[cfg(unix)]
    fn prior_state_bytes() -> Vec<u8> {
        br#"{"version":2,"sweep_mode":"full","repos":{"placeholder-org/alpha":{"cursor":"2026-08-01T00:00:00Z","records":{}},"other-placeholder-org/beta":{"cursor":"2026-08-02T00:00:00Z","records":{}}}}"#
            .to_vec()
    }

    #[cfg(unix)]
    fn write_prior_generation(home: &Path) -> (Vec<u8>, Vec<u8>) {
        let queue = prior_queue_bytes();
        let state = prior_state_bytes();
        fs::write(home.join("queue.jsonl"), &queue).expect("write prior queue");
        fs::write(home.join("state.json"), &state).expect("write prior state");
        (queue, state)
    }

    #[cfg(unix)]
    #[test]
    fn every_repository_failure_exits_nonzero_and_preserves_durable_bytes() {
        if env::var(ACQUISITION_GUARD_CHILD).as_deref() == Ok("total-failure") {
            let mut minter = FixtureMinter {
                successful_anchors: BTreeSet::new(),
            };
            let error = run_sweep_with_minter(&acquisition_guard_options(), &mut minter)
                .expect_err("zero acquired repositories must refuse");
            assert!(matches!(
                &error,
                SweepError::AcquisitionRefused {
                    acquired: 0,
                    configured: 2,
                    minimum: MIN_ACQUIRED_REPOSITORIES_TO_WRITE,
                    ..
                }
            ));
            eprintln!("{error}");
            std::process::exit(1);
        }

        let home = tempdir().expect("temporary OSTROM_HOME");
        write_acquisition_guard_roster(home.path());
        write_fixture_github_worker(home.path());
        let (queue_before, state_before) = write_prior_generation(home.path());
        let output = run_acquisition_guard_child(
            home.path(),
            "sweep::tests::every_repository_failure_exits_nonzero_and_preserves_durable_bytes",
            "total-failure",
        );

        assert!(
            !output.status.success(),
            "total failure unexpectedly succeeded"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("refusing to overwrite queue and state"));
        assert!(stderr.contains("acquisition succeeded for 0 of 2"));
        assert_eq!(
            fs::read(home.path().join("queue.jsonl")).expect("read preserved queue"),
            queue_before
        );
        assert_eq!(
            fs::read(home.path().join("state.json")).expect("read preserved state"),
            state_before
        );
        assert!(!home.path().join("previous").exists());
    }

    #[cfg(unix)]
    #[test]
    fn one_acquired_repository_crosses_threshold_and_merges_failed_rows() {
        if env::var(ACQUISITION_GUARD_CHILD).as_deref() == Ok("partial-success") {
            let mut minter = FixtureMinter {
                successful_anchors: BTreeSet::from(["placeholder-org/alpha".to_owned()]),
            };
            let outcome = run_sweep_with_minter(&acquisition_guard_options(), &mut minter)
                .expect("one acquired repository crosses the write threshold")
                .0;
            assert_eq!(outcome.project_count, 2);
            assert!(outcome.faults.iter().any(|fault| {
                fault.contains(
                    "repository acquisition produced no result for other-placeholder-org/beta",
                )
            }));
            return;
        }

        let home = tempdir().expect("temporary OSTROM_HOME");
        write_acquisition_guard_roster(home.path());
        write_fixture_github_worker(home.path());
        let (queue_before, state_before) = write_prior_generation(home.path());
        let output = run_acquisition_guard_child(
            home.path(),
            "sweep::tests::one_acquired_repository_crosses_threshold_and_merges_failed_rows",
            "partial-success",
        );
        assert!(
            output.status.success(),
            "partial sweep stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let queue = fs::read_to_string(home.path().join("queue.jsonl")).expect("read merged queue");
        assert!(!queue.contains("placeholder-org/alpha#1"));
        assert!(queue.contains("other-placeholder-org/beta#2"));
        assert!(queue.contains("other-placeholder-org/beta#0"));
        let state: Value = serde_json::from_slice(
            &fs::read(home.path().join("state.json")).expect("read merged state"),
        )
        .expect("parse merged state");
        assert_eq!(
            state["repos"]["other-placeholder-org/beta"]["cursor"],
            "2026-08-02T00:00:00Z"
        );
        assert_eq!(
            fs::read(home.path().join("previous/queue.jsonl")).expect("read previous queue"),
            queue_before
        );
        assert_eq!(
            fs::read(home.path().join("previous/state.json")).expect("read previous state"),
            state_before
        );
    }

    #[cfg(unix)]
    #[test]
    fn fully_successful_sweep_writes_normally_and_preserves_previous_generation() {
        if env::var(ACQUISITION_GUARD_CHILD).as_deref() == Ok("full-success") {
            let mut minter = FixtureMinter {
                successful_anchors: BTreeSet::from([
                    "placeholder-org/alpha".to_owned(),
                    "other-placeholder-org/beta".to_owned(),
                ]),
            };
            run_sweep_with_minter(&acquisition_guard_options(), &mut minter)
                .expect("fully acquired sweep succeeds");
            return;
        }

        let home = tempdir().expect("temporary OSTROM_HOME");
        write_acquisition_guard_roster(home.path());
        write_fixture_github_worker(home.path());
        let (queue_before, state_before) = write_prior_generation(home.path());
        let output = run_acquisition_guard_child(
            home.path(),
            "sweep::tests::fully_successful_sweep_writes_normally_and_preserves_previous_generation",
            "full-success",
        );
        assert!(
            output.status.success(),
            "successful sweep stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_ne!(
            fs::read(home.path().join("queue.jsonl")).expect("read new queue"),
            queue_before
        );
        assert_ne!(
            fs::read(home.path().join("state.json")).expect("read new state"),
            state_before
        );
        assert_eq!(
            fs::read(home.path().join("previous/queue.jsonl")).expect("read previous queue"),
            queue_before
        );
        assert_eq!(
            fs::read(home.path().join("previous/state.json")).expect("read previous state"),
            state_before
        );
    }
}
