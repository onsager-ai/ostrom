//! Bounded lifecycle management for implementer worktrees.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use chrono::DateTime;
use ostrom_core::WorkOrder;
use thiserror::Error;

use crate::{Clock, environment, read_trace};

pub const DEFAULT_WORKTREE_RETENTION_DAYS: u64 = 7;
pub const DEFAULT_WORKTREE_CEILING_BYTES: u64 = 20 * 1024 * 1024 * 1024;
const SECONDS_PER_DAY: u64 = 24 * 60 * 60;

#[derive(Debug, Error)]
pub enum WorktreeError {
    #[error("{0} must be a positive integer")]
    InvalidEnvironment(&'static str),
    #[error("could not inspect worktree path {path}: {source}")]
    Inspect {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("refusing to remove worktree path outside {root}: {path}")]
    OutsideRoot { root: String, path: String },
    #[error("could not remove registered worktree {path}: {detail}")]
    GitRemove { path: String, detail: String },
    #[error("could not inspect git worktree registry for {path}: {detail}")]
    GitRegistry { path: String, detail: String },
    #[error("could not remove worktree path {path}: {source}")]
    Remove {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeRemovalReason {
    Closed,
    Orphan,
}

impl WorktreeRemovalReason {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Orphan => "orphan",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeRemoval {
    pub path: PathBuf,
    pub reason: WorktreeRemovalReason,
}

impl std::fmt::Display for WorktreeRemoval {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "removed {} implementer worktree {}",
            self.reason.as_str(),
            self.path.display()
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorktreeSweep {
    pub removals: Vec<WorktreeRemoval>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorktreeFootprint {
    pub count: usize,
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy)]
enum OrderState {
    Open,
    Closed(u64),
}

#[must_use]
pub fn worktree_root(state_root: &Path) -> PathBuf {
    state_root.join("implementer-worktrees")
}

pub fn configured_retention_days() -> Result<u64, WorktreeError> {
    positive_environment(
        environment::MANDATE_WORKTREE_RETENTION_DAYS,
        DEFAULT_WORKTREE_RETENTION_DAYS,
    )
}

pub fn configured_ceiling_bytes() -> Result<u64, WorktreeError> {
    positive_environment(
        environment::MANDATE_WORKTREE_CEILING_BYTES,
        DEFAULT_WORKTREE_CEILING_BYTES,
    )
}

fn positive_environment(
    variable: environment::EnvironmentVariable,
    default: u64,
) -> Result<u64, WorktreeError> {
    let Some(value) = variable.value() else {
        return Ok(default);
    };
    value
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or(WorktreeError::InvalidEnvironment(variable.name))
}

/// Remove the per-order build cache while retaining its branch and sources.
pub fn reap_build_cache(state_root: &Path, item_id: &str) -> Result<bool, WorktreeError> {
    let root = worktree_root(state_root);
    let target = root
        .join(crate::work_order::item_hash(item_id))
        .join("target");
    remove_confined(&root, &target)
}

/// Reconcile the implementer directory and expire closed orders.
pub fn sweep_worktrees(
    state_root: &Path,
    clock: &Clock,
    retention_days: u64,
) -> Result<WorktreeSweep, WorktreeError> {
    let root = worktree_root(state_root);
    if !root.exists() {
        return Ok(WorktreeSweep::default());
    }
    let states = order_states(state_root);
    let retention_seconds = retention_days.saturating_mul(SECONDS_PER_DAY);
    let now = clock.epoch_seconds();
    let mut paths = fs::read_dir(&root)
        .map_err(|source| inspect_error(&root, source))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            fs::symlink_metadata(path)
                .is_ok_and(|metadata| metadata.is_dir() || metadata.file_type().is_symlink())
        })
        .collect::<Vec<_>>();
    paths.sort();

    let mut removals = Vec::new();
    for path in paths {
        confined_existing_path(&root, &path)?;
        if !git_registry_contains(&path)? {
            remove_confined(&root, &path)?;
            removals.push(WorktreeRemoval {
                path,
                reason: WorktreeRemovalReason::Orphan,
            });
            continue;
        }
        let Some(hash) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(OrderState::Closed(closed_at)) = states.get(hash) else {
            continue;
        };
        if now <= closed_at.saturating_add(retention_seconds) {
            continue;
        }
        remove_registered_worktree(&root, &path)?;
        removals.push(WorktreeRemoval {
            path,
            reason: WorktreeRemovalReason::Closed,
        });
    }
    Ok(WorktreeSweep { removals })
}

pub fn worktree_footprint(root: &Path) -> Result<WorktreeFootprint, WorktreeError> {
    if !root.exists() {
        return Ok(WorktreeFootprint::default());
    }
    let mut footprint = WorktreeFootprint::default();
    for entry in fs::read_dir(root).map_err(|source| inspect_error(root, source))? {
        let entry = entry.map_err(|source| inspect_error(root, source))?;
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|source| inspect_error(&entry.path(), source))?;
        if metadata.is_dir() {
            footprint.count += 1;
            footprint.bytes = footprint.bytes.saturating_add(path_bytes(&entry.path())?);
        }
    }
    Ok(footprint)
}

fn path_bytes(path: &Path) -> Result<u64, WorktreeError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| inspect_error(path, source))?;
    if !metadata.is_dir() {
        return Ok(metadata.len());
    }
    let mut bytes = metadata.len();
    for entry in fs::read_dir(path).map_err(|source| inspect_error(path, source))? {
        let entry = entry.map_err(|source| inspect_error(path, source))?;
        bytes = bytes.saturating_add(path_bytes(&entry.path())?);
    }
    Ok(bytes)
}

fn order_states(state_root: &Path) -> BTreeMap<String, OrderState> {
    let rows = read_trace(&state_root.join("sprint.jsonl"))
        .map(|trace| {
            trace
                .rows
                .into_iter()
                .filter_map(Result::ok)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let terminal_ids = rows
        .iter()
        .filter(|row| matches!(row.kind.as_str(), "work-completed" | "work-failed"))
        .filter_map(|row| row.fact.get("order_id")?.as_str())
        .collect::<BTreeSet<_>>();
    let mut open_items = BTreeSet::new();
    let mut closed_items = BTreeMap::new();
    for row in &rows {
        let Some(item_id) = row.fact.get("item_id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(order_id) = row.fact.get("order_id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if row.kind == "work-dispatched" && !terminal_ids.contains(order_id) {
            open_items.insert(item_id.to_owned());
        } else if matches!(row.kind.as_str(), "work-completed" | "work-failed")
            && let Some(epoch) = timestamp_epoch(&row.ts)
        {
            closed_items
                .entry(item_id.to_owned())
                .and_modify(|current: &mut u64| *current = (*current).max(epoch))
                .or_insert(epoch);
        }
    }

    let orders_dir = state_root.join("work-orders");
    if let Ok(entries) = fs::read_dir(orders_dir) {
        for entry in entries.filter_map(Result::ok) {
            let Some(order) = fs::read(entry.path())
                .ok()
                .and_then(|bytes| WorkOrder::from_json(&bytes).ok())
            else {
                continue;
            };
            if !terminal_ids.contains(order.order_id.as_str()) {
                open_items.insert(order.item_id);
            }
        }
    }

    let mut states = closed_items
        .into_iter()
        .map(|(item_id, closed_at)| {
            (
                crate::work_order::item_hash(&item_id),
                OrderState::Closed(closed_at),
            )
        })
        .collect::<BTreeMap<_, _>>();
    for item_id in open_items {
        states.insert(crate::work_order::item_hash(&item_id), OrderState::Open);
    }
    states
}

fn timestamp_epoch(value: &str) -> Option<u64> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .and_then(|timestamp| u64::try_from(timestamp.timestamp()).ok())
}

fn git_registry_contains(path: &Path) -> Result<bool, WorktreeError> {
    if fs::symlink_metadata(path.join(".git")).is_err() {
        return Ok(false);
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .map_err(|source| inspect_error(path, source))?;
    if !output.status.success() {
        if linked_git_directory(path).is_some_and(|git_directory| !git_directory.exists()) {
            return Ok(false);
        }
        return Err(WorktreeError::GitRegistry {
            path: path.display().to_string(),
            detail: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    let expected = fs::canonicalize(path).ok();
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .any(|registered| {
            expected.as_deref().is_some_and(|expected| {
                fs::canonicalize(registered)
                    .ok()
                    .is_some_and(|registered| registered == expected)
            })
        }))
}

fn linked_git_directory(path: &Path) -> Option<PathBuf> {
    let source = fs::read_to_string(path.join(".git")).ok()?;
    let git_directory = source.trim().strip_prefix("gitdir: ")?;
    let git_directory = PathBuf::from(git_directory);
    Some(if git_directory.is_absolute() {
        git_directory
    } else {
        path.join(git_directory)
    })
}

fn remove_registered_worktree(root: &Path, path: &Path) -> Result<(), WorktreeError> {
    confined_existing_path(root, path)?;
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["worktree", "remove", "--force"])
        .arg(path)
        .output()
        .map_err(|source| inspect_error(path, source))?;
    if !output.status.success() {
        return Err(WorktreeError::GitRemove {
            path: path.display().to_string(),
            detail: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(())
}

fn remove_confined(root: &Path, path: &Path) -> Result<bool, WorktreeError> {
    if !path.exists() {
        return Ok(false);
    }
    let path = confined_existing_path(root, path)?;
    let metadata = fs::symlink_metadata(&path).map_err(|source| inspect_error(&path, source))?;
    let result = if metadata.is_dir() {
        fs::remove_dir_all(&path)
    } else {
        fs::remove_file(&path)
    };
    result.map_err(|source| WorktreeError::Remove {
        path: path.display().to_string(),
        source,
    })?;
    Ok(true)
}

fn confined_existing_path(root: &Path, path: &Path) -> Result<PathBuf, WorktreeError> {
    let canonical_root = fs::canonicalize(root).map_err(|source| inspect_error(root, source))?;
    let metadata = fs::symlink_metadata(path).map_err(|source| inspect_error(path, source))?;
    if metadata.file_type().is_symlink() {
        return Err(outside_root(&canonical_root, path));
    }
    let canonical_path = fs::canonicalize(path).map_err(|source| inspect_error(path, source))?;
    if canonical_path == canonical_root || !canonical_path.starts_with(&canonical_root) {
        return Err(outside_root(&canonical_root, &canonical_path));
    }
    Ok(canonical_path)
}

fn inspect_error(path: &Path, source: std::io::Error) -> WorktreeError {
    WorktreeError::Inspect {
        path: path.display().to_string(),
        source,
    }
}

fn outside_root(root: &Path, path: &Path) -> WorktreeError {
    WorktreeError::OutsideRoot {
        root: root.display().to_string(),
        path: path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, fs, path::Path, process::Command};

    use chrono::{DateTime, Utc};
    use serde_json::json;
    use tempfile::TempDir;

    use super::{
        WorktreeError, WorktreeRemovalReason, remove_confined, sweep_worktrees, worktree_root,
    };
    use crate::{Clock, work_order::item_hash};

    const ITEM_ID: &str = "placeholder-org/alpha#42";

    struct Fixture {
        _root: TempDir,
        state: std::path::PathBuf,
        source: std::path::PathBuf,
        worktree: std::path::PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().expect("temporary worktree fixture");
            let state = root.path().join("state");
            let source = root.path().join("source");
            let worktree = worktree_root(&state).join(item_hash(ITEM_ID));
            fs::create_dir_all(&source).expect("create source repository");
            git(&source, &["init", "-b", "main"]);
            git(
                &source,
                &["config", "user.email", "fixture@example.invalid"],
            );
            git(&source, &["config", "user.name", "Fixture"]);
            fs::write(source.join("README.md"), "fixture\n").expect("write fixture");
            git(&source, &["add", "README.md"]);
            git(&source, &["commit", "-m", "base"]);
            fs::create_dir_all(worktree.parent().expect("worktree root"))
                .expect("create worktree root");
            git(
                &source,
                &[
                    "worktree",
                    "add",
                    "-b",
                    "fixture/order",
                    worktree.to_str().expect("UTF-8 worktree"),
                    "main",
                ],
            );
            Self {
                _root: root,
                state,
                source,
                worktree,
            }
        }

        fn write_trace(&self, rows: &[serde_json::Value]) {
            let source = rows
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            fs::write(self.state.join("sprint.jsonl"), format!("{source}\n"))
                .expect("write trace fixture");
        }
    }

    fn clock() -> Clock {
        Clock::fixed(
            DateTime::parse_from_rfc3339("2026-08-20T00:00:00Z")
                .expect("fixed clock")
                .with_timezone(&Utc),
        )
    }

    fn dispatch() -> serde_json::Value {
        trace_row("2026-08-01T00:00:00Z", "work-dispatched")
    }

    fn terminal() -> serde_json::Value {
        trace_row("2026-08-02T00:00:00Z", "work-completed")
    }

    fn trace_row(timestamp: &str, kind: &str) -> serde_json::Value {
        json!({
            "ts": timestamp,
            "kind": kind,
            "fact": {
                "item_id": ITEM_ID,
                "order_id": "placeholder-order"
            },
            "narration": {}
        })
    }

    fn git(path: &Path, arguments: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(arguments)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn registered_under(source: &Path, root: &Path) -> BTreeSet<std::path::PathBuf> {
        let output = Command::new("git")
            .arg("-C")
            .arg(source)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .expect("list worktrees");
        assert!(output.status.success());
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.strip_prefix("worktree "))
            .map(std::path::PathBuf::from)
            .filter(|path| path.starts_with(root))
            .collect()
    }

    fn directories_under(root: &Path) -> BTreeSet<std::path::PathBuf> {
        fs::read_dir(root)
            .expect("read worktree root")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect()
    }

    #[test]
    fn an_old_open_order_is_never_reaped() {
        let fixture = Fixture::new();
        fixture.write_trace(&[dispatch()]);

        let sweep = sweep_worktrees(&fixture.state, &clock(), 1).expect("sweep open order");

        assert!(sweep.removals.is_empty());
        assert!(fixture.worktree.is_dir());
        assert!(
            registered_under(&fixture.source, &worktree_root(&fixture.state))
                .contains(&fixture.worktree)
        );
    }

    #[test]
    fn a_closed_order_is_removed_through_git_after_retention() {
        let fixture = Fixture::new();
        fixture.write_trace(&[dispatch(), terminal()]);

        let sweep = sweep_worktrees(&fixture.state, &clock(), 7).expect("sweep closed order");

        assert_eq!(sweep.removals.len(), 1);
        assert_eq!(sweep.removals[0].reason, WorktreeRemovalReason::Closed);
        assert!(!fixture.worktree.exists());
        assert!(registered_under(&fixture.source, &worktree_root(&fixture.state)).is_empty());
    }

    #[test]
    fn orphan_reconciliation_is_reported_agrees_with_git_and_is_idempotent() {
        let fixture = Fixture::new();
        fixture.write_trace(&[dispatch()]);
        let orphan = worktree_root(&fixture.state).join("orphan-placeholder");
        fs::create_dir(&orphan).expect("create orphan");
        fs::write(orphan.join("artifact"), "placeholder").expect("write orphan artifact");

        let first = sweep_worktrees(&fixture.state, &clock(), 7).expect("first sweep");

        assert_eq!(first.removals.len(), 1);
        assert_eq!(first.removals[0].reason, WorktreeRemovalReason::Orphan);
        let rendered = first.removals[0].to_string();
        assert!(rendered.contains("removed orphan implementer worktree"));
        assert!(rendered.contains(orphan.to_str().expect("UTF-8 orphan path")));
        assert!(!orphan.exists());
        assert_eq!(
            directories_under(&worktree_root(&fixture.state)),
            registered_under(&fixture.source, &worktree_root(&fixture.state))
        );

        let second = sweep_worktrees(&fixture.state, &clock(), 7).expect("second sweep");
        assert!(second.removals.is_empty());
    }

    #[test]
    fn a_crafted_path_outside_the_worktree_root_is_refused() {
        let fixture = tempfile::tempdir().expect("temporary safety fixture");
        let root = fixture.path().join("implementer-worktrees");
        let outside = fixture.path().join("outside");
        fs::create_dir(&root).expect("create worktree root");
        fs::create_dir(&outside).expect("create outside directory");
        fs::write(outside.join("keep"), "must remain").expect("write outside sentinel");

        let error = remove_confined(&root, &outside).expect_err("outside removal must fail");

        assert!(matches!(error, WorktreeError::OutsideRoot { .. }));
        assert!(outside.join("keep").is_file());
    }
}
