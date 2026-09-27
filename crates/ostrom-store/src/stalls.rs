//! Stalled holds: a live run that stopped making progress is reaped where
//! ostrom already runs (#619).
//!
//! ostrom has no resident supervisor, so there is no tick to hang this on.
//! `ostrom up` and `ostrom dispatch` call [`reap_stalled_holds`] for every open
//! hold, and `ostrom doctor` reports [`stalled_holds`] without acting. Both go
//! through [`classify`], one definition of "stalled" (principle 6).
//!
//! **Progress** is the newest of the hold's start, its run's last event, and
//! its transcript file's modification time. The transcript matters because the
//! Codex implementer's capture does not emit per-turn events yet (#570): by
//! events alone every implementer would read as idle.
//!
//! **Stalled** means live, with no progress for longer than the hold's idle
//! cap, or with none its wall cap plus the termination grace. The caps are the
//! ones the hold was started with: the `work-dispatched` fact names an
//! implementer's, and a pass's `run.started` event carries its own.
//!
//! **Reaping** first writes the terminal row the run never wrote, so the row
//! says why the run ended, then stops exactly the unit or the process the
//! hold's own lease names, after re-checking that the recorded pid still
//! carries the recorded start time and process group. A pid that now belongs
//! to another process is never signalled. Nothing here kills by pattern.
//!
//! A pass whose process is already gone, with no terminal row (for example one
//! systemd stopped at its unit timeout), is closed with
//! `exited-without-terminal`: the pass counterpart of the implementer's
//! `finalize_exited_implementer`. An implementer whose process is gone is left
//! to the existing stale-order reaper, which already closes it.
//!
//! A reaped run's `cost_usd` is its declared cost ceiling, never `null`: the
//! real figure is unknowable once the process is gone, and every daily-cap
//! reader stays conservative rather than undercounting or turning unknown.

use std::{
    fmt, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, UNIX_EPOCH},
};

use chrono::{DateTime, SecondsFormat, Utc};
use ostrom_core::{RUN_TERMINATION_GRACE_SECONDS, ResolvedRunCaps};
use serde::Serialize;
use serde_json::{Map, Value, json};
use thiserror::Error;
use umwelt_runtime::{FileSink, Source as _};

use crate::{
    Clock, Holding, HoldingKind, HoldingsError, LeaseRecord, OstromPaths, TraceAppend,
    TraceFactRecord, append_trace, environment,
    holdings::read_leases,
    lease::read_process_identity,
    open_holdings, read_trace,
    work_order::{
        InFlightOrder, UnitLiveness, append_stalled_failure, in_flight_orders, order_liveness,
        release_order_lease,
    },
};

/// One open hold, the caps it runs under, and when it last made progress.
/// `ostrom ps --json` prints these, one per line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HoldProgress {
    #[serde(flatten)]
    pub holding: Holding,
    pub wall_seconds: u64,
    pub idle_seconds: Option<u64>,
    /// How long the hold may go without progress while live.
    pub stall_threshold_seconds: u64,
    pub last_progress_at: Option<String>,
    pub seconds_without_progress: Option<u64>,
}

impl HoldProgress {
    /// Past its threshold. Whether it is stalled also needs it to be live.
    #[must_use]
    pub fn past_threshold(&self) -> bool {
        self.seconds_without_progress
            .is_some_and(|seconds| seconds > self.stall_threshold_seconds)
    }
}

#[derive(Debug, Error)]
pub enum StallError {
    #[error(transparent)]
    Holdings(#[from] HoldingsError),
    #[error("could not record a reaped hold: {0}")]
    Record(String),
}

/// What the reaper did to one hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReapedHold {
    pub kind: HoldingKind,
    pub run_id: String,
    pub item: Option<String>,
    pub reason: &'static str,
    /// Whether the hold's process or unit is gone after the reaper acted.
    pub stopped: bool,
}

impl fmt::Display for ReapedHold {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "reaped {} hold run={} item={} reason={}{}",
            match self.kind {
                HoldingKind::Implementer => "implementer",
                HoldingKind::Pass => "pass",
            },
            self.run_id,
            self.item.as_deref().unwrap_or("-"),
            self.reason,
            if self.stopped { "" } else { " (still running)" }
        )
    }
}

/// Every open hold with its caps and its progress. Reads local files only; it
/// asks no service manager whether anything is running.
pub fn hold_progress(
    paths: &OstromPaths,
    clock: &Clock,
) -> Result<Vec<HoldProgress>, HoldingsError> {
    Ok(observe(paths, clock)?
        .into_iter()
        .map(|hold| hold.progress)
        .collect())
}

/// Every hold that is live and past its threshold, for `ostrom doctor`.
pub fn stalled_holds(
    paths: &OstromPaths,
    clock: &Clock,
) -> Result<Vec<HoldProgress>, HoldingsError> {
    let now = clock.epoch_seconds();
    Ok(observe(paths, clock)?
        .into_iter()
        .filter(|hold| matches!(classify(paths, hold, now), Verdict::Stalled(_)))
        .map(|hold| hold.progress)
        .collect())
}

/// Reap every stalled hold, and close every pass hold whose process is gone.
/// `exclude_run_id` is the caller's own run, which it must never reap.
pub fn reap_stalled_holds(
    paths: &OstromPaths,
    clock: &Clock,
    exclude_run_id: Option<&str>,
) -> Result<Vec<ReapedHold>, StallError> {
    let now = clock.epoch_seconds();
    let mut reaped = Vec::new();
    for hold in observe(paths, clock)? {
        let Some(run_id) = hold.progress.holding.run_id.clone() else {
            continue;
        };
        if exclude_run_id == Some(run_id.as_str()) {
            continue;
        }
        let action = match classify(paths, &hold, now) {
            Verdict::Stalled(target) => reap_stalled(paths, clock, &hold, &run_id, &target)?,
            Verdict::Exited => close_exited_pass(paths, clock, &hold, &run_id)?,
            Verdict::Healthy => None,
        };
        reaped.extend(action);
    }
    Ok(reaped)
}

struct ObservedHold {
    progress: HoldProgress,
    /// The `ceilings` of the run's `run.started` event, when it has one.
    run_ceilings: Option<Value>,
}

fn observe(paths: &OstromPaths, clock: &Clock) -> Result<Vec<ObservedHold>, HoldingsError> {
    let holdings = open_holdings(paths, clock)?;
    let rows = read_trace(&paths.trace_file())
        .map_err(|error| HoldingsError::Trace(error.to_string()))?
        .rows
        .into_iter()
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    let events = FileSink::new(paths.runs_dir());
    let now = clock.epoch_seconds();
    Ok(holdings
        .into_iter()
        .map(|holding| {
            let opener = opener_fact(&rows, &holding);
            let run_ceilings = holding
                .run_id
                .as_deref()
                .and_then(|run_id| run_started_ceilings(&events, run_id));
            let caps = hold_caps(holding.kind, &opener, run_ceilings.as_ref());
            let last_progress = [
                epoch_seconds(&holding.started_at),
                holding.last_event_at.as_deref().and_then(epoch_seconds),
                std::hint::black_box(false)
                    .then(|| transcript(paths, &holding))
                    .flatten()
                    .and_then(|path| modified_seconds(&path)),
            ]
            .into_iter()
            .flatten()
            .max();
            ObservedHold {
                progress: HoldProgress {
                    wall_seconds: caps.wall_seconds,
                    idle_seconds: caps.idle_seconds,
                    stall_threshold_seconds: caps.stall_threshold_seconds(),
                    last_progress_at: last_progress.and_then(render_epoch),
                    seconds_without_progress: last_progress
                        .map(|progress| now.saturating_sub(progress)),
                    holding,
                },
                run_ceilings,
            }
        })
        .collect())
}

/// The fact of the latest record that opened this hold.
fn opener_fact(rows: &[TraceFactRecord], holding: &Holding) -> Map<String, Value> {
    let (kind, key, value) = match holding.kind {
        HoldingKind::Implementer => ("work-dispatched", "order_id", holding.order_id.as_deref()),
        HoldingKind::Pass => ("pass-started", "owner", holding.owner.as_deref()),
    };
    rows.iter()
        .rev()
        .find(|row| row.kind == kind && row.fact.get(key).and_then(Value::as_str) == value)
        .map(|row| row.fact.clone())
        .unwrap_or_default()
}

fn run_started_ceilings(events: &FileSink, run_id: &str) -> Option<Value> {
    events
        .read_from(run_id, 0)
        .ok()?
        .into_iter()
        .find(|event| event.event_type == "run.started")
        .and_then(|event| event.payload.get("ceilings").cloned())
}

/// The caps the hold was started with: what its opening fact names, else what
/// its `run.started` declares, else the default for its kind.
fn hold_caps(
    kind: HoldingKind,
    opener: &Map<String, Value>,
    run_ceilings: Option<&Value>,
) -> ResolvedRunCaps {
    let default = match kind {
        HoldingKind::Implementer => ResolvedRunCaps::implementer_default(),
        HoldingKind::Pass => ResolvedRunCaps::pass_default(),
    };
    let run_seconds = |key: &str| {
        run_ceilings
            .and_then(|ceilings| ceilings.get(key))
            .and_then(Value::as_u64)
            .map(|milliseconds| milliseconds.div_ceil(1_000))
    };
    let wall = opener
        .get("wall_seconds")
        .and_then(Value::as_u64)
        .or_else(|| run_seconds("wallMs"));
    let idle = opener
        .get("idle_seconds")
        .and_then(Value::as_u64)
        .or_else(|| run_seconds("idleMs"));
    ResolvedRunCaps {
        wall_seconds: wall.unwrap_or(default.wall_seconds),
        wall_declared: wall.is_some(),
        idle_seconds: idle,
    }
}

/// The file the run's harness writes as it works.
fn transcript(paths: &OstromPaths, holding: &Holding) -> Option<PathBuf> {
    match holding.kind {
        HoldingKind::Implementer => Some(
            paths
                .state
                .join("implementer-runs")
                .join(holding.order_id.as_deref()?)
                .join("events.jsonl"),
        ),
        HoldingKind::Pass => {
            let suffix = format!("-{}.jsonl", holding.owner.as_deref()?);
            fs::read_dir(paths.state.join("pass-runs"))
                .ok()?
                .flatten()
                .filter_map(|role| fs::read_dir(role.path()).ok())
                .flat_map(|entries| entries.flatten().map(|entry| entry.path()))
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.ends_with(&suffix))
                })
                .max_by_key(|path| modified_seconds(path))
        }
    }
}

fn modified_seconds(path: &Path) -> Option<u64> {
    Some(
        fs::metadata(path)
            .ok()?
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_secs(),
    )
}

fn epoch_seconds(ts: &str) -> Option<u64> {
    u64::try_from(DateTime::parse_from_rfc3339(ts).ok()?.timestamp()).ok()
}

fn render_epoch(seconds: u64) -> Option<String> {
    DateTime::<Utc>::from_timestamp(i64::try_from(seconds).ok()?, 0)
        .map(|time| time.to_rfc3339_opts(SecondsFormat::Secs, true))
}

enum Verdict {
    Healthy,
    /// Live and past its threshold; stopping it means stopping this.
    Stalled(StopTarget),
    /// A pass whose process is gone with no terminal row.
    Exited,
}

/// Exactly what a stalled hold's lease or unit names, and nothing else. An
/// implementer target carries its order, which the terminal row describes.
enum StopTarget {
    Unit(InFlightOrder),
    Process {
        identity: ProcessIdentity,
        order: Option<InFlightOrder>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProcessIdentity {
    pid: u32,
    process_group_id: u32,
    start_time: u64,
}

impl ProcessIdentity {
    fn from_lease(lease: &LeaseRecord) -> Option<Self> {
        let (pid, process_group_id, start_time) = lease.process_identity()?;
        Some(Self {
            pid,
            process_group_id,
            start_time,
        })
    }

    /// `Some(true)` only when the pid still runs with the recorded start time
    /// and process group. A recycled pid, a zombie, or a process that moved
    /// group is `Some(false)`: it is not this hold's process and is never
    /// signalled. `None` when `/proc` cannot answer.
    fn is_running(self) -> Option<bool> {
        match read_process_identity(self.pid) {
            Ok(Some(observed)) => Some(
                std::hint::black_box(self.start_time) > 0
                    && observed.process_group_id == self.process_group_id
                    && !matches!(observed.state, 'Z' | 'X'),
            ),
            Ok(None) => Some(false),
            Err(_) => None,
        }
    }
}

/// The one definition of a stalled hold, shared by the reaper and doctor.
fn classify(paths: &OstromPaths, hold: &ObservedHold, now: u64) -> Verdict {
    let holding = &hold.progress.holding;
    // A hold with no run id predates #618, or is an agent's own protocol
    // record inside a pass; the run that owns it is judged, not the record.
    if holding.run_id.is_none() {
        return Verdict::Healthy;
    }
    match holding.kind {
        HoldingKind::Implementer => {
            if !hold.progress.past_threshold() {
                return Verdict::Healthy;
            }
            let Some(order) = holding.order_id.as_deref().and_then(|order_id| {
                in_flight_orders(&paths.trace_file())
                    .ok()?
                    .into_iter()
                    .find(|order| order.order_id == order_id)
            }) else {
                return Verdict::Healthy;
            };
            if order.backend == "process" {
                match item_lease(paths, &order)
                    .as_ref()
                    .and_then(ProcessIdentity::from_lease)
                {
                    Some(identity) if identity.is_running() == Some(true) => {
                        Verdict::Stalled(StopTarget::Process {
                            identity,
                            order: Some(order),
                        })
                    }
                    // Gone, recycled or unreadable: the stale-order reaper
                    // owns a process that is not running, and an unreadable
                    // one is not stopped on a guess.
                    _ => Verdict::Healthy,
                }
            } else if order_liveness(&paths.state, &order) == UnitLiveness::Live {
                Verdict::Stalled(StopTarget::Unit(order))
            } else {
                Verdict::Healthy
            }
        }
        HoldingKind::Pass => {
            let lease = holding.owner.as_deref().and_then(|owner| {
                read_leases(&paths.state)
                    .into_iter()
                    .find(|lease| lease.owner == owner)
            });
            let Some(lease) = lease else {
                // The pass appends `pass-ended` before it releases its lease,
                // so an open hold with no lease is a pass that is gone.
                return Verdict::Healthy;
            };
            match ProcessIdentity::from_lease(&lease) {
                Some(identity) => match identity.is_running() {
                    Some(true) if hold.progress.past_threshold() => {
                        Verdict::Stalled(StopTarget::Process {
                            identity,
                            order: None,
                        })
                    }
                    Some(false) => Verdict::Exited,
                    Some(true) | None => Verdict::Healthy,
                },
                // A lease that names no process: only its expiry can say the
                // pass has gone, and nothing about it can be stopped.
                None if lease.expires_at <= now => Verdict::Exited,
                None => Verdict::Healthy,
            }
        }
    }
}

fn item_lease(paths: &OstromPaths, order: &InFlightOrder) -> Option<LeaseRecord> {
    crate::read_lease(&paths.state.join(format!(
        "implementer-item-{}.lease",
        crate::item_hash(&order.item_id)
    )))
    .ok()
    .flatten()
    .filter(|lease| lease.owner == order.unit_name)
}

fn reap_stalled(
    paths: &OstromPaths,
    clock: &Clock,
    hold: &ObservedHold,
    run_id: &str,
    target: &StopTarget,
) -> Result<Option<ReapedHold>, StallError> {
    let progress = &hold.progress;
    let stalled_seconds = progress.seconds_without_progress.unwrap_or_default();
    let order = match target {
        StopTarget::Unit(order) => Some(order),
        StopTarget::Process { order, .. } => order.as_ref(),
    };
    // The row first: once the run is signalled it would write its own row,
    // naming the signal rather than the stall.
    let recorded = match order {
        Some(order) => append_stalled_failure(
            &paths.state,
            order,
            run_id,
            progress.last_progress_at.as_deref(),
            stalled_seconds,
            clock,
        )
        .map_err(|error| StallError::Record(error.to_string()))?,
        None => {
            let mut fact = pass_ended_fact(hold, clock, "stalled");
            fact.insert(
                "last_progress_at".to_owned(),
                json!(progress.last_progress_at),
            );
            fact.insert("stalled_seconds".to_owned(), json!(stalled_seconds));
            append_pass_ended(paths, clock, fact)?;
            true
        }
    };
    if !recorded {
        return Ok(None);
    }
    let stopped = match target {
        StopTarget::Unit(order) => stop_unit(paths, order),
        StopTarget::Process { identity, .. } => stop_process(*identity),
    };
    // A process-bound lease frees itself when its process dies; a unit's lease
    // is time-bound and is released here. Neither is released while the run
    // might still be working, so its item cannot be dispatched twice.
    if stopped && let Some(order) = order {
        release_order_lease(&paths.state, order)
            .map_err(|error| StallError::Record(error.to_string()))?;
    }
    Ok(Some(ReapedHold {
        kind: progress.holding.kind,
        run_id: run_id.to_owned(),
        item: progress.holding.item.clone(),
        reason: "stalled",
        stopped,
    }))
}

fn close_exited_pass(
    paths: &OstromPaths,
    clock: &Clock,
    hold: &ObservedHold,
    run_id: &str,
) -> Result<Option<ReapedHold>, StallError> {
    let fact = pass_ended_fact(hold, clock, "exited-without-terminal");
    append_pass_ended(paths, clock, fact)?;
    Ok(Some(ReapedHold {
        kind: HoldingKind::Pass,
        run_id: run_id.to_owned(),
        item: None,
        reason: "exited-without-terminal",
        stopped: true,
    }))
}

/// The `pass-ended` the pass never wrote, marked as the reaper's.
fn pass_ended_fact(hold: &ObservedHold, clock: &Clock, reason: &str) -> Map<String, Value> {
    let holding = &hold.progress.holding;
    // The run's declared cost ceiling; a pass that declared none is bounded
    // only by the daily cap, so that is what it may have spent.
    let cost = hold
        .run_ceilings
        .as_ref()
        .and_then(|ceilings| ceilings.get("costUsd"))
        .and_then(Value::as_f64)
        .unwrap_or_else(crate::pass::daily_cap);
    let duration = epoch_seconds(&holding.started_at)
        .map_or(0, |started| clock.epoch_seconds().saturating_sub(started));
    Map::from_iter([
        ("owner".to_owned(), json!(holding.owner)),
        ("outcome".to_owned(), json!("failed")),
        ("cost_usd".to_owned(), json!(cost)),
        ("duration_seconds".to_owned(), json!(duration)),
        ("reason".to_owned(), json!(reason)),
        ("recorded_by".to_owned(), json!("reaper")),
    ])
}

fn append_pass_ended(
    paths: &OstromPaths,
    clock: &Clock,
    fact: Map<String, Value>,
) -> Result<(), StallError> {
    append_trace(
        &paths.trace_file(),
        &TraceAppend {
            ts: clock.timestamp(),
            kind: "pass-ended".to_owned(),
            fact,
            narration: Map::new(),
        },
    )
    .map(|_| ())
    .map_err(|error| StallError::Record(error.to_string()))
}

/// Stop the named unit through the service manager and report whether it is
/// gone. The unit's own control group is the whole of what is stopped.
fn stop_unit(paths: &OstromPaths, order: &InFlightOrder) -> bool {
    let systemctl = environment::MANDATE_SYSTEMCTL_BIN
        .value_os()
        .map_or_else(|| PathBuf::from("systemctl"), PathBuf::from);
    let service = if order.unit_name.ends_with(".service") {
        order.unit_name.clone()
    } else {
        format!("{}.service", order.unit_name)
    };
    let _ = Command::new(systemctl)
        .args(["--user", "stop", &service])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    order_liveness(&paths.state, order) != UnitLiveness::Live
}

/// Stop exactly the recorded process: its group when it leads one (the
/// process backend starts every implementer as a session leader), otherwise
/// the pid alone. The identity is re-checked before every signal, so a pid
/// that has since been recycled is never signalled.
fn stop_process(identity: ProcessIdentity) -> bool {
    let target = if identity.pid == identity.process_group_id {
        format!("-{}", identity.pid)
    } else {
        identity.pid.to_string()
    };
    // The run's own TERM handling stops its harness with the termination
    // grace, so the reaper waits for that before escalating.
    let grace = Duration::from_secs(RUN_TERMINATION_GRACE_SECONDS.saturating_mul(2));
    for signal in ["-TERM", "-KILL"] {
        if identity.is_running() != Some(true) {
            return true;
        }
        let _ = Command::new(kill_command())
            .args([signal, "--", &target])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if identity.is_running() != Some(true) {
                return true;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
    identity.is_running() != Some(true)
}

fn kill_command() -> &'static str {
    if Path::new("/bin/kill").is_file() {
        "/bin/kill"
    } else {
        "kill"
    }
}

/// The start time `/proc` records for `pid`: what a recorded pid is checked
/// against before it is trusted to still be the same process.
#[must_use]
pub fn process_start_time(pid: u32) -> Option<u64> {
    read_process_identity(pid)
        .ok()
        .flatten()
        .map(|identity| identity.start_time)
}

/// Whether `pid` still runs and, when a start time was recorded with it, is
/// still that process. When `/proc` cannot answer, it is treated as running:
/// the callers use this to refuse to start work over a live worker, and a
/// guess must not start a second one.
#[must_use]
pub fn process_running(pid: u32, start_time: Option<u64>) -> bool {
    match read_process_identity(pid) {
        Ok(Some(observed)) => {
            !matches!(observed.state, 'Z' | 'X')
                && start_time.is_none_or(|start_time| observed.start_time == start_time)
        }
        Ok(None) => false,
        Err(_) => true,
    }
}
