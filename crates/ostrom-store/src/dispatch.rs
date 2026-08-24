use std::{
    collections::BTreeSet,
    env,
    ffi::{OsStr, OsString},
    fs,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output},
    thread,
    time::{Duration, Instant},
};

use ostrom_core::{
    BranchListing, BranchListingFault, BranchListingOutcome, MandateConfig, RemoteBranch,
    WorkOrder, resolve_exact_branch,
};
use serde_json::{Map, Value, json};

use crate::{
    Clock, LeaseRecord, OstromPaths, TraceAppend,
    app_token::{
        AuthenticatedCommandError, GitHubInstallationTokenMinter, InstallationTokenMinter,
        ScopedAppTokenRequest, authenticated_output,
    },
    append_trace, configured_retention_days, environment, load_config_or_defaults, read_lease,
    read_trace, sweep_worktrees,
    work_order::{implementer_lease_ttl, in_flight_orders, reap_stale_work_orders},
};

const DEFAULT_DAILY_CAP_USD: f64 = 50.0;
const DEFAULT_MAX_IMPLEMENTERS: usize = 2;
const DEFAULT_MAX_IMPLEMENTERS_PER_REPOSITORY: usize = 1;
const REMOTE_BRANCH_PAGE_SIZE: usize = 100;
const REMOTE_BRANCH_PAGE_LIMIT: usize = 100;
const IMPLEMENTER_STARTUP_GRACE_MILLISECONDS: u64 = 1_000;

#[derive(Debug, Clone)]
pub struct DispatchRequest {
    pub paths: OstromPaths,
    pub working_directory: PathBuf,
    pub plugin_root: PathBuf,
    pub order_file: PathBuf,
    pub clock: Clock,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchOutcome {
    Started(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchError {
    pub code: i32,
    pub message: String,
}

impl std::fmt::Display for DispatchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for DispatchError {}

impl DispatchError {
    fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone)]
struct ListingState {
    outcome: Option<BranchListingOutcome>,
    page_count: usize,
    branch_count: usize,
    matched_branch: Option<String>,
    error: Option<String>,
}

impl ListingState {
    fn empty() -> Self {
        Self {
            outcome: None,
            page_count: 0,
            branch_count: 0,
            matched_branch: None,
            error: None,
        }
    }

    fn from_listing(listing: &BranchListing) -> Self {
        Self {
            outcome: Some(listing.outcome),
            page_count: listing.page_count,
            branch_count: listing.branch_count,
            matched_branch: listing.matched.as_ref().map(|branch| branch.name.clone()),
            error: None,
        }
    }

    fn from_fault(fault: &BranchListingFault) -> Self {
        Self {
            outcome: Some(BranchListingOutcome::ListingDegraded),
            page_count: fault.page_count,
            branch_count: fault.branch_count,
            matched_branch: None,
            error: Some(fault.detail.chars().take(2000).collect()),
        }
    }
}

struct DispatchContext<'a> {
    request: &'a DispatchRequest,
    order: WorkOrder,
    item_hash: String,
    unit_name: String,
    backend: String,
    listing: ListingState,
    matched_key: Option<(&'static str, String)>,
}

pub fn run_dispatch(request: &DispatchRequest) -> Result<DispatchOutcome, DispatchError> {
    let mut minter = GitHubInstallationTokenMinter;
    run_dispatch_with_minter(request, &mut minter)
}

fn run_dispatch_with_minter(
    request: &DispatchRequest,
    minter: &mut dyn InstallationTokenMinter,
) -> Result<DispatchOutcome, DispatchError> {
    if !request.order_file.is_file() {
        return Err(DispatchError::new(
            2,
            format!(
                "ostrom work order: {} is not a file",
                request.order_file.display()
            ),
        ));
    }
    let order_bytes = fs::read(&request.order_file).map_err(|_| {
        DispatchError::new(
            2,
            format!(
                "ostrom work order: invalid schema_version 1 work order at {}",
                request.order_file.display()
            ),
        )
    })?;
    let order = WorkOrder::from_json(&order_bytes).map_err(|_| {
        DispatchError::new(
            2,
            format!(
                "ostrom work order: invalid schema_version 1 work order at {}",
                request.order_file.display()
            ),
        )
    })?;
    let item_hash = order.item_hash();
    let unit_name = format!("ostrom-implementer-{}", &item_hash[..16]);
    let mut context = DispatchContext {
        request,
        order,
        item_hash,
        unit_name,
        backend: environment::MANDATE_DISPATCH_BACKEND
            .value()
            .unwrap_or_else(|| "systemd".to_owned()),
        listing: ListingState::empty(),
        matched_key: None,
    };

    let retention_days = configured_retention_days()
        .map_err(|error| DispatchError::new(2, format!("ostrom dispatch: {error}")))?;
    let sweep = sweep_worktrees(&request.paths.state, &request.clock, retention_days)
        .map_err(|error| DispatchError::new(1, format!("ostrom dispatch: {error}")))?;
    for removal in sweep.removals {
        eprintln!("ostrom dispatch: {removal}");
    }

    preflight_worktree(&context)?;
    let config = load_config_or_defaults(&request.paths, &request.working_directory).ok();
    resolve_source_repository(&context, config.as_ref())?;

    let pages = match list_remote_branches(&context, minter) {
        Ok(pages) => pages,
        Err(fault) => {
            context.listing = ListingState::from_fault(&fault);
            let _ = append_failure(
                &context,
                "branch-listing-degraded",
                FailureDetail::default(),
            );
            return Err(DispatchError::new(
                1,
                format!(
                    "ostrom dispatch: could not verify remote branches for {} in {}: {}",
                    context.order.item_id, context.order.repository, fault.detail
                ),
            ));
        }
    };
    let listing = resolve_exact_branch(
        &pages,
        &context.order.branch_name,
        REMOTE_BRANCH_PAGE_SIZE,
        REMOTE_BRANCH_PAGE_LIMIT,
    )
    .map_err(|fault| {
        context.listing = ListingState::from_fault(&fault);
        let _ = append_failure(
            &context,
            "branch-listing-degraded",
            FailureDetail::default(),
        );
        DispatchError::new(
            1,
            format!(
                "ostrom dispatch: could not verify remote branches for {} in {}: {}",
                context.order.item_id, context.order.repository, fault.detail
            ),
        )
    })?;
    context.listing = ListingState::from_listing(&listing);
    if let Some(branch) = listing.matched.as_ref() {
        reject_unlanded_branch(&mut context, &pages, branch, minter)?;
    }
    reject_closing_pull_requests(&mut context, minter)?;

    let resolved_codex = resolve_codex(&context)?;
    let resolved_node = resolve_node(&context, &resolved_codex)?;
    let resolved_ostrom = resolve_ostrom(&context)?;
    let inherited_path = environment::PATH
        .value()
        .unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".to_owned());
    let node_dir = resolved_node.parent().unwrap_or_else(|| Path::new("."));
    let unit_path = format!("{}:{inherited_path}", node_dir.display());
    let executable = Command::new(&resolved_codex)
        .arg("--version")
        .env("PATH", &unit_path)
        .output()
        .is_ok_and(|output| output.status.success());
    if !executable {
        let _ = append_failure(&context, "codex-unavailable", FailureDetail::default());
        return Err(DispatchError::new(
            1,
            format!(
                "ostrom dispatch: Codex is unavailable: {} cannot execute with resolved Node {}",
                resolved_codex.display(),
                resolved_node.display()
            ),
        ));
    }

    // Reap before acquiring this item's lease. If an old order is genuinely
    // still live, its possibly expired lease must not be replaced merely to
    // discover the duplicate after the fact.
    reap_stale_work_orders(&request.paths.state, &request.clock)
        .map_err(|error| DispatchError::new(1, error.to_string()))?;
    if in_flight_orders(&request.paths.trace_file())
        .map_err(|error| DispatchError::new(1, error.to_string()))?
        .iter()
        .any(|order| order.item_id == context.order.item_id)
    {
        return Err(DispatchError::new(
            3,
            format!(
                "ostrom dispatch: an earlier work-dispatched row has no terminal row for {}",
                context.order.item_id
            ),
        ));
    }

    let derived_lease_ttl = implementer_lease_ttl(&context.order);
    let lease_ttl = match environment::MANDATE_IMPLEMENTER_LEASE_TTL_SECONDS.value() {
        Some(value) => value
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| {
                DispatchError::new(
                    2,
                    format!(
                        "ostrom dispatch: could not acquire implementer lease for {} (rc=2)",
                        context.order.item_id
                    ),
                )
            })?,
        None => derived_lease_ttl,
    };
    let lease_path = request
        .paths
        .state
        .join(format!("implementer-item-{}.lease", context.item_hash));
    let mut lease =
        acquire_dispatch_lease(&lease_path, &context.unit_name, lease_ttl, &request.clock)
            .map_err(|code| {
                let message = if code == 3 {
                    format!(
                        "ostrom dispatch: item already has a live implementer lease: {}",
                        context.order.item_id
                    )
                } else {
                    format!(
                        "ostrom dispatch: could not acquire implementer lease for {} (rc={code})",
                        context.order.item_id
                    )
                };
                DispatchError::new(code, message)
            })?;

    let result = after_lease(
        &context,
        config.as_ref(),
        &resolved_codex,
        &resolved_ostrom,
        &unit_path,
        &mut lease,
        minter,
    );
    if result.is_err() {
        lease.release();
    }
    result
}

fn after_lease(
    context: &DispatchContext<'_>,
    config: Option<&MandateConfig>,
    resolved_codex: &Path,
    resolved_ostrom: &Path,
    unit_path: &str,
    lease: &mut LeaseGuard,
    minter: &mut dyn InstallationTokenMinter,
) -> Result<DispatchOutcome, DispatchError> {
    let open_prs = gh_json(
        context,
        "metadata:read,pull_requests:read",
        &[
            "gh",
            "pr",
            "list",
            "--repo",
            &context.order.repository,
            "--state",
            "open",
            "--limit",
            "1000",
            "--json",
            "number,title,body,url",
        ],
        minter,
    )
    .map_err(|_| {
        DispatchError::new(
            1,
            format!(
                "ostrom dispatch: could not verify open pull requests for {}",
                context.order.item_id
            ),
        )
    })?;
    if open_prs.as_array().is_some_and(|pulls| {
        pulls.iter().any(|pull| {
            let text = format!(
                "{}\n{}",
                pull["title"].as_str().unwrap_or_default(),
                pull["body"].as_str().unwrap_or_default()
            );
            text.contains(&context.order.item_id)
                || closing_reference(&text, &context.order.item_ref)
        })
    }) {
        return Err(DispatchError::new(
            3,
            format!(
                "ostrom dispatch: an open pull request already references {}",
                context.order.item_id
            ),
        ));
    }

    reap_stale_work_orders(&context.request.paths.state, &context.request.clock)
        .map_err(|error| DispatchError::new(1, error.to_string()))?;
    let trace = read_trace(&context.request.paths.trace_file())
        .map_err(|error| DispatchError::new(1, format!("ostrom dispatch: {error}")))?;
    let rows = trace
        .rows
        .into_iter()
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    let inflight = rows
        .iter()
        .filter(|row| row.kind == "work-dispatched")
        .filter(|dispatch| {
            let Some(order_id) = dispatch.fact.get("order_id").and_then(Value::as_str) else {
                return false;
            };
            !rows.iter().any(|terminal| {
                matches!(terminal.kind.as_str(), "work-completed" | "work-failed")
                    && terminal.fact.get("order_id").and_then(Value::as_str) == Some(order_id)
            })
        })
        .collect::<Vec<_>>();
    if inflight
        .iter()
        .any(|row| row.fact.get("item_id").and_then(Value::as_str) == Some(&context.order.item_id))
    {
        return Err(DispatchError::new(
            3,
            format!(
                "ostrom dispatch: an earlier work-dispatched row has no terminal row for {}",
                context.order.item_id
            ),
        ));
    }

    let max_implementers = positive_usize_env(environment::MANDATE_MAX_IMPLEMENTERS)?
        .unwrap_or(DEFAULT_MAX_IMPLEMENTERS);
    let project_default = config
        .into_iter()
        .flat_map(|config| &config.projects)
        .find(|project| project.repo.as_str() == context.order.repository)
        .and_then(|project| project.max_implementers_per_repository)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(DEFAULT_MAX_IMPLEMENTERS_PER_REPOSITORY);
    let max_per_repository =
        positive_usize_env(environment::MANDATE_MAX_IMPLEMENTERS_PER_REPOSITORY)?
            .unwrap_or(project_default);
    if inflight.len() >= max_implementers {
        return Err(DispatchError::new(
            3,
            format!(
                "ostrom dispatch: concurrency limit reached ({}/{max_implementers})",
                inflight.len()
            ),
        ));
    }
    let repository_inflight = inflight
        .iter()
        .filter(|row| {
            row.fact
                .get("item_id")
                .and_then(Value::as_str)
                .and_then(|id| id.rsplit_once('#').map(|(repository, _)| repository))
                == Some(context.order.repository.as_str())
        })
        .count();
    if repository_inflight >= max_per_repository {
        return Err(DispatchError::new(
            3,
            format!(
                "ostrom dispatch: per-repository concurrency limit reached for {} ({repository_inflight}/{max_per_repository})",
                context.order.repository
            ),
        ));
    }

    let daily_cap = environment::MANDATE_DAILY_CAP_USD
        .value()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| *value > 0.0)
        .unwrap_or(DEFAULT_DAILY_CAP_USD);
    let day = context.request.clock.date();
    let actual = rows
        .iter()
        .filter(|row| row.ts.starts_with(&day))
        .filter(|row| {
            matches!(
                row.kind.as_str(),
                "pass-ended" | "work-completed" | "work-failed"
            )
        })
        .filter_map(|row| row.fact.get("cost_usd").and_then(Value::as_f64))
        .sum::<f64>();
    let reserved = inflight
        .iter()
        .filter_map(|row| row.fact.get("cost_ceiling_usd").and_then(Value::as_f64))
        .sum::<f64>();
    let projected = actual + reserved + context.order.cost();
    if projected > daily_cap {
        return Err(DispatchError::new(
            3,
            format!(
                "ostrom dispatch: daily spend cap would be exceeded by this order ({} > {} USD)",
                render_number(projected),
                render_number(daily_cap)
            ),
        ));
    }
    if context.backend != "systemd" {
        return Err(DispatchError::new(
            2,
            format!("ostrom dispatch: unsupported backend: {}", context.backend),
        ));
    }

    let systemd = environment::MANDATE_SYSTEMD_RUN_BIN
        .value_os()
        .map_or_else(|| PathBuf::from("systemd-run"), PathBuf::from);
    let state_environment = dispatch_state_environment(&context.request.paths);
    let lease_name = format!("implementer-item-{}.lease", context.item_hash);
    // The established systemd-run override is a synchronous fixture seam. Its
    // stubs either do not create units or run the child to completion, so only
    // the real backend (or a fixture with an explicit systemctl seam) can be
    // checked for post-launch liveness.
    let verify_startup = environment::MANDATE_SYSTEMD_RUN_BIN.value_os().is_none()
        || environment::MANDATE_SYSTEMCTL_BIN.value_os().is_some();
    if !verify_startup {
        append_dispatched(context)?;
    }
    let started = Instant::now();
    let mut launch = Command::new(systemd);
    launch
        .args([
            "--user",
            "--unit",
            &context.unit_name,
            "--description",
            &format!("Ostrom implementer {}", context.order.item_id),
            "--collect",
            "--no-block",
            "--property",
            "RuntimeMaxSec=infinity",
            "--property",
            "KillMode=control-group",
            "--setenv",
            &state_environment,
            "--setenv",
            &format!(
                "CLAUDE_PLUGIN_ROOT={}",
                context.request.plugin_root.display()
            ),
            "--setenv",
            &format!("MANDATE_DAILY_CAP_USD={}", render_number(daily_cap)),
            "--setenv",
            &format!("MANDATE_MAX_IMPLEMENTERS={max_implementers}"),
            "--setenv",
            &format!("MANDATE_MAX_IMPLEMENTERS_PER_REPOSITORY={max_per_repository}"),
            "--setenv",
            &format!("MANDATE_DISPATCH_BACKEND={}", context.backend),
            "--setenv",
            &format!("MANDATE_LEASE_NAME={lease_name}"),
            "--setenv",
            &format!("CODEX_BIN={}", resolved_codex.display()),
            "--setenv",
            &format!("PATH={unit_path}"),
        ])
        .arg(resolved_ostrom)
        .arg("implement");
    let status = launch
        .arg(&context.request.order_file)
        .arg(&context.unit_name)
        .status();
    if !status.is_ok_and(|status| status.success()) {
        let _ = append_failure(
            context,
            "dispatch-failed",
            FailureDetail {
                duration_seconds: started.elapsed().as_secs(),
                ..FailureDetail::default()
            },
        );
        return Err(DispatchError::new(
            1,
            format!(
                "ostrom dispatch: systemd backend failed to launch {}",
                context.unit_name
            ),
        ));
    }
    if verify_startup && !implementer_unit_is_alive(&context.unit_name) {
        let _ = append_failure(
            context,
            "dispatch-startup-failed",
            FailureDetail {
                duration_seconds: started.elapsed().as_secs(),
                ..FailureDetail::default()
            },
        );
        return Err(DispatchError::new(
            1,
            format!(
                "ostrom dispatch: implementer exited during startup: {}",
                context.unit_name
            ),
        ));
    }
    if verify_startup {
        append_dispatched(context)?;
    }
    lease.disarm();
    Ok(DispatchOutcome::Started(context.unit_name.clone()))
}

fn dispatch_state_environment(paths: &OstromPaths) -> String {
    environment::CLAUDE_CONFIG_DIR
        .value_os()
        .filter(|value| !value.to_string_lossy().trim().is_empty())
        .map_or_else(
            || format!("OSTROM_HOME={}", paths.state.display()),
            |config| format!("CLAUDE_CONFIG_DIR={}", PathBuf::from(config).display()),
        )
}

fn implementer_unit_is_alive(unit_name: &str) -> bool {
    let grace_milliseconds = environment::MANDATE_IMPLEMENTER_STARTUP_GRACE_MILLISECONDS
        .value()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(IMPLEMENTER_STARTUP_GRACE_MILLISECONDS);
    if grace_milliseconds > 0 {
        thread::sleep(Duration::from_millis(grace_milliseconds));
    }
    let systemctl = environment::MANDATE_SYSTEMCTL_BIN
        .value_os()
        .map_or_else(|| PathBuf::from("systemctl"), PathBuf::from);
    Command::new(systemctl)
        .args(["--user", "is-active", "--quiet", unit_name])
        .status()
        .is_ok_and(|status| status.success())
}

fn preflight_worktree(context: &DispatchContext<'_>) -> Result<(), DispatchError> {
    let root = context
        .request
        .paths
        .state
        .join("implementer-worktrees")
        .join(&context.item_hash);
    if !root.exists() {
        return Ok(());
    }
    let existing = git_text(&root, &["branch", "--show-current"]).unwrap_or_default();
    if existing.is_empty() || existing == context.order.branch_name {
        return Ok(());
    }
    let status =
        git_text(&root, &["status", "--porcelain"]).unwrap_or_else(|| "unreadable".to_owned());
    let default_ref = local_default_ref(&root);
    let unpublished = default_ref
        .as_deref()
        .and_then(|reference| has_unpublished_tree(&root, reference));
    let ahead = default_ref
        .as_deref()
        .and_then(|reference| {
            git_text(
                &root,
                &["rev-list", "--count", &format!("{reference}..HEAD")],
            )
        })
        .and_then(|value| value.parse::<u64>().ok());
    if !status.is_empty() || unpublished.is_none() || unpublished == Some(true) {
        let detail = FailureDetail {
            worktree_path: Some(root.clone()),
            branch_name: Some(existing.clone()),
            ahead_of_default: ahead
                .map_or_else(|| Some(json!("unknown")), |value| Some(json!(value))),
            ..FailureDetail::default()
        };
        let _ = append_failure(context, "worktree-branch-mismatch", detail);
        return Err(DispatchError::new(
            3,
            format!(
                "ostrom dispatch: worktree branch mismatch preserves work at {} on {} (order expects {})",
                root.display(),
                existing,
                context.order.branch_name
            ),
        ));
    }
    Ok(())
}

fn has_unpublished_tree(worktree: &Path, default_ref: &str) -> Option<bool> {
    // Squash merging changes ancestry forever; equal trees prove the branch's
    // work landed even while its original commit remains ahead of main.
    let status = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["diff", "--quiet", default_ref, "HEAD"])
        .status()
        .ok()?;
    match status.code() {
        Some(0) => Some(false),
        Some(1) => Some(true),
        _ => None,
    }
}

fn resolve_source_repository(
    context: &DispatchContext<'_>,
    config: Option<&MandateConfig>,
) -> Result<PathBuf, DispatchError> {
    let result = if let Some(source) = environment::MANDATE_IMPLEMENTER_SOURCE_REPO.value_os() {
        let source = PathBuf::from(source);
        if source.is_dir() {
            Ok(source)
        } else {
            Err("source-repository-not-found")
        }
    } else if config.is_some_and(|config| config.search_roots.is_empty()) {
        Err("source-repository-roots-unconfigured")
    } else if let Some(config) = config {
        find_source_repository(&context.order.repository, &config.search_roots)
    } else {
        Err("source-repository-not-found")
    };
    result.map_err(|reason| {
        let _ = append_failure(
            context,
            reason,
            FailureDetail {
                repository: Some(context.order.repository.clone()),
                ..FailureDetail::default()
            },
        );
        DispatchError::new(
            3,
            format!(
                "ostrom dispatch: {reason}: repository={}",
                context.order.repository
            ),
        )
    })
}

fn find_source_repository(repository: &str, roots: &[String]) -> Result<PathBuf, &'static str> {
    let mut primary = Vec::new();
    let mut linked = Vec::new();
    for root in roots {
        collect_git_markers(Path::new(root), &mut primary, &mut linked, repository);
    }
    primary.sort();
    primary.dedup();
    linked.sort();
    linked.dedup();
    primary.into_iter().next().ok_or({
        if linked.is_empty() {
            "source-repository-not-found"
        } else {
            "source-repository-linked-worktree-only"
        }
    })
}

fn collect_git_markers(
    directory: &Path,
    primary: &mut Vec<PathBuf>,
    linked: &mut Vec<PathBuf>,
    repository: &str,
) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.file_name().is_some_and(|name| name == ".git") {
            let Some(candidate) = path.parent() else {
                continue;
            };
            let remote = git_text(candidate, &["remote", "get-url", "origin"]).unwrap_or_default();
            if normalize_remote(&remote) == repository {
                if path.is_dir() {
                    primary.push(candidate.to_path_buf());
                } else if path.is_file() {
                    linked.push(candidate.to_path_buf());
                }
            }
        } else if path.is_dir() {
            collect_git_markers(&path, primary, linked, repository);
        }
    }
}

fn normalize_remote(remote: &str) -> &str {
    let remote = remote.strip_suffix(".git").unwrap_or(remote);
    remote
        .strip_prefix("https://github.com/")
        .or_else(|| remote.strip_prefix("git@github.com:"))
        .unwrap_or(remote)
}

/// Say what GitHub actually returned instead of calling everything malformed.
///
/// `gh api` prints a JSON **error object to stdout and exits zero** when
/// credentials are rejected, so a rejected token parsed as "the response was
/// malformed" — the listing expects an array. Three distinct causes reached the
/// operator under that one message on 2026-08-19, each needing separate
/// investigation: a plugin root that did not exist, a wrapper that could not
/// resolve `ostrom`, and rejected credentials.
///
/// The repository already treats this distinction as load-bearing: a repository
/// the App is not installed on is an authentication fault to the sweep, not an
/// empty result, because a silently empty queue reads as a healthy quiet
/// portfolio (#106). A degraded listing that is really an auth failure reads as
/// a transient GitHub problem and gets retried forever.
fn describe_unparseable_listing(stdout: &[u8]) -> String {
    if let Ok(value) = serde_json::from_slice::<Value>(stdout) {
        if let Some(message) = value.get("message").and_then(Value::as_str) {
            // GitHub's own words are far more useful than ours.
            return format!("was refused by GitHub: {message}");
        }
        return "returned JSON that is not a branch array".to_owned();
    }
    // Bounded, so a huge or binary body cannot flood the trace, and lossy so a
    // non-UTF-8 body still says something rather than nothing.
    let prefix = String::from_utf8_lossy(&stdout[..stdout.len().min(200)]);
    let prefix = prefix.trim();
    if prefix.is_empty() {
        return "response was empty".to_owned();
    }
    format!("response was malformed; began: {prefix}")
}

fn list_remote_branches(
    context: &DispatchContext<'_>,
    minter: &mut dyn InstallationTokenMinter,
) -> Result<Vec<Vec<RemoteBranch>>, BranchListingFault> {
    let mut pages = Vec::new();
    let mut branch_count = 0;
    for page_number in 1..=REMOTE_BRANCH_PAGE_LIMIT {
        let endpoint = format!(
            "repos/{}/branches?per_page={REMOTE_BRANCH_PAGE_SIZE}&page={page_number}",
            context.order.repository
        );
        let output = gh_output(
            context,
            "metadata:read,contents:read",
            &["gh", "api", &endpoint],
            minter,
        )
        .map_err(|error| BranchListingFault {
            page_count: pages.len(),
            branch_count,
            detail: match error {
                AuthenticatedCommandError::Authentication(error) => {
                    format!("page {page_number} authentication failed: {error}")
                }
                AuthenticatedCommandError::Transport(error) => {
                    format!("page {page_number} transport failed: {error}")
                }
            },
        })?;
        if !output.status.success() {
            let code = output.status.code().unwrap_or(1);
            let stderr = String::from_utf8_lossy(&output.stderr)
                .trim_end()
                .to_owned();
            let suffix = if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            };
            return Err(BranchListingFault {
                page_count: pages.len(),
                branch_count,
                detail: format!("page {page_number} failed (rc={code}){suffix}"),
            });
        }
        let stderr = String::from_utf8_lossy(&output.stderr)
            .trim_end()
            .to_owned();
        if !stderr.is_empty() {
            return Err(BranchListingFault {
                page_count: pages.len(),
                branch_count,
                detail: format!("page {page_number} wrote stderr: {stderr}"),
            });
        }
        let page: Vec<RemoteBranch> =
            serde_json::from_slice(&output.stdout).map_err(|_| BranchListingFault {
                page_count: pages.len(),
                branch_count,
                detail: format!(
                    "page {page_number} {}",
                    describe_unparseable_listing(&output.stdout)
                ),
            })?;
        if page.len() > REMOTE_BRANCH_PAGE_SIZE || page.iter().any(|branch| !branch.valid()) {
            return Err(BranchListingFault {
                page_count: pages.len(),
                branch_count,
                detail: format!("page {page_number} response was malformed"),
            });
        }
        branch_count += page.len();
        let terminal = page.len() < REMOTE_BRANCH_PAGE_SIZE;
        pages.push(page);
        if terminal {
            return Ok(pages);
        }
        if page_number == REMOTE_BRANCH_PAGE_LIMIT {
            return Err(BranchListingFault {
                page_count: pages.len(),
                branch_count,
                detail: format!(
                    "listing reached page limit {REMOTE_BRANCH_PAGE_LIMIT} without proving exhaustion"
                ),
            });
        }
    }
    unreachable!("bounded branch loop returns at its limit")
}

fn reject_unlanded_branch(
    context: &mut DispatchContext<'_>,
    pages: &[Vec<RemoteBranch>],
    branch: &RemoteBranch,
    minter: &mut dyn InstallationTokenMinter,
) -> Result<(), DispatchError> {
    let default = gh_text_quiet(
        context,
        "metadata:read",
        &[
            "gh",
            "repo",
            "view",
            &context.order.repository,
            "--json",
            "defaultBranchRef",
            "--jq",
            ".defaultBranchRef.name",
        ],
        minter,
    );
    let ahead = default
        .as_deref()
        .and_then(|name| {
            pages
                .iter()
                .flatten()
                .find(|candidate| candidate.name == name)
        })
        .and_then(|default| {
            gh_text_quiet(
                context,
                "metadata:read,contents:read",
                &[
                    "gh",
                    "api",
                    &format!(
                        "repos/{}/compare/{}...{}",
                        context.order.repository, default.commit.sha, branch.commit.sha
                    ),
                    "--jq",
                    ".ahead_by",
                ],
                minter,
            )
        })
        .and_then(|value| value.parse::<u64>().ok());
    let pulls = gh_json(
        context,
        "metadata:read,pull_requests:read",
        &[
            "gh",
            "pr",
            "list",
            "--repo",
            &context.order.repository,
            "--head",
            &branch.name,
            "--state",
            "all",
            "--json",
            "number,state,mergedAt",
        ],
        minter,
    )
    .map_err(|_| {
        DispatchError::new(
            1,
            format!(
                "ostrom dispatch: could not verify pull requests for branch {} in {}",
                branch.name, context.order.repository
            ),
        )
    })?;
    let landed = pulls.as_array().is_some_and(|pulls| {
        !pulls.is_empty()
            && pulls
                .iter()
                .all(|pull| pull["state"].as_str() == Some("MERGED"))
    });
    if landed {
        return Ok(());
    }
    context.matched_key = Some(("branch_name", context.order.branch_name.clone()));
    let detail = FailureDetail {
        branch_name: Some(branch.name.clone()),
        repository: Some(context.order.repository.clone()),
        head_sha: Some(branch.commit.sha.clone()),
        ahead_of_default: Some(ahead.map_or_else(|| json!("unknown"), |value| json!(value))),
        ..FailureDetail::default()
    };
    let _ = append_failure(context, "branch-already-pushed", detail);
    Err(DispatchError::new(
        3,
        format!(
            "ostrom dispatch: remote work already exists: matched_key=branch_name:{} repository={} branch={} head={} ahead={}",
            context.order.branch_name,
            context.order.repository,
            branch.name,
            branch.commit.sha,
            ahead.map_or_else(|| "unknown".to_owned(), |value| value.to_string())
        ),
    ))
}

fn reject_closing_pull_requests(
    context: &mut DispatchContext<'_>,
    minter: &mut dyn InstallationTokenMinter,
) -> Result<(), DispatchError> {
    let references = gh_json(
        context,
        "metadata:read,issues:read,pull_requests:read",
        &[
            "gh",
            "issue",
            "view",
            &context.order.item_ref,
            "--repo",
            &context.order.repository,
            "--json",
            "closedByPullRequestsReferences",
        ],
        minter,
    )
    .map_err(|_| {
        DispatchError::new(
            1,
            format!(
                "ostrom dispatch: could not verify closing pull requests for {}",
                context.order.item_id
            ),
        )
    })?;
    let Some(references) = references
        .get("closedByPullRequestsReferences")
        .and_then(Value::as_array)
    else {
        return Err(DispatchError::new(
            1,
            format!(
                "ostrom dispatch: closing pull request references were malformed for {}",
                context.order.item_id
            ),
        ));
    };
    let mut urls = BTreeSet::new();
    for reference in references {
        let Some(url) = reference
            .get("url")
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty())
        else {
            return Err(DispatchError::new(
                1,
                format!(
                    "ostrom dispatch: closing pull request references were malformed for {}",
                    context.order.item_id
                ),
            ));
        };
        urls.insert(url.to_owned());
    }
    for url in urls {
        let pull = gh_json(
            context,
            "metadata:read,pull_requests:read",
            &[
                "gh",
                "pr",
                "view",
                &url,
                "--json",
                "number,state,mergedAt,url",
            ],
            minter,
        )
        .map_err(|_| {
            DispatchError::new(
                1,
                format!(
                    "ostrom dispatch: could not resolve closing pull request {url} for {}",
                    context.order.item_id
                ),
            )
        })?;
        let valid = pull["number"].is_number()
            && matches!(pull["state"].as_str(), Some("OPEN" | "CLOSED" | "MERGED"))
            && pull["url"].as_str() == Some(&url);
        if !valid {
            return Err(DispatchError::new(
                1,
                format!("ostrom dispatch: closing pull request state was malformed for {url}"),
            ));
        }
        if matches!(pull["state"].as_str(), Some("OPEN" | "MERGED")) {
            context.matched_key = Some(("closing_pull_request", url.clone()));
            let _ = append_failure(
                context,
                "branch-already-pushed",
                FailureDetail {
                    repository: Some(context.order.repository.clone()),
                    ..FailureDetail::default()
                },
            );
            return Err(DispatchError::new(
                3,
                format!(
                    "ostrom dispatch: remote work already exists: matched_key=closing_pull_request:{url} item={}",
                    context.order.item_id
                ),
            ));
        }
    }
    Ok(())
}

fn resolve_codex(context: &DispatchContext<'_>) -> Result<PathBuf, DispatchError> {
    let command = environment::CODEX_BIN
        .value_os()
        .map_or_else(|| "codex".into(), PathBuf::from);
    let resolved = if command.components().count() > 1 {
        absolute_executable(&command)
    } else {
        find_on_path(&command).or_else(|| find_in_nvm(&command))
    };
    resolved.ok_or_else(|| {
        let _ = append_failure(context, "codex-unavailable", FailureDetail::default());
        DispatchError::new(
            1,
            format!(
                "ostrom dispatch: Codex is unavailable: {} was not found",
                command.display()
            ),
        )
    })
}

fn resolve_node(context: &DispatchContext<'_>, codex: &Path) -> Result<PathBuf, DispatchError> {
    NodeResolver::from_environment().resolve().ok_or_else(|| {
        let _ = append_failure(context, "codex-unavailable", FailureDetail::default());
        DispatchError::new(
            1,
            format!(
                "ostrom dispatch: Codex is unavailable: Node.js could not be resolved for {}",
                codex.display()
            ),
        )
    })
}

fn resolve_ostrom(context: &DispatchContext<'_>) -> Result<PathBuf, DispatchError> {
    let override_path = environment::MANDATE_OSTROM_BIN.value_os();
    let command = override_path
        .as_ref()
        .map_or_else(|| PathBuf::from("ostrom"), PathBuf::from);
    let resolved = if command.components().count() > 1 {
        absolute_executable(&command)
    } else {
        find_on_path(&command)
    };
    resolved.ok_or_else(|| {
        let _ = append_failure(context, "ostrom-unavailable", FailureDetail::default());
        let message = if override_path.is_some() {
            format!(
                "ostrom dispatch: MANDATE_OSTROM_BIN is unavailable: {} was not found",
                command.display()
            )
        } else {
            "ostrom dispatch: MANDATE_OSTROM_BIN is unset and ostrom was not found on PATH"
                .to_owned()
        };
        DispatchError::new(1, message)
    })
}

fn absolute_executable(candidate: &Path) -> Option<PathBuf> {
    // Being a regular file is not enough. A `MANDATE_OSTROM_BIN` pointing at a
    // present-but-unexecutable file would otherwise pass this resolution, let
    // dispatch reserve capacity and take the per-item lease, and only fail when
    // `systemd-run` tried to exec it — reported as `dispatch-failed` rather than
    // the `ostrom-unavailable` it actually is. The whole point of resolving
    // before the lease is that an unusable binary costs nothing.
    if !crate::pass::is_executable_file(candidate) {
        return None;
    }
    if candidate.is_absolute() {
        Some(candidate.to_path_buf())
    } else {
        candidate.canonicalize().ok()
    }
}

fn find_on_path(command: &Path) -> Option<PathBuf> {
    find_on_path_in(command, environment::PATH.value_os().as_deref())
}

fn find_on_path_in(command: &Path, path: Option<&OsStr>) -> Option<PathBuf> {
    env::split_paths(path?).find_map(|directory| absolute_executable(&directory.join(command)))
}

fn find_in_nvm(command: &Path) -> Option<PathBuf> {
    let home = nonempty_env_path(environment::HOME);
    let nvm = env_path_or_home(environment::NVM_DIR, home.as_deref(), ".nvm")?;
    find_in_nvm_root(command, &nvm)
}

fn find_in_nvm_root(command: &Path, nvm: &Path) -> Option<PathBuf> {
    let default = fs::read_to_string(nvm.join("alias/default"))
        .ok()?
        .lines()
        .next()?
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    let version = default.strip_prefix('v').unwrap_or(&default);
    let parts = version.split('.').collect::<Vec<_>>();
    if parts.len() == 3 && parts.iter().all(|part| is_ascii_number(part)) {
        return absolute_executable(
            &nvm.join("versions/node")
                .join(format!("v{version}"))
                .join("bin")
                .join(command),
        );
    }
    if parts.len() != 1 || !is_ascii_number(version) {
        return None;
    }

    let prefix = format!("v{version}.");
    let mut candidates = fs::read_dir(nvm.join("versions/node"))
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let suffix = name.to_str()?.strip_prefix(&prefix)?;
            let (minor, patch) = suffix.split_once('.')?;
            if patch.contains('.') || !is_ascii_number(minor) || !is_ascii_number(patch) {
                return None;
            }
            Some((
                entry.path().join("bin").join(command),
                minor.parse::<u64>().ok()?,
                patch.parse::<u64>().ok()?,
            ))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| left.0.cmp(&right.0));

    let mut best = None;
    let mut best_version = None;
    for (candidate, minor, patch) in candidates {
        if best_version.is_none_or(|current| (minor, patch) > current) {
            if let Some(candidate) = absolute_executable(&candidate) {
                best = Some(candidate);
                best_version = Some((minor, patch));
            }
        }
    }
    best
}

fn is_ascii_number(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn nonempty_env_path(variable: environment::EnvironmentVariable) -> Option<PathBuf> {
    variable
        .value_os()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn env_path_or_home(
    variable: environment::EnvironmentVariable,
    home: Option<&Path>,
    home_suffix: &str,
) -> Option<PathBuf> {
    nonempty_env_path(variable).or_else(|| home.map(|path| path.join(home_suffix)))
}

#[derive(Debug)]
struct NodeResolver {
    path: Option<OsString>,
    nvm_dir: Option<PathBuf>,
    fnm_dirs: Vec<PathBuf>,
    volta_home: Option<PathBuf>,
    asdf_data_dir: Option<PathBuf>,
    standalone: Vec<PathBuf>,
}

impl NodeResolver {
    fn from_environment() -> Self {
        let home = nonempty_env_path(environment::HOME);
        let mut fnm_dirs = Vec::new();
        if let Some(directory) =
            env_path_or_home(environment::FNM_DIR, home.as_deref(), ".local/share/fnm")
        {
            fnm_dirs.push(directory);
        }
        if let Some(home) = &home {
            fnm_dirs.push(home.join(".fnm"));
        }

        let standalone = environment::OSTROM_NODE_FALLBACKS.value_os().map_or_else(
            || {
                let mut paths = vec![
                    PathBuf::from("/usr/local/bin/node"),
                    PathBuf::from("/opt/homebrew/bin/node"),
                ];
                if let Some(home) = &home {
                    paths.push(home.join(".local/bin/node"));
                }
                paths
            },
            |paths| {
                paths
                    .to_string_lossy()
                    .split_whitespace()
                    .map(PathBuf::from)
                    .collect()
            },
        );

        Self {
            path: environment::PATH.value_os(),
            nvm_dir: env_path_or_home(environment::NVM_DIR, home.as_deref(), ".nvm"),
            fnm_dirs,
            volta_home: env_path_or_home(environment::VOLTA_HOME, home.as_deref(), ".volta"),
            asdf_data_dir: env_path_or_home(environment::ASDF_DATA_DIR, home.as_deref(), ".asdf"),
            standalone,
        }
    }

    fn resolve(&self) -> Option<PathBuf> {
        let command = Path::new("node");
        find_on_path_in(command, self.path.as_deref())
            .or_else(|| {
                self.nvm_dir
                    .as_deref()
                    .and_then(|directory| find_in_nvm_root(command, directory))
            })
            .or_else(|| {
                self.fnm_dirs.iter().find_map(|directory| {
                    absolute_executable(&directory.join("aliases/default/bin/node"))
                })
            })
            .or_else(|| {
                self.volta_home
                    .as_deref()
                    .and_then(|directory| absolute_executable(&directory.join("bin/node")))
            })
            .or_else(|| {
                self.asdf_data_dir
                    .as_deref()
                    .and_then(|directory| absolute_executable(&directory.join("shims/node")))
            })
            .or_else(|| {
                self.standalone
                    .iter()
                    .find_map(|candidate| absolute_executable(candidate))
            })
    }
}

fn positive_usize_env(
    variable: environment::EnvironmentVariable,
) -> Result<Option<usize>, DispatchError> {
    let Some(value) = variable.value_os() else {
        return Ok(None);
    };
    let parsed = value
        .to_string_lossy()
        .parse::<usize>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            DispatchError::new(
                2,
                format!(
                    "ostrom dispatch: {} must be a positive integer",
                    variable.name
                ),
            )
        })?;
    Ok(Some(parsed))
}

fn gh_output(
    context: &DispatchContext<'_>,
    permissions: &str,
    command: &[&str],
    minter: &mut dyn InstallationTokenMinter,
) -> Result<Output, AuthenticatedCommandError> {
    authenticated_output(
        &context.request.paths,
        ScopedAppTokenRequest::new(
            "builder",
            &context.order.repository,
            &context.order.repository,
            permissions,
        ),
        command,
        minter,
    )
}

fn gh_json(
    context: &DispatchContext<'_>,
    permissions: &str,
    command: &[&str],
    minter: &mut dyn InstallationTokenMinter,
) -> Result<Value, String> {
    let output =
        gh_output(context, permissions, command, minter).map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    serde_json::from_slice(&output.stdout).map_err(|error| error.to_string())
}

fn gh_text_quiet(
    context: &DispatchContext<'_>,
    permissions: &str,
    command: &[&str],
    minter: &mut dyn InstallationTokenMinter,
) -> Option<String> {
    let output = gh_output(context, permissions, command, minter).ok()?;
    output
        .status
        .success()
        .then(|| {
            String::from_utf8_lossy(&output.stdout)
                .trim_end()
                .to_owned()
        })
        .filter(|value| !value.is_empty())
}

#[derive(Debug, Default)]
struct FailureDetail {
    duration_seconds: u64,
    worktree_path: Option<PathBuf>,
    branch_name: Option<String>,
    repository: Option<String>,
    head_sha: Option<String>,
    ahead_of_default: Option<Value>,
}

fn append_failure(
    context: &DispatchContext<'_>,
    reason: &str,
    detail: FailureDetail,
) -> Result<(), DispatchError> {
    let mut fact = Map::new();
    fact.insert("schema_version".to_owned(), json!(1));
    fact.insert("item_id".to_owned(), json!(context.order.item_id));
    fact.insert("order_id".to_owned(), json!(context.order.order_id));
    fact.insert("unit_name".to_owned(), json!(context.unit_name));
    fact.insert("backend".to_owned(), json!(context.backend));
    fact.insert(
        "cost_ceiling_usd".to_owned(),
        context.order.cost_ceiling_usd.clone(),
    );
    fact.insert(
        "token_ceiling".to_owned(),
        context.order.token_ceiling.clone(),
    );
    fact.insert("cost_usd".to_owned(), json!(0));
    fact.insert(
        "duration_seconds".to_owned(),
        json!(detail.duration_seconds),
    );
    fact.insert("pr_url".to_owned(), Value::Null);
    fact.insert("reason".to_owned(), json!(reason));
    fact.insert(
        "worktree_path".to_owned(),
        detail
            .worktree_path
            .map_or(Value::Null, |path| json!(path.display().to_string())),
    );
    fact.insert(
        "branch_name".to_owned(),
        detail.branch_name.map_or(Value::Null, Value::String),
    );
    fact.insert(
        "repository".to_owned(),
        detail.repository.map_or(Value::Null, Value::String),
    );
    fact.insert(
        "head_sha".to_owned(),
        detail.head_sha.map_or(Value::Null, Value::String),
    );
    fact.insert(
        "ahead_of_default".to_owned(),
        detail.ahead_of_default.unwrap_or(Value::Null),
    );
    fact.insert(
        "usage".to_owned(),
        json!({
            "input_tokens": 0,
            "cached_input_tokens": 0,
            "output_tokens": 0,
            "reasoning_output_tokens": 0
        }),
    );
    if let Some((kind, value)) = &context.matched_key {
        fact.insert(
            "matched_key".to_owned(),
            json!({"type": kind, "value": value}),
        );
    }
    if let Some(outcome) = context.listing.outcome {
        fact.insert(
            "branch_listing".to_owned(),
            listing_json(&context.listing, outcome),
        );
    }
    if let Err(error) =
        crate::reap_build_cache(&context.request.paths.state, &context.order.item_id)
    {
        eprintln!("ostrom dispatch: could not reap build cache: {error}");
    }
    append_fact(context, "work-failed", fact)
}

fn append_dispatched(context: &DispatchContext<'_>) -> Result<(), DispatchError> {
    let mut fact = Map::new();
    fact.insert("schema_version".to_owned(), json!(1));
    fact.insert("item_id".to_owned(), json!(context.order.item_id));
    fact.insert("order_id".to_owned(), json!(context.order.order_id));
    fact.insert("unit_name".to_owned(), json!(context.unit_name));
    fact.insert("backend".to_owned(), json!(context.backend));
    fact.insert(
        "cost_ceiling_usd".to_owned(),
        context.order.cost_ceiling_usd.clone(),
    );
    fact.insert(
        "token_ceiling".to_owned(),
        context.order.token_ceiling.clone(),
    );
    fact.insert("cost_usd".to_owned(), Value::Null);
    fact.insert("duration_seconds".to_owned(), json!(0));
    fact.insert(
        "branch_listing".to_owned(),
        listing_json(
            &context.listing,
            context
                .listing
                .outcome
                .unwrap_or(BranchListingOutcome::ProvenExhaustiveNoMatch),
        ),
    );
    append_fact(context, "work-dispatched", fact)
        .map_err(|_| DispatchError::new(1, "ostrom dispatch: could not record work-dispatched"))
}

fn listing_json(state: &ListingState, outcome: BranchListingOutcome) -> Value {
    json!({
        "outcome": outcome.as_str(),
        "page_count": state.page_count,
        "branch_count": state.branch_count,
        "matched_branch": state.matched_branch,
        "error": state.error
    })
}

fn append_fact(
    context: &DispatchContext<'_>,
    kind: &str,
    fact: Map<String, Value>,
) -> Result<(), DispatchError> {
    append_trace(
        &context.request.paths.trace_file(),
        &TraceAppend {
            ts: context.request.clock.timestamp(),
            kind: kind.to_owned(),
            fact,
            narration: Map::new(),
        },
    )
    .map(|_| ())
    .map_err(|error| DispatchError::new(1, format!("ostrom dispatch: {error}")))
}

struct LeaseGuard {
    path: PathBuf,
    owner: String,
    armed: bool,
}

impl LeaseGuard {
    fn release(&mut self) {
        if self.armed
            && read_lease(&self.path)
                .ok()
                .flatten()
                .is_some_and(|lease| lease.owner == self.owner)
        {
            let _ = fs::remove_file(&self.path);
        }
        self.armed = false;
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        self.release();
    }
}

fn acquire_dispatch_lease(
    path: &Path,
    owner: &str,
    ttl: u64,
    clock: &Clock,
) -> Result<LeaseGuard, i32> {
    let now = clock.epoch_seconds();
    if let Ok(Some(existing)) = read_lease(path) {
        let derived_expiry = existing.started_at.saturating_add(ttl);
        if existing.expires_at.min(derived_expiry) > now {
            return Err(3);
        }
        fs::remove_file(path).map_err(|_| 3)?;
    } else if path.exists() {
        return Err(3);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|_| 1)?;
    }
    let lease = LeaseRecord {
        owner: owner.to_owned(),
        started_at: now,
        expires_at: now.saturating_add(ttl),
    };
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                3
            } else {
                1
            }
        })?;
    crate::set_private_file_mode(path).map_err(|_| 1)?;
    let mut bytes = serde_json::to_vec(&lease).map_err(|_| 1)?;
    bytes.push(b'\n');
    file.write_all(&bytes).map_err(|_| 1)?;
    Ok(LeaseGuard {
        path: path.to_path_buf(),
        owner: owner.to_owned(),
        armed: true,
    })
}

fn closing_reference(text: &str, item_ref: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "close ",
        "closes ",
        "closed ",
        "fix ",
        "fixes ",
        "fixed ",
        "ref ",
        "refs ",
        "references ",
    ]
    .iter()
    .any(|verb| lower.contains(&format!("{verb}{}", item_ref.to_ascii_lowercase())))
}

fn local_default_ref(worktree: &Path) -> Option<String> {
    let symbolic = git_text(
        worktree,
        &["symbolic-ref", "-q", "refs/remotes/origin/HEAD"],
    );
    if symbolic.as_deref().is_some_and(|reference| {
        git_success(
            worktree,
            &["rev-parse", "--verify", &format!("{reference}^{{commit}}")],
        )
    }) {
        return symbolic;
    }
    for candidate in ["refs/remotes/origin/main", "refs/remotes/origin/master"] {
        if git_success(
            worktree,
            &["rev-parse", "--verify", &format!("{candidate}^{{commit}}")],
        ) {
            return Some(candidate.to_owned());
        }
    }
    let refs = git_text(
        worktree,
        &["for-each-ref", "--format=%(refname)", "refs/remotes/origin"],
    )?
    .lines()
    .filter(|reference| !reference.ends_with("/HEAD"))
    .map(str::to_owned)
    .collect::<Vec<_>>();
    (refs.len() == 1).then(|| refs[0].clone())
}

fn git_text(directory: &Path, arguments: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .output()
        .ok()?;
    output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_owned()
    })
}

fn git_success(directory: &Path, arguments: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .status()
        .is_ok_and(|status| status.success())
}

fn render_number(number: f64) -> String {
    if number.fract() == 0.0 {
        format!("{number:.0}")
    } else {
        number.to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, fs, path::Path, process::Command};

    use tempfile::tempdir;

    use ostrom_core::WorkOrder;
    use serde_json::json;

    use super::{
        NodeResolver, absolute_executable, describe_unparseable_listing, find_in_nvm_root,
        has_unpublished_tree,
    };
    use crate::work_order::implementer_lease_ttl;

    fn git(path: &Path, arguments: &[&str]) {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(path)
                .args(arguments)
                .status()
                .expect("run git")
                .success(),
            "git {arguments:?}"
        );
    }

    #[cfg(unix)]
    fn executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        fs::create_dir_all(path.parent().expect("executable parent")).expect("create parent");
        fs::write(path, "#!/bin/sh\nexit 0\n").expect("write executable");
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod executable");
    }

    #[cfg(unix)]
    #[test]
    fn node_resolution_is_first_hit_wins_across_every_supported_layout() {
        let root = tempdir().expect("temporary node resolution fixture");
        let path_node = root.path().join("path/node");
        let nvm = root.path().join("nvm");
        let older_nvm_node = nvm.join("versions/node/v20.8.9/bin/node");
        let nvm_node = nvm.join("versions/node/v20.10.1/bin/node");
        let fnm = root.path().join("fnm");
        let fnm_node = fnm.join("aliases/default/bin/node");
        let legacy_fnm = root.path().join("home/.fnm");
        let legacy_fnm_node = legacy_fnm.join("aliases/default/bin/node");
        let volta = root.path().join("volta");
        let volta_node = volta.join("bin/node");
        let asdf = root.path().join("asdf");
        let asdf_node = asdf.join("shims/node");
        let standalone_node = root.path().join("standalone/node");
        let resolver = NodeResolver {
            path: Some(OsString::from(root.path().join("path"))),
            nvm_dir: Some(nvm.clone()),
            fnm_dirs: vec![fnm, legacy_fnm],
            volta_home: Some(volta),
            asdf_data_dir: Some(asdf),
            standalone: vec![standalone_node.clone()],
        };

        assert_eq!(resolver.resolve(), None);

        executable(&standalone_node);
        assert_eq!(
            resolver.resolve().as_deref(),
            Some(standalone_node.as_path())
        );

        executable(&asdf_node);
        assert_eq!(resolver.resolve().as_deref(), Some(asdf_node.as_path()));

        executable(&volta_node);
        assert_eq!(resolver.resolve().as_deref(), Some(volta_node.as_path()));

        executable(&legacy_fnm_node);
        assert_eq!(
            resolver.resolve().as_deref(),
            Some(legacy_fnm_node.as_path())
        );

        executable(&fnm_node);
        assert_eq!(resolver.resolve().as_deref(), Some(fnm_node.as_path()));

        fs::create_dir_all(nvm.join("alias")).expect("create nvm alias directory");
        fs::write(nvm.join("alias/default"), "  v20 \nignored\n").expect("write major alias");
        executable(&older_nvm_node);
        executable(&nvm_node);
        assert_eq!(resolver.resolve().as_deref(), Some(nvm_node.as_path()));

        executable(&path_node);
        assert_eq!(resolver.resolve().as_deref(), Some(path_node.as_path()));
    }

    #[cfg(unix)]
    #[test]
    fn nvm_resolution_uses_only_the_default_alias() {
        let root = tempdir().expect("temporary nvm resolution fixture");
        let nvm = root.path().join("nvm");
        let default_node = nvm.join("versions/node/v18.19.1/bin/codex");
        let newer_node = nvm.join("versions/node/v22.1.0/bin/codex");
        executable(&default_node);
        executable(&newer_node);
        fs::create_dir_all(nvm.join("alias")).expect("create nvm alias directory");
        fs::write(nvm.join("alias/default"), " v18.19.1 \n").expect("write exact alias");

        assert_eq!(
            find_in_nvm_root(Path::new("codex"), &nvm).as_deref(),
            Some(default_node.as_path())
        );

        fs::write(nvm.join("alias/default"), "node\n").expect("write unsupported alias");
        assert_eq!(find_in_nvm_root(Path::new("codex"), &nvm), None);
    }

    /// The alias `nvm alias default 24` records a bare major version, not a
    /// full one, and it is what the operator's machine actually holds: the
    /// shim this replaced resolved it to the newest matching install. The
    /// exact-version case above exercises a different branch entirely, so
    /// without this the code path that runs in production is the untested one.
    #[test]
    fn a_major_version_alias_resolves_to_the_newest_matching_install() {
        let root = tempdir().expect("temporary nvm major alias fixture");
        let nvm = root.path().join("nvm");
        // Deliberately spans majors and puts a higher *minor* below a lower
        // one lexicographically: "v24.18.0" sorts before "v24.9.0" as text,
        // so a string comparison would pick the wrong one.
        for version in ["v22.22.3", "v24.9.0", "v24.15.0", "v24.18.0"] {
            executable(&nvm.join(format!("versions/node/{version}/bin/codex")));
        }
        fs::create_dir_all(nvm.join("alias")).expect("create nvm alias directory");
        fs::write(nvm.join("alias/default"), "24\n").expect("write major alias");

        assert_eq!(
            find_in_nvm_root(Path::new("codex"), &nvm).as_deref(),
            Some(nvm.join("versions/node/v24.18.0/bin/codex").as_path())
        );
    }

    /// A newer install whose binary is absent must not shadow the newest one
    /// that is actually runnable — otherwise a half-removed version makes the
    /// resolver report nothing rather than falling back.
    #[test]
    fn a_major_alias_skips_a_version_whose_binary_is_missing() {
        let root = tempdir().expect("temporary nvm partial install fixture");
        let nvm = root.path().join("nvm");
        executable(&nvm.join("versions/node/v24.15.0/bin/codex"));
        fs::create_dir_all(nvm.join("versions/node/v24.18.0/bin"))
            .expect("create version directory with no binary");
        fs::create_dir_all(nvm.join("alias")).expect("create nvm alias directory");
        fs::write(nvm.join("alias/default"), "24\n").expect("write major alias");

        assert_eq!(
            find_in_nvm_root(Path::new("codex"), &nvm).as_deref(),
            Some(nvm.join("versions/node/v24.15.0/bin/codex").as_path())
        );
    }

    #[test]
    fn launch_boundary_is_an_explicit_command_not_an_in_process_side_effect() {
        let source = include_str!("dispatch.rs");
        assert!(source.contains("Command::new(systemd)"));
        for forbidden in [["git", "push"], ["git", "branch"]] {
            assert!(!source.contains(&forbidden.join(" ")));
        }
    }

    #[test]
    fn implementer_lease_ttl_tracks_both_order_ceilings() {
        let order = |cost, tokens| {
            WorkOrder::from_json(
                &serde_json::to_vec(&json!({
                    "schema_version": 1,
                    "item_id": "placeholder-org/alpha#7",
                    "repository": "placeholder-org/alpha",
                    "item_ref": "#7",
                    "branch_name": "ostrom/7-placeholder",
                    "spec": "Change a placeholder fixture.",
                    "acceptance_criteria": ["The placeholder changes."],
                    "constraints": ["Use placeholder data only."],
                    "order_id": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                    "created_at": "2026-08-01T00:00:00Z",
                    "cost_ceiling_usd": cost,
                    "token_ceiling": tokens
                }))
                .expect("serialize order"),
            )
            .expect("valid order")
        };

        assert_eq!(implementer_lease_ttl(&order(20, 500_000)), 5_300);
        assert_eq!(implementer_lease_ttl(&order(30, 100)), 7_500);
    }

    #[test]
    fn rust_credential_callers_do_not_reference_the_shell_wrapper() {
        let wrapper = ["gh-as", ".sh"].concat();
        for (name, source) in [
            ("dispatch.rs", include_str!("dispatch.rs")),
            ("implement.rs", include_str!("implement.rs")),
            ("publish.rs", include_str!("publish.rs")),
        ] {
            assert!(
                !source.contains(&wrapper),
                "source grep found the credential wrapper in {name}"
            );
        }
    }

    #[test]
    fn dispatch_mismatch_guard_accepts_a_real_squash_merge() {
        let fixture = tempdir().expect("temporary repository");
        let repo = fixture.path().join("placeholder-alpha");
        fs::create_dir(&repo).expect("create repository");
        git(&repo, &["init", "-b", "main"]);
        git(&repo, &["config", "user.email", "fixture@example.invalid"]);
        git(&repo, &["config", "user.name", "Fixture"]);
        fs::write(repo.join("README.md"), "base\n").expect("write base");
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "-m", "base"]);
        git(&repo, &["switch", "-c", "candidate/placeholder"]);
        fs::write(repo.join("README.md"), "base\nchange\n").expect("write change");
        git(&repo, &["commit", "-am", "change"]);
        git(&repo, &["switch", "main"]);
        git(&repo, &["merge", "--squash", "candidate/placeholder"]);
        git(&repo, &["commit", "-m", "squash change"]);
        git(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        git(&repo, &["switch", "candidate/placeholder"]);

        assert_eq!(
            has_unpublished_tree(&repo, "refs/remotes/origin/main"),
            Some(false)
        );
        assert_eq!(
            super::git_text(
                &repo,
                &["rev-list", "--count", "refs/remotes/origin/main..HEAD"]
            )
            .as_deref(),
            Some("1"),
            "the ancestry-only guard would refuse this branch forever"
        );
    }

    /// A present-but-unexecutable path must not resolve. Dispatch resolves the
    /// binary *before* it reserves capacity and takes the per-item lease, so a
    /// resolution that accepts an unusable file converts a free, named
    /// `ostrom-unavailable` into a lease-consuming `dispatch-failed` at exec
    /// time. This is the same defect class fixed in the pass guard in #286.
    /// `gh api` exits **zero** and prints a JSON error object when credentials
    /// are rejected, so the listing parser saw an object where it wanted an
    /// array and reported "malformed". That sent three separate causes to the
    /// operator under one message on 2026-08-19.
    #[test]
    fn a_refused_listing_names_github_rather_than_blaming_the_shape() {
        let refused = br#"{"message":"Bad credentials","status":"401"}"#;
        assert_eq!(
            describe_unparseable_listing(refused),
            "was refused by GitHub: Bad credentials"
        );

        // Valid JSON, wrong shape: still not an auth problem, and says so.
        assert_eq!(
            describe_unparseable_listing(b"{\"unexpected\":true}"),
            "returned JSON that is not a branch array"
        );

        // Genuinely malformed carries a bounded prefix, so the next occurrence
        // is diagnosable from the trace alone.
        let described = describe_unparseable_listing(b"<html>gateway timeout</html>");
        assert!(
            described.starts_with("response was malformed; began: <html>"),
            "got: {described}"
        );

        assert_eq!(describe_unparseable_listing(b"   "), "response was empty");

        // A huge body must not flood the trace.
        let flood = vec![b'x'; 10_000];
        assert!(describe_unparseable_listing(&flood).len() < 300);
    }

    #[cfg(unix)]
    #[test]
    fn a_non_executable_file_does_not_resolve_as_the_cli() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempdir().expect("temporary resolution fixture");
        let candidate = root.path().join("ostrom");
        fs::write(&candidate, "#!/usr/bin/env bash\nexit 0\n").expect("write candidate");

        fs::set_permissions(&candidate, fs::Permissions::from_mode(0o644))
            .expect("clear the executable bits");
        assert!(absolute_executable(&candidate).is_none());

        fs::set_permissions(&candidate, fs::Permissions::from_mode(0o755))
            .expect("restore the executable bits");
        assert_eq!(
            absolute_executable(&candidate).as_deref(),
            Some(candidate.as_path())
        );
    }
}
