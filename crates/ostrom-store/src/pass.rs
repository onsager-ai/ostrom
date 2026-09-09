use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread,
    time::Duration,
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
    RunEventStart, SignalFlags, TraceAppend, append_trace, environment, generated_run_id,
    pass_control, pass_control::ControlInput, read_lease, read_pass_state, read_trace,
    selection::dispatchability_snapshot, write_pass_state,
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
// EX_TEMPFAIL: the pass is held at its daily spend cap and can run once
// the ceiling resets or is raised.
const BUDGET_HELD_EXIT_CODE: i32 = 75;
const DEFAULT_DAILY_CAP_USD: f64 = 50.0;
const DEFAULT_LEASE_TTL_SECONDS: u64 = 3_600;
const PASS_TERMINATION_GRACE: Duration = Duration::from_millis(PASS_KILL_GRACE_MS);

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
    pub caps: RunCaps,
    pub clock: Clock,
    /// The value [`std::env::consts::OS`] would report, threaded through
    /// explicitly so the ostrom#544 platform fallback can be exercised by
    /// injection rather than by requiring a Windows host in CI. Production
    /// always passes `std::env::consts::OS` itself.
    pub platform: &'static str,
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
}

impl PassError {
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::Failed { code, .. } => *code,
            Self::LeaseHeld(_) => 0,
            Self::Disarmed(_) => DISARMED_EXIT_CODE,
            Self::BudgetHeld(_) => BUDGET_HELD_EXIT_CODE,
        }
    }

    fn failed(role: PassRole, message: impl Into<String>, code: i32) -> Self {
        Self::Failed {
            role: role.name(),
            message: message.into(),
            code,
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
        "no-op" => EventRunOutcome::NoOp,
        // The fact ledger calls a spend refusal held; ethogram calls it blocked.
        "held" => EventRunOutcome::Blocked,
        "timed-out" => EventRunOutcome::TimedOut,
        "capped" => EventRunOutcome::Capped,
        "permission-denied" => EventRunOutcome::PermissionDenied,
        "interrupted" => EventRunOutcome::Interrupted,
        "canceled" => EventRunOutcome::Canceled,
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
    let mut events = RunEventGuard::start(
        &request.paths,
        request.events_fd,
        request.facts_only,
        request.clock.clone(),
        RunEventStart {
            run_id: generated_run_id(request.role.name(), &request.clock),
            kind: RunKind::Loop,
            actor: request.role.name().to_owned(),
            harness: "claude".to_owned(),
            model: None,
            schedule: None,
            repository: None,
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
        events,
        control: None,
        process_exit: ProcessExit::Abnormal,
        permission_bridge: None,
    };
    append_trace(
        &request.paths.trace_file(),
        &TraceAppend {
            ts: guard.trace_time.clone(),
            kind: "pass-started".to_owned(),
            fact: Map::from_iter([("owner".to_owned(), json!(owner))]),
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
            &request.prompt,
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
    guard.finish()?;
    if status.success() {
        Ok(())
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
        RunEventGuard, RunEventStart, RunKind, append_observed, read_trace, sink_refused_draft,
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
                kind: RunKind::Loop,
                actor: "builder".to_owned(),
                harness: "claude".to_owned(),
                model: None,
                schedule: None,
                repository: None,
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
        assert!(
            refusal["detail"]
                .as_str()
                .expect("validation detail")
                .contains("expected a string")
        );
        assert_eq!(refusal["truncated"], false);
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
        assert_eq!(refusal["truncated"], true);
        assert_eq!(
            refusal["detail"]
                .as_str()
                .expect("bounded detail")
                .chars()
                .count(),
            MAX_EXCERPT_SCALARS
        );
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
mod exit_code_tests {
    use super::{BUDGET_HELD_EXIT_CODE, DISARMED_EXIT_CODE, PassError};

    #[test]
    fn budget_held_disarmed_and_lease_held_have_distinct_exit_codes() {
        assert_eq!(BUDGET_HELD_EXIT_CODE, 75);
        assert_eq!(DISARMED_EXIT_CODE, 78);
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
    // The pass-level counterpart to `resolve_derived_settings_tests`: the same
    // injection (`PassRequest::platform`), run through the whole pass so the
    // real `Command` this pass spawns is asserted on, not a stand-in for it.
    use std::{fs, os::unix::fs::PermissionsExt};

    use umwelt_runtime::{FileSink, Source};

    use super::{Clock, OstromPaths, PassRequest, PassRole, run_pass};

    const CLAUDE_STREAM_JSON: &str = concat!(
        "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"fallback-fixture\",\"model\":\"claude-fixture\"}\n",
        "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"tool-fixture\",\"name\":\"Bash\",\"input\":{\"command\":\"true\"}}]}}\n",
        "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"tool-fixture\",\"content\":\"ok\",\"is_error\":false}]}}\n",
        "{\"type\":\"result\",\"subtype\":\"success\",\"session_id\":\"fallback-fixture\",\"duration_ms\":1,\"num_turns\":1,\"total_cost_usd\":0,",
        "\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"cache_read_input_tokens\":2,\"cache_creation_input_tokens\":3}}\n"
    );

    #[test]
    fn a_policy_adopted_pass_survives_a_platform_with_no_bridge() {
        let root = tempfile::tempdir().expect("temporary pass fixture");
        let paths = OstromPaths {
            config: root.path().to_path_buf(),
            state: root.path().to_path_buf(),
        };
        fs::write(paths.state.join("loop-armed"), "").expect("arm pass");
        let argv_file = root.path().join("argv.txt");
        let claude_bin = root.path().join("claude-stub");
        fs::write(
            &claude_bin,
            format!(
                "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" > {}\nprintf '%s' '{CLAUDE_STREAM_JSON}'\n",
                argv_file.display()
            ),
        )
        .expect("write claude stub");
        fs::set_permissions(&claude_bin, fs::Permissions::from_mode(0o755)).expect("chmod stub");

        let derived =
            r#"{"permissions":{"defaultMode":"dontAsk","allow":["Bash(ostrom deploy *)"]}}"#;
        let request = PassRequest {
            paths: paths.clone(),
            working_directory: root.path().to_path_buf(),
            role: PassRole::Builder,
            prompt: "ostrom#544 fallback fixture".to_owned(),
            permission_mode: PassRole::Builder.default_permission_mode(),
            derived_settings: Some(derived.to_owned()),
            claude_bin,
            signals: Default::default(),
            supervisor_pid: None,
            events_fd: None,
            control_fd: None,
            facts_only: false,
            caps: Default::default(),
            clock: Clock::default(),
            // Injected, not the host running this test (ostrom#544): CI runs
            // this on Linux, where the real bridge would otherwise succeed
            // and this test would exercise nothing.
            platform: "windows",
        };
        run_pass(&request).expect("a platform with no bridge must not fail the pass");

        let argv = fs::read_to_string(&argv_file).expect("read captured argv");
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
            settings_path.starts_with(paths.runs_dir()),
            "fallback settings must live under the run directory: {settings_path:?}"
        );
        assert_eq!(
            fs::read_to_string(&settings_path).expect("read fallback settings"),
            derived
        );
        assert!(
            !paths
                .state
                .join("roles/builder.derived.settings.json")
                .exists(),
            "a fallback pass must never write the shared derived settings file"
        );

        let run_entries: Vec<_> = fs::read_dir(paths.runs_dir())
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
        let events = FileSink::new(paths.runs_dir())
            .read_from(&run_id, 0)
            .expect("read durable events");
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
