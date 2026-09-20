use std::{
    collections::BTreeSet,
    fs,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};

use chrono::DateTime;
use ethogram::{
    CAPTURE_REFUSED, CaptureRefusalCause, CaptureRefusedPayload, ControlKind,
    ControlRequestedPayload, EventDraft, MAX_EXCERPT_SCALARS, PayloadExtension, RunKind,
    RunOutcome as EventRunOutcome, excerpt,
};
use ostrom_core::PermissionMode;
use serde_json::{Map, Value, json};
use thiserror::Error;
use umwelt_capture::{
    CaptureFault, ChildStdoutSource, LineSource, Normaliser, claude::ClaudeNormaliser,
};
use umwelt_runtime::{
    CapTrip, CapsWatchdog, ProcessExit, ResumeError, ResumedSession, RunCaps, RunControl,
    SessionResumer, SinkFault, SystemClock,
};

use crate::{
    Clock, LeaseActionError, OstromPaths, OwnedLease, PassState, RunEventError, RunEventGuard,
    RunEventStart, SignalFlags, SkippedRepository, SweepOptions, TraceAppend, append_trace,
    environment, generated_run_id, generation_is_fresh, latest_successful_generation,
    load_sweep_snapshot, pass_control,
    pass_control::ControlInput,
    read_lease, read_pass_state, read_trace,
    selection::dispatchability_snapshot,
    sweep::{
        SWEEP_LEASE_CEILING_SECONDS, SweepError, run_sweep_holding_lease, wait_for_sweep_lease_for,
    },
    write_pass_state,
};

pub const MAX_TURNS: &str = "200";
/// Umwelt's `RunCaps::default()` uses ten seconds; this is five. The divergence
/// is inherited rather than argued: five seconds was already ostrom's TERM-to-KILL
/// grace in `terminate_child_process_group`, and #494 promoted that literal to
/// this constant so one value drives both ostrom's own termination path (through
/// `PASS_TERMINATION_GRACE`) and the cap grace handed to Umwelt, rather than
/// letting the two drift apart.
///
/// Whether five seconds is right for a *cap* grace has therefore never been
/// decided on its own terms. Recorded as a shared value with one history, not as
/// a considered halving of Umwelt's default, so the next reader does not mistake
/// an inheritance for a judgement.
pub const PASS_KILL_GRACE_MS: u64 = 5_000;
// EX_CONFIG: the pass invocation is valid, but the local arm configuration
// explicitly refuses to execute it.
const DISARMED_EXIT_CODE: i32 = 78;
// EX_UNAVAILABLE: the pass needs a support program, the harness, that is
// present but cannot do what this pass requires (ostrom#587). It has its own
// code rather than sharing EX_CONFIG with a disarmed pass, so a consumer
// reading only the exit status cannot mistake a harness too old for the
// bridge for a pass the operator chose not to run.
const HARNESS_UNAVAILABLE_EXIT_CODE: i32 = 69;
// EX_TEMPFAIL: the pass is held at its daily spend cap and can run once
// the ceiling resets or is raised.
const BUDGET_HELD_EXIT_CODE: i32 = 75;
// Sysexits has no "resource busy" meaning, so 76 is chosen only because it is
// a free value alongside the three exit codes above, not for its sysexits
// `EX_PROTOCOL` name. Lease contention (ostrom#599) gets its own status
// rather than sharing BUDGET_HELD_EXIT_CODE: contention resolves itself by
// retrying unchanged, unlike a daily-cap hold, which calls for different
// operator action. The same number is used on the `ostrom sweep` CLI path
// (not routed through `PassError`) so a supervisor sees one status for one
// condition regardless of which surface hit it.
pub const SWEEP_LEASE_CONTENTION_EXIT_CODE: i32 = 76;
const DEFAULT_DAILY_CAP_USD: f64 = 50.0;
const DEFAULT_LEASE_TTL_SECONDS: u64 = 3_600;
const PASS_TERMINATION_GRACE: Duration = Duration::from_millis(PASS_KILL_GRACE_MS);
const BRIDGE_HARNESS_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Render a permission mode as the flag value the Claude harness expects.
///
/// The mapping lives here rather than on the policy type because it is one
/// harness's spelling of the concept, not the concept itself.
const fn permission_mode_flag(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Auto => "auto",
        PermissionMode::Manual => "manual",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassRole {
    Builder,
    Gatekeeper,
}

impl PassRole {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Builder => "builder",
            Self::Gatekeeper => "gatekeeper",
        }
    }

    /// The prompt this build ships for the role.
    ///
    /// This is the floor, not the source of truth: a repository or operator
    /// manifest that declares the role's operation supplies the prompt
    /// instead, so changing what an agent is told is a policy edit rather
    /// than a release. The embedded copy is what runs when nothing is
    /// declared, which keeps an ungoverned repository working.
    #[must_use]
    pub const fn default_prompt(self) -> &'static str {
        match self {
            Self::Builder => {
                include_str!("../assets/prompts/work.md")
            }
            // The gatekeeper drives the merge protocol once per candidate, so
            // both halves are one prompt. They are two files only because the
            // protocol is long enough to be worth reading on its own.
            Self::Gatekeeper => concat!(
                include_str!("../assets/prompts/gatekeep.md"),
                "\n\n",
                include_str!("../assets/prompts/merge.md"),
            ),
        }
    }
}

/// The triage prompt `ostrom init` writes, owned by the crate that holds the
/// asset. Triage is not a `PassRole`: it runs as an operation's `agent/claude`
/// step from policy, not as `ostrom pass <role>`, so it has no variant above.
/// It is a const rather than a cross-crate `include_str!` so the file stays
/// owned here and moving the assets directory cannot silently break a consumer.
pub const TRIAGE_PROMPT: &str = include_str!("../assets/prompts/triage.md");

impl PassRole {
    /// The permission mode this build ships for the role.
    ///
    /// Like the prompt, this is the floor: an actor declaration in policy
    /// overrides it. The builder writes unattended; the gatekeeper judges and
    /// must not act without confirmation.
    #[must_use]
    pub const fn default_permission_mode(self) -> PermissionMode {
        match self {
            Self::Builder => PermissionMode::Auto,
            Self::Gatekeeper => PermissionMode::Manual,
        }
    }

    const fn inner_lease(self) -> &'static str {
        match self {
            Self::Builder => "builder.lease",
            Self::Gatekeeper => "sprint.lease",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PassRequest {
    pub paths: OstromPaths,
    pub working_directory: PathBuf,
    pub role: PassRole,
    /// The resolved prompt for this pass: policy-declared when the manifest
    /// binds the role's operation, otherwise [`PassRole::default_prompt`].
    pub prompt: String,
    /// The resolved permission mode: the actor declaration when policy
    /// supplies one, otherwise [`PassRole::default_permission_mode`].
    pub permission_mode: PermissionMode,
    /// The harness profile derived from the actor's policy grants, when policy
    /// declares the actor. `None` falls back to the operator's hand-written
    /// `roles/<role>.settings.json`.
    pub derived_settings: Option<String>,
    pub claude_bin: PathBuf,
    pub signals: SignalFlags,
    pub supervisor_pid: Option<u32>,
    pub events_fd: Option<u32>,
    pub control_fd: Option<u32>,
    pub facts_only: bool,
    /// Present for a loop-bound pass and omitted for an unbound pass.
    pub repositories: Option<Vec<String>>,
    pub skipped_repositories: Vec<SkippedRepository>,
    /// Hard child-command scope. This is also present for an unbound pass,
    /// where it contains the available set.
    pub repository_scope: Option<Vec<String>>,
    /// Production passes always supply freshness policy. Direct library
    /// callers may omit it when another layer already owns freshness.
    pub sweep: Option<PassSweepRequest>,
    pub caps: RunCaps,
    pub clock: Clock,
    /// The value [`std::env::consts::OS`] would report, threaded through
    /// explicitly so the ostrom#544 platform fallback can be exercised by
    /// injection rather than by requiring a Windows host in CI. Production
    /// always passes `std::env::consts::OS` itself.
    pub platform: &'static str,
}

#[derive(Debug, Clone)]
pub struct PassSweepRequest {
    pub options: SweepOptions,
    pub max_age_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PassSweepOutcome {
    Swept(String),
    Reused(String),
    NotManaged,
}

#[derive(Debug)]
struct PreparedSweep {
    outcome: PassSweepOutcome,
    snapshots: Option<Vec<crate::RepositorySnapshot>>,
}

#[derive(Debug)]
struct PreparedSession {
    prompt: String,
    candidate_count: Option<usize>,
}

impl PassSweepOutcome {
    fn generation_id(&self) -> Option<&str> {
        match self {
            Self::Swept(generation) | Self::Reused(generation) => Some(generation),
            Self::NotManaged => None,
        }
    }

    fn record(&self, fact: &mut Map<String, Value>) {
        match self {
            Self::Swept(generation) => {
                fact.insert("sweep".to_owned(), json!("swept"));
                fact.insert("generation_id".to_owned(), json!(generation));
            }
            Self::Reused(generation) => {
                fact.insert("sweep".to_owned(), json!("reused"));
                fact.insert("generation_id".to_owned(), json!(generation));
            }
            Self::NotManaged => {}
        }
    }
}

/// A `prepare_sweep` failure, distinguishing lease contention from every
/// other sweep failure so the caller can give contention its own reason and
/// exit status (ostrom#599) instead of flattening every cause to a string.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
enum PrepareSweepError {
    #[error("{0}")]
    LeaseContention(String),
    #[error("{0}")]
    Other(String),
}

/// Classify a sweep failure as lease contention or anything else. Both
/// `SweepError::LeaseHeld` (the pass's own single acquisition attempt) and
/// `SweepError::LeaseWaitTimedOut` (the bounded wait ran out) mean the same
/// thing to a caller: the lease stayed held by someone else, not that the
/// sweep itself failed.
fn classify_sweep_error(error: SweepError) -> PrepareSweepError {
    if matches!(
        error,
        SweepError::LeaseHeld | SweepError::LeaseWaitTimedOut { .. }
    ) {
        PrepareSweepError::LeaseContention(error.to_string())
    } else {
        PrepareSweepError::Other(error.to_string())
    }
}

/// The trace `reason` and pass exit status a `prepare_sweep` failure is
/// recorded and exited with. A pure function so the mapping itself, not just
/// its use inside `run_pass`, is directly testable (ostrom#599).
const fn sweep_failure_status(error: &PrepareSweepError) -> (&'static str, i32) {
    match error {
        PrepareSweepError::LeaseContention(_) => {
            ("sweep-lease-contention", SWEEP_LEASE_CONTENTION_EXIT_CODE)
        }
        PrepareSweepError::Other(_) => ("sweep-failed", 1),
    }
}

/// Takes an explicit sweep-lease wait ceiling (ostrom#599) rather than
/// defaulting to `SWEEP_LEASE_CEILING_SECONDS` itself, so a test that wants
/// to drive contention to its exit status is not stuck with the production
/// 1800s default. `run_pass`'s own call site passes
/// `SWEEP_LEASE_CEILING_SECONDS` explicitly for its production wait.
fn prepare_sweep(
    request: &PassRequest,
    sweep_wait: Duration,
) -> Result<PreparedSweep, PrepareSweepError> {
    let Some(sweep) = &request.sweep else {
        return Ok(PreparedSweep {
            outcome: PassSweepOutcome::NotManaged,
            snapshots: None,
        });
    };
    // The freshness decision and gatekeeper snapshot read share the writer's
    // lease. A pass that arrives during a sweep waits once, then evaluates the
    // generation the completed writer actually left behind.
    let lease =
        wait_for_sweep_lease_for(&request.paths, sweep_wait).map_err(classify_sweep_error)?;
    let latest = latest_successful_generation(&request.paths).map_err(classify_sweep_error)?;
    let reusable = latest.filter(|generation| {
        generation_is_fresh(generation, request.clock.now(), sweep.max_age_seconds)
    });
    if let Some(generation) = reusable {
        if request.role != PassRole::Gatekeeper {
            return Ok(PreparedSweep {
                outcome: PassSweepOutcome::Reused(generation.id),
                snapshots: None,
            });
        }
        // A mismatched state/snapshot pair is not reusable. Keeping the lease
        // and falling through performs the same single repair sweep as plan.
        if let Ok(snapshots) = load_sweep_snapshot(&request.paths, &generation) {
            return Ok(PreparedSweep {
                outcome: PassSweepOutcome::Reused(generation.id),
                snapshots: Some(snapshots),
            });
        }
    }
    let outcome = run_sweep_holding_lease(&sweep.options, lease).map_err(classify_sweep_error)?;
    let snapshots = (request.role == PassRole::Gatekeeper)
        .then(|| load_sweep_snapshot(&request.paths, &outcome.generation))
        .transpose()
        .map_err(classify_sweep_error)?;
    Ok(PreparedSweep {
        outcome: PassSweepOutcome::Swept(outcome.generation.id),
        snapshots,
    })
}

fn session_prompt(request: &PassRequest, sweep: &PreparedSweep) -> Result<PreparedSession, String> {
    if request.role != PassRole::Gatekeeper || request.sweep.is_none() {
        return Ok(PreparedSession {
            prompt: request.prompt.clone(),
            candidate_count: None,
        });
    }
    let generation_id = sweep
        .outcome
        .generation_id()
        .ok_or_else(|| "managed gatekeeper pass has no sweep generation".to_owned())?;
    let scope = request.repository_scope.as_ref().map(|repositories| {
        repositories
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>()
    });
    let snapshots = sweep
        .snapshots
        .as_ref()
        .ok_or_else(|| "managed gatekeeper pass has no sweep snapshot".to_owned())?;
    let mut candidates = BTreeSet::new();
    let mut snapshot_repositories = BTreeSet::new();
    for snapshot in snapshots {
        let repository = snapshot.repo.as_str();
        if scope
            .as_ref()
            .is_some_and(|repositories| !repositories.contains(repository))
        {
            continue;
        }
        snapshot_repositories.insert(repository.to_owned());
        for pull_request in &snapshot.open_prs {
            let number = pull_request.get("number").and_then(Value::as_u64).ok_or_else(|| {
                format!(
                    "sweep generation `{generation_id}` has a pull request without a numeric number in `{repository}`"
                )
            })?;
            candidates.insert((repository.to_owned(), number));
        }
    }
    let effective_repositories = request
        .repository_scope
        .clone()
        .unwrap_or_else(|| snapshot_repositories.into_iter().collect());
    let candidates = candidates
        .into_iter()
        .map(|(repository, number)| json!({"repository": repository, "number": number}))
        .collect::<Vec<_>>();
    let input = json!({
        "generation_id": generation_id,
        "effective_repositories": effective_repositories,
        "pull_requests": candidates,
    });
    Ok(PreparedSession {
        prompt: format!(
            "{}\n\n## Sweep snapshot candidates for this pass\n\nUse this pass-supplied JSON as the complete candidate input. Judge only its `pull_requests`; do not enumerate live pull requests or add a repository.\n\n```json\n{}\n```\n",
            request.prompt,
            serde_json::to_string_pretty(&input).expect("gatekeeper input serializes")
        ),
        candidate_count: Some(
            input["pull_requests"]
                .as_array()
                .expect("gatekeeper candidates are an array")
                .len(),
        ),
    })
}

#[derive(Debug, Error)]
pub enum PassError {
    #[error("ostrom {role} pass: {message}")]
    Failed {
        role: &'static str,
        message: String,
        code: i32,
    },
    #[error("ostrom {0} pass: another pass already holds {0}-pass.lease; skipping")]
    LeaseHeld(&'static str),
    #[error("ostrom {0} pass: loop is disarmed")]
    Disarmed(&'static str),
    #[error("ostrom {0} pass: daily spend cap reached; held until the ceiling resets or is raised")]
    BudgetHeld(&'static str),
    #[error("ostrom {0} pass: no-effective-repositories")]
    NoEffectiveRepositories(&'static str),
    #[error(
        "ostrom {role} pass: Claude Code {version} at {path} cannot run a bridged pass: the permission bridge needs --permission-prompts and --permission-prompt-tool, available from Claude Code {minimum} (the lowest verified version); upgrade Claude Code to at least {minimum}"
    )]
    HarnessUnsupported {
        role: &'static str,
        version: String,
        path: PathBuf,
        minimum: crate::permission_bridge::HarnessVersion,
    },
}

impl PassError {
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::Failed { code, .. } => *code,
            Self::LeaseHeld(_) => 0,
            Self::Disarmed(_) => DISARMED_EXIT_CODE,
            Self::BudgetHeld(_) => BUDGET_HELD_EXIT_CODE,
            Self::NoEffectiveRepositories(_) => 3,
            Self::HarnessUnsupported { .. } => HARNESS_UNAVAILABLE_EXIT_CODE,
        }
    }

    fn failed(role: PassRole, message: impl Into<String>, code: i32) -> Self {
        Self::Failed {
            role: role.name(),
            message: message.into(),
            code,
        }
    }

    fn harness_unsupported(role: PassRole, path: &Path, version: impl Into<String>) -> Self {
        Self::HarnessUnsupported {
            role: role.name(),
            version: version.into(),
            path: path.to_owned(),
            minimum: crate::permission_bridge::MIN_BRIDGE_HARNESS_VERSION,
        }
    }
}

struct PassGuard {
    role: PassRole,
    paths: OstromPaths,
    lease: OwnedLease,
    owner: String,
    started_epoch: u64,
    trace_time: String,
    started: bool,
    child_spawned: bool,
    outcome: Option<String>,
    reason: Option<String>,
    cost_usd: Option<f64>,
    clock: Clock,
    dispatchability_hash: Option<String>,
    queue_count: Option<usize>,
    dispatchable_count: Option<usize>,
    repositories: Option<Vec<String>>,
    skipped_repositories: Vec<SkippedRepository>,
    events: RunEventGuard,
    control: Option<RunControl<NoSteer>>,
    process_exit: ProcessExit,
    permission_bridge: Option<crate::permission_bridge::PermissionBridge>,
}

struct NoSteer;

impl SessionResumer for NoSteer {
    fn resume(&self, _session_id: &str, _text: &str) -> Result<ResumedSession, ResumeError> {
        Err(ResumeError::Unsupported)
    }
}

/// The outcome a pass is recorded with when its guard finishes.
///
/// Extracted from `PassGuard::finish` so the unwinding branch can be tested
/// without a seam. It was previously reachable only by making production code
/// panic on an environment variable, which is a test hook living in `src`.
fn terminal_outcome(explicit: Option<String>, panicking: bool) -> String {
    explicit.unwrap_or_else(|| {
        if panicking {
            "failed".to_owned()
        } else {
            "completed".to_owned()
        }
    })
}

impl PassGuard {
    fn finish(&mut self) -> Result<(), PassError> {
        let mut failure = self
            .permission_bridge
            .take()
            .and_then(|bridge| bridge.close().err())
            .map(|error| {
                PassError::failed(
                    self.role,
                    format!("could not remove permission channel: {error}"),
                    1,
                )
            });
        if failure.is_some() {
            self.outcome = Some("failed".to_owned());
            self.reason = Some("permission-channel-cleanup".to_owned());
        }
        let outcome = terminal_outcome(self.outcome.clone(), thread::panicking());
        if self.started {
            let now = self.clock.epoch_seconds();
            let mut fact = Map::new();
            fact.insert("owner".to_owned(), json!(self.owner));
            fact.insert("outcome".to_owned(), json!(outcome));
            fact.insert(
                "cost_usd".to_owned(),
                self.cost_usd.map_or(Value::Null, |cost| json!(cost)),
            );
            // A fixed injected clock pins the duration too. Production passes use
            // a realtime clock; deterministic callers can inject a fixed instant.
            fact.insert(
                "duration_seconds".to_owned(),
                json!(if self.clock.is_fixed() {
                    0
                } else {
                    now.saturating_sub(self.started_epoch)
                }),
            );
            if let Some(reason) = &self.reason {
                fact.insert("reason".to_owned(), json!(reason));
            }
            if let Some(hash) = &self.dispatchability_hash {
                fact.insert("dispatchability_hash".to_owned(), json!(hash));
            }
            if let Some(count) = self.queue_count {
                fact.insert("queue_count".to_owned(), json!(count));
            }
            if let Some(count) = self.dispatchable_count {
                fact.insert("dispatchable_count".to_owned(), json!(count));
            }
            if let Some(repositories) = &self.repositories {
                fact.insert("repositories".to_owned(), json!(repositories));
            }
            if !self.skipped_repositories.is_empty() {
                fact.insert(
                    "skipped_repositories".to_owned(),
                    json!(self.skipped_repositories),
                );
            }
            if let Err(error) = append_trace(
                &self.paths.trace_file(),
                &TraceAppend {
                    ts: self.trace_time.clone(),
                    kind: "pass-ended".to_owned(),
                    fact,
                    narration: Map::new(),
                },
            ) {
                failure = Some(PassError::failed(
                    self.role,
                    format!("could not append pass-ended: {error}"),
                    1,
                ));
            }
            self.started = false;
        }
        let event_outcome = event_outcome(&outcome);
        let event_reason = event_reason(event_outcome.clone(), self.reason.clone());
        let event_result = if let Some(control) = &mut self.control {
            self.events
                .process_exited(
                    control,
                    self.process_exit,
                    event_outcome,
                    event_reason,
                    self.cost_usd,
                    None,
                )
                .map_err(|error| error.to_string())
        } else {
            self.events
                .finish(event_outcome, event_reason, self.cost_usd, None)
                .map_err(|error| error.to_string())
        };
        if let Err(error) = event_result
            && failure.is_none()
        {
            failure = Some(PassError::failed(
                self.role,
                format!("could not append run.finished: {error}"),
                1,
            ));
        }
        if self.child_spawned {
            release_inner_lease(self);
        }
        if self.lease.release().is_err() && failure.is_none() {
            failure = Some(PassError::failed(
                self.role,
                "could not release pass lease",
                1,
            ));
        }
        failure.map_or(Ok(()), Err)
    }
}

fn event_outcome(outcome: &str) -> EventRunOutcome {
    match outcome {
        "completed" => EventRunOutcome::Completed,
        "no-op" | "no-candidates" => EventRunOutcome::NoOp,
        // The fact ledger calls a spend refusal held; ethogram calls it blocked.
        "held" => EventRunOutcome::Blocked,
        "timed-out" => EventRunOutcome::TimedOut,
        "capped" => EventRunOutcome::Capped,
        "permission-denied" => EventRunOutcome::PermissionDenied,
        "interrupted" => EventRunOutcome::Interrupted,
        "canceled" => EventRunOutcome::Canceled,
        // A run whose process never ran, such as ostrom#587's harness refusal.
        // Without this arm it would fall through to `Failed`, which ethogram
        // reserves for a process that ran and did not succeed.
        "unstarted" => EventRunOutcome::Unstarted,
        _ => EventRunOutcome::Failed,
    }
}

fn event_reason(outcome: EventRunOutcome, reason: Option<String>) -> Option<String> {
    if outcome == EventRunOutcome::Blocked && reason.as_deref() == Some("daily-cap") {
        return Some("budget".to_owned());
    }
    reason.or_else(|| matches!(outcome, EventRunOutcome::Failed).then(|| "pass-failed".to_owned()))
}

fn wire_ceilings(caps: RunCaps) -> Option<ethogram::RunCeilings> {
    (caps.wall_ms.is_some()
        || caps.idle_ms.is_some()
        || caps.turns.is_some()
        || caps.tokens.is_some()
        || caps.cost_usd.is_some())
    .then(|| caps.to_wire())
}

fn refuse_empty_repository_scope(
    request: &PassRequest,
    events: &mut RunEventGuard,
) -> Result<(), PassError> {
    let owner = events.run_id().to_owned();
    let repositories = request.repositories.as_ref().cloned().unwrap_or_default();
    let common = Map::from_iter([
        ("owner".to_owned(), json!(owner)),
        ("repositories".to_owned(), json!(repositories)),
        (
            "skipped_repositories".to_owned(),
            json!(request.skipped_repositories),
        ),
        ("reason".to_owned(), json!("no-effective-repositories")),
    ]);
    append_trace(
        &request.paths.trace_file(),
        &TraceAppend {
            ts: request.clock.timestamp(),
            kind: "pass-started".to_owned(),
            fact: common.clone(),
            narration: Map::new(),
        },
    )
    .map_err(|error| {
        PassError::failed(
            request.role,
            format!("could not append pass-started: {error}"),
            1,
        )
    })?;
    let mut terminal = common;
    terminal.insert("outcome".to_owned(), json!("failed"));
    terminal.insert("cost_usd".to_owned(), json!(0.0));
    terminal.insert("duration_seconds".to_owned(), json!(0));
    append_trace(
        &request.paths.trace_file(),
        &TraceAppend {
            ts: request.clock.timestamp(),
            kind: "pass-ended".to_owned(),
            fact: terminal,
            narration: Map::new(),
        },
    )
    .map_err(|error| {
        PassError::failed(
            request.role,
            format!("could not append pass-ended: {error}"),
            1,
        )
    })?;
    events
        .finish(
            EventRunOutcome::Failed,
            Some("no-effective-repositories".to_owned()),
            Some(0.0),
            None,
        )
        .map_err(|error| {
            PassError::failed(
                request.role,
                format!("could not append run.finished: {error}"),
                1,
            )
        })?;
    Err(PassError::NoEffectiveRepositories(request.role.name()))
}

impl Drop for PassGuard {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

/// The result of resolving a derived (policy-adopted) profile into settings
/// for this run: either a live bridge, or ostrom#544's fallback for a
/// platform with no private-channel form.
enum DerivedRunSettings {
    Bridged(crate::permission_bridge::PermissionBridge),
    Fallback { path: PathBuf, warning: EventDraft },
}

/// Writes the derived profile for one run, never the shared
/// `roles/<role>.derived.settings.json` two concurrent passes of one role
/// would overwrite. This is the ostrom#544 fallback's settings write; the
/// bridge writes its own copy under its private channel directory instead.
fn write_fallback_settings(
    run_directory: &Path,
    run_id: &str,
    derived: &str,
) -> io::Result<PathBuf> {
    fs::create_dir_all(run_directory)?;
    let path = run_directory.join(format!(
        "{}.settings.json",
        umwelt_runtime::run_directory_name(run_id)
    ));
    fs::write(&path, derived)?;
    Ok(path)
}

/// Whether `platform` can host a bridge decides the whole shape of this
/// run's settings: a bridge and its per-run MCP config when it can, or
/// ostrom#544's fallback -- still per-run settings, no MCP config, one named
/// warning -- when it cannot. Pure aside from the filesystem writes either
/// branch makes, and takes `platform` as an explicit argument rather than
/// reading `std::env::consts::OS` itself, so the fallback is unit-testable by
/// injecting an unsupported name instead of requiring a Windows host.
fn resolve_derived_settings(
    run_directory: &Path,
    run_id: &str,
    derived: &str,
    executable: &Path,
    platform: &str,
) -> io::Result<DerivedRunSettings> {
    if crate::permission_bridge::platform_supports_bridge(platform) {
        let bridge = crate::permission_bridge::PermissionBridge::create(
            run_directory,
            run_id,
            derived,
            executable,
        )?;
        Ok(DerivedRunSettings::Bridged(bridge))
    } else {
        let path = write_fallback_settings(run_directory, run_id, derived)?;
        Ok(DerivedRunSettings::Fallback {
            path,
            warning: crate::permission_bridge::platform_fallback_warning(platform),
        })
    }
}

pub fn run_pass(request: &PassRequest) -> Result<(), PassError> {
    run_pass_with_bridge_probe_timeout(
        request,
        BRIDGE_HARNESS_PROBE_TIMEOUT,
        Duration::from_secs(SWEEP_LEASE_CEILING_SECONDS),
    )
}

fn probe_bridge_harness_version(
    claude_bin: &Path,
    timeout: Duration,
) -> Result<crate::permission_bridge::HarnessVersion, String> {
    let mut stdout = tempfile::tempfile()
        .map_err(|error| format!("version stdout could not be captured: {error}"))?;
    let mut stderr = tempfile::tempfile()
        .map_err(|error| format!("version stderr could not be captured: {error}"))?;
    let child_stdout = stdout
        .try_clone()
        .map_err(|error| format!("version stdout could not be captured: {error}"))?;
    let child_stderr = stderr
        .try_clone()
        .map_err(|error| format!("version stderr could not be captured: {error}"))?;
    let mut child = Command::new(claude_bin)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::from(child_stdout))
        .stderr(Stdio::from(child_stderr))
        .spawn()
        .map_err(|error| format!("version probe could not start: {error}"))?;
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("version probe could not be waited for: {error}"));
            }
        }
        if started.elapsed() >= timeout {
            // This probe is not an agent session and gets no process group. Kill
            // exactly the child PID so the ten-second compatibility check cannot
            // turn into an unbounded pre-launch wait (ostrom#587).
            let kill_result = child.kill();
            let _ = child.wait();
            return Err(kill_result.map_or_else(
                |error| {
                    format!(
                        "version probe timed out after {} ms and could not be killed: {error}",
                        timeout.as_millis()
                    )
                },
                |()| format!("version probe timed out after {} ms", timeout.as_millis()),
            ));
        }
        thread::sleep(Duration::from_millis(10));
    };

    let mut stdout_bytes = Vec::new();
    stdout
        .seek(SeekFrom::Start(0))
        .and_then(|_| stdout.read_to_end(&mut stdout_bytes))
        .map_err(|error| format!("version stdout could not be read: {error}"))?;
    let mut stderr_bytes = Vec::new();
    stderr
        .seek(SeekFrom::Start(0))
        .and_then(|_| stderr.read_to_end(&mut stderr_bytes))
        .map_err(|error| format!("version stderr could not be read: {error}"))?;
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr_bytes);
        let detail = stderr.lines().next().unwrap_or_default().trim();
        return Err(if detail.is_empty() {
            format!("version probe exited {status}")
        } else {
            format!("version probe exited {status}: {detail}")
        });
    }
    let stdout = String::from_utf8(stdout_bytes)
        .map_err(|_| "version stdout was not valid UTF-8".to_owned())?;
    crate::permission_bridge::parse_harness_version(&stdout).map_err(|error| {
        let first_line = stdout.lines().next().unwrap_or_default();
        format!("version output could not be parsed ({error}): {first_line:?}")
    })
}

fn run_pass_with_bridge_probe_timeout(
    request: &PassRequest,
    bridge_probe_timeout: Duration,
    sweep_wait: Duration,
) -> Result<(), PassError> {
    let mut events = RunEventGuard::start(
        &request.paths,
        request.events_fd,
        request.facts_only,
        request.clock.clone(),
        RunEventStart {
            run_id: generated_run_id(request.role.name(), &request.clock),
            // The pass is still a one-off handoff when it is bound to a loop
            // declaration. The optional repository list below carries that
            // binding without inventing a schedule value at this layer.
            kind: RunKind::Handoff,
            actor: request.role.name().to_owned(),
            harness: "claude".to_owned(),
            model: None,
            schedule: None,
            repository: None,
            repositories: request.repositories.clone(),
            // No `PassRequest` field carries an order id or equivalent intent
            // reference (unlike `implement.rs`'s `order.order_id`), so this
            // stays `None` rather than being synthesised from the role or the
            // prompt.
            work_order: None,
            ceilings: wire_ceilings(request.caps),
        },
    )
    .map_err(|error| {
        PassError::failed(
            request.role,
            format!("could not append run.started: {error}"),
            1,
        )
    })?;
    if request.repositories.as_ref().is_some_and(Vec::is_empty) {
        return refuse_empty_repository_scope(request, &mut events);
    }
    let mut watchdog = CapsWatchdog::start(
        request.caps,
        SystemClock::default(),
        events.sink(),
        events.run_id(),
    )
    .map_err(|error| {
        PassError::failed(
            request.role,
            format!("could not start run watchdog: {error}"),
            1,
        )
    })?;
    if let Err(error) = validate_arm(request) {
        events
            .finish(
                EventRunOutcome::NoOp,
                Some("disarmed".to_owned()),
                Some(0.0),
                None,
            )
            .map_err(|event_error| {
                PassError::failed(
                    request.role,
                    format!("could not append run.finished: {event_error}"),
                    1,
                )
            })?;
        return Err(error);
    }
    fs::create_dir_all(&request.paths.state).map_err(|error| {
        PassError::failed(
            request.role,
            format!("could not create state directory: {error}"),
            1,
        )
    })?;
    let lease_now = request.clock.epoch_seconds();
    let started_epoch = request.clock.epoch_seconds();
    let lease_name = format!("{}-pass.lease", request.role.name());
    let ttl =
        positive_env(environment::MANDATE_LEASE_TTL_SECONDS).unwrap_or(DEFAULT_LEASE_TTL_SECONDS);

    let prior = read_pass_state(&request.paths.state, request.role.name())
        .map_err(|error| PassError::failed(request.role, error.to_string(), 1))?;
    let mut state = prior.unwrap_or_else(|| PassState {
        role_id: generated_role_id(&request.clock),
        wake: 0,
        dispatchability_hash: None,
    });
    let next_wake = state.wake.saturating_add(1);
    let owner = format!("{}-{}-wake{next_wake}", request.role.name(), state.role_id);
    let lease = match OwnedLease::acquire(&request.paths.state, &lease_name, &owner, lease_now, ttl)
    {
        Ok(lease) => lease,
        Err(
            LeaseActionError::Held
            | LeaseActionError::HeldOrUnreadable
            | LeaseActionError::ReclamationInProgress
            | LeaseActionError::ChangedDuringReclamation
            | LeaseActionError::AcquiredConcurrently,
        ) => {
            events
                .finish(
                    EventRunOutcome::NoOp,
                    Some("lease-held".to_owned()),
                    Some(0.0),
                    None,
                )
                .map_err(|error| {
                    PassError::failed(
                        request.role,
                        format!("could not append run.finished: {error}"),
                        1,
                    )
                })?;
            return Err(PassError::LeaseHeld(request.role.name()));
        }
        Err(error) => {
            return Err(PassError::failed(
                request.role,
                format!("could not acquire {lease_name}: {error:?}"),
                1,
            ));
        }
    };
    state.wake = next_wake;
    write_pass_state(&request.paths.state, request.role.name(), &state)
        .map_err(|error| PassError::failed(request.role, error.to_string(), 1))?;

    let trace_time = request.clock.timestamp();
    let mut guard = PassGuard {
        role: request.role,
        paths: request.paths.clone(),
        lease,
        owner: owner.clone(),
        started_epoch,
        trace_time,
        started: false,
        child_spawned: false,
        outcome: None,
        reason: None,
        cost_usd: None,
        clock: request.clock.clone(),
        dispatchability_hash: None,
        queue_count: None,
        dispatchable_count: None,
        repositories: request.repositories.clone(),
        skipped_repositories: request.skipped_repositories.clone(),
        events,
        control: None,
        process_exit: ProcessExit::Abnormal,
        permission_bridge: None,
    };
    let prepared = prepare_sweep(request, sweep_wait).and_then(|sweep| {
        session_prompt(request, &sweep)
            .map(|session| (sweep, session))
            .map_err(PrepareSweepError::Other)
    });
    let (sweep, session) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            let (reason, exit_code) = sweep_failure_status(&error);
            let mut fact = Map::from_iter([
                ("owner".to_owned(), json!(owner)),
                ("sweep".to_owned(), json!("failed")),
                ("reason".to_owned(), json!(reason)),
            ]);
            if let Some(repositories) = &request.repositories {
                fact.insert("repositories".to_owned(), json!(repositories));
            }
            if !request.skipped_repositories.is_empty() {
                fact.insert(
                    "skipped_repositories".to_owned(),
                    json!(request.skipped_repositories),
                );
            }
            append_trace(
                &request.paths.trace_file(),
                &TraceAppend {
                    ts: guard.trace_time.clone(),
                    kind: "pass-started".to_owned(),
                    fact,
                    narration: Map::new(),
                },
            )
            .map_err(|trace_error| {
                PassError::failed(
                    request.role,
                    format!("could not append pass-started: {trace_error}"),
                    1,
                )
            })?;
            guard.started = true;
            guard.outcome = Some("failed".to_owned());
            guard.reason = Some(reason.to_owned());
            guard.cost_usd = Some(0.0);
            guard.finish()?;
            return Err(PassError::failed(
                request.role,
                format!("{reason}: {error}"),
                exit_code,
            ));
        }
    };
    let mut start_fact = Map::from_iter([("owner".to_owned(), json!(owner))]);
    sweep.outcome.record(&mut start_fact);
    if let Some(repositories) = &request.repositories {
        start_fact.insert("repositories".to_owned(), json!(repositories));
    }
    if !request.skipped_repositories.is_empty() {
        start_fact.insert(
            "skipped_repositories".to_owned(),
            json!(request.skipped_repositories),
        );
    }
    append_trace(
        &request.paths.trace_file(),
        &TraceAppend {
            ts: guard.trace_time.clone(),
            kind: "pass-started".to_owned(),
            fact: start_fact,
            narration: Map::new(),
        },
    )
    .map_err(|error| {
        PassError::failed(
            request.role,
            format!("could not append pass-started: {error}"),
            1,
        )
    })?;
    guard.started = true;
    if request.repositories.is_some()
        && request.role == PassRole::Gatekeeper
        && session.candidate_count == Some(0)
    {
        guard.outcome = Some("no-candidates".to_owned());
        guard.cost_usd = Some(0.0);
        guard.finish()?;
        return Ok(());
    }
    let watermark = read_trace(&request.paths.trace_file())
        .map_err(|error| PassError::failed(request.role, error.to_string(), 1))?
        .rows
        .len();

    check_signal(request, &mut guard, None, &watchdog)?;
    // A profile derived from the actor's policy grants wins over a
    // hand-maintained file: the grant is the authorization, and a settings file
    // beside it is a copy that can drift out of agreement with the policy it is
    // supposed to express. The hand-written file remains the path for an
    // operator who has adopted no policy.
    let roles = request.paths.state.join("roles");
    let settings = if let Some(derived) = &request.derived_settings {
        guard.outcome = Some("failed".to_owned());
        let directory = request
            .paths
            .runs_dir()
            .join(umwelt_runtime::run_directory_name(guard.events.run_id()));
        let executable = std::env::current_exe()
            .map_err(|error| PassError::failed(request.role, error.to_string(), 1))?;
        match resolve_derived_settings(
            &directory,
            guard.events.run_id(),
            derived,
            &executable,
            request.platform,
        )
        .map_err(|error| {
            PassError::failed(
                request.role,
                format!("could not prepare permission settings: {error}"),
                1,
            )
        })? {
            DerivedRunSettings::Bridged(bridge) => {
                let path = bridge.settings_path().to_owned();
                guard.permission_bridge = Some(bridge);
                path
            }
            DerivedRunSettings::Fallback { path, warning } => {
                // ostrom#544: this platform has no private-channel form (only
                // Linux and macOS do), so the pass proceeds as it did before
                // #541 -- `guard.permission_bridge` stays `None`, which is
                // already what makes a live answer on the control descriptor
                // fall through to the same `unsupported` refusal any other
                // control verb this pass does not implement receives, and
                // already what keeps `--mcp-config` and friends off the
                // harness invocation below. The warning says why, by name.
                guard.events.append(warning).map_err(|error| {
                    PassError::failed(
                        request.role,
                        format!("could not append permission-bridge warning: {error}"),
                        1,
                    )
                })?;
                path
            }
        }
    } else {
        let path = roles.join(format!("{}.settings.json", request.role.name()));
        if !path.is_file() {
            guard.outcome = Some("failed".to_owned());
            return Err(PassError::failed(
                request.role,
                format!("{} missing", path.display()),
                1,
            ));
        }
        path
    };
    if !is_executable_file(&request.claude_bin) {
        guard.outcome = Some("failed".to_owned());
        return Err(PassError::failed(
            request.role,
            format!("{} is not marked executable", request.claude_bin.display()),
            1,
        ));
    }
    if guard.permission_bridge.is_some() {
        let probe = probe_bridge_harness_version(&request.claude_bin, bridge_probe_timeout);
        let refusal = match probe {
            Ok(version) if version < crate::permission_bridge::MIN_BRIDGE_HARNESS_VERSION => {
                Some(PassError::harness_unsupported(
                    request.role,
                    &request.claude_bin,
                    version.to_string(),
                ))
            }
            Ok(_) => None,
            Err(reason) => Some(PassError::harness_unsupported(
                request.role,
                &request.claude_bin,
                format!("version could not be established ({reason})"),
            )),
        };
        if let Some(error) = refusal {
            // `unstarted`, not `no-op`. In ethogram's closed outcome set, `no-op`
            // says the run went ahead and found nothing to do, and `failed` says
            // the process ran and did not succeed. Neither is true here:
            // `run.started` was emitted and the agent was never spawned, which is
            // exactly what `unstarted` means, with `reason` naming why. A harness
            // that cannot run the bridge is a broken environment, and it must not
            // read as a quiet pass to anything folding these records (ostrom#587).
            guard.outcome = Some("unstarted".to_owned());
            guard.reason = Some(crate::permission_bridge::HARNESS_UNSUPPORTED_REASON.to_owned());
            guard.cost_usd = Some(0.0);
            guard.finish()?;
            return Err(error);
        }
    }
    let spent = daily_spend(&request.paths, &request.clock.date());
    let cap = daily_cap();
    if spent >= cap {
        guard.outcome = Some("held".to_owned());
        guard.reason = Some("daily-cap".to_owned());
        let decision = crate::budget::decision_request(
            &request.paths,
            &request.clock,
            cap,
            &format!("{spent} USD spent"),
        );
        guard
            .events
            .request_decision(&request.paths, &guard.trace_time, &decision)
            .map_err(|error| {
                PassError::failed(
                    request.role,
                    format!("could not emit budget decision: {error}"),
                    1,
                )
            })?;
        guard.finish()?;
        return Err(PassError::BudgetHeld(request.role.name()));
    }
    if request.role == PassRole::Builder {
        let previous_hash = state.dispatchability_hash.clone();
        if let Ok(snapshot) = dispatchability_snapshot(&request.paths, &request.working_directory) {
            guard.dispatchability_hash = Some(snapshot.hash.clone());
            guard.queue_count = Some(snapshot.queue_count);
            guard.dispatchable_count = Some(snapshot.dispatchable_count);
            if snapshot.dispatchable_count == 0
                && previous_hash.as_deref() == Some(snapshot.hash.as_str())
            {
                guard.outcome = Some("no-op".to_owned());
                guard.reason = Some("no-dispatchable-work-unchanged".to_owned());
                guard.cost_usd = Some(0.0);
                guard.finish()?;
                return Ok(());
            }
        }
    }

    let run_dir = request
        .paths
        .state
        .join("pass-runs")
        .join(request.role.name());
    fs::create_dir_all(&run_dir).map_err(|error| {
        PassError::failed(
            request.role,
            format!("could not create run directory: {error}"),
            1,
        )
    })?;
    let log = run_dir.join(format!(
        "{}-{owner}.jsonl",
        request.clock.now().format("%Y%m%dT%H%M%SZ")
    ));
    let output = fs::File::create(&log).map_err(|error| {
        PassError::failed(
            request.role,
            format!("could not create transcript: {error}"),
            1,
        )
    })?;
    let error_output = output.try_clone().map_err(|error| {
        PassError::failed(
            request.role,
            format!("could not clone transcript: {error}"),
            1,
        )
    })?;
    let mut command = Command::new(&request.claude_bin);
    command
        .args([
            "--print",
            "--settings",
            &settings.display().to_string(),
            "--permission-mode",
            permission_mode_flag(request.permission_mode),
            "--output-format",
            "stream-json",
            "--verbose",
            "--max-turns",
            MAX_TURNS,
            &session.prompt,
        ])
        // The harness reads its inherited stdin to EOF even when the prompt
        // is an argument, and a supervisor that follows docs/pass-control.md
        // hands us the control descriptor as a dup of fd 0 -- so an inherited
        // stdin lets the harness eat the principal's answer, racing our own
        // reader for it. Three of five real passes lost their control that
        // way (#528). The pass never writes to the harness's stdin.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(error_output));
    if let Some(bridge) = &guard.permission_bridge {
        bridge.configure(&mut command);
    }
    if let Some(repositories) = &request.repository_scope {
        command.env(
            environment::OSTROM_EFFECTIVE_REPOSITORIES.name,
            repositories.join(","),
        );
    }
    set_process_group(&mut command);
    let mut child = command.spawn().map_err(|error| {
        PassError::failed(request.role, format!("could not start Claude: {error}"), 1)
    })?;
    guard.child_spawned = true;
    guard.control = Some(RunControl::new(
        guard.events.run_id(),
        None,
        request.caps,
        NoSteer,
    ));
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| PassError::failed(request.role, "Claude stdout pipe was unavailable", 1))?;
    let (capture, capture_thread) = capture_stdout(stdout, output);
    let wait_result = wait_for_child(request, &mut guard, &mut child, &mut watchdog, &capture);
    if wait_result.is_err() && child.try_wait().ok().flatten().is_none() {
        terminate_child_process_group(&mut child, PASS_TERMINATION_GRACE);
        let _ = child.wait();
    }
    let capture_result = capture_thread
        .join()
        .map_err(|_| PassError::failed(request.role, "Claude transcript capture panicked", 1))?;
    capture_result.map_err(|error| {
        PassError::failed(
            request.role,
            format!("could not write Claude transcript: {error}"),
            1,
        )
    })?;
    let status = wait_result?;
    guard.process_exit = if status.success() {
        ProcessExit::Normal
    } else {
        ProcessExit::Abnormal
    };
    let transcript = read_transcript(&log);
    guard.cost_usd = transcript.cost_usd;
    reconcile_outcome(&mut guard, watermark, status, transcript.permission_denied);
    if status.success()
        && !matches!(
            guard.outcome.as_deref(),
            Some("failed" | "permission-denied")
        )
        && let Some(hash) = &guard.dispatchability_hash
    {
        state.dispatchability_hash = Some(hash.clone());
        if let Err(error) = write_pass_state(&request.paths.state, request.role.name(), &state) {
            guard.outcome = Some("failed".to_owned());
            return Err(PassError::failed(request.role, error.to_string(), 1));
        }
    }
    prune_transcripts(&run_dir);
    let code = status.code().unwrap_or(1);
    // Capture the terminal outcome `guard.finish()` is about to write, before
    // it runs: `finish()` only overwrites `guard.outcome` on its own internal
    // failure (permission-channel cleanup), a branch that always returns
    // `Err` and so never reaches the `?` below. `thread::panicking()` mirrors
    // exactly what `finish()` itself will fall back to when no explicit
    // outcome was recorded (`terminal_outcome`'s only other caller), so this
    // is the same string that lands in the `pass-ended` fact and on the wire.
    let recorded_outcome = terminal_outcome(guard.outcome.clone(), thread::panicking());
    guard.finish()?;
    if status.success() {
        // ostrom#478/#485's defect class, third instance: the agent process
        // can exit 0 while its own pass-ended report says it failed (a real
        // 2026-09-10 builder pass did exactly this, recorded
        // `failed-repair-scan` with `exit_code: 1` and still returned 0). A
        // scheduler reads exit status first, so a recorded failure must make
        // this process exit non-zero even though Claude itself did not fail.
        // `event_outcome` is the one place that decides "failed" for the
        // wire; reusing it here, rather than a second list of failure
        // strings, is what keeps the exit status and the wire from drifting
        // apart (repo principle 6).
        // `Unstarted` counts too. Before ostrom#587 gave it an arm,
        // `event_outcome` sent an unrecognised `unstarted` to its `Failed`
        // catch-all, so a recorded `unstarted` already exited non-zero here.
        // A run that did start cannot honestly record that it did not, and
        // the new arm must not turn that into a silent exit 0.
        if matches!(
            event_outcome(&recorded_outcome),
            EventRunOutcome::Failed | EventRunOutcome::Unstarted
        ) {
            Err(PassError::failed(
                request.role,
                format!(
                    "Claude run exited 0 but the pass recorded outcome {recorded_outcome:?}{}; transcript at {}",
                    guard
                        .reason
                        .as_deref()
                        .map_or_else(String::new, |reason| format!(" (reason: {reason})")),
                    log.display()
                ),
                1,
            ))
        } else {
            Ok(())
        }
    } else {
        Err(PassError::failed(
            request.role,
            format!(
                "Claude run failed (rc={code}); transcript at {}",
                log.display()
            ),
            code,
        ))
    }
}

fn validate_arm(request: &PassRequest) -> Result<(), PassError> {
    let path = request.paths.state.join("loop-armed");
    let Ok(contents) = fs::read_to_string(&path) else {
        return Err(PassError::Disarmed(request.role.name()));
    };
    if contents.is_empty() {
        return Ok(());
    }
    let value = contents.strip_suffix('\n').unwrap_or(&contents);
    if value.contains('\n') {
        return Err(PassError::Disarmed(request.role.name()));
    }
    if !valid_arm_expiry(value) {
        return Err(PassError::Disarmed(request.role.name()));
    }
    let expiry = DateTime::parse_from_rfc3339(value)
        .map_err(|_| PassError::Disarmed(request.role.name()))?;
    if expiry.timestamp() <= request.clock.epoch_seconds() as i64 {
        return Err(PassError::Disarmed(request.role.name()));
    }
    Ok(())
}

fn valid_arm_expiry(value: &str) -> bool {
    let bytes = value.as_bytes();
    let punctuation = matches!(bytes.len(), 20 | 25)
        && bytes.get(4) == Some(&b'-')
        && bytes.get(7) == Some(&b'-')
        && bytes.get(10) == Some(&b'T')
        && bytes.get(13) == Some(&b':')
        && bytes.get(16) == Some(&b':')
        && (bytes.get(19) == Some(&b'Z')
            || (matches!(bytes.get(19), Some(b'+' | b'-')) && bytes.get(22) == Some(&b':')));
    punctuation
        && bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19 | 22) || byte.is_ascii_digit()
        })
}

fn wait_for_child(
    request: &PassRequest,
    guard: &mut PassGuard,
    child: &mut Child,
    watchdog: &mut CapsWatchdog<SystemClock>,
    capture: &Receiver<Result<String, CaptureFault>>,
) -> Result<ExitStatus, PassError> {
    let mut normaliser = ClaudeNormaliser::new();
    let mut capture_open = true;
    let mut normaliser_finished = false;
    let mut status = None;
    let control_input = request.control_fd.map(pass_control::read_control);

    loop {
        if let Some(bridge) = &mut guard.permission_bridge {
            bridge.poll(&guard.events).map_err(|error| {
                PassError::failed(guard.role, format!("permission event: {error}"), 1)
            })?;
        }
        if capture_open {
            match capture.recv_timeout(Duration::from_millis(50)) {
                Ok(Ok(raw)) => {
                    let drafts = match normaliser.line(&raw) {
                        Ok(drafts) => drafts,
                        Err(fault) => vec![capture_refused_draft(guard.events.run_id(), &fault)],
                    };
                    if let Some(trip) = append_observed(guard, watchdog, drafts)? {
                        return Err(apply_cap_trip(request, guard, child, trip));
                    }
                }
                Ok(Err(fault)) => {
                    if let Some(trip) = append_observed(
                        guard,
                        watchdog,
                        vec![capture_refused_draft(guard.events.run_id(), &fault)],
                    )? {
                        return Err(apply_cap_trip(request, guard, child, trip));
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => capture_open = false,
            }
        } else {
            thread::sleep(Duration::from_millis(50));
        }

        if !capture_open && !normaliser_finished {
            let drafts = match normaliser.finish() {
                Ok(drafts) => drafts,
                Err(fault) => vec![capture_refused_draft(guard.events.run_id(), &fault)],
            };
            normaliser_finished = true;
            if let Some(trip) = append_observed(guard, watchdog, drafts)? {
                return Err(apply_cap_trip(request, guard, child, trip));
            }
        }

        if let Some(input) = &control_input
            && let Ok(input) = input.try_recv()
        {
            match input {
                ControlInput::Request(input) if input.kind == ControlKind::Interrupt => {
                    let control = guard.control.as_mut().expect("spawned pass has RunControl");
                    guard
                        .events
                        .interrupt_from_descriptor(control, input, child, watchdog)
                        .map_err(|error| {
                            PassError::failed(
                                guard.role,
                                format!("could not apply descriptor interrupt: {error}"),
                                1,
                            )
                        })?;
                    guard.outcome = Some("interrupted".to_owned());
                    return Err(PassError::failed(
                        guard.role,
                        "interrupted by control descriptor",
                        130,
                    ));
                }
                ControlInput::Request(input)
                    if input.kind == ControlKind::Answer && guard.permission_bridge.is_some() =>
                {
                    guard
                        .permission_bridge
                        .as_mut()
                        .expect("bridge present")
                        .answer(&guard.events, input)
                        .map_err(|error| {
                            PassError::failed(guard.role, format!("permission answer: {error}"), 1)
                        })?;
                }
                ControlInput::Request(input) => {
                    // NoSteer cannot resume this pass. Refuse immediately; the
                    // runtime's steer method would queue until process exit.
                    for draft in pass_control::unsupported(&input) {
                        guard.events.append(draft).map_err(|error| {
                            PassError::failed(
                                guard.role,
                                format!("could not record control refusal: {error}"),
                                1,
                            )
                        })?;
                    }
                }
                ControlInput::Refused(detail) => {
                    eprintln!("ostrom control: {detail}");
                    guard
                        .events
                        .append(pass_control::refuse_input(guard.events.run_id(), &detail))
                        .map_err(|error| {
                            PassError::failed(
                                guard.role,
                                format!("could not record control refusal: {error}"),
                                1,
                            )
                        })?;
                }
                ControlInput::Ended(cause) => {
                    // Non-terminal: the descriptor stopped, the pass has not.
                    // #528's incident is exactly the absence of this branch.
                    eprintln!("ostrom control: {cause}");
                    guard
                        .events
                        .append(pass_control::ended(&cause))
                        .map_err(|error| {
                            PassError::failed(
                                guard.role,
                                format!("could not record control descriptor termination: {error}"),
                                1,
                            )
                        })?;
                }
            }
        }

        if status.is_none() {
            status = child.try_wait().map_err(|error| {
                PassError::failed(
                    request.role,
                    format!("could not wait for Claude: {error}"),
                    1,
                )
            })?;
            if status.is_some() {
                kill_remaining_process_group(child.id());
            }
        }

        if status.is_none() {
            check_signal(request, guard, Some(child), watchdog)?;
        }

        if let Some(trip) = watchdog.check() {
            return Err(apply_cap_trip(request, guard, child, trip));
        }

        if !capture_open && let Some(status) = status {
            return Ok(status);
        }
    }
}

fn capture_stdout(
    stdout: std::process::ChildStdout,
    transcript: fs::File,
) -> (
    Receiver<Result<String, CaptureFault>>,
    thread::JoinHandle<std::io::Result<()>>,
) {
    let (sender, receiver) = mpsc::channel();
    let thread = thread::spawn(move || {
        copy_capture_lines(ChildStdoutSource::new(stdout), transcript, |line| {
            let _ = sender.send(line);
        })
    });
    (receiver, thread)
}

fn copy_capture_lines(
    source: impl LineSource,
    mut transcript: impl Write,
    mut send: impl FnMut(Result<String, CaptureFault>),
) -> std::io::Result<()> {
    let mut write_fault = None;
    for line in source {
        if let Ok(raw) = &line
            && write_fault.is_none()
            && let Err(error) = transcript
                .write_all(raw.as_bytes())
                .and_then(|()| transcript.write_all(b"\n"))
        {
            write_fault = Some(error);
        }
        send(line);
    }
    if let Some(error) = write_fault {
        Err(error)
    } else {
        transcript.flush()
    }
}

fn append_observed(
    guard: &PassGuard,
    watchdog: &mut CapsWatchdog<SystemClock>,
    drafts: Vec<EventDraft>,
) -> Result<Option<CapTrip>, PassError> {
    for draft in drafts {
        let source_type = draft.event_type.clone();
        let event = guard.events.append(draft).or_else(|error| {
            let refusal =
                sink_refused_draft(guard.role, guard.events.run_id(), source_type, error)?;
            // Validation refused before writing or consuming a sequence. Record
            // the loss on this run and keep observing the remaining drafts.
            // A failure to store the refusal still follows the existing error path.
            guard.events.append(refusal).map_err(|error| {
                PassError::failed(
                    guard.role,
                    format!("could not append captured agent event: {error}"),
                    1,
                )
            })
        })?;
        if let Some(trip) = watchdog.observe(&event).map_err(|error| {
            PassError::failed(
                guard.role,
                format!("could not observe captured agent event: {error}"),
                1,
            )
        })? {
            return Ok(Some(trip));
        }
    }
    Ok(None)
}

fn sink_refused_draft(
    role: PassRole,
    run_id: &str,
    source_type: String,
    error: RunEventError,
) -> Result<EventDraft, PassError> {
    let mut payload = CaptureRefusedPayload {
        cause: CaptureRefusalCause::Malformed,
        source_run_id: run_id.to_owned(),
        source_seq: None,
        source_type: Some(source_type),
        field: None,
        count: None,
        max: None,
        detail: None,
        truncated: None,
        extra: PayloadExtension::new(),
    };
    match error {
        RunEventError::Sink(SinkFault::OverBound {
            path,
            count,
            max,
            unit,
        }) => {
            payload.cause = CaptureRefusalCause::OverBound;
            payload.field = path;
            payload.count = Some(count as u64);
            payload.max = Some(max as u64);
            // Ethogram's extension preserves the sink's scalar/byte distinction.
            payload.extra.insert("unit".to_owned(), json!(unit));
        }
        RunEventError::Sink(SinkFault::Invalid { path, detail }) => {
            let detail = excerpt(&detail, MAX_EXCERPT_SCALARS);
            payload.field = Some(path);
            payload.detail = Some(detail.text);
            payload.truncated = Some(detail.truncated);
        }
        error => {
            return Err(PassError::failed(
                role,
                format!("could not append captured agent event: {error}"),
                1,
            ));
        }
    }
    Ok(EventDraft {
        event_type: CAPTURE_REFUSED.to_owned(),
        payload: serde_json::to_value(payload).expect("capture.refused payload serialises"),
        captured_at: None,
    })
}

fn capture_refused_draft(run_id: &str, fault: &CaptureFault) -> EventDraft {
    let detail = excerpt(&fault.to_string(), MAX_EXCERPT_SCALARS);
    EventDraft {
        event_type: CAPTURE_REFUSED.to_owned(),
        payload: serde_json::to_value(CaptureRefusedPayload {
            cause: CaptureRefusalCause::Malformed,
            source_run_id: run_id.to_owned(),
            source_seq: None,
            source_type: Some("claude.stream-json".to_owned()),
            field: None,
            count: None,
            max: None,
            detail: Some(detail.text),
            truncated: Some(detail.truncated),
            extra: PayloadExtension::new(),
        })
        .expect("capture.refused payload serialises"),
        captured_at: None,
    }
}

fn apply_cap_trip(
    request: &PassRequest,
    guard: &mut PassGuard,
    child: &mut Child,
    trip: CapTrip,
) -> PassError {
    let cap = trip.cap();
    let finished = trip.finished().clone();
    if let Err(error) = guard.events.terminate_cap(trip, child) {
        return PassError::failed(
            request.role,
            format!(
                "could not append {0} cap terminal event: {error}",
                cap.name()
            ),
            1,
        );
    }
    guard.outcome = Some(finished.outcome.as_str().to_owned());
    guard.reason = finished.reason;
    guard.cost_usd = finished.cost_usd;
    PassError::failed(
        request.role,
        format!("Claude run reached its {} cap", cap.name()),
        1,
    )
}

fn check_signal(
    request: &PassRequest,
    guard: &mut PassGuard,
    child: Option<&mut Child>,
    watchdog: &CapsWatchdog<SystemClock>,
) -> Result<(), PassError> {
    let signal = request.signals.take_pending();
    // A killed supervisor cannot write the signal handoff. Watching the
    // original PID gives its orphaned worker the same bounded cleanup path.
    let orphaned = request
        .supervisor_pid
        .is_some_and(|pid| !process_alive(pid));
    if signal.is_none() && !orphaned {
        return Ok(());
    }
    let name = signal.unwrap_or("TERM");
    guard.outcome = Some(if name == "TERM" {
        "timed-out".to_owned()
    } else {
        "failed".to_owned()
    });
    let code = match name {
        "HUP" => 129,
        "INT" => 130,
        _ => 143,
    };
    if let (Some(child), Some(control)) = (child, &mut guard.control) {
        let request = ControlRequestedPayload {
            control_id: format!(
                "scheduler-sig{}-{}",
                name.to_ascii_lowercase(),
                request.clock.epoch_seconds()
            ),
            kind: ControlKind::Interrupt,
            // Required only for `answer`;
            // this constructs an interrupt, so both stay absent.
            decision_id: None,
            option_id: None,
            text: None,
            truncated: None,
            by: "scheduler".to_owned(),
            extra: PayloadExtension::new(),
        };
        guard
            .events
            .interrupt(control, request, Some(child), watchdog)
            .map_err(|error| {
                PassError::failed(
                    guard.role,
                    format!("could not apply scheduler interrupt: {error}"),
                    1,
                )
            })?;
    }
    Err(PassError::failed(
        request.role,
        format!("received SIG{name}"),
        code,
    ))
}

fn reconcile_outcome(
    guard: &mut PassGuard,
    watermark: usize,
    status: ExitStatus,
    permission_denied: bool,
) {
    if permission_denied {
        guard.outcome = Some("permission-denied".to_owned());
        guard.reason = None;
        return;
    }
    let rows = read_trace(&guard.paths.trace_file())
        .map(|trace| {
            trace
                .rows
                .into_iter()
                .skip(watermark)
                .filter_map(Result::ok)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let prefix = format!("{}-", guard.role.name());
    let inner_start = rows.iter().enumerate().rev().find_map(|(index, row)| {
        (row.kind == "pass-started")
            .then(|| row.fact.get("owner").and_then(Value::as_str))
            .flatten()
            .filter(|owner| owner.starts_with(&prefix) && *owner != guard.owner)
            .map(|owner| (index, owner.to_owned()))
    });
    if let Some((inner_index, inner_owner)) = inner_start {
        guard.outcome = if status.success() {
            rows.iter()
                .skip(inner_index + 1)
                .rev()
                .find(|row| {
                    row.kind == "pass-ended"
                        && row
                            .fact
                            .get("owner")
                            .and_then(Value::as_str)
                            .is_none_or(|owner| owner == inner_owner)
                })
                .and_then(|row| row.fact.get("outcome").and_then(Value::as_str))
                .map(str::to_owned)
                .or_else(|| Some("completed".to_owned()))
        } else {
            Some("failed".to_owned())
        };
    } else if status.success() {
        guard.outcome = Some("no-op".to_owned());
        guard.reason = Some(inner_lease_reason(guard));
    } else {
        guard.outcome = Some("failed".to_owned());
    }
}

fn inner_lease_reason(guard: &PassGuard) -> String {
    let path = guard.paths.state.join(guard.role.inner_lease());
    if read_lease(&path)
        .ok()
        .flatten()
        .is_some_and(|lease| lease.started_at < guard.started_epoch)
    {
        "lease-held".to_owned()
    } else {
        "blocked".to_owned()
    }
}

fn release_inner_lease(guard: &PassGuard) {
    let path = guard.paths.state.join(guard.role.inner_lease());
    let Ok(Some(record)) = read_lease(&path) else {
        return;
    };
    if record.started_at < guard.started_epoch {
        eprintln!(
            "ostrom {} pass: inner lease {} started at {}, before this pass's own start at {}; leaving it to its own owner",
            guard.role.name(),
            guard.role.inner_lease(),
            record.started_at,
            guard.started_epoch
        );
        return;
    }
    eprintln!(
        "ostrom {} pass: releasing inner lease {} held by {} (started_at={}, pass start={})",
        guard.role.name(),
        guard.role.inner_lease(),
        record.owner,
        record.started_at,
        guard.started_epoch
    );
    if let Ok(mut lease) =
        OwnedLease::adopt(&guard.paths.state, guard.role.inner_lease(), &record.owner)
    {
        let _ = lease.release();
    }
}

/// Whether the file is *marked* executable, which is what the message says.
///
/// `is_file()` alone was neither: a path with no execute bit passed the guard
/// and then failed in `Command::spawn`, reporting a permission error against a
/// path the operator has to work backwards from.
///
/// This is deliberately not full `-x` parity. The shell's `-x` is `access(2)`
/// with `X_OK`, which answers "can *this* process execute it" and so accounts
/// for ownership and group; the mode test answers "is it marked executable at
/// all". Closing that gap needs `access(2)`, which is not in std and does not
/// justify a dependency for a diagnostic. The remaining case — marked
/// executable but not executable *by us* — still fails at spawn, exactly as it
/// did before this guard existed. So the message states what is checked rather
/// than implying the stronger claim.
#[cfg(unix)]
pub(crate) fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
pub(crate) fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

fn daily_spend(paths: &OstromPaths, day: &str) -> f64 {
    read_trace(&paths.trace_file())
        .map(|trace| {
            trace
                .rows
                .into_iter()
                .filter_map(Result::ok)
                .filter(|row| row.kind == "pass-ended" && row.ts.starts_with(day))
                .filter_map(|row| row.fact.get("cost_usd").and_then(Value::as_f64).to_owned())
                .sum()
        })
        .unwrap_or_default()
}

fn daily_cap() -> f64 {
    environment::MANDATE_DAILY_CAP_USD
        .value()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite())
        .unwrap_or(DEFAULT_DAILY_CAP_USD)
}

#[derive(Default)]
struct TranscriptSummary {
    cost_usd: Option<f64>,
    permission_denied: bool,
}

fn read_transcript(path: &Path) -> TranscriptSummary {
    let Ok(contents) = fs::read_to_string(path) else {
        return TranscriptSummary::default();
    };
    contents
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .fold(TranscriptSummary::default(), |mut summary, event| {
            if event.get("type").and_then(Value::as_str) == Some("result") {
                summary.cost_usd = event
                    .get("total_cost_usd")
                    .and_then(Value::as_f64)
                    .or(summary.cost_usd);
                summary.permission_denied |= event
                    .get("permission_denials")
                    .and_then(Value::as_array)
                    .is_some_and(|denials| !denials.is_empty());
            }
            summary
        })
}

fn prune_transcripts(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut paths = entries
        .flatten()
        .filter_map(|entry| {
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, entry.path()))
        })
        .collect::<Vec<_>>();
    paths.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    for (_, path) in paths.into_iter().skip(30) {
        let _ = fs::remove_file(path);
    }
}

fn generated_role_id(clock: &Clock) -> String {
    let nanos = clock.now().timestamp_subsec_nanos();
    format!("{:08x}", nanos ^ std::process::id())
}

fn positive_env(variable: environment::EnvironmentVariable) -> Option<u64> {
    variable
        .value()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value > 0)
}

#[cfg(unix)]
fn set_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(not(unix))]
fn set_process_group(_command: &mut Command) {}

pub(crate) fn terminate_child_process_group(child: &mut Child, grace: Duration) -> Option<String> {
    let pid = child.id();
    let group = format!("-{pid}");
    let _ = Command::new(kill_command())
        .args(["-TERM", "--", &group])
        .status();
    let deadline = std::time::Instant::now() + grace;
    while std::time::Instant::now() < deadline {
        let _ = child.try_wait();
        if !process_group_alive(pid) {
            return Some("SIGTERM".to_owned());
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = Command::new(kill_command())
        .args(["-KILL", "--", &group])
        .status();
    // KILL escalation is the operationally significant outcome even if one
    // member of the process group had already stopped cooperatively on TERM.
    Some("SIGKILL".to_owned())
}

pub(crate) fn kill_remaining_process_group(pid: u32) {
    if process_group_alive(pid) {
        let group = format!("-{pid}");
        let _ = Command::new(kill_command())
            .args(["-KILL", "--", &group])
            .status();
    }
}

fn process_group_alive(pid: u32) -> bool {
    Command::new(kill_command())
        .args(["-0", "--", &format!("-{pid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

pub(crate) fn process_alive(pid: u32) -> bool {
    Command::new(kill_command())
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn kill_command() -> &'static str {
    if Path::new("/bin/kill").is_file() {
        "/bin/kill"
    } else {
        "kill"
    }
}

#[cfg(all(test, unix))]
mod executable_tests {
    use std::{fs, os::unix::fs::PermissionsExt};

    use tempfile::tempdir;

    use super::is_executable_file;

    #[test]
    fn a_present_but_unexecutable_file_is_not_accepted() {
        // pass.sh used `-x`. A plain is_file() check would let this through to
        // Command::spawn, which reports a permission error against a path the
        // operator then has to work backwards from.
        let fixture = tempdir().expect("temporary directory");
        let path = fixture.path().join("placeholder-claude");
        fs::write(&path, "#!/usr/bin/env bash\nexit 0\n").expect("write stub");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("drop the mode bits");
        assert!(!is_executable_file(&path));

        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("restore the mode");
        assert!(is_executable_file(&path));
    }

    #[test]
    fn a_directory_is_not_an_executable_file() {
        let fixture = tempdir().expect("temporary directory");
        assert!(!is_executable_file(fixture.path()));
    }
}

#[cfg(test)]
mod sink_refusal_tests {
    use ethogram::{
        AGENT_TEXT, CAPTURE_REFUSED, EventDraft, MAX_EXCERPT_SCALARS, MAX_PAYLOAD_BYTES,
        MAX_TEXT_SCALARS,
    };
    use serde_json::{Value, json};
    use umwelt_runtime::{CapsWatchdog, FileSink, RunCaps, SinkFault, Source, SystemClock};

    use super::{
        Clock, OstromPaths, OwnedLease, PassError, PassGuard, PassRole, ProcessExit, RunEventError,
        RunEventGuard, RunEventStart, RunKind, append_observed, capture_refused_draft, read_trace,
        sink_refused_draft,
    };

    fn assert_refused_pass_finishes(payload: Value) -> Value {
        let root = tempfile::tempdir().expect("temporary pass");
        let paths = OstromPaths {
            config: root.path().to_path_buf(),
            state: root.path().to_path_buf(),
        };
        let clock = Clock::default();
        let run_id = "sink-refusal-pass";
        let events = RunEventGuard::start(
            &paths,
            None,
            false,
            clock.clone(),
            RunEventStart {
                run_id: run_id.to_owned(),
                // ostrom#546: this fixture stands in for what `run_pass`
                // itself emits, so it must agree with the real producer.
                kind: RunKind::Handoff,
                actor: "builder".to_owned(),
                harness: "claude".to_owned(),
                model: None,
                schedule: None,
                repository: None,
                repositories: None,
                work_order: None,
                ceilings: None,
            },
        )
        .expect("start real sink");
        let mut watchdog = CapsWatchdog::start(
            RunCaps::default(),
            SystemClock::default(),
            events.sink(),
            run_id,
        )
        .expect("start watchdog");
        let lease = OwnedLease::acquire(
            &paths.state,
            "builder-pass.lease",
            run_id,
            clock.epoch_seconds(),
            60,
        )
        .expect("acquire pass lease");
        let mut guard = PassGuard {
            role: PassRole::Builder,
            paths: paths.clone(),
            lease,
            owner: run_id.to_owned(),
            started_epoch: clock.epoch_seconds(),
            trace_time: clock.timestamp(),
            started: true,
            child_spawned: false,
            outcome: None,
            reason: None,
            cost_usd: None,
            clock,
            dispatchability_hash: None,
            queue_count: None,
            dispatchable_count: None,
            repositories: None,
            skipped_repositories: Vec::new(),
            events,
            control: None,
            process_exit: ProcessExit::Normal,
            permission_bridge: None,
        };
        let drafts = [payload, json!({"text": "still working"})]
            .map(|payload| EventDraft {
                event_type: AGENT_TEXT.to_owned(),
                payload,
                captured_at: None,
            })
            .to_vec();

        // No synthetic SinkFault: the real FileSink must refuse the first
        // draft, and append_observed must still process the next in the batch.
        assert!(
            append_observed(&guard, &mut watchdog, drafts)
                .expect("observe drafts")
                .is_none()
        );
        guard.finish().expect("pass finishes successfully");
        let events = FileSink::new(paths.runs_dir())
            .read_from(run_id, 0)
            .expect("read durable events");
        assert_eq!(
            events
                .iter()
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>(),
            ["run.started", CAPTURE_REFUSED, AGENT_TEXT, "run.finished"]
        );
        // ostrom#546: a pass is a dispatched, unscheduled run, so its
        // run.started must declare `handoff` and never `schedule` -- the
        // fact that made `loop` wrong for every pass.
        assert_eq!(events[0].payload["kind"], "handoff");
        assert!(
            events[0]
                .payload
                .as_object()
                .unwrap()
                .get("schedule")
                .is_none(),
            "a pass's run.started must not carry a schedule: {:?}",
            events[0].payload
        );
        assert_eq!(
            events.iter().map(|event| event.seq).collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        assert!(events.iter().all(|event| event.run_id == run_id));
        assert_eq!(events[2].payload["text"], "still working");
        assert_eq!(events[3].payload["outcome"], "completed");
        let refusal = &events[1].payload;
        assert_eq!(refusal["sourceRunId"], run_id);
        assert_eq!(refusal["sourceType"], AGENT_TEXT);
        assert!(refusal.get("sourceSeq").is_none());
        let trace = read_trace(&paths.trace_file()).expect("read pass trace");
        assert_eq!(trace.rows.len(), 1);
        let ended = trace.rows[0].as_ref().expect("valid pass-ended fact");
        assert_eq!(ended.kind, "pass-ended");
        assert_eq!(ended.fact["outcome"], "completed");
        assert!(!paths.state.join("builder-pass.lease").exists());
        refusal.clone()
    }

    #[test]
    fn over_bound_agent_text_is_refused_by_the_real_sink_and_the_pass_finishes() {
        let count = MAX_TEXT_SCALARS + 1;
        let refusal = assert_refused_pass_finishes(json!({"text": "🦀".repeat(count)}));
        assert_eq!(refusal["cause"], "over_bound");
        assert_eq!(refusal["field"], "payload.text");
        assert_eq!(refusal["count"], count);
        assert_eq!(refusal["max"], MAX_TEXT_SCALARS);
        assert_eq!(refusal["unit"], "scalars");
    }

    #[test]
    fn over_bound_payload_bytes_keep_their_unit_and_the_pass_finishes() {
        let payload = json!({
            "text": "within the scalar bound",
            "chunks": vec!["x".repeat(MAX_TEXT_SCALARS); MAX_PAYLOAD_BYTES / MAX_TEXT_SCALARS + 1],
        });
        let bytes = serde_json::to_vec(&payload).expect("payload bytes").len();
        assert!(bytes > MAX_PAYLOAD_BYTES);
        let refusal = assert_refused_pass_finishes(payload);
        assert_eq!(refusal["cause"], "over_bound");
        assert!(refusal.get("field").is_none());
        assert_eq!(refusal["count"], bytes);
        assert_eq!(refusal["max"], MAX_PAYLOAD_BYTES);
        assert_eq!(refusal["unit"], "bytes");
    }

    #[test]
    fn invalid_agent_text_is_refused_by_the_real_sink_and_the_pass_finishes() {
        let refusal = assert_refused_pass_finishes(json!({"text": false}));
        assert_eq!(refusal["cause"], "malformed");
        // Deliberately not pinned to the SDK's wording, not even a substring.
        // `capture.refused.detail` is documented **non-authoritative**
        // (onsager-ai/ethogram#15): the countable facts are the typed fields, and consumers
        // must not key on diagnostic prose. This assertion used to pin that
        // prose, which is the one field the producing repository says is not a
        // contract -- so it was not merely brittle, it was asserting against a
        // stated rule.
        //
        // What ostrom owns is that the sink refused, that the refusal was
        // stored, and which field it attributed the refusal to. `field` is a
        // countable fact in the same payload and holds `payload.text`, so it
        // survives any rewording and fails if the attribution moves. The
        // detail is asserted non-empty and nothing more.
        assert_eq!(refusal["field"], "payload.text");
        assert!(
            refusal["detail"]
                .as_str()
                .is_some_and(|detail| !detail.is_empty()),
            "a refusal must still explain itself"
        );
        assert!(refusal.get("count").is_none());
        assert!(refusal.get("max").is_none());
    }

    #[test]
    fn invalid_draft_detail_is_bounded_so_the_refusal_can_be_stored() {
        let refusal = assert_refused_pass_finishes(json!({
            "text": "valid text",
            "truncated": "🦀".repeat(MAX_EXCERPT_SCALARS + 1),
        }));
        assert_eq!(refusal["cause"], "malformed");
        // No truncation assertion here any more. ethogram's validation
        // messages stopped echoing the offending value, so this input no
        // longer produces an oversized detail and the assertion would pass by
        // guarding nothing. The bounding property is ostrom's, so it is tested
        // at ostrom's own seam instead -- see the two `bounding_` tests below.
        assert!(
            refusal["detail"]
                .as_str()
                .is_some_and(|detail| !detail.is_empty()),
            "the refusal must still be stored with a detail"
        );
    }

    /// The bounding property, at ostrom's seam rather than through ethogram.
    ///
    /// `sink_refused_draft` excerpts a sink's detail to `MAX_EXCERPT_SCALARS`
    /// before storing it, so a refusal can always be written. That decision is
    /// ostrom's: it chooses to excerpt and it chooses the bound. It used to be
    /// covered incidentally, because ethogram's validation messages echoed the
    /// offending value and so ran long. They no longer do, which would have
    /// left the guard passing vacuously -- so it is asserted directly, with an
    /// oversized detail this test constructs rather than one it hopes to
    /// provoke.
    fn refusal_detail(detail: String) -> Value {
        let draft = sink_refused_draft(
            PassRole::Builder,
            "sink-refusal-seam",
            AGENT_TEXT.to_owned(),
            RunEventError::Sink(SinkFault::Invalid {
                path: "payload.text".to_owned(),
                detail,
            }),
        )
        .expect("a sink refusal always drafts");
        draft.payload
    }

    #[test]
    fn bounding_truncates_an_oversized_detail() {
        let payload = refusal_detail("🦀".repeat(MAX_EXCERPT_SCALARS + 1));
        assert_eq!(payload["truncated"], true);
        assert_eq!(
            payload["detail"]
                .as_str()
                .expect("bounded detail")
                .chars()
                .count(),
            MAX_EXCERPT_SCALARS
        );
    }

    /// The same bound, on the other producer of `capture.refused`.
    ///
    /// `capture_refused_draft` excerpts a `CaptureFault`'s message the same way
    /// `sink_refused_draft` excerpts a sink's detail, and until now nothing
    /// tripped it. A normaliser rejection carries the offending input in its
    /// reason, so an oversized raw line reaches this path in production --
    /// which is exactly the case that must stay storable.
    #[test]
    fn a_capture_fault_message_is_bounded_too() {
        let draft = capture_refused_draft(
            "capture-bound",
            &umwelt_capture::CaptureFault::NormaliserRejected {
                reason: "🦀".repeat(MAX_EXCERPT_SCALARS + 1),
            },
        );
        assert_eq!(draft.payload["truncated"], true);
        assert_eq!(
            draft.payload["detail"]
                .as_str()
                .expect("bounded detail")
                .chars()
                .count(),
            MAX_EXCERPT_SCALARS
        );
    }

    #[test]
    fn bounding_leaves_a_detail_within_the_bound_alone() {
        let payload = refusal_detail("short enough".to_owned());
        assert_eq!(payload["truncated"], false);
        assert_eq!(payload["detail"], "short enough");
    }

    #[test]
    fn existing_sink_faults_still_fail_the_pass_with_the_original_diagnostic() {
        for fault in [
            SinkFault::Gap {
                expected: 2,
                got: 3,
            },
            SinkFault::Duplicate(1),
            SinkFault::Finished,
            SinkFault::Io("disk full".to_owned()),
            SinkFault::Malformed {
                line: 2,
                message: "torn line".to_owned(),
            },
        ] {
            let error = RunEventError::Sink(fault);
            let expected = format!("could not append captured agent event: {error}");
            let error =
                sink_refused_draft(PassRole::Gatekeeper, "pass", AGENT_TEXT.to_owned(), error)
                    .expect_err("existing fault remains fatal");
            assert_eq!(error.exit_code(), 1);
            assert!(
                matches!(error, PassError::Failed { role: "gatekeeper", message, code: 1 } if message == expected)
            );
        }
    }
}

#[cfg(test)]
mod terminal_outcome_tests {
    use super::terminal_outcome;

    #[test]
    fn an_unwinding_pass_is_recorded_as_failed() {
        assert_eq!(terminal_outcome(None, true), "failed");
    }

    #[test]
    fn a_clean_pass_is_recorded_as_completed() {
        assert_eq!(terminal_outcome(None, false), "completed");
    }

    #[test]
    fn an_explicit_outcome_survives_an_unwind() {
        assert_eq!(
            terminal_outcome(Some("refused".to_owned()), true),
            "refused",
            "a pass that already decided its outcome keeps it"
        );
    }
}

#[cfg(test)]
mod event_outcome_tests {
    use super::{EventRunOutcome, event_outcome};

    // The wire mapping is a match with a `Failed` catch-all, so an outcome
    // the fact ledger records but this function forgets goes out as `failed`
    // without any error. ostrom#587's refusal depends on `unstarted` surviving.
    #[test]
    fn an_unstarted_run_reaches_the_wire_as_unstarted_not_failed() {
        assert_eq!(event_outcome("unstarted"), EventRunOutcome::Unstarted);
        assert_eq!(event_outcome("no-op"), EventRunOutcome::NoOp);
        assert_eq!(event_outcome("no-candidates"), EventRunOutcome::NoOp);
        assert_eq!(event_outcome("not-an-outcome"), EventRunOutcome::Failed);
    }
}

#[cfg(test)]
mod exit_code_tests {
    use super::{
        BUDGET_HELD_EXIT_CODE, DISARMED_EXIT_CODE, HARNESS_UNAVAILABLE_EXIT_CODE, PassError,
        SWEEP_LEASE_CONTENTION_EXIT_CODE,
    };

    #[test]
    fn budget_held_disarmed_and_lease_held_have_distinct_exit_codes() {
        assert_eq!(BUDGET_HELD_EXIT_CODE, 75);
        assert_eq!(DISARMED_EXIT_CODE, 78);
        assert_eq!(SWEEP_LEASE_CONTENTION_EXIT_CODE, 76);
        assert_eq!(
            PassError::BudgetHeld("builder").exit_code(),
            BUDGET_HELD_EXIT_CODE
        );
        assert_eq!(
            PassError::Disarmed("builder").exit_code(),
            DISARMED_EXIT_CODE
        );
        assert_eq!(PassError::LeaseHeld("builder").exit_code(), 0);
    }

    // A consumer that has only the exit status must still be able to tell the
    // refusals apart (ostrom#587). The codes are distinct, and a harness
    // refusal in particular does not share disarmed's EX_CONFIG. Sweep-lease
    // contention (ostrom#599) is included: it must not collide with the
    // daily-cap hold it is deliberately distinct from.
    #[test]
    fn a_harness_refusal_exits_with_its_own_code_not_disarmed() {
        let harness = PassError::HarnessUnsupported {
            role: "builder",
            version: "2.1.238".to_owned(),
            path: std::path::PathBuf::from("claude"),
            minimum: crate::permission_bridge::MIN_BRIDGE_HARNESS_VERSION,
        };
        assert_eq!(HARNESS_UNAVAILABLE_EXIT_CODE, 69);
        assert_eq!(harness.exit_code(), HARNESS_UNAVAILABLE_EXIT_CODE);
        let codes = [
            harness.exit_code(),
            PassError::Disarmed("builder").exit_code(),
            PassError::BudgetHeld("builder").exit_code(),
            PassError::LeaseHeld("builder").exit_code(),
            SWEEP_LEASE_CONTENTION_EXIT_CODE,
        ];
        let distinct: std::collections::BTreeSet<i32> = codes.iter().copied().collect();
        assert_eq!(distinct.len(), codes.len(), "exit codes collide: {codes:?}");
    }
}

#[cfg(test)]
mod sweep_lease_contention_mapping_tests {
    use super::{
        PrepareSweepError, SWEEP_LEASE_CONTENTION_EXIT_CODE, SweepError, classify_sweep_error,
        sweep_failure_status,
    };

    // The mapping this guards: a sweep failure caused by the lease staying
    // held by someone else must read as contention, not as a generic sweep
    // failure, however it was observed (an immediate `LeaseHeld` from a
    // single acquisition attempt, or a `LeaseWaitTimedOut` once the bounded
    // wait ran out).
    #[test]
    fn lease_held_and_lease_wait_timed_out_both_classify_as_contention() {
        assert!(matches!(
            classify_sweep_error(SweepError::LeaseHeld),
            PrepareSweepError::LeaseContention(_)
        ));
        assert!(matches!(
            classify_sweep_error(SweepError::LeaseWaitTimedOut { seconds: 1_800 }),
            PrepareSweepError::LeaseContention(_)
        ));
    }

    #[test]
    fn every_other_sweep_error_classifies_as_a_plain_failure() {
        assert!(matches!(
            classify_sweep_error(SweepError::State("boom".to_owned())),
            PrepareSweepError::Other(_)
        ));
    }

    // The exit-status mapping itself: contention gets the dedicated status
    // and its own reason; anything else keeps the pre-existing exit 1 and
    // "sweep-failed" reason so an unrelated sweep failure is unaffected.
    #[test]
    fn contention_gets_its_own_reason_and_exit_status_everything_else_keeps_sweep_failed() {
        assert_eq!(
            sweep_failure_status(&PrepareSweepError::LeaseContention(
                "sweep lease remained held for 1800 seconds".to_owned()
            )),
            ("sweep-lease-contention", SWEEP_LEASE_CONTENTION_EXIT_CODE)
        );
        assert_eq!(
            sweep_failure_status(&PrepareSweepError::Other("boom".to_owned())),
            ("sweep-failed", 1)
        );
    }
}

#[cfg(test)]
mod resolve_derived_settings_tests {
    // Exercises the same pure decision `run_pass` uses (`platform_supports_bridge`
    // via `resolve_derived_settings`), by injecting an unsupported OS name -- never
    // by gating on `cfg(target_os)` or requiring a Windows host (ostrom#544).
    use super::{DerivedRunSettings, resolve_derived_settings};
    use std::path::Path;

    const DERIVED: &str =
        r#"{"permissions":{"defaultMode":"dontAsk","allow":["Bash(ostrom deploy *)"]}}"#;

    #[test]
    fn unsupported_platform_writes_per_run_settings_with_no_bridge_and_one_warning() {
        let root = tempfile::tempdir().expect("temporary run directory");
        let resolved = resolve_derived_settings(
            root.path(),
            "fallback-run",
            DERIVED,
            Path::new("ostrom"),
            "unsupported",
        )
        .expect("fallback resolves without a bridge");
        let DerivedRunSettings::Fallback { path, warning } = resolved else {
            panic!("an unsupported platform must not produce a bridge");
        };
        // Per run, inside this run's own directory -- never the shared
        // roles/<role>.derived.settings.json two concurrent passes of one
        // role would overwrite (the defect #541 fixed, which must survive
        // regardless of bridge support).
        assert_eq!(path, root.path().join("fallback-run.settings.json"));
        assert_eq!(
            std::fs::read_to_string(&path).expect("read fallback settings"),
            DERIVED,
            "the fallback must not rewrite the derived profile"
        );
        // No MCP config is ever rendered for a fallback run: there is no path
        // to hold one, so nothing can be pointed to by `--mcp-config`.
        assert!(
            !root.path().join("mcp.json").exists(),
            "a fallback run must never render mcp.json"
        );
        assert_eq!(warning.payload["stage"], "permission-bridge");
        assert!(
            warning.payload["message"]
                .as_str()
                .unwrap()
                .contains("unsupported"),
            "{warning:?}"
        );
    }

    #[test]
    fn linux_and_macos_still_produce_a_bridge() {
        for os in ["linux", "macos"] {
            let root = tempfile::tempdir().expect("temporary run directory");
            let resolved = resolve_derived_settings(
                root.path(),
                "bridged-run",
                DERIVED,
                Path::new("ostrom"),
                os,
            )
            .unwrap_or_else(|error| panic!("{os} bridge creation failed: {error}"));
            assert!(
                matches!(resolved, DerivedRunSettings::Bridged(_)),
                "{os} must still take the bridge path"
            );
        }
    }
}

#[cfg(all(test, unix))]
mod platform_fallback_pass_tests {
    // These pass-level tests run the real bridge/fallback decision and the real
    // `Command` construction against a stub. Only the ten-second production
    // timeout has a seam; timeout coverage injects a shorter value.
    use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};

    use umwelt_runtime::{FileSink, Source};

    use super::{
        Clock, HARNESS_UNAVAILABLE_EXIT_CODE, OstromPaths, PassError, PassRequest, PassRole,
        run_pass, run_pass_with_bridge_probe_timeout,
    };
    use crate::{
        SWEEP_LEASE_CEILING_SECONDS, permission_bridge::MIN_BRIDGE_HARNESS_VERSION, read_trace,
    };

    const CLAUDE_STREAM_JSON: &str = concat!(
        "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"fallback-fixture\",\"model\":\"claude-fixture\"}\n",
        "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"tool-fixture\",\"name\":\"Bash\",\"input\":{\"command\":\"true\"}}]}}\n",
        "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"tool-fixture\",\"content\":\"ok\",\"is_error\":false}]}}\n",
        "{\"type\":\"result\",\"subtype\":\"success\",\"session_id\":\"fallback-fixture\",\"duration_ms\":1,\"num_turns\":1,\"total_cost_usd\":0,",
        "\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"cache_read_input_tokens\":2,\"cache_creation_input_tokens\":3}}\n"
    );

    const DERIVED: &str =
        r#"{"permissions":{"defaultMode":"dontAsk","allow":["Bash(ostrom deploy *)"]}}"#;

    struct HarnessFixture {
        root: tempfile::TempDir,
        paths: OstromPaths,
        claude_bin: PathBuf,
        argv_file: PathBuf,
        calls_file: PathBuf,
    }

    impl HarnessFixture {
        fn new(version_case: &str) -> Self {
            let root = tempfile::tempdir().expect("temporary pass fixture");
            let paths = OstromPaths {
                config: root.path().to_path_buf(),
                state: root.path().to_path_buf(),
            };
            fs::write(paths.state.join("loop-armed"), "").expect("arm pass");
            let argv_file = root.path().join("argv.txt");
            let calls_file = root.path().join("calls.txt");
            let claude_bin = root.path().join("claude-stub");
            fs::write(
                &claude_bin,
                format!(
                    "#!/usr/bin/env bash\nprintf '%s\\n' \"$1\" >> '{}'\nif [[ \"$1\" == \"--version\" ]]; then\n{version_case}\nfi\nprintf '%s\\n' \"$@\" > '{}'\nprintf '%s' '{CLAUDE_STREAM_JSON}'\n",
                    calls_file.display(),
                    argv_file.display(),
                ),
            )
            .expect("write claude stub");
            fs::set_permissions(&claude_bin, fs::Permissions::from_mode(0o755))
                .expect("chmod stub");
            Self {
                root,
                paths,
                claude_bin,
                argv_file,
                calls_file,
            }
        }

        fn request(&self, platform: &'static str) -> PassRequest {
            PassRequest {
                paths: self.paths.clone(),
                working_directory: self.root.path().to_path_buf(),
                role: PassRole::Builder,
                prompt: "ostrom#587 harness floor fixture".to_owned(),
                permission_mode: PassRole::Builder.default_permission_mode(),
                derived_settings: Some(DERIVED.to_owned()),
                claude_bin: self.claude_bin.clone(),
                signals: Default::default(),
                supervisor_pid: None,
                events_fd: None,
                control_fd: None,
                facts_only: false,
                repositories: None,
                skipped_repositories: Vec::new(),
                repository_scope: None,
                sweep: None,
                caps: Default::default(),
                clock: Clock::default(),
                platform,
            }
        }

        fn argv(&self) -> String {
            fs::read_to_string(&self.argv_file).expect("read captured agent argv")
        }

        fn calls(&self) -> String {
            fs::read_to_string(&self.calls_file).expect("read captured harness calls")
        }
    }

    fn successful_version(version: impl std::fmt::Display) -> String {
        format!("printf '%s\\n' '{version} (Claude Code)'\nexit 0")
    }

    fn assert_harness_refusal(fixture: &HarnessFixture, timeout: Duration) -> PassError {
        // This fixture's request carries no sweep (`sweep: None`), so the
        // sweep-lease wait never comes into play; the production ceiling is
        // passed here only to keep this call site honest about what `run_pass`
        // itself would use.
        let error = run_pass_with_bridge_probe_timeout(
            &fixture.request("linux"),
            timeout,
            Duration::from_secs(SWEEP_LEASE_CEILING_SECONDS),
        )
        .expect_err("the bridged pass must refuse this harness");
        assert_eq!(error.exit_code(), HARNESS_UNAVAILABLE_EXIT_CODE);
        assert!(
            matches!(&error, PassError::HarnessUnsupported { .. }),
            "the refusal must keep its dedicated type: {error:?}"
        );
        assert!(
            !fixture.argv_file.exists(),
            "the stub received an agent invocation: {}",
            fixture.argv()
        );
        error
    }

    #[test]
    fn bridged_pass_refuses_a_version_below_the_lowest_verified_version() {
        let fixture = HarnessFixture::new(&successful_version("2.1.238"));
        let error = assert_harness_refusal(&fixture, Duration::from_secs(1));
        let message = error.to_string();
        assert!(message.contains("2.1.238"), "{message}");
        assert!(
            message.contains(&MIN_BRIDGE_HARNESS_VERSION.to_string()),
            "{message}"
        );
        let harness_path = fixture.claude_bin.to_string_lossy();
        assert!(message.contains(harness_path.as_ref()), "{message}");
        assert!(message.contains("--permission-prompts"), "{message}");
        assert!(message.contains("--permission-prompt-tool"), "{message}");
        assert!(
            message.contains("upgrade Claude Code to at least"),
            "{message}"
        );
        assert_eq!(fixture.calls(), "--version\n");

        let trace = read_trace(&fixture.paths.trace_file()).expect("read pass trace");
        let terminal = trace
            .rows
            .into_iter()
            .filter_map(Result::ok)
            .find(|row| row.kind == "pass-ended")
            .expect("the refusal records pass-ended");
        assert_eq!(terminal.fact["outcome"], "unstarted");
        assert_eq!(terminal.fact["reason"], "harness-unsupported");

        let run_id = fs::read_dir(fixture.paths.runs_dir())
            .expect("read run records")
            .next()
            .expect("one run record")
            .expect("read run entry")
            .file_name()
            .into_string()
            .expect("UTF-8 run id");
        let events = FileSink::new(fixture.paths.runs_dir())
            .read_from(&run_id, 0)
            .expect("read durable events");
        assert!(
            events
                .iter()
                .all(|event| !event.event_type.starts_with("agent.")),
            "a refused pass must not emit agent events: {events:?}"
        );
        assert_eq!(events.last().unwrap().payload["outcome"], "unstarted");
        assert_eq!(
            events.last().unwrap().payload["reason"],
            "harness-unsupported"
        );
    }

    #[test]
    fn bridged_pass_accepts_the_lowest_verified_version_and_adds_bridge_flags() {
        let fixture = HarnessFixture::new(&successful_version(MIN_BRIDGE_HARNESS_VERSION));
        run_pass(&fixture.request("linux"))
            .expect("the lowest verified version must pass the bridge floor");
        assert_eq!(fixture.calls(), "--version\n--print\n");
        assert!(
            fixture
                .argv()
                .lines()
                .any(|arg| arg == "--permission-prompts"),
            "the accepted pass did not receive bridge flags"
        );
    }

    #[test]
    fn bridged_pass_refuses_garbage_version_output() {
        let fixture = HarnessFixture::new("printf '%s\\n' 'not a version'\nexit 0");
        let error = assert_harness_refusal(&fixture, Duration::from_secs(1));
        assert!(error.to_string().contains("could not be parsed"));
    }

    #[test]
    fn bridged_pass_refuses_a_nonzero_version_probe() {
        let fixture = HarnessFixture::new("printf '%s\\n' 'probe failed' >&2\nexit 17");
        let error = assert_harness_refusal(&fixture, Duration::from_secs(1));
        let message = error.to_string();
        assert!(message.contains("exited"), "{message}");
        assert!(message.contains("probe failed"), "{message}");
    }

    #[test]
    fn bridged_pass_refuses_a_version_probe_that_cannot_spawn() {
        let fixture = HarnessFixture::new(&successful_version(MIN_BRIDGE_HARNESS_VERSION));
        fs::write(&fixture.claude_bin, "not an executable format")
            .expect("replace stub with invalid executable");
        let error = assert_harness_refusal(&fixture, Duration::from_secs(1));
        assert!(error.to_string().contains("could not start"));
    }

    #[test]
    fn bridged_pass_refuses_a_version_probe_timeout() {
        let fixture = HarnessFixture::new("while :; do :; done");
        let error = assert_harness_refusal(&fixture, Duration::from_millis(30));
        assert!(error.to_string().contains("timed out"));
    }

    #[test]
    fn fallback_pass_launches_the_same_old_harness_without_a_version_probe() {
        let fixture = HarnessFixture::new(&successful_version("2.1.238"));
        let request = fixture.request("windows");
        run_pass(&request).expect("a platform with no bridge must not fail the pass");
        assert_eq!(
            fixture.calls(),
            "--print\n",
            "a fallback pass must not run the version probe"
        );

        let argv = fixture.argv();
        for flag in [
            "--mcp-config",
            "--strict-mcp-config",
            "--permission-prompts",
            "--permission-prompt-tool",
        ] {
            assert!(
                !argv.contains(flag),
                "a fallback pass must never pass {flag} to the harness: {argv}"
            );
        }
        let lines: Vec<&str> = argv.lines().collect();
        let settings_index = lines
            .iter()
            .position(|line| *line == "--settings")
            .expect("--settings must still be passed");
        let settings_path = std::path::PathBuf::from(lines[settings_index + 1]);
        assert!(
            settings_path.starts_with(fixture.paths.runs_dir()),
            "fallback settings must live under the run directory: {settings_path:?}"
        );
        assert_eq!(
            fs::read_to_string(&settings_path).expect("read fallback settings"),
            DERIVED
        );
        assert!(
            !fixture
                .paths
                .state
                .join("roles/builder.derived.settings.json")
                .exists(),
            "a fallback pass must never write the shared derived settings file"
        );

        let run_entries: Vec<_> = fs::read_dir(fixture.paths.runs_dir())
            .expect("read runs directory")
            .map(|entry| entry.expect("run directory entry"))
            .collect();
        assert_eq!(
            run_entries.len(),
            1,
            "expected exactly one run directory: {run_entries:?}"
        );
        let run_id = run_entries[0]
            .file_name()
            .into_string()
            .expect("run id is valid UTF-8");
        let events = FileSink::new(fixture.paths.runs_dir())
            .read_from(&run_id, 0)
            .expect("read durable events");
        // ostrom#546: `run_pass` is invoked as a one-off dispatch, never a
        // declared schedule, so its run.started must declare `handoff` and
        // carry no `schedule` -- the fact that made `loop` wrong here.
        assert_eq!(events.first().unwrap().event_type, "run.started");
        assert_eq!(events[0].payload["kind"], "handoff");
        assert!(
            events[0]
                .payload
                .as_object()
                .unwrap()
                .get("schedule")
                .is_none(),
            "a pass's run.started must not carry a schedule: {:?}",
            events[0].payload
        );
        let warnings: Vec<_> = events
            .iter()
            .filter(|event| event.event_type == ethogram::AGENT_WARNING)
            .collect();
        assert_eq!(
            warnings.len(),
            1,
            "expected exactly one agent.warning: {events:?}"
        );
        assert_eq!(warnings[0].payload["stage"], "permission-bridge");
        assert!(
            warnings[0].payload["message"]
                .as_str()
                .unwrap()
                .contains("windows"),
            "the warning must name the platform: {:?}",
            warnings[0].payload
        );
        // The stub harness reports no inner `ostrom pass` work of its own, so
        // this run's own terminal outcome is the ordinary "no-op" a bare
        // stream produces regardless of platform (see pass_lifecycle.rs's
        // identical `stream_script(CLAUDE_STREAM_JSON)` fixtures) -- the
        // point here is that it is a clean terminal event at all, not
        // "failed" the way #544 regressed it to.
        assert_eq!(events.last().unwrap().event_type, "run.finished");
        assert_ne!(
            events.last().unwrap().payload["outcome"],
            "failed",
            "a platform with no bridge must not fail the pass: {events:?}"
        );
    }
}

#[cfg(test)]
mod sweep_freshness_tests {
    use std::{
        fs,
        path::PathBuf,
        thread,
        time::{Duration, Instant},
    };

    use serde_json::json;

    use super::{
        BRIDGE_HARNESS_PROBE_TIMEOUT, PassError, PassRequest, PassRole, PassSweepOutcome,
        PassSweepRequest, PrepareSweepError, SWEEP_LEASE_CONTENTION_EXIT_CODE, prepare_sweep,
        run_pass, run_pass_with_bridge_probe_timeout,
    };
    use crate::{
        Clock, OstromPaths, OwnedLease, PublishTarget, SWEEP_LEASE_CEILING_SECONDS, SweepMode,
        SweepOptions, read_trace,
    };

    /// The production sweep-lease wait ceiling, for the tests below that are
    /// not exercising the ceiling itself.
    const DEFAULT_SWEEP_WAIT: Duration = Duration::from_secs(SWEEP_LEASE_CEILING_SECONDS);

    #[test]
    fn shipped_prompts_do_not_run_a_sweep() {
        for (name, prompt) in [
            ("work", include_str!("../assets/prompts/work.md")),
            ("gatekeep", include_str!("../assets/prompts/gatekeep.md")),
            ("merge", include_str!("../assets/prompts/merge.md")),
            ("triage", include_str!("../assets/prompts/triage.md")),
        ] {
            assert!(
                !prompt.contains("ostrom sweep"),
                "{name} prompt runs a sweep"
            );
        }
    }

    fn request(
        root: &std::path::Path,
        now: &str,
        fixture: Option<PathBuf>,
        max_age_seconds: u64,
    ) -> PassRequest {
        let paths = OstromPaths {
            config: root.to_path_buf(),
            state: root.to_path_buf(),
        };
        let clock = Clock::fixed(now.parse().expect("valid pass time"));
        PassRequest {
            paths: paths.clone(),
            working_directory: root.to_path_buf(),
            role: PassRole::Builder,
            prompt: "unused freshness prompt".to_owned(),
            permission_mode: PassRole::Builder.default_permission_mode(),
            derived_settings: None,
            claude_bin: root.join("agent-must-not-run"),
            signals: Default::default(),
            supervisor_pid: None,
            events_fd: None,
            control_fd: None,
            facts_only: false,
            repositories: None,
            skipped_repositories: Vec::new(),
            repository_scope: None,
            sweep: Some(PassSweepRequest {
                options: SweepOptions {
                    paths,
                    working_directory: root.to_path_buf(),
                    executable: root.join("unused-ostrom"),
                    plugin_root: root.to_path_buf(),
                    started_at: clock.now(),
                    requested_mode: SweepMode::Auto,
                    fixture,
                    publish: PublishTarget::Disabled,
                    policy: None,
                },
                max_age_seconds,
            }),
            caps: Default::default(),
            clock,
            platform: std::env::consts::OS,
        }
    }

    fn write_sweep_inputs(root: &std::path::Path) -> PathBuf {
        fs::write(
            root.join("mandates.yaml"),
            "provider: file\ncadence_hours: 1\nstuck_after_days: 7\nprojects:\n  - repo: placeholder-org/alpha\n",
        )
        .expect("write mandate roster");
        let fixture = root.join("sweep.json");
        fs::write(
            &fixture,
            serde_json::to_vec(&json!({
                "repositories": [{
                    "repo": "placeholder-org/alpha",
                    "issues": [],
                    "open_prs": []
                }]
            }))
            .expect("serialize sweep fixture"),
        )
        .expect("write sweep fixture");
        fixture
    }

    #[test]
    fn young_generation_is_reused_and_old_or_absent_generations_are_swept() {
        let fresh = tempfile::tempdir().expect("fresh generation fixture");
        fs::write(
            fresh.path().join("state.json"),
            serde_json::to_vec(&json!({
                "sweep_generation": {
                    "id": "young-generation",
                    "completed_at": "2026-09-15T10:00:00Z"
                }
            }))
            .expect("serialize state"),
        )
        .expect("write state");
        assert_eq!(
            prepare_sweep(
                &request(fresh.path(), "2026-09-15T10:29:59Z", None, 1_800),
                DEFAULT_SWEEP_WAIT,
            )
            .map(|prepared| prepared.outcome),
            Ok(PassSweepOutcome::Reused("young-generation".to_owned()))
        );

        for completed_at in [None, Some("2026-09-15T09:59:59Z")] {
            let stale = tempfile::tempdir().expect("stale generation fixture");
            let fixture = write_sweep_inputs(stale.path());
            if let Some(completed_at) = completed_at {
                fs::write(
                    stale.path().join("state.json"),
                    serde_json::to_vec(&json!({
                        "sweep_generation": {
                            "id": "old-generation",
                            "completed_at": completed_at
                        }
                    }))
                    .expect("serialize old generation"),
                )
                .expect("write old generation");
            }
            let outcome = prepare_sweep(
                &request(stale.path(), "2026-09-15T10:30:00Z", Some(fixture), 1_800),
                DEFAULT_SWEEP_WAIT,
            )
            .expect("stale pass sweeps");
            assert!(matches!(outcome.outcome, PassSweepOutcome::Swept(_)));
        }
    }

    #[test]
    fn failed_sweep_ends_the_pass_before_an_agent_turn_and_records_the_reason() {
        let root = tempfile::tempdir().expect("failed sweep fixture");
        fs::write(root.path().join("loop-armed"), "").expect("arm pass");
        let result = run_pass(&request(root.path(), "2026-09-15T10:30:00Z", None, 1_800));
        let error = result.expect_err("missing sweep roster fails the pass");
        assert!(error.to_string().contains("sweep-failed"), "{error}");
        assert!(!root.path().join("agent-must-not-run").exists());
        assert!(!root.path().join("pass-runs").exists());
        let trace = read_trace(&root.path().join("sprint.jsonl")).expect("read pass trace");
        let rows = trace
            .rows
            .iter()
            .map(|row| row.as_ref().expect("valid pass trace row"))
            .collect::<Vec<_>>();
        assert_eq!(rows[0].kind, "pass-started");
        assert_eq!(rows[0].fact["sweep"], "failed");
        assert_eq!(rows[1].kind, "pass-ended");
        assert_eq!(rows[1].fact["reason"], "sweep-failed");
    }

    #[test]
    fn pass_waits_for_a_held_sweep_lease_then_reuses_the_completed_generation() {
        let root = tempfile::tempdir().expect("held sweep lease fixture");
        let paths = OstromPaths {
            config: root.path().to_path_buf(),
            state: root.path().to_path_buf(),
        };
        let lease = OwnedLease::acquire(
            &paths.state,
            "sweep.lease",
            "concurrent-sweep",
            Clock::realtime().epoch_seconds(),
            60,
        )
        .expect("hold sweep lease");
        let state_path = root.path().join("state.json");
        let snapshot_path = root.path().join("sweep-snapshot.json");
        let writer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            let generation = json!({
                "id": "concurrent-generation",
                "completed_at": "2026-09-15T10:20:00Z"
            });
            fs::write(
                state_path,
                serde_json::to_vec(&json!({"sweep_generation": generation.clone()}))
                    .expect("serialize completed sweep state"),
            )
            .expect("write completed sweep state");
            fs::write(
                snapshot_path,
                serde_json::to_vec(&json!({
                    "generation": generation,
                    "repositories": []
                }))
                .expect("serialize completed sweep snapshot"),
            )
            .expect("write completed sweep snapshot");
            drop(lease);
        });

        let started = Instant::now();
        let prepared = prepare_sweep(
            &request(root.path(), "2026-09-15T10:30:00Z", None, 1_800),
            DEFAULT_SWEEP_WAIT,
        )
        .expect("pass reuses concurrent sweep");
        writer.join().expect("concurrent sweep writer");
        assert!(started.elapsed() >= Duration::from_millis(50));
        assert_eq!(
            prepared.outcome,
            PassSweepOutcome::Reused("concurrent-generation".to_owned())
        );
    }

    // ostrom#599: a holder that keeps renewing past the wait ceiling must
    // classify as contention, not a generic sweep failure. Uses the
    // injectable `sweep_wait` bound so this is reached in milliseconds
    // instead of the production 1800s ceiling.
    #[test]
    fn a_sweep_lease_held_past_the_wait_classifies_as_contention() {
        let root = tempfile::tempdir().expect("contended sweep lease fixture");
        let paths = OstromPaths {
            config: root.path().to_path_buf(),
            state: root.path().to_path_buf(),
        };
        let held = OwnedLease::acquire(
            &paths.state,
            "sweep.lease",
            "in-flight-sweep",
            Clock::realtime().epoch_seconds(),
            60,
        )
        .expect("hold sweep lease");

        let error = prepare_sweep(
            &request(root.path(), "2026-09-15T10:30:00Z", None, 1_800),
            Duration::from_millis(75),
        )
        .expect_err("a held sweep lease must not be treated as a successful sweep");

        assert!(
            matches!(error, PrepareSweepError::LeaseContention(_)),
            "{error:?}"
        );
        drop(held);
    }

    // The end-to-end case the pure classification test above cannot reach on
    // its own (ostrom#599): a pass whose sweep preparation contends exits
    // with the dedicated status and reason, before any agent turn runs, the
    // same way `failed_sweep_ends_the_pass_before_an_agent_turn_and_records_the_reason`
    // proves it for a generic sweep failure. The injectable `sweep_wait`
    // (the third argument here, unavailable through the public `run_pass`)
    // is what makes this reachable without a real 1800s wait.
    #[test]
    fn a_pass_whose_sweep_preparation_contends_exits_with_the_contention_status() {
        let root = tempfile::tempdir().expect("contended pass fixture");
        fs::write(root.path().join("loop-armed"), "").expect("arm pass");
        let paths = OstromPaths {
            config: root.path().to_path_buf(),
            state: root.path().to_path_buf(),
        };
        let held = OwnedLease::acquire(
            &paths.state,
            "sweep.lease",
            "in-flight-sweep",
            Clock::realtime().epoch_seconds(),
            60,
        )
        .expect("hold sweep lease");

        let error = run_pass_with_bridge_probe_timeout(
            &request(root.path(), "2026-09-15T10:30:00Z", None, 1_800),
            BRIDGE_HARNESS_PROBE_TIMEOUT,
            Duration::from_millis(75),
        )
        .expect_err("a held sweep lease must exit the pass with contention, not run the agent");

        assert_eq!(error.exit_code(), SWEEP_LEASE_CONTENTION_EXIT_CODE);
        assert!(
            matches!(&error, PassError::Failed { code, .. } if *code == SWEEP_LEASE_CONTENTION_EXIT_CODE),
            "{error:?}"
        );
        assert!(
            error.to_string().contains("sweep-lease-contention"),
            "{error}"
        );
        assert!(!root.path().join("agent-must-not-run").exists());
        let trace = read_trace(&root.path().join("sprint.jsonl")).expect("read pass trace");
        let rows = trace
            .rows
            .iter()
            .map(|row| row.as_ref().expect("valid pass trace row"))
            .collect::<Vec<_>>();
        assert_eq!(rows[1].kind, "pass-ended");
        assert_eq!(rows[1].fact["reason"], "sweep-lease-contention");
        drop(held);
    }

    #[test]
    fn gatekeeper_repairs_a_fresh_state_snapshot_mismatch_with_one_sweep() {
        let root = tempfile::tempdir().expect("mismatched sweep fixture");
        let fixture = write_sweep_inputs(root.path());
        fs::write(
            root.path().join("state.json"),
            serde_json::to_vec(&json!({
                "sweep_generation": {
                    "id": "state-generation",
                    "completed_at": "2026-09-15T10:20:00Z"
                }
            }))
            .expect("serialize mismatched state"),
        )
        .expect("write mismatched state");
        fs::write(
            root.path().join("sweep-snapshot.json"),
            serde_json::to_vec(&json!({
                "generation": {
                    "id": "snapshot-generation",
                    "completed_at": "2026-09-15T10:20:00Z"
                },
                "repositories": []
            }))
            .expect("serialize mismatched snapshot"),
        )
        .expect("write mismatched snapshot");
        let mut request = request(root.path(), "2026-09-15T10:30:00Z", Some(fixture), 1_800);
        request.role = PassRole::Gatekeeper;
        request.repository_scope = Some(vec!["placeholder-org/alpha".to_owned()]);

        let prepared =
            prepare_sweep(&request, DEFAULT_SWEEP_WAIT).expect("gatekeeper repairs mismatch");
        assert!(matches!(prepared.outcome, PassSweepOutcome::Swept(_)));
        assert_eq!(prepared.snapshots.as_ref().map(Vec::len), Some(1));
        let state: serde_json::Value = serde_json::from_slice(
            &fs::read(root.path().join("state.json")).expect("read repaired state"),
        )
        .expect("parse repaired state");
        let snapshot: serde_json::Value = serde_json::from_slice(
            &fs::read(root.path().join("sweep-snapshot.json")).expect("read repaired snapshot"),
        )
        .expect("parse repaired snapshot");
        assert_eq!(state["sweep_generation"], snapshot["generation"]);
    }
}
