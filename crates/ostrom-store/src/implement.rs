//! Execute one durable work order in a dedicated worktree.
//!
//! The process is intentionally not wall-clock bounded: systemd owns its
//! lifecycle while the order's reservation and weighted-token ceiling bound
//! spend. Codex edits offline; authenticated fetch, publish, and PR operations
//! remain outside its sandbox.

use std::{
    env, fs,
    fs::File,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Output, Stdio},
    thread,
    time::Duration,
};

use chrono::{DateTime, Utc};
use ostrom_core::{MandateConfig, WorkOrder};
use serde_json::{Map, Value, json};
use thiserror::Error;

use crate::{
    Clock, LeaseActionError, OstromPaths, OwnedLease, SignalFlags, TraceAppend,
    app_token::{
        AuthenticatedCommandError, GitHubInstallationTokenMinter, InstallationTokenMinter,
        ScopedAppTokenRequest, authenticated_output,
    },
    append_trace, environment, load_config_or_defaults,
};

#[derive(Debug, Clone)]
pub struct ImplementRequest {
    pub paths: OstromPaths,
    pub working_directory: PathBuf,
    pub plugin_root: PathBuf,
    pub order_file: PathBuf,
    pub unit_name: String,
    pub signals: SignalFlags,
    pub supervisor_pid: Option<u32>,
    pub clock: Clock,
}

#[derive(Debug, Error)]
#[error("ostrom implementer: {message}")]
pub struct ImplementError {
    pub code: i32,
    pub reason: String,
    pub message: String,
}

impl ImplementError {
    fn new(code: i32, reason: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code,
            reason: reason.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Usage {
    input_tokens: u64,
    fresh_input_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    output_tokens: u64,
    reasoning_output_tokens: u64,
    cached_input_tokens_available: bool,
}

impl Usage {
    fn weighted(&self) -> u64 {
        // Cached GPT-5-Codex input costs one tenth of fresh input, so the
        // established 0.2 fresh-input weight becomes 0.02 for cached input.
        // Missing cache accounting takes the conservative all-fresh path.
        let input = if self.cached_input_tokens_available {
            self.fresh_input_tokens.unwrap_or_default() as f64 * 0.2
                + self.cached_input_tokens.unwrap_or_default() as f64 * 0.02
        } else {
            self.input_tokens as f64 * 0.2
        };
        (input + self.output_tokens as f64).ceil() as u64
    }

    fn json(&self) -> Value {
        json!({
            "input_tokens": self.input_tokens,
            "fresh_input_tokens": self.fresh_input_tokens,
            "cached_input_tokens": self.cached_input_tokens,
            "output_tokens": self.output_tokens,
            "reasoning_output_tokens": self.reasoning_output_tokens,
            "cached_input_tokens_available": self.cached_input_tokens_available,
        })
    }
}

struct TerminalGuard {
    paths: OstromPaths,
    lease: OwnedLease,
    order: WorkOrder,
    unit_name: String,
    backend: String,
    started: DateTime<Utc>,
    clock: Clock,
    failure_reason: String,
    failure_message: Option<String>,
    terminal_written: bool,
    worktree: Option<PathBuf>,
    source_repository: Option<PathBuf>,
    default_branch: Option<String>,
    events_file: Option<PathBuf>,
    termination_signal: Option<String>,
    pr_url: Option<String>,
    remote_head_sha: Option<String>,
    conflicted_paths: Vec<String>,
    withheld_paths: Vec<String>,
}

impl TerminalGuard {
    fn append_terminal(&mut self, kind: &str, reason: Option<&str>) -> Result<(), ImplementError> {
        let duration = self
            .clock
            .now()
            .signed_duration_since(self.started)
            .num_seconds()
            .max(0);
        let usage = self
            .events_file
            .as_deref()
            .map(read_usage)
            .unwrap_or_default();
        let weighted = usage.weighted();
        // This is Ostrom's normalized order-cost estimate, not a provider
        // invoice. Keeping it numeric on every terminal row lets completed
        // work replace its in-flight reservation in the daily-cap total.
        let cost = weighted as f64 / self.order.tokens() as f64 * self.order.cost();
        // A retry can turn an expensive partial edit into a cheap completion,
        // so failed worktrees remain addressable even when the child stopped
        // before its first commit.
        let preserved = if kind == "work-failed" {
            self.worktree.as_ref().and_then(|path| {
                let default_ref = self
                    .default_branch
                    .as_ref()
                    .map(|branch| format!("refs/remotes/origin/{branch}"));
                (path.exists()
                    && (working_tree_dirty(path)
                        || default_ref
                            .as_deref()
                            .is_some_and(|reference| has_unpublished_tree(path, reference))))
                .then(|| path.display().to_string())
            })
        } else {
            None
        };
        let branch = preserved.as_ref().map(|_| self.order.branch_name.clone());
        let fact = Map::from_iter([
            ("schema_version".to_owned(), json!(1)),
            ("item_id".to_owned(), json!(self.order.item_id)),
            ("order_id".to_owned(), json!(self.order.order_id)),
            ("unit_name".to_owned(), json!(self.unit_name)),
            ("backend".to_owned(), json!(self.backend)),
            (
                "cost_ceiling_usd".to_owned(),
                self.order.cost_ceiling_usd.clone(),
            ),
            ("token_ceiling".to_owned(), self.order.token_ceiling.clone()),
            ("weighted_tokens".to_owned(), json!(weighted)),
            ("cost_usd".to_owned(), json!(cost)),
            ("duration_seconds".to_owned(), json!(duration)),
            ("pr_url".to_owned(), json!(self.pr_url)),
            (
                "reason".to_owned(),
                reason.map_or(Value::Null, |value| json!(value)),
            ),
            ("message".to_owned(), json!(self.failure_message)),
            (
                "termination_signal".to_owned(),
                json!(self.termination_signal),
            ),
            (
                "source_repository_path".to_owned(),
                json!(
                    self.source_repository
                        .as_ref()
                        .map(|path| path.display().to_string())
                ),
            ),
            ("worktree_path".to_owned(), json!(preserved)),
            ("branch_name".to_owned(), json!(branch)),
            ("remote_head_sha".to_owned(), json!(self.remote_head_sha)),
            ("conflicted_paths".to_owned(), json!(self.conflicted_paths)),
            ("withheld_paths".to_owned(), json!(self.withheld_paths)),
            ("usage".to_owned(), usage.json()),
        ]);
        append_trace(
            &self.paths.trace_file(),
            &TraceAppend {
                ts: self.clock.timestamp(),
                kind: kind.to_owned(),
                fact,
                narration: Map::new(),
            },
        )
        .map_err(|error| {
            ImplementError::new(
                1,
                "terminal-trace-failed",
                format!("could not append {kind}: {error}"),
            )
        })?;
        self.terminal_written = true;
        Ok(())
    }

    fn fail(&mut self, error: &ImplementError) {
        self.failure_reason.clone_from(&error.reason);
        self.failure_message = Some(error.message.clone());
    }

    fn finish(&mut self) -> Result<(), ImplementError> {
        let mut failure = None;
        if !self.terminal_written {
            let reason = self.failure_reason.clone();
            if let Err(error) = self.append_terminal("work-failed", Some(&reason)) {
                failure = Some(error);
            }
        }
        if self.lease.release().is_err() && failure.is_none() {
            failure = Some(ImplementError::new(
                1,
                "lease-release-failed",
                "could not release implementer lease",
            ));
        }
        failure.map_or(Ok(()), Err)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

pub fn run_implement(request: &ImplementRequest) -> Result<String, ImplementError> {
    let mut minter = GitHubInstallationTokenMinter;
    run_implement_with_minter(request, &mut minter)
}

fn run_implement_with_minter(
    request: &ImplementRequest,
    minter: &mut dyn InstallationTokenMinter,
) -> Result<String, ImplementError> {
    // Dispatch passes the exact lease name so cleanup can be armed before the
    // work order is even readable. Without this handoff, a truncated or missing
    // order strands the dispatch-owned lease without ever constructing an RAII
    // guard.
    let inherited_lease_name = environment::MANDATE_LEASE_NAME
        .value()
        .filter(|name| !name.trim().is_empty());
    let mut inherited_lease = inherited_lease_name
        .as_deref()
        .map(|lease_name| {
            adopt_implementer_lease(
                request,
                lease_name,
                &request.order_file.display().to_string(),
            )
        })
        .transpose()?;
    let order_bytes = fs::read(&request.order_file).map_err(|_| {
        ImplementError::new(
            2,
            "work-order-invalid",
            "invalid schema_version 1 work order",
        )
    })?;
    let order = WorkOrder::from_json(&order_bytes).map_err(|_| {
        ImplementError::new(
            2,
            "work-order-invalid",
            "invalid schema_version 1 work order",
        )
    })?;
    let lease_name = format!("implementer-item-{}.lease", order.item_hash());
    // Dispatch owns this item lease until a terminal row is durable; adoption
    // prevents an independently launched implementer from spending the order.
    if inherited_lease_name
        .as_deref()
        .is_some_and(|inherited| inherited != lease_name)
    {
        return Err(ImplementError::new(
            1,
            "lease-name-mismatch",
            format!("lease-name-mismatch: {}", order.item_id),
        ));
    }
    let lease = inherited_lease.take().map_or_else(
        || adopt_implementer_lease(request, &lease_name, &order.item_id),
        Ok,
    )?;
    let mut guard = TerminalGuard {
        paths: request.paths.clone(),
        lease,
        order,
        unit_name: request.unit_name.clone(),
        backend: environment::MANDATE_DISPATCH_BACKEND
            .value()
            .unwrap_or_else(|| "systemd".to_owned()),
        started: request.clock.now(),
        clock: request.clock.clone(),
        failure_reason: "implementer-exited".to_owned(),
        failure_message: None,
        terminal_written: false,
        worktree: None,
        source_repository: None,
        default_branch: None,
        events_file: None,
        termination_signal: None,
        pr_url: None,
        remote_head_sha: None,
        conflicted_paths: Vec::new(),
        withheld_paths: Vec::new(),
    };
    match implement_inner(request, &mut guard, minter) {
        Ok(url) => {
            guard.pr_url = Some(url.clone());
            guard.append_terminal("work-completed", None)?;
            guard.finish()?;
            Ok(url)
        }
        Err(error) => {
            guard.fail(&error);
            Err(error)
        }
    }
}

fn adopt_implementer_lease(
    request: &ImplementRequest,
    lease_name: &str,
    item: &str,
) -> Result<OwnedLease, ImplementError> {
    OwnedLease::adopt(&request.paths.state, lease_name, &request.unit_name).map_err(|error| {
        let reason = match error {
            LeaseActionError::OwnerMismatch => "lease-owner-mismatch",
            _ => "lease-missing",
        };
        ImplementError::new(1, reason, format!("{reason}: {item}"))
    })
}

fn implement_inner(
    request: &ImplementRequest,
    guard: &mut TerminalGuard,
    minter: &mut dyn InstallationTokenMinter,
) -> Result<String, ImplementError> {
    let _ = termination_grace()?;
    check_interrupt(request, guard, None)?;
    let config = load_config_or_defaults(&request.paths, &request.working_directory).ok();
    let source = resolve_source_repository(&guard.order.repository, config.as_ref())?;
    guard.source_repository = Some(source.clone());

    let codex = environment::CODEX_BIN
        .value_os()
        .map_or_else(|| PathBuf::from("codex"), PathBuf::from);
    if !command_available(&codex) {
        return Err(ImplementError::new(
            1,
            "codex-unavailable",
            format!("Codex is unavailable: {}", codex.display()),
        ));
    }
    let default_branch = default_branch_result(gh_text(
        request,
        &guard.order.repository,
        "metadata:read",
        &[
            "gh",
            "repo",
            "view",
            &guard.order.repository,
            "--json",
            "defaultBranchRef",
            "--jq",
            ".defaultBranchRef.name",
        ],
        minter,
    ))?;
    guard.default_branch = Some(default_branch.clone());
    let remote = format!("https://github.com/{}.git", guard.order.repository);
    gh_status(
        request,
        &guard.order.repository,
        "metadata:read,contents:read",
        &[
            "git",
            "-C",
            &source.display().to_string(),
            "fetch",
            &remote,
            &format!("{default_branch}:refs/remotes/origin/{default_branch}"),
        ],
        minter,
    )
    .map_err(|error| {
        github_operation_error(error, "fetch-failed", "default branch fetch failed")
    })?;

    let worktree = request
        .paths
        .state
        .join("implementer-worktrees")
        .join(guard.order.item_hash());
    fs::create_dir_all(worktree.parent().expect("worktree has parent")).map_err(|error| {
        ImplementError::new(
            1,
            "worktree-create-failed",
            format!("could not create worktree directory: {error}"),
        )
    })?;
    prepare_worktree(
        &source,
        &worktree,
        &guard.order.branch_name,
        &default_branch,
    )?;
    guard.worktree = Some(worktree.clone());
    check_interrupt(request, guard, None)?;
    let preexisting_commits = git_text(
        &worktree,
        &[
            "rev-list",
            "--count",
            &format!("refs/remotes/origin/{default_branch}..HEAD"),
        ],
    )
    .and_then(|value| value.parse::<u64>().ok())
    .unwrap_or_default();

    let runs = request
        .paths
        .state
        .join("implementer-runs")
        .join(&guard.order.order_id);
    fs::create_dir_all(&runs)
        .map_err(|error| ImplementError::new(1, "run-directory-failed", error.to_string()))?;
    let prompt_file = runs.join("prompt.md");
    let result_file = runs.join("result.md");
    let events_file = runs.join("events.jsonl");
    fs::write(&prompt_file, prompt(&guard.order))
        .map_err(|error| ImplementError::new(1, "prompt-write-failed", error.to_string()))?;
    let events = File::create(&events_file)
        .map_err(|error| ImplementError::new(1, "event-stream-create-failed", error.to_string()))?;
    let errors = events
        .try_clone()
        .map_err(|error| ImplementError::new(1, "event-stream-create-failed", error.to_string()))?;
    guard.events_file = Some(events_file.clone());
    let input = File::open(&prompt_file)
        .map_err(|error| ImplementError::new(1, "prompt-read-failed", error.to_string()))?;
    let mut command = Command::new(&codex);
    // Never-approve plus workspace-write permits the requested diff without
    // giving the model either network access or authenticated publication.
    command
        .args([
            "exec",
            "--json",
            "-C",
            &worktree.display().to_string(),
            "-s",
            "workspace-write",
            "-c",
            "approval_policy=\"never\"",
            "-c",
            "sandbox_workspace_write.network_access=false",
            "-c",
            "web_search=\"disabled\"",
            "-o",
            &result_file.display().to_string(),
        ])
        .stdin(Stdio::from(input))
        .stdout(Stdio::from(events))
        .stderr(Stdio::from(errors));
    set_process_group(&mut command);
    let mut child = command.spawn().map_err(|error| {
        ImplementError::new(
            1,
            "codex-unavailable",
            format!("could not start Codex: {error}"),
        )
    })?;
    let status = wait_for_codex(request, guard, &mut child)?;
    if !status.success() {
        let code = exit_code(status);
        let reason = codex_failure_reason(code, &events_file);
        return Err(ImplementError::new(
            code,
            reason,
            format!("Codex exited with status {code}"),
        ));
    }

    // Codex reports usage only in its terminal event today. This deliberately
    // preserves the shell enforcement point instead of pretending the ceiling
    // can stop spend that has already happened.
    let usage = read_usage(&events_file);
    if usage.weighted() > guard.order.tokens() {
        return Err(ImplementError::new(
            1,
            "token-ceiling-exceeded",
            format!(
                "weighted token ceiling exceeded ({} > {})",
                usage.weighted(),
                guard.order.tokens()
            ),
        ));
    }
    if !worktree_has_changes(&worktree, &default_branch) && preexisting_commits == 0 {
        return Err(ImplementError::new(
            1,
            "no-changes",
            "Codex produced no changes",
        ));
    }
    if working_tree_dirty(&worktree) {
        git_required(&worktree, &["add", "-A"], "stage-failed")?;
        git_required(
            &worktree,
            &[
                "commit",
                "-m",
                &format!("feat: implement {}", guard.order.item_ref),
                "-m",
                "Ostrom-Role: builder",
            ],
            "commit-failed",
        )?;
    }
    withhold_workflows(guard, &worktree, &default_branch)?;
    publish_branch(request, guard, &worktree, &remote, minter)?;
    create_pull_request(request, guard, &runs, &default_branch, minter)
}

fn prepare_worktree(
    source: &Path,
    worktree: &Path,
    branch: &str,
    default_branch: &str,
) -> Result<(), ImplementError> {
    let default_ref = format!("refs/remotes/origin/{default_branch}");
    if worktree.exists() {
        let current = git_text(worktree, &["branch", "--show-current"]).ok_or_else(|| {
            ImplementError::new(1, "worktree-unreadable", "worktree branch is unreadable")
        })?;
        if current == branch {
            return Ok(());
        }
        if working_tree_dirty(worktree) || has_unpublished_tree(worktree, &default_ref) {
            return Err(ImplementError::new(
                1,
                "worktree-branch-mismatch",
                format!("worktree preserves unpublished work on {current}"),
            ));
        }
        if git_success(
            source,
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
        ) {
            git_required(worktree, &["switch", branch], "worktree-retarget-failed")?;
        } else {
            git_required(
                worktree,
                &["switch", "-c", branch, &default_ref],
                "worktree-retarget-failed",
            )?;
        }
        return Ok(());
    }
    // A branch created outside this item-keyed worktree may contain work whose
    // ownership cannot be proven, even when no worktree currently checks it out.
    if git_success(
        source,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    ) {
        let existing = branch_checkout_path(source, branch)
            .unwrap_or_else(|| "not checked out in any worktree".to_owned());
        return Err(ImplementError::new(
            1,
            "worktree-branch-already-exists",
            format!("branch {branch} already exists outside the item worktree: {existing}"),
        ));
    }
    let worktree_text = worktree.display().to_string();
    git_required(
        source,
        &[
            "worktree",
            "add",
            "-b",
            branch,
            &worktree_text,
            &default_ref,
        ],
        "worktree-create-failed",
    )
}

fn has_unpublished_tree(worktree: &Path, default_ref: &str) -> bool {
    // `ahead` is permanently nonzero for a squash-merged branch even though
    // its complete tree is already on the default branch.
    !git_success(worktree, &["diff", "--quiet", default_ref, "HEAD"])
}

fn worktree_has_changes(worktree: &Path, default_branch: &str) -> bool {
    working_tree_dirty(worktree)
        || has_unpublished_tree(worktree, &format!("refs/remotes/origin/{default_branch}"))
}

fn working_tree_dirty(worktree: &Path) -> bool {
    git_text(worktree, &["status", "--porcelain"]).is_none_or(|value| !value.is_empty())
}

fn wait_for_codex(
    request: &ImplementRequest,
    guard: &mut TerminalGuard,
    child: &mut Child,
) -> Result<ExitStatus, ImplementError> {
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| ImplementError::new(1, "codex-wait-failed", error.to_string()))?
        {
            crate::pass::kill_remaining_process_group(child.id());
            return Ok(status);
        }
        if let Err(error) = check_interrupt(request, guard, Some(child)) {
            let _ = child.wait();
            return Err(error);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn check_interrupt(
    request: &ImplementRequest,
    guard: &mut TerminalGuard,
    child: Option<&mut Child>,
) -> Result<(), ImplementError> {
    let signal = request.signals.take_pending();
    // Parent death is treated as termination so a killed transient-unit
    // wrapper cannot strand Codex or any descendant in the item worktree.
    let orphaned = request
        .supervisor_pid
        .is_some_and(|pid| !crate::pass::process_alive(pid));
    if signal.is_none() && !orphaned {
        return Ok(());
    }
    let name = signal.unwrap_or("TERM");
    if let Some(child) = child {
        // Five seconds lets Codex flush terminal output and run ordinary
        // cleanup without retaining the item lease indefinitely. Tests may
        // shorten the grace period through the established environment knob.
        let grace = termination_grace()?;
        guard.termination_signal = crate::pass::terminate_child_process_group(child, grace);
    } else {
        guard.termination_signal = Some(format!("SIG{name}"));
    }
    let code = match name {
        "HUP" => 129,
        "INT" => 130,
        _ => 143,
    };
    Err(ImplementError::new(
        code,
        format!("signal-{name}"),
        format!("received SIG{name}"),
    ))
}

fn termination_grace() -> Result<Duration, ImplementError> {
    match environment::MANDATE_IMPLEMENTER_TERMINATION_GRACE_SECONDS.value() {
        Some(value) => value
            .parse::<u64>()
            .ok()
            .filter(|seconds| *seconds > 0)
            .map(Duration::from_secs)
            .ok_or_else(|| {
                ImplementError::new(
                    2,
                    "termination-grace-invalid",
                    "termination grace must be a positive integer",
                )
            }),
        None => Ok(Duration::from_secs(5)),
    }
}

fn codex_failure_reason(code: i32, events_file: &Path) -> String {
    let events = fs::read_to_string(events_file).unwrap_or_default();
    match code {
        126 | 127 => "codex-unavailable".to_owned(),
        1 if events.lines().any(|line| {
            line.starts_with("Error loading config.toml:")
                || (line.starts_with("Error: features.")
                    && line.contains(" is required when ")
                    && line.ends_with(" is enabled"))
        }) =>
        {
            "codex-invocation-invalid".to_owned()
        }
        2 if events
            .lines()
            .any(|line| line.starts_with("Usage: codex exec ")) =>
        {
            "codex-invocation-invalid".to_owned()
        }
        _ => format!("codex-exit-{code}"),
    }
}

#[cfg(unix)]
fn exit_code(status: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;

    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1)
}

#[cfg(not(unix))]
fn exit_code(status: ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

fn read_usage(path: &Path) -> Usage {
    let mut usage = Usage {
        cached_input_tokens_available: true,
        ..Usage::default()
    };
    let mut turns = 0_u64;
    let Ok(contents) = fs::read_to_string(path) else {
        return Usage::default();
    };
    for event in contents
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
    {
        if event.get("type").and_then(Value::as_str) != Some("turn.completed") {
            continue;
        }
        let Some(turn) = event.get("usage").and_then(Value::as_object) else {
            continue;
        };
        turns += 1;
        usage.input_tokens += turn
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        match turn.get("cached_input_tokens").and_then(Value::as_u64) {
            Some(value) => {
                *usage.cached_input_tokens.get_or_insert(0) += value;
            }
            None => usage.cached_input_tokens_available = false,
        }
        usage.output_tokens += turn
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        usage.reasoning_output_tokens += turn
            .get("reasoning_output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default();
    }
    if turns == 0 || !usage.cached_input_tokens_available {
        usage.cached_input_tokens = None;
        usage.fresh_input_tokens = None;
        usage.cached_input_tokens_available = false;
    } else {
        usage.fresh_input_tokens = Some(
            usage
                .input_tokens
                .saturating_sub(usage.cached_input_tokens.unwrap_or_default()),
        );
    }
    usage
}

fn withhold_workflows(
    guard: &mut TerminalGuard,
    worktree: &Path,
    default_branch: &str,
) -> Result<(), ImplementError> {
    let default_ref = format!("refs/remotes/origin/{default_branch}");
    let paths = git_text(
        worktree,
        &["diff", "--name-only", &format!("{default_ref}...HEAD")],
    )
    .ok_or_else(|| {
        ImplementError::new(
            1,
            "workflow-file-check-failed",
            "could not inspect publish paths",
        )
    })?;
    guard.withheld_paths = paths
        .lines()
        .filter(|path| path.starts_with(".github/workflows/"))
        .map(str::to_owned)
        .collect();
    for path in &guard.withheld_paths {
        if git_success(
            worktree,
            &["cat-file", "-e", &format!("{default_ref}:{path}")],
        ) {
            git_required(
                worktree,
                &["checkout", &default_ref, "--", path],
                "workflow-file-check-failed",
            )?;
        } else {
            git_required(
                worktree,
                &["rm", "-f", "--", path],
                "workflow-file-check-failed",
            )?;
        }
    }
    if !guard.withheld_paths.is_empty() {
        git_required(
            worktree,
            &["commit", "--amend", "--no-edit", "--allow-empty"],
            "commit-failed",
        )?;
        if !has_unpublished_tree(worktree, &default_ref) {
            return Err(ImplementError::new(
                1,
                "workflow-file-unpushable",
                format!(
                    "only workflow files changed; withheld paths: {}",
                    guard.withheld_paths.join(",")
                ),
            ));
        }
    }
    Ok(())
}

fn publish_branch(
    request: &ImplementRequest,
    guard: &mut TerminalGuard,
    worktree: &Path,
    remote: &str,
    minter: &mut dyn InstallationTokenMinter,
) -> Result<(), ImplementError> {
    let worktree_text = worktree.display().to_string();
    let refspec = format!("HEAD:refs/heads/{}", guard.order.branch_name);
    let first_push = gh_output(
        request,
        &guard.order.repository,
        "metadata:read,contents:write",
        &["git", "-C", &worktree_text, "push", remote, &refspec],
        minter,
    )
    .map_err(|error| github_boundary_error(error, "push-failed", "could not run push"))?;
    if first_push.status.success() {
        return Ok(());
    }
    let push_diagnostic = format!(
        "{}\n{}",
        String::from_utf8_lossy(&first_push.stdout),
        String::from_utf8_lossy(&first_push.stderr)
    )
    .to_ascii_lowercase();
    if !push_diagnostic.contains("non-fast-forward") && !push_diagnostic.contains("fetch first") {
        return Err(ImplementError::new(1, "push-failed", "branch push failed"));
    }
    // A concurrently advanced review branch is merged forward exactly once;
    // published history is never rewritten by the implementation harness.
    gh_status(
        request,
        &guard.order.repository,
        "metadata:read,contents:read",
        &[
            "git",
            "-C",
            &worktree_text,
            "fetch",
            remote,
            &format!("refs/heads/{}", guard.order.branch_name),
        ],
        minter,
    )
    .map_err(|error| {
        github_operation_error(error, "push-failed", "could not fetch advanced branch")
    })?;
    guard.remote_head_sha = git_text(worktree, &["rev-parse", "FETCH_HEAD"]);
    if !git_success(worktree, &["merge", "--no-edit", "FETCH_HEAD"]) {
        guard.conflicted_paths = git_text(worktree, &["diff", "--name-only", "--diff-filter=U"])
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect();
        let _ = Command::new("git")
            .args(["-C", &worktree_text, "merge", "--abort"])
            .status();
        let reason = if guard.conflicted_paths.is_empty() {
            "push-failed"
        } else {
            "branch-conflicted"
        };
        return Err(ImplementError::new(1, reason, "branch repair failed"));
    }
    gh_status(
        request,
        &guard.order.repository,
        "metadata:read,contents:write",
        &["git", "-C", &worktree_text, "push", remote, &refspec],
        minter,
    )
    .map(|_| ())
    .map_err(|error| github_operation_error(error, "push-failed", "push retry failed"))
}

fn create_pull_request(
    request: &ImplementRequest,
    guard: &TerminalGuard,
    runs: &Path,
    default_branch: &str,
    minter: &mut dyn InstallationTokenMinter,
) -> Result<String, ImplementError> {
    let body = runs.join("pr-body.md");
    fs::write(
        &body,
        pull_request_body(&guard.order, &guard.withheld_paths),
    )
    .map_err(|error| ImplementError::new(1, "pr-body-write-failed", error.to_string()))?;
    gh_text(
        request,
        &guard.order.repository,
        "metadata:read,pull_requests:write",
        &[
            "gh",
            "pr",
            "create",
            "--repo",
            &guard.order.repository,
            "--base",
            default_branch,
            "--head",
            &guard.order.branch_name,
            "--title",
            &format!("Implement {}", guard.order.item_id),
            "--body-file",
            &body.display().to_string(),
        ],
        minter,
    )
    .map_err(|error| {
        github_operation_error(error, "pr-create-failed", "pull request creation failed")
    })
}

fn prompt(order: &WorkOrder) -> String {
    format!(
        "Implement this work order. Work only in the current worktree. Do not commit, push, open a pull request, or use the network; the outer harness owns those steps. Do not modify anything under `.github/workflows/`; any such edit will be reverted before publication rather than published. Run proportionate tests. Do not redesign the agreed spec.\n\nItem: {}\nBranch: {}\nCost ceiling: ${}; weighted-token ceiling: {}\n\nSpec:\n{}\n\nAcceptance criteria:\n{}\n\nConstraints:\n{}\n",
        order.item_id,
        order.branch_name,
        order.cost(),
        order.tokens(),
        order.spec,
        order
            .acceptance_criteria
            .iter()
            .map(|value| format!("- {value}"))
            .collect::<Vec<_>>()
            .join("\n"),
        order
            .constraints
            .iter()
            .map(|value| format!("- {value}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

fn pull_request_body(order: &WorkOrder, withheld: &[String]) -> String {
    let withheld_section = if withheld.is_empty() {
        String::new()
    } else {
        format!(
            "## Withheld workflow paths\n\nThese paths were restored to the default branch and are not included in this pull request:\n\n{}\n\n",
            withheld
                .iter()
                .map(|path| format!("- `{path}`"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };
    format!(
        "Closes {}\n\n## Work order\n\n{}\n\n## Acceptance criteria\n\n{}\n\n{}## Implementation harness\n\nCodex ran non-interactively with `workspace-write`, approval policy `never`, and network disabled. The outer implementer wrapper performed fetch, commit, push, and pull-request creation outside the Codex sandbox.\n\nThe order reserved ${} and enforced a {} weighted-token ceiling.\n\nOstrom-Role: builder\n",
        order.item_id,
        order.spec,
        order
            .acceptance_criteria
            .iter()
            .map(|value| format!("- {value}"))
            .collect::<Vec<_>>()
            .join("\n"),
        withheld_section,
        order.cost(),
        order.tokens()
    )
}

fn resolve_source_repository(
    repository: &str,
    config: Option<&MandateConfig>,
) -> Result<PathBuf, ImplementError> {
    if let Some(source) = environment::MANDATE_IMPLEMENTER_SOURCE_REPO.value_os() {
        let source = PathBuf::from(source);
        return source.is_dir().then_some(source).ok_or_else(|| {
            ImplementError::new(
                1,
                "source-repository-not-found",
                "source repository override is not a directory",
            )
        });
    }
    let roots = config
        .map(|config| config.search_roots.as_slice())
        .unwrap_or_default();
    if roots.is_empty() {
        return Err(ImplementError::new(
            1,
            "source-repository-roots-unconfigured",
            "source repository roots are unconfigured",
        ));
    }
    let mut primary = Vec::new();
    let mut linked = Vec::new();
    for root in roots {
        collect_repositories(Path::new(root), repository, &mut primary, &mut linked);
    }
    primary.sort();
    primary.dedup();
    if let Some(path) = primary.into_iter().next() {
        return Ok(path);
    }
    if linked.is_empty() {
        return Err(ImplementError::new(
            1,
            "source-repository-not-found",
            "source repository not found",
        ));
    }
    linked.sort();
    Err(ImplementError::new(
        1,
        "source-repository-linked-worktree-only",
        format!(
            "source repository was found only as a linked worktree: {}",
            linked[0].display()
        ),
    ))
}

fn branch_checkout_path(source: &Path, branch: &str) -> Option<String> {
    let output = git_text(source, &["worktree", "list", "--porcelain"])?;
    let reference = format!("refs/heads/{branch}");
    let mut path = None;
    for line in output.lines() {
        if let Some(value) = line.strip_prefix("worktree ") {
            path = Some(value.to_owned());
        } else if line.strip_prefix("branch ") == Some(reference.as_str()) {
            return path;
        } else if line.is_empty() {
            path = None;
        }
    }
    None
}

fn collect_repositories(
    directory: &Path,
    repository: &str,
    primary: &mut Vec<PathBuf>,
    linked: &mut Vec<PathBuf>,
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
                } else {
                    linked.push(candidate.to_path_buf());
                }
            }
        } else if path.is_dir() {
            collect_repositories(&path, repository, primary, linked);
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

fn gh_status(
    request: &ImplementRequest,
    repository: &str,
    permissions: &str,
    command: &[&str],
    minter: &mut dyn InstallationTokenMinter,
) -> Result<Output, GitHubOperationError> {
    let output = gh_output(request, repository, permissions, command, minter)
        .map_err(GitHubOperationError::Boundary)?;
    output
        .status
        .success()
        .then_some(output)
        .ok_or(GitHubOperationError::Rejected)
}

fn gh_output(
    request: &ImplementRequest,
    repository: &str,
    permissions: &str,
    command: &[&str],
    minter: &mut dyn InstallationTokenMinter,
) -> Result<Output, AuthenticatedCommandError> {
    authenticated_output(
        &request.paths,
        ScopedAppTokenRequest::new("builder", repository, repository, permissions),
        command,
        minter,
    )
}

fn gh_text(
    request: &ImplementRequest,
    repository: &str,
    permissions: &str,
    command: &[&str],
    minter: &mut dyn InstallationTokenMinter,
) -> Result<String, GitHubOperationError> {
    gh_status(request, repository, permissions, command, minter)
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

enum GitHubOperationError {
    Boundary(AuthenticatedCommandError),
    Rejected,
}

fn github_boundary_error(
    error: AuthenticatedCommandError,
    rejected_reason: &'static str,
    rejected_message: &'static str,
) -> ImplementError {
    github_operation_error(
        GitHubOperationError::Boundary(error),
        rejected_reason,
        rejected_message,
    )
}

fn github_operation_error(
    error: GitHubOperationError,
    rejected_reason: &'static str,
    rejected_message: &'static str,
) -> ImplementError {
    match error {
        GitHubOperationError::Boundary(AuthenticatedCommandError::Authentication(error)) => {
            ImplementError::new(
                1,
                "github-authentication-failed",
                format!("GitHub authentication failed: {error}"),
            )
        }
        GitHubOperationError::Boundary(AuthenticatedCommandError::Transport(error)) => {
            ImplementError::new(
                1,
                "github-command-transport-failed",
                format!("authenticated command transport failed: {error}"),
            )
        }
        GitHubOperationError::Rejected => ImplementError::new(1, rejected_reason, rejected_message),
    }
}

fn default_branch_result(
    result: Result<String, GitHubOperationError>,
) -> Result<String, ImplementError> {
    let branch = result.map_err(|error| {
        github_operation_error(
            error,
            "default-branch-query-failed",
            "could not query default branch",
        )
    })?;
    if branch.is_empty() {
        Err(ImplementError::new(
            1,
            "default-branch-missing",
            "default branch is missing",
        ))
    } else {
        Ok(branch)
    }
}

fn command_available(command: &Path) -> bool {
    if command.components().count() > 1 {
        return command.is_file();
    }
    environment::PATH.value_os().is_some_and(|path| {
        env::split_paths(&path).any(|directory| directory.join(command).is_file())
    })
}

fn git_required(
    directory: &Path,
    arguments: &[&str],
    reason: &'static str,
) -> Result<(), ImplementError> {
    git_success(directory, arguments)
        .then_some(())
        .ok_or_else(|| {
            ImplementError::new(1, reason, format!("git {} failed", arguments.join(" ")))
        })
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
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(unix)]
fn set_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(not(unix))]
fn set_process_group(_command: &mut Command) {}

#[cfg(test)]
mod tests {
    use std::{fs, process::Command};

    use tempfile::tempdir;

    use super::{
        GitHubOperationError, default_branch_result, has_unpublished_tree, prepare_worktree,
    };
    use crate::{AppTokenError, app_token::AuthenticatedCommandError};

    fn git(path: &std::path::Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn default_branch_faults_name_authentication_transport_and_empty_results() {
        let authentication = default_branch_result(Err(GitHubOperationError::Boundary(
            AuthenticatedCommandError::Authentication(AppTokenError::Credentials(
                "placeholder credentials unavailable".to_owned(),
            )),
        )))
        .expect_err("authentication must be named");
        assert_eq!(authentication.reason, "github-authentication-failed");

        let transport = default_branch_result(Err(GitHubOperationError::Boundary(
            AuthenticatedCommandError::Transport("placeholder spawn failure".to_owned()),
        )))
        .expect_err("transport must be named");
        assert_eq!(transport.reason, "github-command-transport-failed");

        let empty = default_branch_result(Ok(String::new())).expect_err("empty branch must fail");
        assert_eq!(empty.reason, "default-branch-missing");
    }

    #[test]
    fn squash_merged_branch_is_not_unpublished_work() {
        let fixture = tempdir().expect("temporary repository");
        let repo = fixture.path().join("placeholder-alpha");
        fs::create_dir(&repo).expect("create repository");
        git(&repo, &["init", "-b", "main"]);
        git(&repo, &["config", "user.email", "fixture@example.invalid"]);
        git(&repo, &["config", "user.name", "Fixture"]);
        fs::write(repo.join("README.md"), "base\n").expect("write base");
        git(&repo, &["add", "README.md"]);
        git(&repo, &["commit", "-m", "base"]);
        git(&repo, &["branch", "candidate/placeholder"]);
        git(&repo, &["switch", "candidate/placeholder"]);
        fs::write(repo.join("README.md"), "base\nchange\n").expect("write change");
        git(&repo, &["commit", "-am", "change"]);
        git(&repo, &["switch", "main"]);
        git(&repo, &["merge", "--squash", "candidate/placeholder"]);
        git(&repo, &["commit", "-m", "squash change"]);
        git(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        git(&repo, &["switch", "candidate/placeholder"]);

        assert!(!has_unpublished_tree(&repo, "refs/remotes/origin/main"));
        git(&repo, &["switch", "main"]);
        let worktree = fixture.path().join("item-worktree");
        git(
            &repo,
            &[
                "worktree",
                "add",
                &worktree.display().to_string(),
                "candidate/placeholder",
            ],
        );
        prepare_worktree(&repo, &worktree, "candidate/retry", "main")
            .expect("squash-merged branch may be retargeted");
    }
}
