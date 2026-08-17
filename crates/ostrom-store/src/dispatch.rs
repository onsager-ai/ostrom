use std::{
    collections::BTreeSet,
    env, fs,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use chrono::{DateTime, Utc};
use ostrom_core::{
    BranchListing, BranchListingFault, BranchListingOutcome, MandateConfig, RemoteBranch,
    WorkOrder, resolve_exact_branch,
};
use serde_json::{Map, Value, json};

use crate::{
    LeaseRecord, OstromPaths, TraceAppend, append_trace, load_config_or_defaults, read_lease,
    read_trace,
};

const DEFAULT_DAILY_CAP_USD: f64 = 50.0;
const DEFAULT_MAX_IMPLEMENTERS: usize = 2;
const DEFAULT_MAX_IMPLEMENTERS_PER_REPOSITORY: usize = 1;
const REMOTE_BRANCH_PAGE_SIZE: usize = 100;
const REMOTE_BRANCH_PAGE_LIMIT: usize = 100;
const DEFAULT_IMPLEMENTER_LEASE_TTL_SECONDS: u64 = 2_592_000;
// The production comparison window keeps rollback to one environment change.
const DEFAULT_IMPLEMENTER_ENGINE: &str = "shell";

#[derive(Debug, Clone)]
pub struct DispatchRequest {
    pub paths: OstromPaths,
    pub working_directory: PathBuf,
    pub plugin_root: PathBuf,
    pub order_file: PathBuf,
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
        backend: env::var("MANDATE_DISPATCH_BACKEND").unwrap_or_else(|_| "systemd".to_owned()),
        listing: ListingState::empty(),
        matched_key: None,
    };

    preflight_worktree(&context)?;
    let config = load_config_or_defaults(&request.paths, &request.working_directory).ok();
    resolve_source_repository(&context, config.as_ref())?;

    let pages = match list_remote_branches(&context) {
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
        reject_unlanded_branch(&mut context, &pages, branch)?;
    }
    reject_closing_pull_requests(&mut context)?;

    let resolved_codex = resolve_codex(&context)?;
    let resolved_node = resolve_node(&context, &resolved_codex)?;
    let inherited_path =
        env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".to_owned());
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

    let lease_ttl = match env::var("MANDATE_IMPLEMENTER_LEASE_TTL_SECONDS") {
        Ok(value) => value
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
        Err(_) => DEFAULT_IMPLEMENTER_LEASE_TTL_SECONDS,
    };
    let lease_path = request
        .paths
        .state
        .join(format!("implementer-item-{}.lease", context.item_hash));
    let mut lease =
        acquire_dispatch_lease(&lease_path, &context.unit_name, lease_ttl).map_err(|code| {
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
        &unit_path,
        &mut lease,
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
    unit_path: &str,
    lease: &mut LeaseGuard,
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

    let max_implementers =
        positive_usize_env("MANDATE_MAX_IMPLEMENTERS")?.unwrap_or(DEFAULT_MAX_IMPLEMENTERS);
    let project_default = config
        .into_iter()
        .flat_map(|config| &config.projects)
        .find(|project| project.repo.as_str() == context.order.repository)
        .and_then(|project| project.max_implementers_per_repository)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(DEFAULT_MAX_IMPLEMENTERS_PER_REPOSITORY);
    let max_per_repository =
        positive_usize_env("MANDATE_MAX_IMPLEMENTERS_PER_REPOSITORY")?.unwrap_or(project_default);
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

    let daily_cap = env::var("MANDATE_DAILY_CAP_USD")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| *value > 0.0)
        .unwrap_or(DEFAULT_DAILY_CAP_USD);
    let now = env::var("MANDATE_NOW_EPOCH")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64
        });
    let day = DateTime::<Utc>::from_timestamp(now, 0)
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
        .format("%Y-%m-%d")
        .to_string();
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

    let implementer_engine = env::var("MANDATE_IMPLEMENTER_ENGINE")
        .unwrap_or_else(|_| DEFAULT_IMPLEMENTER_ENGINE.to_owned());
    let (implementer, implementer_verb) = implementer_launch(
        &context.request.plugin_root,
        &implementer_engine,
        env::var_os("MANDATE_IMPLEMENTER_BIN"),
        env::var_os("MANDATE_OSTROM_BIN"),
    )?;
    append_dispatched(context)?;
    let systemd = env::var_os("MANDATE_SYSTEMD_RUN_BIN")
        .map_or_else(|| PathBuf::from("systemd-run"), PathBuf::from);
    let config_dir = env::var_os("CLAUDE_CONFIG_DIR")
        .map_or_else(|| context.request.paths.config.clone(), PathBuf::from);
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
            &format!("CLAUDE_CONFIG_DIR={}", config_dir.display()),
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
            &format!("CODEX_BIN={}", resolved_codex.display()),
            "--setenv",
            &format!("PATH={unit_path}"),
        ])
        .arg(&implementer);
    if let Some(verb) = implementer_verb {
        launch.arg(verb);
    }
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
    lease.disarm();
    Ok(DispatchOutcome::Started(context.unit_name.clone()))
}

fn implementer_launch(
    plugin_root: &Path,
    engine: &str,
    shell_override: Option<std::ffi::OsString>,
    rust_override: Option<std::ffi::OsString>,
) -> Result<(PathBuf, Option<&'static str>), DispatchError> {
    match engine {
        "shell" => Ok((
            shell_override.map_or_else(|| plugin_root.join("scripts/implement.sh"), PathBuf::from),
            None,
        )),
        "rust" => Ok((
            rust_override.map_or_else(|| PathBuf::from("ostrom"), PathBuf::from),
            Some("implement"),
        )),
        other => Err(DispatchError::new(
            2,
            format!("ostrom dispatch: unsupported implementer engine: {other}"),
        )),
    }
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
    let result = if let Some(source) = env::var_os("MANDATE_IMPLEMENTER_SOURCE_REPO") {
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

fn list_remote_branches(
    context: &DispatchContext<'_>,
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
        )
        .map_err(|error| BranchListingFault {
            page_count: pages.len(),
            branch_count,
            detail: format!("page {page_number} failed (rc=1): {error}"),
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
                detail: format!("page {page_number} response was malformed"),
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

fn reject_closing_pull_requests(context: &mut DispatchContext<'_>) -> Result<(), DispatchError> {
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
    let command = env::var_os("CODEX_BIN").map_or_else(|| "codex".into(), PathBuf::from);
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
    let output = Command::new("bash")
        .arg(context.request.plugin_root.join("scripts/run-node.sh"))
        .arg("--resolve-only")
        .output();
    let resolved = output
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|path| PathBuf::from(path.trim_end()));
    resolved.ok_or_else(|| {
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

fn absolute_executable(candidate: &Path) -> Option<PathBuf> {
    candidate
        .metadata()
        .ok()
        .filter(|metadata| metadata.is_file())?;
    if candidate.is_absolute() {
        Some(candidate.to_path_buf())
    } else {
        candidate.canonicalize().ok()
    }
}

fn find_on_path(command: &Path) -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?)
        .find_map(|directory| absolute_executable(&directory.join(command)))
}

fn find_in_nvm(command: &Path) -> Option<PathBuf> {
    let nvm = env::var_os("NVM_DIR")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".nvm")))?;
    let mut candidates = fs::read_dir(nvm.join("versions/node"))
        .ok()?
        .flatten()
        .map(|entry| entry.path().join("bin").join(command))
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        let left_version = node_version(left);
        let right_version = node_version(right);
        right_version
            .cmp(&left_version)
            .then_with(|| right.cmp(left))
    });
    candidates
        .into_iter()
        .find_map(|candidate| absolute_executable(&candidate))
}

fn node_version(candidate: &Path) -> Vec<u64> {
    candidate
        .parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .and_then(|version| version.to_str())
        .unwrap_or_default()
        .trim_start_matches('v')
        .split('.')
        .map(|part| part.parse().unwrap_or_default())
        .collect()
}

fn positive_usize_env(name: &str) -> Result<Option<usize>, DispatchError> {
    let Some(value) = env::var_os(name) else {
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
                format!("ostrom dispatch: {name} must be a positive integer"),
            )
        })?;
    Ok(Some(parsed))
}

fn gh_output(
    context: &DispatchContext<'_>,
    permissions: &str,
    command: &[&str],
) -> Result<Output, String> {
    let gh_as = env::var_os("MANDATE_GH_AS_BIN").map_or_else(
        || context.request.plugin_root.join("scripts/gh-as.sh"),
        PathBuf::from,
    );
    Command::new("bash")
        .arg(gh_as)
        .arg("builder")
        .arg(&context.order.repository)
        .arg("--repositories")
        .arg(&context.order.repository)
        .arg("--permissions")
        .arg(permissions)
        .arg("--")
        .args(command)
        .output()
        .map_err(|error| error.to_string())
}

fn gh_json(
    context: &DispatchContext<'_>,
    permissions: &str,
    command: &[&str],
) -> Result<Value, String> {
    let output = gh_output(context, permissions, command)?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    serde_json::from_slice(&output.stdout).map_err(|error| error.to_string())
}

fn gh_text_quiet(
    context: &DispatchContext<'_>,
    permissions: &str,
    command: &[&str],
) -> Option<String> {
    let output = gh_output(context, permissions, command).ok()?;
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
    let timestamp = env::var("MANDATE_TRACE_TIME").unwrap_or_else(|_| {
        DateTime::<Utc>::from(SystemTime::now())
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    });
    append_trace(
        &context.request.paths.trace_file(),
        &TraceAppend {
            ts: timestamp,
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

fn acquire_dispatch_lease(path: &Path, owner: &str, ttl: u64) -> Result<LeaseGuard, i32> {
    let now = env::var("MANDATE_LEASE_NOW_EPOCH")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        });
    if let Ok(Some(existing)) = read_lease(path) {
        if existing.expires_at > now {
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
    use std::{
        fs,
        path::{Path, PathBuf},
        process::Command,
    };

    use tempfile::tempdir;

    use super::{DEFAULT_IMPLEMENTER_ENGINE, has_unpublished_tree, implementer_launch};

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

    #[test]
    fn launch_boundary_is_an_explicit_command_not_an_in_process_side_effect() {
        let source = include_str!("dispatch.rs");
        assert!(source.contains("Command::new(systemd)"));
        for forbidden in [["git", "push"], ["git", "branch"]] {
            assert!(!source.contains(&forbidden.join(" ")));
        }
    }

    #[test]
    fn implementer_defaults_to_shell_and_rust_requires_explicit_selection() {
        let root = Path::new("/placeholder/plugin");
        assert_eq!(DEFAULT_IMPLEMENTER_ENGINE, "shell");
        assert_eq!(
            implementer_launch(root, DEFAULT_IMPLEMENTER_ENGINE, None, None)
                .expect("default launch"),
            (root.join("scripts/implement.sh"), None)
        );
        assert_eq!(
            implementer_launch(root, "rust", None, None).expect("Rust launch"),
            (PathBuf::from("ostrom"), Some("implement"))
        );
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
}
