use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use ostrom_core::WorkOrder;
use serde::Serialize;
use serde_json::{Map, Value};
use thiserror::Error;

use crate::{
    Clock, OstromPaths, TraceAppend,
    app_token::{
        GitHubInstallationTokenMinter, InstallationTokenMinter, ScopedAppTokenRequest,
        authenticated_output,
    },
    append_trace, read_lease,
};

#[derive(Debug, Clone)]
pub struct ReapWorktreesOptions {
    pub paths: OstromPaths,
    pub apply: bool,
    pub clock: Clock,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorktreeReapReport {
    pub schema_version: u8,
    pub item_id: Option<String>,
    pub item_hash: String,
    pub repository: Option<String>,
    pub branch_name: Option<String>,
    pub worktree_path: PathBuf,
    pub dry_run: bool,
    pub outcome: String,
    pub reason: String,
    pub bytes: u64,
    pub reclaimed_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReapWorktreesSummary {
    pub schema_version: u8,
    pub dry_run: bool,
    pub scanned_count: usize,
    pub candidate_count: usize,
    pub retained_count: usize,
    pub reaped_count: usize,
    pub candidate_bytes: u64,
    pub reclaimed_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReapWorktreesOutcome {
    pub reports: Vec<WorktreeReapReport>,
    pub summary: ReapWorktreesSummary,
}

#[derive(Debug, Error)]
pub enum ReapWorktreesError {
    #[error("ostrom reap-worktrees: could not read worktree root {0}")]
    RootRead(String),
    #[error("ostrom reap-worktrees: could not append trace: {0}")]
    Trace(String),
}

pub fn run_reap_worktrees(
    options: &ReapWorktreesOptions,
) -> Result<ReapWorktreesOutcome, ReapWorktreesError> {
    let mut minter = GitHubInstallationTokenMinter;
    run_reap_worktrees_with_minter(options, &mut minter)
}

fn run_reap_worktrees_with_minter(
    options: &ReapWorktreesOptions,
    minter: &mut dyn InstallationTokenMinter,
) -> Result<ReapWorktreesOutcome, ReapWorktreesError> {
    let root = options.paths.state.join("implementer-worktrees");
    if !root.exists() {
        let summary = ReapWorktreesSummary {
            schema_version: 1,
            dry_run: !options.apply,
            scanned_count: 0,
            candidate_count: 0,
            retained_count: 0,
            reaped_count: 0,
            candidate_bytes: 0,
            reclaimed_bytes: 0,
        };
        append_summary(options, &summary)?;
        return Ok(ReapWorktreesOutcome {
            reports: Vec::new(),
            summary,
        });
    }
    let mut entries = fs::read_dir(&root)
        .map_err(|_| ReapWorktreesError::RootRead(root.display().to_string()))?
        .map(|entry| {
            let entry = entry?;
            let file_type = entry.file_type()?;
            Ok((entry, file_type))
        })
        .collect::<Result<Vec<_>, std::io::Error>>()
        .map_err(|_| ReapWorktreesError::RootRead(root.display().to_string()))?
        .into_iter()
        .filter_map(|(entry, file_type)| file_type.is_dir().then_some(entry))
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());

    let mut reports = Vec::with_capacity(entries.len());
    for entry in entries {
        let worktree = entry.path();
        let item_hash = entry.file_name().to_string_lossy().into_owned();
        let report = inspect_worktree(options, &worktree, item_hash, minter);
        append_report(options, &report)?;
        reports.push(report);
    }
    let summary = ReapWorktreesSummary {
        schema_version: 1,
        dry_run: !options.apply,
        scanned_count: reports.len(),
        candidate_count: reports
            .iter()
            .filter(|report| matches!(report.outcome.as_str(), "would-reap" | "reaped"))
            .count(),
        retained_count: reports
            .iter()
            .filter(|report| report.outcome == "retained")
            .count(),
        reaped_count: reports
            .iter()
            .filter(|report| report.outcome == "reaped")
            .count(),
        candidate_bytes: reports
            .iter()
            .filter(|report| matches!(report.outcome.as_str(), "would-reap" | "reaped"))
            .map(|report| report.bytes)
            .sum(),
        reclaimed_bytes: reports.iter().map(|report| report.reclaimed_bytes).sum(),
    };
    append_summary(options, &summary)?;
    Ok(ReapWorktreesOutcome { reports, summary })
}

fn inspect_worktree(
    options: &ReapWorktreesOptions,
    worktree: &Path,
    item_hash: String,
    minter: &mut dyn InstallationTokenMinter,
) -> WorktreeReapReport {
    let mut report = WorktreeReapReport {
        schema_version: 1,
        item_id: None,
        item_hash: item_hash.clone(),
        repository: None,
        branch_name: None,
        worktree_path: worktree.to_path_buf(),
        dry_run: !options.apply,
        outcome: "retained".to_owned(),
        reason: "work-order-unavailable".to_owned(),
        bytes: 0,
        reclaimed_bytes: 0,
    };
    report.bytes = match directory_bytes(worktree) {
        Ok(bytes) => bytes,
        Err(()) => {
            report.reason = "worktree-size-unreadable".to_owned();
            return report;
        }
    };
    let lease_path = options
        .paths
        .state
        .join(format!("implementer-item-{item_hash}.lease"));
    match read_lease(&lease_path) {
        Ok(Some(lease)) if lease.expires_at > options.clock.epoch_seconds() => {
            report.reason = "live-implementer-lease".to_owned();
            return report;
        }
        Ok(_) => {}
        Err(_) => {
            report.reason = "implementer-lease-unreadable".to_owned();
            return report;
        }
    }
    match worktree_status(worktree) {
        WorktreeStatus::Dirty => {
            report.reason = "dirty-worktree".to_owned();
            return report;
        }
        WorktreeStatus::Unreadable => {
            report.reason = "git-status-unreadable".to_owned();
            return report;
        }
        WorktreeStatus::Clean => {}
    }
    let order_path = options
        .paths
        .work_orders_dir()
        .join(format!("{item_hash}.json"));
    let Some(order) = fs::read(&order_path)
        .ok()
        .and_then(|bytes| WorkOrder::from_json(&bytes).ok())
        .filter(|order| order.item_hash() == item_hash)
    else {
        return report;
    };
    report.item_id = Some(order.item_id.clone());
    report.repository = Some(order.repository.clone());
    let branch = git_text(worktree, &["branch", "--show-current"])
        .filter(|branch| !branch.is_empty())
        .unwrap_or_else(|| order.branch_name.clone());
    report.branch_name = Some(branch.clone());

    let resolution = match resolution_state(options, &order, &branch, minter) {
        Ok(resolution) => resolution,
        Err(()) => {
            report.reason = "github-state-unreadable".to_owned();
            return report;
        }
    };
    let (reap_reason, published_tip) = match resolution {
        Resolution::Retain(reason) => {
            report.reason = reason.to_owned();
            return report;
        }
        Resolution::Reap(reason, tip) => (reason, tip),
    };
    report.reason = reap_reason.to_owned();
    if !options.apply {
        report.outcome = "would-reap".to_owned();
        return report;
    }
    let Some(source) = primary_worktree(worktree) else {
        report.reason = "worktree-source-unreadable".to_owned();
        return report;
    };
    match reclaim_worktree(&source, worktree, &branch, published_tip.as_deref()) {
        Ok(_) => {
            report.outcome = "reaped".to_owned();
            report.reclaimed_bytes = report.bytes;
        }
        Err(error) if error.starts_with(UNMERGED_LOCAL_COMMITS_PREFIX) => {
            report.reason = UNMERGED_LOCAL_COMMITS_REASON.to_owned();
        }
        Err(_) => report.reason = "worktree-removal-failed".to_owned(),
    }
    report
}

enum Resolution {
    Retain(&'static str),
    /// The reason to reap, plus the `headRefOid` GitHub last reported for the
    /// pull request that resolved this item -- the tip `reclaim_worktree`
    /// must find the branch to be an ancestor of before it is safe to
    /// destroy. `None` when no pull request exists to supply one at all: the
    /// content-safety guard in `reclaim_worktree` then has nothing to verify
    /// against and refuses on its own, independent of this resolution.
    Reap(&'static str, Option<String>),
}

/// One pull request's identity, state and last-known head commit, gathered
/// from either a `--head`-filtered listing or a specific `gh pr view`. Kept
/// as a plain record (not folded into a set of state strings) because
/// choosing which pull request's `headRefOid` to trust when several resolved
/// this item requires comparing `number`, not just states.
struct PullSummary {
    number: i64,
    state: String,
    head_ref_oid: Option<String>,
}

fn parse_pulls(value: &Value) -> Result<Vec<PullSummary>, ()> {
    let pulls = value.as_array().ok_or(())?;
    let mut summaries = Vec::with_capacity(pulls.len());
    for pull in pulls {
        let number = pull.get("number").and_then(Value::as_i64).ok_or(())?;
        let state = pull.get("state").and_then(Value::as_str).ok_or(())?;
        if !matches!(state, "OPEN" | "CLOSED" | "MERGED") {
            return Err(());
        }
        let head_ref_oid = pull
            .get("headRefOid")
            .and_then(Value::as_str)
            .map(str::to_owned);
        summaries.push(PullSummary {
            number,
            state: state.to_owned(),
            head_ref_oid,
        });
    }
    Ok(summaries)
}

fn resolution_state(
    options: &ReapWorktreesOptions,
    order: &WorkOrder,
    branch: &str,
    minter: &mut dyn InstallationTokenMinter,
) -> Result<Resolution, ()> {
    let matching_refs = gh_json(
        options,
        &order.repository,
        "metadata:read,contents:read",
        &[
            "gh",
            "api",
            &format!(
                "repos/{}/git/matching-refs/heads/{branch}",
                order.repository
            ),
        ],
        minter,
    )?;
    let refs = matching_refs.as_array().ok_or(())?;
    if refs.iter().any(|reference| {
        reference.get("ref").and_then(Value::as_str) == Some(&format!("refs/heads/{branch}"))
    }) {
        return Ok(Resolution::Retain("remote-branch-present"));
    }

    let branch_pulls = gh_json(
        options,
        &order.repository,
        "metadata:read,pull_requests:read",
        &[
            "gh",
            "pr",
            "list",
            "--repo",
            &order.repository,
            "--head",
            branch,
            "--state",
            "all",
            "--json",
            "number,state,url,headRefOid",
        ],
        minter,
    )?;
    let mut pulls = parse_pulls(&branch_pulls)?;
    let issue = gh_json(
        options,
        &order.repository,
        "metadata:read,issues:read,pull_requests:read",
        &[
            "gh",
            "issue",
            "view",
            &order.item_ref,
            "--repo",
            &order.repository,
            "--json",
            "state,closedByPullRequestsReferences",
        ],
        minter,
    )?;
    let item_state = issue.get("state").and_then(Value::as_str).ok_or(())?;
    if !matches!(item_state, "OPEN" | "CLOSED") {
        return Err(());
    }
    let references = issue
        .get("closedByPullRequestsReferences")
        .and_then(Value::as_array)
        .ok_or(())?;
    let mut urls = BTreeSet::new();
    for reference in references {
        let url = reference.get("url").and_then(Value::as_str).ok_or(())?;
        urls.insert(url.to_owned());
    }
    for url in urls {
        let pull = gh_json(
            options,
            &order.repository,
            "metadata:read,pull_requests:read",
            &[
                "gh",
                "pr",
                "view",
                &url,
                "--json",
                "number,state,url,headRefOid",
            ],
            minter,
        )?;
        if pull.get("url").and_then(Value::as_str) != Some(url.as_str()) {
            return Err(());
        }
        let number = pull.get("number").and_then(Value::as_i64).ok_or(())?;
        let state = pull.get("state").and_then(Value::as_str).ok_or(())?;
        if !matches!(state, "OPEN" | "CLOSED" | "MERGED") {
            return Err(());
        }
        let head_ref_oid = pull
            .get("headRefOid")
            .and_then(Value::as_str)
            .map(str::to_owned);
        pulls.push(PullSummary {
            number,
            state: state.to_owned(),
            head_ref_oid,
        });
    }
    let open_pulls = gh_json(
        options,
        &order.repository,
        "metadata:read,pull_requests:read",
        &[
            "gh",
            "pr",
            "list",
            "--repo",
            &order.repository,
            "--state",
            "open",
            "--limit",
            "1000",
            "--json",
            "number,title,body,url",
        ],
        minter,
    )?;
    let open_pulls = open_pulls.as_array().ok_or(())?;
    let text_matched_open = open_pulls.iter().any(|pull| {
        let text = format!(
            "{}\n{}",
            pull.get("title")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            pull.get("body").and_then(Value::as_str).unwrap_or_default()
        );
        text.contains(&order.item_id) || closing_reference(&text, &order.item_ref)
    });
    if text_matched_open || pulls.iter().any(|pull| pull.state == "OPEN") {
        return Ok(Resolution::Retain("pull-request-open"));
    }
    // Several pull requests can have resolved this item (a closing reference
    // plus its own `--head` match, or a history of superseded pull
    // requests); the newest one's `headRefOid` is the tip GitHub most
    // recently observed, so it is the one `reclaim_worktree` must check
    // ancestry against.
    if let Some(newest) = pulls
        .iter()
        .filter(|pull| matches!(pull.state.as_str(), "MERGED" | "CLOSED"))
        .max_by_key(|pull| pull.number)
    {
        return Ok(Resolution::Reap(
            "pull-request-resolved-remote-branch-absent",
            newest.head_ref_oid.clone(),
        ));
    }
    if item_state == "CLOSED" {
        return Ok(Resolution::Reap("item-closed-no-open-pull-request", None));
    }
    Ok(Resolution::Retain("item-open-no-resolved-pull-request"))
}

fn closing_reference(text: &str, item_ref: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let item_ref = item_ref.to_ascii_lowercase();
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
    .any(|verb| {
        let needle = format!("{verb}{item_ref}");
        let mut search_from = 0;
        while let Some(offset) = lower[search_from..].find(needle.as_str()) {
            let start = search_from + offset;
            let end = start + needle.len();
            // `fixes #7` must not match `fixes #712`: the reference is only a
            // match when nothing that could extend it (another digit) follows.
            let extends = lower[end..]
                .chars()
                .next()
                .is_some_and(|character| character.is_ascii_digit());
            if !extends {
                return true;
            }
            search_from = start + 1;
        }
        false
    })
}

/// The one `gh_json` for both this module and `dispatch.rs`: checks exit
/// status only. `gh` writes update notices and rate-limit warnings to stderr
/// on exit 0, so treating any stderr output as failure retained every
/// worktree as `github-state-unreadable` in exactly the environments this
/// reaper exists for.
pub(crate) fn gh_json_output(
    paths: &OstromPaths,
    scope: ScopedAppTokenRequest<'_>,
    command: &[&str],
    minter: &mut dyn InstallationTokenMinter,
) -> Result<Value, String> {
    let output =
        authenticated_output(paths, scope, command, minter).map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    serde_json::from_slice(&output.stdout).map_err(|error| error.to_string())
}

fn gh_json(
    options: &ReapWorktreesOptions,
    repository: &str,
    permissions: &str,
    command: &[&str],
    minter: &mut dyn InstallationTokenMinter,
) -> Result<Value, ()> {
    gh_json_output(
        &options.paths,
        ScopedAppTokenRequest::new("builder", repository, repository, permissions),
        command,
        minter,
    )
    .map_err(|_| ())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorktreeStatus {
    Clean,
    Dirty,
    Unreadable,
}

pub(crate) fn worktree_status(worktree: &Path) -> WorktreeStatus {
    let Ok(output) = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["status", "--porcelain"])
        .output()
    else {
        return WorktreeStatus::Unreadable;
    };
    if !output.status.success() {
        return WorktreeStatus::Unreadable;
    }
    if output.stdout.is_empty() {
        WorktreeStatus::Clean
    } else {
        WorktreeStatus::Dirty
    }
}

pub(crate) fn directory_bytes(path: &Path) -> Result<u64, ()> {
    let metadata = fs::symlink_metadata(path).map_err(|_| ())?;
    if !metadata.is_dir() {
        return Ok(metadata.len());
    }
    let mut bytes = metadata.len();
    for entry in fs::read_dir(path).map_err(|_| ())? {
        let entry = entry.map_err(|_| ())?;
        bytes = bytes.saturating_add(directory_bytes(&entry.path())?);
    }
    Ok(bytes)
}

fn primary_worktree(worktree: &Path) -> Option<PathBuf> {
    let common = git_text(
        worktree,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    Path::new(&common).parent().map(Path::to_path_buf)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReclaimedWorktree {
    pub bytes: u64,
    pub branch_count: usize,
}

/// `reclaim_worktree`'s error message starts with this when the branch has
/// commits the default branch does not, so a caller can tell "would destroy
/// unpushed work" apart from every other failure without inventing a second
/// error type across the crate boundary this function is shared over.
pub(crate) const UNMERGED_LOCAL_COMMITS_PREFIX: &str = "unmerged-local-commits:";
pub(crate) const UNMERGED_LOCAL_COMMITS_REASON: &str = "unmerged-local-commits";

pub(crate) fn reclaim_worktree(
    source: &Path,
    worktree: &Path,
    expected_branch: &str,
    published_tip: Option<&str>,
) -> Result<ReclaimedWorktree, String> {
    let bytes = if worktree.exists() {
        directory_bytes(worktree).map_err(|()| "worktree size is unreadable".to_owned())?
    } else {
        0
    };
    let mut branches = BTreeSet::new();
    if worktree.exists() {
        match worktree_status(worktree) {
            WorktreeStatus::Clean => {}
            WorktreeStatus::Dirty => return Err("worktree has uncommitted changes".to_owned()),
            WorktreeStatus::Unreadable => return Err("git status is unreadable".to_owned()),
        }
        if let Some(branch) =
            git_text(worktree, &["branch", "--show-current"]).filter(|branch| !branch.is_empty())
        {
            branches.insert(branch);
        }
    }
    branches.insert(expected_branch.to_owned());

    // `worktree_status` above only catches uncommitted changes. A worktree
    // can be clean and still hold commits that were never pushed -- an
    // implementer committed after its last push, then was killed or timed
    // out. Refuse before anything is removed: `git branch -D` below would
    // otherwise make those commits unreachable, from both this function's
    // callers (`inspect_worktree` and `reclaim_merged_worktree`).
    for branch in &branches {
        if !local_branch_exists(source, branch) {
            continue;
        }
        match branch_tip_is_published(source, branch, published_tip) {
            Ok(true) => {}
            Ok(false) => {
                return Err(format!(
                    "{UNMERGED_LOCAL_COMMITS_PREFIX} branch {branch} carries commits the published tip does not"
                ));
            }
            Err(()) => {
                return Err(format!(
                    "{UNMERGED_LOCAL_COMMITS_PREFIX} could not verify branch {branch} against a published tip"
                ));
            }
        }
    }

    if worktree.exists() {
        let output = Command::new("git")
            .arg("-C")
            .arg(source)
            .arg("worktree")
            .arg("remove")
            .arg(worktree)
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
        }
    }
    let mut branch_count = 0;
    for branch in branches {
        if !local_branch_exists(source, &branch) {
            continue;
        }
        let output = Command::new("git")
            .arg("-C")
            .arg(source)
            .args(["branch", "-D", "--"])
            .arg(&branch)
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
        }
        branch_count += 1;
    }
    Ok(ReclaimedWorktree {
        bytes,
        branch_count,
    })
}

/// Whether `branch`'s tip in `source` is an ancestor of, or equal to,
/// `published_tip` -- the last commit GitHub reported observing on this
/// branch (a pull request's `headRefOid`). `Ok(true)` proves GitHub saw
/// everything on `branch`: this holds even across a squash merge, which
/// gives the default branch an unrelated new SHA, because `published_tip` is
/// the branch's own pre-squash tip and an unmodified branch is always
/// exactly equal to it -- unlike comparing against the default branch's
/// current tip, which drifts as the default branch moves for unrelated
/// reasons. `Ok(false)` means `branch` is a strict descendant carrying
/// commits GitHub never saw, or its history is unrelated to `published_tip`
/// altogether. `Err(())` covers everything that cannot be told apart from
/// unsafe: no `published_tip` at all (no pull request resolved this item, or
/// the one that did carried no `headRefOid`), or a `published_tip` this
/// repository has no record of. The caller must refuse in every case except
/// the first.
fn branch_tip_is_published(
    source: &Path,
    branch: &str,
    published_tip: Option<&str>,
) -> Result<bool, ()> {
    let published_tip = published_tip.ok_or(())?;
    let output = Command::new("git")
        .arg("-C")
        .arg(source)
        .args(["merge-base", "--is-ancestor", branch, published_tip])
        .output()
        .map_err(|_| ())?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(()),
    }
}

fn local_branch_exists(source: &Path, branch: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(source)
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .status()
        .is_ok_and(|status| status.success())
}

fn git_text(path: &Path, arguments: &[&str]) -> Option<String> {
    let output: Output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(arguments)
        .output()
        .ok()?;
    output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_owned()
    })
}

fn append_report(
    options: &ReapWorktreesOptions,
    report: &WorktreeReapReport,
) -> Result<(), ReapWorktreesError> {
    let kind = if report.outcome == "retained" {
        "worktree-retained"
    } else {
        "worktree-reaped"
    };
    let fact = serde_json::to_value(report)
        .expect("reap report serializes")
        .as_object()
        .expect("reap report is an object")
        .clone();
    append_fact(options, kind, fact)
}

fn append_summary(
    options: &ReapWorktreesOptions,
    summary: &ReapWorktreesSummary,
) -> Result<(), ReapWorktreesError> {
    let fact = serde_json::to_value(summary)
        .expect("reap summary serializes")
        .as_object()
        .expect("reap summary is an object")
        .clone();
    append_fact(options, "worktree-reap-completed", fact)
}

fn append_fact(
    options: &ReapWorktreesOptions,
    kind: &str,
    fact: Map<String, Value>,
) -> Result<(), ReapWorktreesError> {
    append_trace(
        &options.paths.trace_file(),
        &TraceAppend {
            ts: options.clock.timestamp(),
            kind: kind.to_owned(),
            fact,
            narration: Map::new(),
        },
    )
    .map(|_| ())
    .map_err(|error| ReapWorktreesError::Trace(error.to_string()))
}
