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
    let reap_reason = match resolution {
        Resolution::Retain(reason) => {
            report.reason = reason.to_owned();
            return report;
        }
        Resolution::Reap(reason) => reason,
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
    match reclaim_worktree(
        &options.paths,
        &order.repository,
        &source,
        worktree,
        &branch,
        minter,
    ) {
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
    Reap(&'static str),
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
            "number,state,url",
        ],
        minter,
    )?;
    let mut pull_states = pull_states(&branch_pulls)?;
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
            &["gh", "pr", "view", &url, "--json", "state,url"],
            minter,
        )?;
        if pull.get("url").and_then(Value::as_str) != Some(url.as_str()) {
            return Err(());
        }
        let state = pull.get("state").and_then(Value::as_str).ok_or(())?;
        if !matches!(state, "OPEN" | "CLOSED" | "MERGED") {
            return Err(());
        }
        pull_states.insert(state.to_owned());
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
    if open_pulls.iter().any(|pull| {
        let text = format!(
            "{}\n{}",
            pull.get("title")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            pull.get("body").and_then(Value::as_str).unwrap_or_default()
        );
        text.contains(&order.item_id) || closing_reference(&text, &order.item_ref)
    }) {
        pull_states.insert("OPEN".to_owned());
    }
    if pull_states.contains("OPEN") {
        return Ok(Resolution::Retain("pull-request-open"));
    }
    if pull_states.contains("MERGED") || pull_states.contains("CLOSED") {
        return Ok(Resolution::Reap(
            "pull-request-resolved-remote-branch-absent",
        ));
    }
    if item_state == "CLOSED" {
        return Ok(Resolution::Reap("item-closed-no-open-pull-request"));
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

fn pull_states(value: &Value) -> Result<BTreeSet<String>, ()> {
    let pulls = value.as_array().ok_or(())?;
    let mut states = BTreeSet::new();
    for pull in pulls {
        let state = pull.get("state").and_then(Value::as_str).ok_or(())?;
        if !matches!(state, "OPEN" | "CLOSED" | "MERGED") {
            return Err(());
        }
        states.insert(state.to_owned());
    }
    Ok(states)
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

#[allow(clippy::too_many_arguments)]
pub(crate) fn reclaim_worktree(
    paths: &OstromPaths,
    repository: &str,
    source: &Path,
    worktree: &Path,
    expected_branch: &str,
    minter: &mut dyn InstallationTokenMinter,
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
        match branch_has_unpublished_tree(paths, repository, source, branch, minter) {
            Ok(false) => {}
            Ok(true) => {
                return Err(format!(
                    "{UNMERGED_LOCAL_COMMITS_PREFIX} branch {branch} has content not on the default branch"
                ));
            }
            Err(()) => {
                return Err(format!(
                    "{UNMERGED_LOCAL_COMMITS_PREFIX} could not verify branch {branch} is fully merged into the default branch"
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

/// Whether `branch` carries a tree the repository's default branch does not.
/// `Ok(false)` proves the branch is fully merged; anything else means the
/// caller must not delete `branch`.
///
/// This compares trees, not commit ancestry: a squash merge creates a new
/// commit on the default branch with a different SHA, so a squash-merged
/// branch's original commits are never ancestors of it and `rev-list --count`
/// would report it "ahead" forever, refusing every reclaim including the one
/// this guard exists to allow. Comparing trees the way `implement.rs`'s
/// `has_unpublished_tree` already does lets a squash-merged branch whose
/// content fully landed be told apart from one carrying genuinely unpublished
/// work.
fn branch_has_unpublished_tree(
    paths: &OstromPaths,
    repository: &str,
    source: &Path,
    branch: &str,
    minter: &mut dyn InstallationTokenMinter,
) -> Result<bool, ()> {
    // Refresh the default branch's tip first: in the merged case the remote
    // copy of `branch` may already be gone, so its own upstream is not
    // available to compare against the way `leaves.rs` does for a live
    // branch -- the repository's default branch always is, but a source
    // repository that has not fetched recently must not compare against a
    // stale tree. Go through the same authenticated channel as every other
    // GitHub network call here: a plain `git fetch origin` would depend on
    // ambient credentials this process does not carry.
    if let Some(default_branch) = gh_json_output(
        paths,
        ScopedAppTokenRequest::new("builder", repository, repository, "metadata:read"),
        &[
            "gh",
            "repo",
            "view",
            repository,
            "--json",
            "defaultBranchRef",
        ],
        minter,
    )
    .ok()
    .and_then(|value| {
        value
            .get("defaultBranchRef")
            .and_then(|reference| reference.get("name"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    }) {
        let remote = format!("https://github.com/{repository}.git");
        let refspec = format!("{default_branch}:refs/remotes/origin/{default_branch}");
        let _ = authenticated_output(
            paths,
            ScopedAppTokenRequest::new(
                "builder",
                repository,
                repository,
                "metadata:read,contents:read",
            ),
            &[
                "git",
                "-C",
                &source.display().to_string(),
                "fetch",
                "--quiet",
                &remote,
                &refspec,
            ],
            minter,
        );
    }
    // Best-effort above: a solo operator's source repository may have no
    // configured GitHub App, or the fetch may fail for reasons that do not
    // bear on whether `branch` is merged. Fall back to whatever the source
    // repository's own refs already show either way.
    let default_ref = default_branch_reference(source).ok_or(())?;
    // Compare trees directly in `source`, not a worktree's `HEAD`: the
    // branch's worktree may already be gone (the merged case this guard
    // protects), and `source` carries the branch's commits regardless.
    let status = Command::new("git")
        .arg("-C")
        .arg(source)
        .args(["diff", "--quiet", &default_ref, branch])
        .status()
        .map_err(|_| ())?;
    match status.code() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => Err(()),
    }
}

fn default_branch_reference(source: &Path) -> Option<String> {
    let symbolic = git_text(source, &["symbolic-ref", "-q", "refs/remotes/origin/HEAD"]);
    if symbolic
        .as_deref()
        .is_some_and(|reference| verified_commit(source, reference))
    {
        return symbolic;
    }
    for candidate in [
        "refs/remotes/origin/main",
        "refs/remotes/origin/master",
        "refs/heads/main",
        "refs/heads/master",
    ] {
        if verified_commit(source, candidate) {
            return Some(candidate.to_owned());
        }
    }
    None
}

fn verified_commit(source: &Path, reference: &str) -> bool {
    git_text(
        source,
        &["rev-parse", "--verify", &format!("{reference}^{{commit}}")],
    )
    .is_some()
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
