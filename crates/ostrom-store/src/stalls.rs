//! Stalled holds: a live run that stopped making progress is reaped where
//! ostrom already runs (#619, #635).
//!
//! ostrom has no resident supervisor, so there is no tick to hang this on.
//! `ostrom up` and `ostrom dispatch` call [`reap_stalled_holds`], `up` for every
//! open hold and `dispatch` for the implementer holds in its own repositories,
//! and `ostrom doctor` reports [`stalled_holds`] and [`reaper_findings`]
//! without acting. Both go through [`classify`], one definition of "stalled"
//! (principle 6).
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
//! **Reaping** is claim, stop, confirm, record (#635):
//!
//! 1. The reaper creates `<state>/reaping/<run_id>.claim` with `create_new`,
//!    recording the reason, the cost it charges and its own pid and start
//!    time. Exactly one reaper can create it; any other skips the hold, so two
//!    reapers never both stop, record or charge one run.
//! 2. It stops exactly the unit or process the hold's own lease names, after
//!    re-checking that the recorded pid still carries the recorded start time
//!    and process group, and never its own process or process group. Nothing
//!    here kills by pattern.
//! 3. A run that handles the signal writes its own terminal row, and when a
//!    claim names its run it writes the claim's reason and cost: the row says
//!    `stalled` whoever writes it.
//! 4. Only once the stop is confirmed, and only when no terminal row exists
//!    yet (the run was killed or never reached its terminal path), does the
//!    reaper write the row, release an implementer's lease and remove the
//!    claim. A stop it cannot confirm (a liveness it cannot read, a process
//!    that outlived `KILL`, a target it refused) leaves no row, releases
//!    nothing and keeps the claim.
//!
//! The next reaper examines a kept claim before anything else about that
//! hold. A claim whose reaper has died is taken over and completed with what
//! it recorded; one whose run already has a terminal row is removed.
//!
//! A pass whose process is already gone, with no terminal row (for example one
//! systemd stopped at its unit timeout), is closed with
//! `exited-without-terminal` under the same claim. An implementer whose process
//! is gone with no claim is left to the existing stale-order reaper, which
//! already closes it.
//!
//! A pass is judged by the generation of the lease it started under, not by
//! whether some lease still names its owner (#636). While the lease names it,
//! the lease's process is the pass's. Once another pass has taken the lease
//! over, or none holds it, the pass is judged by the process its `pass-started`
//! recorded: one still running with that identity has been displaced, not
//! exited, and is left to end itself with `pass-lease-lost` unless it is past
//! its stall threshold, when that recorded process is stopped like any other
//! stalled pass; only one whose process is gone is closed.
//!
//! A reaped run's `cost_usd` is its declared cost ceiling, never `null`: the
//! real figure is unknowable once the process is gone, and every daily-cap
//! reader stays conservative rather than undercounting or turning unknown.
//!
//! **Suspended time is not silence** (#637). A run's own watchdog measures with
//! a clock that stops while the machine is suspended; the wall clock does not.
//! A gap in progress, and a lease's expiry where the reaper judges it, are
//! measured net of the time the machine spent suspended ([`crate::suspend`]).
//! When that time cannot be read, the reaper judges nothing that invocation:
//! it records why for doctor and reaps nothing, rather than reap blind.
//!
//! Reaping is best effort and scheduling is not: an error is printed, recorded
//! in `<state>/reaping/last-error.json` for doctor, and never stops `up` or
//! `dispatch` from launching.
//!
//! Everything under `<state>/reaping/` is private state that only ostrom reads.

use std::{
    collections::BTreeSet,
    fmt, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, UNIX_EPOCH},
};

use chrono::{DateTime, SecondsFormat, Utc};
use ostrom_core::{DEFAULT_RUN_COST_CEILING_USD, RUN_TERMINATION_GRACE_SECONDS, ResolvedRunCaps};
use serde::Serialize;
use serde_json::{Map, Value, json};
use thiserror::Error;
use umwelt_runtime::{FileSink, Source as _};

use crate::{
    Clock, Holding, HoldingKind, HoldingsError, LeaseRecord, OstromPaths, TraceAppend,
    TraceFactRecord, append_trace,
    claim::{self, Claim, ClaimError, Holder, HolderKeys},
    environment,
    holdings::read_leases,
    lease::{read_process_identity, read_process_identity_at},
    open_holdings, read_trace,
    suspend::{SuspendSource, SuspendTimeline, SystemSuspend},
    work_order::{
        InFlightOrder, ReapedFailure, UnitLiveness, append_reaped_failure, in_flight_orders,
        order_liveness, release_order_lease,
    },
};

/// The claim's holder fields, named as the #635 spec names them.
const REAPER: HolderKeys = HolderKeys {
    pid: "reaper_pid",
    start_time: "reaper_start_time",
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
    /// Seconds since the last progress, net of the time the machine spent
    /// suspended (#637).
    pub seconds_without_progress: Option<u64>,
    /// How many of the wall-clock seconds since the last progress the machine
    /// spent suspended, at most. `None` when that could not be read, and then
    /// `seconds_without_progress` is the plain wall-clock gap.
    pub suspended_seconds: Option<u64>,
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
    #[error(
        "reaping skipped: the time this machine spent suspended could not be read ({0}), and no stall is judged without it"
    )]
    SuspendUnknown(String),
}

/// What the reaper did to one hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReapedHold {
    pub kind: HoldingKind,
    pub run_id: String,
    pub item: Option<String>,
    pub reason: String,
    /// Whether the stop was confirmed. When it was not, no row was written,
    /// nothing was released, and the claim is kept for the next reaper.
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
            if self.stopped {
                ""
            } else {
                " (stop unconfirmed; claim kept, nothing recorded or released)"
            }
        )
    }
}

/// Every open hold with its caps and its progress. Reads local files only; it
/// asks no service manager whether anything is running.
pub fn hold_progress(
    paths: &OstromPaths,
    clock: &Clock,
) -> Result<Vec<HoldProgress>, HoldingsError> {
    let timeline = SuspendTimeline::read(&paths.state, clock, &SystemSuspend).ok();
    Ok(observe(paths, clock, timeline.as_ref())?
        .into_iter()
        .map(|hold| hold.progress)
        .collect())
}

/// Every hold that is live and past its threshold, for `ostrom doctor`. When
/// the time the machine spent suspended cannot be read, no hold is judged
/// stalled, as the reaper judges none; the reaper records why for doctor.
pub fn stalled_holds(
    paths: &OstromPaths,
    clock: &Clock,
) -> Result<Vec<HoldProgress>, HoldingsError> {
    let Ok(timeline) = SuspendTimeline::read(&paths.state, clock, &SystemSuspend) else {
        return Ok(Vec::new());
    };
    let now = clock.epoch_seconds();
    Ok(observe(paths, clock, Some(&timeline))?
        .into_iter()
        .filter(|hold| matches!(classify(paths, hold, now, &timeline), Verdict::Stalled(_)))
        .map(|hold| hold.progress)
        .collect())
}

/// Reap every stalled hold in scope, complete every claim a dead reaper left,
/// and close every pass hold whose process is gone.
///
/// `exclude_run_id` is the caller's own run, which it must never reap.
/// `repositories` scopes the reap to implementer holds for those repositories;
/// `None` reaps every hold. It never fails: an error is printed under
/// `caller`, recorded for doctor, and the caller goes on scheduling.
pub fn reap_stalled_holds(
    paths: &OstromPaths,
    clock: &Clock,
    caller: &str,
    exclude_run_id: Option<&str>,
    repositories: Option<&BTreeSet<String>>,
) -> Vec<ReapedHold> {
    reap_stalled_holds_with(
        paths,
        clock,
        &SystemSuspend,
        caller,
        exclude_run_id,
        repositories,
    )
}

/// [`reap_stalled_holds`] with the suspended time read from `suspend`. It is
/// read once. When it cannot be read, nothing is judged or reaped, and the
/// reason is recorded where doctor reads it (#637).
pub(crate) fn reap_stalled_holds_with(
    paths: &OstromPaths,
    clock: &Clock,
    suspend: &dyn SuspendSource,
    caller: &str,
    exclude_run_id: Option<&str>,
    repositories: Option<&BTreeSet<String>>,
) -> Vec<ReapedHold> {
    let mut pass = ReapPass::default();
    let reaped = match SuspendTimeline::read(&paths.state, clock, suspend) {
        Ok(timeline) => {
            let reaped = reap_all(
                paths,
                clock,
                &timeline,
                exclude_run_id,
                repositories,
                &mut pass,
            );
            if let Err(error) = timeline.record(&paths.state) {
                pass.errors.push((None, StallError::Record(error)));
            }
            reaped
        }
        Err(why) => {
            pass.errors.push((None, StallError::SuspendUnknown(why)));
            Vec::new()
        }
    };
    record_errors(paths, clock, caller, &pass);
    reaped
}

/// What one reap saw, so its record of failures clears only what it looked at.
#[derive(Default)]
struct ReapPass {
    /// Whether the open holds could be read at all.
    observed: bool,
    /// Every open hold's run id.
    open: BTreeSet<String>,
    /// The run ids this reap examined: in scope, not its own.
    examined: BTreeSet<String>,
    /// Each error, with the run it concerned when it concerned one.
    errors: Vec<(Option<String>, StallError)>,
}

/// What doctor should say about the reaper itself: its last error, and every
/// claim whose stop no live reaper is confirming.
#[must_use]
pub fn reaper_findings(paths: &OstromPaths) -> Vec<String> {
    let mut findings = Vec::new();
    match fs::read(last_error_path(paths)) {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|record| record.get("failures").and_then(Value::as_array).cloned())
        {
            Some(failures) => findings.extend(failures.iter().map(|failure| {
                format!(
                    "the stall reaper failed at {} in ostrom {} (run={}): {}",
                    failure["ts"].as_str().unwrap_or("-"),
                    failure["caller"].as_str().unwrap_or("-"),
                    failure["run_id"].as_str().unwrap_or("-"),
                    failure["error"].as_str().unwrap_or("-")
                )
            })),
            None => findings.push("the last reaper error record is unreadable".to_owned()),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => findings.push(format!(
            "the last reaper error record is unreadable: {error}"
        )),
    }
    for path in claim_files(paths) {
        let Ok(Some(file)) = claim::read(&path) else {
            continue;
        };
        if file.holder(REAPER) == Holder::Alive {
            continue;
        }
        let field = |key: &str| {
            file.record()
                .and_then(|record| record.get(key))
                .and_then(Value::as_str)
                .unwrap_or("-")
                .to_owned()
        };
        findings.push(format!(
            "claim on run={} reason={} kept since {}: its stop was not confirmed",
            field("run_id"),
            field("reason"),
            field("claimed_at")
        ));
    }
    findings
}

/// What a reaper's claim on a run tells the run's own terminal writer: the
/// reason and cost its row must carry, so the trace says why the run ended
/// whichever of the two writes the row (#635).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReapIntent {
    pub reason: String,
    pub cost_usd: f64,
    pub cost_basis: String,
    pub last_progress_at: Option<String>,
    pub stalled_seconds: Option<u64>,
}

impl ReapIntent {
    fn from_record(record: &Map<String, Value>) -> Option<Self> {
        Some(Self {
            reason: record.get("reason")?.as_str()?.to_owned(),
            cost_usd: record.get("cost_usd")?.as_f64()?,
            cost_basis: record.get("cost_basis")?.as_str()?.to_owned(),
            last_progress_at: record
                .get("last_progress_at")
                .and_then(Value::as_str)
                .map(str::to_owned),
            stalled_seconds: record.get("stalled_seconds").and_then(Value::as_u64),
        })
    }

    /// The fields a terminal row gains from the claim, beside its reason and
    /// cost.
    pub(crate) fn row_fields(&self) -> Map<String, Value> {
        let mut fields = Map::from_iter([("cost_basis".to_owned(), json!(self.cost_basis))]);
        if let Some(last_progress_at) = &self.last_progress_at {
            fields.insert("last_progress_at".to_owned(), json!(last_progress_at));
        }
        if let Some(stalled_seconds) = self.stalled_seconds {
            fields.insert("stalled_seconds".to_owned(), json!(stalled_seconds));
        }
        fields
    }
}

/// The reaper's intent for `run_id`, when a claim names that run and the
/// reaper has signalled it. A claim kept without a signal (a stop refused, or
/// a liveness unknown before the first signal) did not end the run: a failure
/// the run records on its own keeps its own reason (#635).
pub(crate) fn reap_intent(state: &Path, run_id: &str) -> Option<ReapIntent> {
    let file = claim::read(&claim_path(state, run_id)).ok().flatten()?;
    let record = file.record()?;
    record
        .get("signalled_at")
        .is_some_and(Value::is_string)
        .then(|| ReapIntent::from_record(record))
        .flatten()
}

/// Record in the claim that a signal is about to be sent, before it is sent,
/// so the run it reaches finds it. `false` when that cannot be recorded: then
/// nothing is signalled.
fn mark_signalled(claim: &mut Claim, clock: &Clock) -> bool {
    claim
        .record()
        .get("signalled_at")
        .is_some_and(Value::is_string)
        || claim.set("signalled_at", json!(clock.timestamp())).is_ok()
}

fn claims_dir(state: &Path) -> PathBuf {
    state.join("reaping")
}

fn claim_path(state: &Path, run_id: &str) -> PathBuf {
    let safe = !run_id.is_empty()
        && !run_id.starts_with('.')
        && run_id.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        });
    let name = if safe {
        run_id.to_owned()
    } else {
        crate::item_hash(run_id)
    };
    claims_dir(state).join(format!("{name}.claim"))
}

fn claim_files(paths: &OstromPaths) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(claims_dir(&paths.state)) else {
        return Vec::new();
    };
    let mut files = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "claim")
        })
        .collect::<Vec<_>>();
    files.sort();
    files
}

fn last_error_path(paths: &OstromPaths) -> PathBuf {
    claims_dir(&paths.state).join("last-error.json")
}

/// Print every error, and keep every failure no later reap has resolved where
/// doctor reads it. A failure is cleared only by a reap that looked at what
/// failed: its hold examined again, or gone; a failure that concerned no one
/// hold, by any reap that could read the holds.
fn record_errors(paths: &OstromPaths, clock: &Clock, caller: &str, pass: &ReapPass) {
    for (_, error) in &pass.errors {
        eprintln!("ostrom {caller}: stall reaper: {error}");
    }
    let path = last_error_path(paths);
    let previous = fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|record| record.get("failures").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    let new = pass
        .errors
        .iter()
        .map(|(run_id, error)| {
            json!({
                "ts": clock.timestamp(),
                "caller": caller,
                "run_id": run_id,
                "error": error.to_string(),
            })
        })
        .collect();
    let failures = retained_failures(previous, pass, new);
    let result = if failures.is_empty() {
        match fs::remove_file(&path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    } else {
        fs::create_dir_all(claims_dir(&paths.state))
            .and_then(|()| fs::write(&path, format!("{}\n", json!({ "failures": failures }))))
    };
    if let Err(error) = result {
        eprintln!(
            "ostrom {caller}: stall reaper: could not update {}: {error}",
            path.display()
        );
    }
}

fn retained_failures(previous: Vec<Value>, pass: &ReapPass, new: Vec<Value>) -> Vec<Value> {
    previous
        .into_iter()
        .filter(|failure| {
            // A failure that concerned no one hold is replaced by this reap's
            // own, if it has one.
            failure["run_id"].as_str().is_some_and(|run_id| {
                !pass.observed || (pass.open.contains(run_id) && !pass.examined.contains(run_id))
            })
        })
        .chain(new)
        .collect()
}

fn reap_all(
    paths: &OstromPaths,
    clock: &Clock,
    timeline: &SuspendTimeline,
    exclude_run_id: Option<&str>,
    repositories: Option<&BTreeSet<String>>,
    pass: &mut ReapPass,
) -> Vec<ReapedHold> {
    let now = clock.epoch_seconds();
    let holds = match observe(paths, clock, Some(timeline)) {
        Ok(holds) => holds,
        Err(error) => {
            pass.errors.push((None, error.into()));
            return Vec::new();
        }
    };
    pass.observed = true;
    pass.open = holds
        .iter()
        .filter_map(|hold| hold.progress.holding.run_id.clone())
        .collect();
    if let Err(error) = remove_settled_claims(paths) {
        pass.errors.push((None, error));
    }
    let mut reaped = Vec::new();
    for hold in holds {
        let Some(run_id) = hold.progress.holding.run_id.clone() else {
            continue;
        };
        if exclude_run_id == Some(run_id.as_str())
            || !in_scope(&hold.progress.holding, repositories)
        {
            continue;
        }
        pass.examined.insert(run_id.clone());
        // A claim a reaper left is examined before anything else is decided
        // about its run: what that reaper decided is carried out, not redone.
        let action = if claim_path(&paths.state, &run_id).exists() {
            resume_claim(paths, clock, &hold, &run_id, now, timeline)
        } else {
            match classify(paths, &hold, now, timeline) {
                Verdict::Stalled(target) => reap_stalled(paths, clock, &hold, &run_id, &target),
                Verdict::Exited => close_exited_pass(paths, clock, &hold, &run_id),
                Verdict::Healthy | Verdict::Displaced => Ok(None),
            }
        };
        match action {
            Ok(action) => reaped.extend(action),
            Err(error) => pass.errors.push((Some(run_id), error)),
        }
    }
    reaped
}

/// Whether a hold is one this reaper may act on. `dispatch` reaps only the
/// implementer holds for its own repositories; a pass names none (#635 N1).
fn in_scope(holding: &Holding, repositories: Option<&BTreeSet<String>>) -> bool {
    let Some(repositories) = repositories else {
        return true;
    };
    holding.kind == HoldingKind::Implementer
        && holding
            .item
            .as_deref()
            .and_then(|item| item.rsplit_once('#'))
            .is_some_and(|(repository, _)| repositories.contains(repository))
}

/// Remove every claim whose reaper has died and whose run already has a
/// terminal row: the run, or that reaper before it died, recorded it.
fn remove_settled_claims(paths: &OstromPaths) -> Result<(), StallError> {
    let files = claim_files(paths);
    if files.is_empty() {
        return Ok(());
    }
    let rows = trace_rows(paths)?;
    for path in files {
        let Ok(Some(file)) = claim::read(&path) else {
            continue;
        };
        if file.holder(REAPER) != Holder::Dead {
            continue;
        }
        let Some(record) = file.record() else {
            continue;
        };
        let kind = match record.get("kind").and_then(Value::as_str) {
            Some("implementer") => HoldingKind::Implementer,
            Some("pass") => HoldingKind::Pass,
            _ => continue,
        };
        let key = |name: &str| record.get(name).and_then(Value::as_str);
        if terminal_row_exists(&rows, kind, key("order_id"), key("owner")) {
            match fs::remove_file(&path) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    return Err(StallError::Record(format!(
                        "could not remove settled claim {}: {error}",
                        path.display()
                    )));
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn trace_rows(paths: &OstromPaths) -> Result<Vec<TraceFactRecord>, StallError> {
    Ok(read_trace(&paths.trace_file())
        .map_err(|error| HoldingsError::Trace(error.to_string()))?
        .rows
        .into_iter()
        .filter_map(Result::ok)
        .collect())
}

fn terminal_row_exists(
    rows: &[TraceFactRecord],
    kind: HoldingKind,
    order_id: Option<&str>,
    owner: Option<&str>,
) -> bool {
    let names = |row: &TraceFactRecord, key: &str, value: Option<&str>| {
        value.is_some_and(|value| row.fact.get(key).and_then(Value::as_str) == Some(value))
    };
    rows.iter().any(|row| match kind {
        HoldingKind::Implementer => {
            matches!(row.kind.as_str(), "work-completed" | "work-failed")
                && names(row, "order_id", order_id)
        }
        HoldingKind::Pass => row.kind == "pass-ended" && names(row, "owner", owner),
    })
}

/// Re-read the trace: has this hold's terminal row been written since the
/// snapshot? Checked immediately before each step that would act on it.
fn hold_closed(paths: &OstromPaths, holding: &Holding) -> Result<bool, StallError> {
    Ok(terminal_row_exists(
        &trace_rows(paths)?,
        holding.kind,
        holding.order_id.as_deref(),
        holding.owner.as_deref(),
    ))
}

struct ObservedHold {
    progress: HoldProgress,
    /// The `ceilings` of the run's `run.started` event, when it has one.
    run_ceilings: Option<Value>,
    /// The process a pass's `pass-started` recorded (#636), when it recorded
    /// one. It outlives a takeover of the lease, which names only the current
    /// holder.
    recorded_process: Option<ProcessIdentity>,
}

/// Every open hold, with its progress measured net of suspended time when
/// `suspend` is known and by the wall clock alone when it is not.
fn observe(
    paths: &OstromPaths,
    clock: &Clock,
    suspend: Option<&SuspendTimeline>,
) -> Result<Vec<ObservedHold>, HoldingsError> {
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
            let recorded_process = (holding.kind == HoldingKind::Pass)
                .then(|| recorded_pass_process(&rows, &holding))
                .flatten();
            let last_progress = [
                epoch_seconds(&holding.started_at),
                holding.last_event_at.as_deref().and_then(epoch_seconds),
                transcript(paths, &holding).and_then(|path| modified_seconds(&path)),
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
                    seconds_without_progress: last_progress.map(|progress| {
                        suspend.map_or_else(
                            || now.saturating_sub(progress),
                            |timeline| timeline.awake_since(progress),
                        )
                    }),
                    suspended_seconds: last_progress.and_then(|progress| {
                        suspend.map(|timeline| timeline.suspended_since(progress))
                    }),
                    holding,
                },
                run_ceilings,
                recorded_process,
            }
        })
        .collect())
}

/// The process the pass's own `pass-started` recorded (#636): the newest
/// opener for its owner that names one. A later row for the same owner that
/// names no process, such as one an agent appends inside the pass, does not
/// hide it.
fn recorded_pass_process(rows: &[TraceFactRecord], holding: &Holding) -> Option<ProcessIdentity> {
    let owner = holding.owner.as_deref()?;
    rows.iter()
        .rev()
        .filter(|row| {
            row.kind == "pass-started"
                && row.fact.get("owner").and_then(Value::as_str) == Some(owner)
        })
        .find_map(|row| ProcessIdentity::from_fact(&row.fact))
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
    /// A pass whose lease another pass has taken over while its own process
    /// still runs (#636), within its stall threshold. It is not exited, and it
    /// is left to end itself with `pass-lease-lost`. Past its threshold it is
    /// [`Verdict::Stalled`], and the process it recorded is what is stopped.
    Displaced,
}

/// Which lease generation a pass hold is judged by (#636).
enum PassGeneration {
    /// The lease still names the hold's owner.
    Current(LeaseRecord),
    /// Another pass holds the lease now, or none does. What remains is the
    /// process the hold recorded when it started, if it recorded one.
    Superseded(Option<ProcessIdentity>),
}

fn pass_generation(paths: &OstromPaths, hold: &ObservedHold) -> PassGeneration {
    hold.progress
        .holding
        .owner
        .as_deref()
        .and_then(|owner| {
            read_leases(&paths.state)
                .into_iter()
                .find(|lease| lease.owner == owner)
        })
        .map_or(
            PassGeneration::Superseded(hold.recorded_process),
            PassGeneration::Current,
        )
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

impl StopTarget {
    fn order(&self) -> Option<&InFlightOrder> {
        match self {
            Self::Unit(order) => Some(order),
            Self::Process { order, .. } => order.as_ref(),
        }
    }
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

    /// The identity a `pass-started` fact records, under the lease's own
    /// field names (#636).
    fn from_fact(fact: &Map<String, Value>) -> Option<Self> {
        let number = |key: &str| fact.get(key).and_then(Value::as_u64);
        Some(Self {
            pid: u32::try_from(number("pid")?).ok()?,
            process_group_id: u32::try_from(number("process_group_id")?).ok()?,
            start_time: number("process_start_time")?,
        })
    }

    /// `Some(true)` only when the pid still runs with the recorded start time
    /// and process group. A recycled pid, a zombie, or a process that moved
    /// group is `Some(false)`: it is not this hold's process and is never
    /// signalled. `None` when `/proc` cannot answer.
    fn is_running(self) -> Option<bool> {
        self.is_running_at(Path::new("/proc"))
    }

    fn is_running_at(self, proc_root: &Path) -> Option<bool> {
        match read_process_identity_at(proc_root, self.pid) {
            Ok(Some(observed)) => Some(
                observed.start_time == self.start_time
                    && observed.process_group_id == self.process_group_id
                    && !matches!(observed.state, 'Z' | 'X'),
            ),
            Ok(None) => Some(false),
            Err(_) => None,
        }
    }
}

/// Whether a lease that names no process has lapsed: its expiry, moved later by
/// the time the machine spent suspended since it was taken (#637). A holder
/// cannot renew while the machine sleeps, so sleep alone never lapses it.
fn lease_lapsed(lease: &LeaseRecord, now: u64, timeline: &SuspendTimeline) -> bool {
    lease
        .expires_at
        .saturating_add(timeline.suspended_since(lease.started_at))
        <= now
}

/// The one definition of a stalled hold, shared by the reaper and doctor.
fn classify(
    paths: &OstromPaths,
    hold: &ObservedHold,
    now: u64,
    timeline: &SuspendTimeline,
) -> Verdict {
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
            let lease = match pass_generation(paths, hold) {
                PassGeneration::Current(lease) => lease,
                // The pass appends `pass-ended` before it releases its lease,
                // so a hold no lease names is a pass that is gone, or one
                // another pass took the lease from (#636). The process it
                // recorded tells the two apart.
                PassGeneration::Superseded(Some(identity)) => {
                    return match identity.is_running() {
                        // A displaced pass that has hung is still a stalled
                        // hold: nothing else will ever end it.
                        Some(true) if hold.progress.past_threshold() => {
                            Verdict::Stalled(StopTarget::Process {
                                identity,
                                order: None,
                            })
                        }
                        Some(true) => Verdict::Displaced,
                        Some(false) => Verdict::Exited,
                        // Not closed on a guess.
                        None => Verdict::Healthy,
                    };
                }
                // A hold that recorded no process (started before #636, or
                // where `/proc` could not be read): nothing else can say it
                // still runs.
                PassGeneration::Superseded(None) => return Verdict::Exited,
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
                None if lease_lapsed(&lease, now, timeline) => Verdict::Exited,
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

/// What a stop established. Only [`StopOutcome::Stopped`] lets the reaper
/// write a row or release a lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopOutcome {
    /// Confirmed gone.
    Stopped,
    /// Still running after `KILL`, or its unit still live after `stop`.
    StillRunning,
    /// Liveness could not be read: an unreadable `/proc`, a unit state
    /// `systemctl` would not report.
    Unknown,
    /// Nothing was signalled: the target was this reaper's own process or
    /// process group, or the claim could not record the signal first.
    Refused,
}

/// The charge a reaped run is recorded with: its declared cost ceiling, or the
/// shared per-run default when it declared none.
fn charge(hold: &ObservedHold, order: Option<&InFlightOrder>) -> (f64, &'static str) {
    if let Some(order) = order {
        // Every order carries its ceiling: declared at creation, or the
        // shared default written into the order then.
        return (order.cost_ceiling_usd, "declared-ceiling");
    }
    // Never the daily cap: one stalled pass must not hold every pass for the
    // day.
    hold.run_ceilings
        .as_ref()
        .and_then(|ceilings| ceilings.get("costUsd"))
        .and_then(Value::as_f64)
        .map_or((DEFAULT_RUN_COST_CEILING_USD, "default-ceiling"), |cost| {
            (cost, "declared-ceiling")
        })
}

fn claim_payload(
    hold: &ObservedHold,
    run_id: &str,
    reason: &str,
    (cost, basis): (f64, &str),
    stalled: bool,
    clock: &Clock,
) -> Map<String, Value> {
    let holding = &hold.progress.holding;
    let mut payload = Map::from_iter([
        ("run_id".to_owned(), json!(run_id)),
        (
            "kind".to_owned(),
            json!(match holding.kind {
                HoldingKind::Implementer => "implementer",
                HoldingKind::Pass => "pass",
            }),
        ),
        ("reason".to_owned(), json!(reason)),
        ("cost_usd".to_owned(), json!(cost)),
        ("cost_basis".to_owned(), json!(basis)),
        ("claimed_at".to_owned(), json!(clock.timestamp())),
        // Set just before the first signal: only a signalled run takes the
        // claim's reason and charge into its own row.
        ("signalled_at".to_owned(), Value::Null),
    ]);
    match holding.kind {
        HoldingKind::Implementer => payload.insert("order_id".to_owned(), json!(holding.order_id)),
        HoldingKind::Pass => payload.insert("owner".to_owned(), json!(holding.owner)),
    };
    if stalled {
        payload.insert(
            "last_progress_at".to_owned(),
            json!(hold.progress.last_progress_at),
        );
        payload.insert(
            "stalled_seconds".to_owned(),
            json!(hold.progress.seconds_without_progress.unwrap_or_default()),
        );
    }
    payload
}

/// Claim the run, or learn that another reaper has it.
fn claim_run(
    paths: &OstromPaths,
    run_id: &str,
    payload: Map<String, Value>,
) -> Result<Option<Claim>, StallError> {
    match claim::create(&claim_path(&paths.state, run_id), REAPER, payload) {
        Ok(claim) => Ok(Some(claim)),
        Err(ClaimError::Held) => Ok(None),
        Err(ClaimError::Io(error)) => Err(StallError::Record(format!(
            "could not claim run {run_id}: {error}"
        ))),
    }
}

fn remove_claim(claim: Claim, run_id: &str) -> Result<(), StallError> {
    claim.remove().map_err(|error| {
        StallError::Record(format!(
            "could not remove the claim on run {run_id}: {error}"
        ))
    })
}

fn reap_stalled(
    paths: &OstromPaths,
    clock: &Clock,
    hold: &ObservedHold,
    run_id: &str,
    target: &StopTarget,
) -> Result<Option<ReapedHold>, StallError> {
    let order = target.order();
    let payload = claim_payload(hold, run_id, "stalled", charge(hold, order), true, clock);
    let Some(mut claim) = claim_run(paths, run_id, payload)? else {
        return Ok(None);
    };
    // The run may have ended on its own since the snapshot.
    if hold_closed(paths, &hold.progress.holding)? {
        remove_claim(claim, run_id)?;
        return Ok(None);
    }
    let mut mark = || mark_signalled(&mut claim, clock);
    let stop = match target {
        StopTarget::Unit(order) => stop_unit(paths, order, &mut mark),
        StopTarget::Process { identity, .. } => stop_process(*identity, &mut mark),
    };
    finish_claim(
        paths,
        clock,
        &hold.progress.holding,
        run_id,
        claim,
        stop,
        order,
    )
}

fn close_exited_pass(
    paths: &OstromPaths,
    clock: &Clock,
    hold: &ObservedHold,
    run_id: &str,
) -> Result<Option<ReapedHold>, StallError> {
    let payload = claim_payload(
        hold,
        run_id,
        "exited-without-terminal",
        charge(hold, None),
        false,
        clock,
    );
    let Some(claim) = claim_run(paths, run_id, payload)? else {
        return Ok(None);
    };
    // Its process was already gone when it was classified; it cannot return.
    finish_claim(
        paths,
        clock,
        &hold.progress.holding,
        run_id,
        claim,
        StopOutcome::Stopped,
        None,
    )
}

/// Carry out a claim a reaper left: taken over only when that reaper has
/// died, then the run is stopped (if it still runs) and recorded with what the
/// claim says.
fn resume_claim(
    paths: &OstromPaths,
    clock: &Clock,
    hold: &ObservedHold,
    run_id: &str,
    now: u64,
    timeline: &SuspendTimeline,
) -> Result<Option<ReapedHold>, StallError> {
    let path = claim_path(&paths.state, run_id);
    let stale = match claim::read(&path) {
        Ok(Some(stale)) => stale,
        Ok(None) => return Ok(None),
        Err(error) => {
            return Err(StallError::Record(format!(
                "could not read the claim on run {run_id}: {error}"
            )));
        }
    };
    // A live reaper is still at work on it, or its holder cannot be read.
    if stale.holder(REAPER) != Holder::Dead {
        return Ok(None);
    }
    let mut claim = match claim::take_over(&path, REAPER, &stale) {
        Ok(claim) => claim,
        Err(ClaimError::Held) => return Ok(None),
        Err(ClaimError::Io(error)) => {
            return Err(StallError::Record(format!(
                "could not take over the claim on run {run_id}: {error}"
            )));
        }
    };
    let holding = &hold.progress.holding;
    if hold_closed(paths, holding)? {
        remove_claim(claim, run_id)?;
        return Ok(None);
    }
    let (stop, order) = match holding.kind {
        HoldingKind::Implementer => {
            let order = holding.order_id.as_deref().and_then(|order_id| {
                in_flight_orders(&paths.trace_file())
                    .ok()?
                    .into_iter()
                    .find(|order| order.order_id == order_id)
            });
            let Some(order) = order else {
                remove_claim(claim, run_id)?;
                return Ok(None);
            };
            let stop = if order.backend == "process" {
                let lease_path = paths.state.join(format!(
                    "implementer-item-{}.lease",
                    crate::item_hash(&order.item_id)
                ));
                match crate::read_lease(&lease_path) {
                    // The run releases its lease as it ends: none left means
                    // its process is gone.
                    Ok(None) => StopOutcome::Stopped,
                    Ok(Some(lease)) if lease.owner == order.unit_name => {
                        ProcessIdentity::from_lease(&lease).map_or(
                            StopOutcome::Unknown,
                            |identity| {
                                stop_process(identity, &mut || mark_signalled(&mut claim, clock))
                            },
                        )
                    }
                    Ok(Some(_)) | Err(_) => StopOutcome::Unknown,
                }
            } else {
                stop_unit(paths, &order, &mut || mark_signalled(&mut claim, clock))
            };
            (stop, Some(order))
        }
        HoldingKind::Pass => {
            let stop = match pass_generation(paths, hold) {
                PassGeneration::Current(lease) => match ProcessIdentity::from_lease(&lease) {
                    Some(identity) => {
                        stop_process(identity, &mut || mark_signalled(&mut claim, clock))
                    }
                    None if lease_lapsed(&lease, now, timeline) => StopOutcome::Stopped,
                    None => StopOutcome::Unknown,
                },
                // A pass another took the lease from may still run (#636):
                // the claim's stop reaches the process it recorded, which is
                // re-checked before any signal and is `Stopped` once gone.
                PassGeneration::Superseded(Some(identity)) => {
                    stop_process(identity, &mut || mark_signalled(&mut claim, clock))
                }
                // The pass appends `pass-ended` before it releases its lease.
                PassGeneration::Superseded(None) => StopOutcome::Stopped,
            };
            (stop, None)
        }
    };
    finish_claim(paths, clock, holding, run_id, claim, stop, order.as_ref())
}

/// Record what a stop established. A confirmed stop gets the row the run did
/// not write, its lease released and its claim removed. Anything else gets
/// nothing: no row, no release, and the claim stays for the next reaper.
fn finish_claim(
    paths: &OstromPaths,
    clock: &Clock,
    holding: &Holding,
    run_id: &str,
    claim: Claim,
    stop: StopOutcome,
    order: Option<&InFlightOrder>,
) -> Result<Option<ReapedHold>, StallError> {
    let intent = ReapIntent::from_record(claim.record())
        .ok_or_else(|| StallError::Record(format!("the claim on run {run_id} is incomplete")))?;
    let reaped = ReapedHold {
        kind: holding.kind,
        run_id: run_id.to_owned(),
        item: holding.item.clone(),
        reason: intent.reason.clone(),
        stopped: stop == StopOutcome::Stopped,
    };
    if stop != StopOutcome::Stopped {
        return Ok(Some(reaped));
    }
    match (holding.kind, order) {
        (HoldingKind::Implementer, Some(order)) => {
            // `append_terminal_row` re-checks the order is still in flight
            // immediately before it appends, so a row the run wrote first
            // stays the only one.
            append_reaped_failure(
                &paths.state,
                order,
                &ReapedFailure {
                    run_id,
                    reason: &intent.reason,
                    cost_usd: intent.cost_usd,
                    cost_basis: &intent.cost_basis,
                    last_progress_at: intent.last_progress_at.as_deref(),
                    stalled_seconds: intent.stalled_seconds,
                },
                clock,
            )
            .map_err(|error| StallError::Record(error.to_string()))?;
            // A unit's lease is time-bound and a process lease outlives a
            // killed process: released here, and only now that the run is
            // confirmed gone, so its item cannot be dispatched twice.
            release_order_lease(&paths.state, order)
                .map_err(|error| StallError::Record(error.to_string()))?;
        }
        (HoldingKind::Implementer, None) => {
            return Err(StallError::Record(format!(
                "run {run_id} has no in-flight order to record"
            )));
        }
        (HoldingKind::Pass, _) => {
            if !hold_closed(paths, holding)? {
                append_pass_ended(paths, clock, pass_ended_fact(holding, clock, &intent))?;
            }
        }
    }
    remove_claim(claim, run_id)?;
    Ok(Some(reaped))
}

/// The `pass-ended` the pass never wrote, marked as the reaper's.
fn pass_ended_fact(holding: &Holding, clock: &Clock, intent: &ReapIntent) -> Map<String, Value> {
    let duration = epoch_seconds(&holding.started_at)
        .map_or(0, |started| clock.epoch_seconds().saturating_sub(started));
    let mut fact = Map::from_iter([
        ("owner".to_owned(), json!(holding.owner)),
        ("outcome".to_owned(), json!("failed")),
        ("cost_usd".to_owned(), json!(intent.cost_usd)),
        ("duration_seconds".to_owned(), json!(duration)),
        ("reason".to_owned(), json!(intent.reason)),
        ("recorded_by".to_owned(), json!("reaper")),
    ]);
    fact.extend(intent.row_fields());
    fact
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

/// Stop the named unit through the service manager. Only a unit the service
/// manager reports gone is stopped; one it will not report on is not. The
/// unit's own control group is the whole of what is stopped.
fn stop_unit(
    paths: &OstromPaths,
    order: &InFlightOrder,
    mark_signalled: &mut dyn FnMut() -> bool,
) -> StopOutcome {
    let systemctl = environment::MANDATE_SYSTEMCTL_BIN
        .value_os()
        .map_or_else(|| PathBuf::from("systemctl"), PathBuf::from);
    let service = if order.unit_name.ends_with(".service") {
        order.unit_name.clone()
    } else {
        format!("{}.service", order.unit_name)
    };
    if !mark_signalled() {
        return StopOutcome::Refused;
    }
    let _ = Command::new(systemctl)
        .args(["--user", "stop", &service])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match order_liveness(&paths.state, order) {
        UnitLiveness::NotLive => StopOutcome::Stopped,
        UnitLiveness::Live => StopOutcome::StillRunning,
        UnitLiveness::Unknown => StopOutcome::Unknown,
    }
}

fn stop_process(
    identity: ProcessIdentity,
    mark_signalled: &mut dyn FnMut() -> bool,
) -> StopOutcome {
    stop_process_at(identity, Path::new("/proc"), mark_signalled)
}

/// Stop exactly the recorded process: its group when it leads one (the
/// process backend starts every implementer as a session leader), otherwise
/// the pid alone. The identity is re-checked before every signal, so a pid
/// that has since been recycled is never signalled, and a liveness `/proc`
/// cannot answer is never taken for a stop.
fn stop_process_at(
    identity: ProcessIdentity,
    proc_root: &Path,
    mark_signalled: &mut dyn FnMut() -> bool,
) -> StopOutcome {
    // `None` while it still runs; otherwise what that settles.
    let settled = || match identity.is_running_at(proc_root) {
        Some(false) => Some(StopOutcome::Stopped),
        None => Some(StopOutcome::Unknown),
        Some(true) => None,
    };
    if let Some(outcome) = settled() {
        return outcome;
    }
    let own = read_process_identity_at(proc_root, std::process::id())
        .ok()
        .flatten()
        .map(|own| (own.pid, own.process_group_id));
    let Some(target) = signal_target(identity, own) else {
        return StopOutcome::Refused;
    };
    // The run's own TERM handling stops its harness with the termination
    // grace, so the reaper waits for that before escalating.
    let grace = Duration::from_secs(RUN_TERMINATION_GRACE_SECONDS.saturating_mul(2));
    let mut marked = false;
    for signal in ["-TERM", "-KILL"] {
        if let Some(outcome) = settled() {
            return outcome;
        }
        if !marked {
            if !mark_signalled() {
                return StopOutcome::Refused;
            }
            marked = true;
        }
        let _ = Command::new(kill_command())
            .args([signal, "--", &target])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if let Some(outcome) = settled() {
                return outcome;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
    settled().unwrap_or(StopOutcome::StillRunning)
}

/// The `kill` argument for `identity`, or `None` when signalling it would
/// reach this reaper: its own pid, or a process group it belongs to. With no
/// readable identity of its own, the reaper signals nothing (#635 N1).
fn signal_target(identity: ProcessIdentity, own: Option<(u32, u32)>) -> Option<String> {
    let (own_pid, own_group) = own?;
    if identity.pid == own_pid {
        return None;
    }
    if identity.pid == identity.process_group_id {
        (identity.process_group_id != own_group).then(|| format!("-{}", identity.pid))
    } else {
        Some(identity.pid.to_string())
    }
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

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, fs};

    use tempfile::tempdir;

    use serde_json::{Value, json};

    use super::{
        ProcessIdentity, ReapPass, StallError, StopOutcome, in_scope, retained_failures,
        signal_target, stop_process_at,
    };
    use crate::{Holding, HoldingKind};

    fn identity(pid: u32, process_group_id: u32) -> ProcessIdentity {
        ProcessIdentity {
            pid,
            process_group_id,
            start_time: 1,
        }
    }

    /// #635 N1: the reaper never signals its own pid or a process group it is
    /// in, and signals nothing when it cannot tell which those are.
    #[test]
    fn the_reaper_never_signals_itself_or_its_own_process_group() {
        let own = Some((100, 50));
        assert_eq!(
            [
                signal_target(identity(100, 100), own),
                signal_target(identity(50, 50), own),
                signal_target(identity(300, 300), own),
                signal_target(identity(301, 50), own),
                signal_target(identity(300, 300), None),
            ],
            [
                None,
                None,
                Some("-300".to_owned()),
                Some("301".to_owned()),
                None,
            ]
        );
    }

    /// #635 S1: a liveness `/proc` cannot answer is not a stop. Nothing is
    /// signalled, and the reaper must record nothing and release nothing.
    #[test]
    fn an_unreadable_proc_entry_is_an_unknown_stop_not_a_confirmed_one() {
        let fixture = tempdir().expect("process information fixture");
        fs::create_dir_all(fixture.path().join("4194301/stat"))
            .expect("create an unreadable stat entry");
        assert_eq!(
            stop_process_at(identity(4_194_301, 4_194_301), fixture.path(), &mut || true),
            StopOutcome::Unknown
        );
    }

    fn holding(kind: HoldingKind, item: Option<&str>) -> Holding {
        Holding {
            kind,
            run_id: Some("run".to_owned()),
            runner: None,
            item: item.map(str::to_owned),
            order_id: None,
            owner: None,
            started_at: "2026-08-01T00:00:00Z".to_owned(),
            age_seconds: None,
            last_event_at: None,
            lease: None,
        }
    }

    /// #635 N1: a scoped reaper (dispatch) acts only on implementer holds for
    /// its own repositories; an unscoped one (up) acts on every hold.
    #[test]
    fn a_scoped_reap_reaches_only_its_own_repositories() {
        let own = BTreeSet::from(["placeholder-org/alpha".to_owned()]);
        let inside = holding(HoldingKind::Implementer, Some("placeholder-org/alpha#7"));
        let outside = holding(HoldingKind::Implementer, Some("placeholder-org/beta#7"));
        let pass = holding(HoldingKind::Pass, None);
        assert_eq!(
            [
                in_scope(&inside, Some(&own)),
                in_scope(&outside, Some(&own)),
                in_scope(&pass, Some(&own)),
                in_scope(&outside, None),
                in_scope(&pass, None),
            ],
            [true, false, false, true, true]
        );
    }

    /// #635 review nit: a clean reap clears only the failures it looked at.
    /// One scoped to other repositories, or one that could not read the holds,
    /// leaves the rest for doctor; a failure that concerned no one hold is
    /// replaced, not accumulated.
    #[test]
    fn a_clean_reap_clears_only_the_failures_it_examined() {
        let failure = |run_id: Option<&str>| json!({"run_id": run_id, "error": "fault"});
        let previous = vec![
            failure(Some("examined-run")),
            failure(Some("unexamined-run")),
            failure(Some("closed-run")),
            failure(None),
        ];
        let pass = ReapPass {
            observed: true,
            open: BTreeSet::from(["examined-run".to_owned(), "unexamined-run".to_owned()]),
            examined: BTreeSet::from(["examined-run".to_owned()]),
            errors: Vec::new(),
        };
        let blind = ReapPass {
            observed: false,
            errors: vec![(None, StallError::Record("fault".to_owned()))],
            ..ReapPass::default()
        };
        let run_ids = |failures: Vec<Value>| {
            failures
                .iter()
                .map(|failure| failure["run_id"].clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            (
                run_ids(retained_failures(previous.clone(), &pass, Vec::new())),
                run_ids(retained_failures(previous, &blind, Vec::new())).len(),
            ),
            (vec![json!("unexamined-run")], 3)
        );
    }

    /// #637: a hold on a real process of this test's own, judged at a fixed
    /// wall-clock instant with the suspended time a fixture reports.
    #[cfg(unix)]
    mod suspended {
        use std::{
            fs,
            os::unix::process::CommandExt as _,
            process::{Child, Command, Stdio},
        };

        use chrono::{DateTime, SecondsFormat, Utc};
        use serde_json::{Value, json};
        use tempfile::{TempDir, tempdir};

        use super::super::{ReapedHold, reap_stalled_holds_with, reaper_findings};
        use crate::{
            Clock, OstromPaths,
            lease::read_process_identity,
            suspend::{SuspendReading, SuspendSource, timeline_path},
        };

        const OWNER: &str = "builder-a1b2c3d4-wake3";
        const RUN: &str = "builder-suspended-placeholder-run";
        /// 2026-08-01T12:00:00Z.
        const NOW: u64 = 1_785_585_600;
        const THREE_HOURS: u64 = 3 * 60 * 60;
        /// The pass started just over three hours ago and has written nothing
        /// since. Undeclared, its stall threshold is the pass wall default
        /// plus the grace, 1805 s.
        const STARTED: u64 = NOW - THREE_HOURS - 10;

        /// A process of this test's own, its own group leader, killed with
        /// its group on drop, never by pattern.
        struct Sleeper(Child);

        impl Sleeper {
            fn start() -> Self {
                Self(
                    Command::new("sleep")
                        .arg("60")
                        .process_group(0)
                        .stdin(Stdio::null())
                        .spawn()
                        .expect("start a sleeping process"),
                )
            }

            fn running(&mut self) -> bool {
                self.0.try_wait().expect("poll the process").is_none()
            }
        }

        impl Drop for Sleeper {
            fn drop(&mut self) {
                let _ = Command::new("/bin/kill")
                    .args(["-KILL", "--", &format!("-{}", self.0.id())])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                let _ = self.0.wait();
            }
        }

        struct Suspended(Result<u64, String>);

        impl SuspendSource for Suspended {
            fn read(&self) -> Result<SuspendReading, String> {
                self.0.clone().map(|suspended_seconds| SuspendReading {
                    boot_id: "fixture-boot".to_owned(),
                    suspended_seconds,
                })
            }
        }

        fn timestamp(epoch: u64) -> String {
            DateTime::<Utc>::from_timestamp(i64::try_from(epoch).expect("epoch"), 0)
                .expect("valid epoch")
                .to_rfc3339_opts(SecondsFormat::Secs, true)
        }

        fn clock(epoch: u64) -> Clock {
            Clock::fixed(
                DateTime::<Utc>::from_timestamp(i64::try_from(epoch).expect("epoch"), 0)
                    .expect("valid epoch"),
            )
        }

        /// A pass hold as `ostrom pass` records one: `pass-started` and the
        /// pass lease, both naming `sleeper` when one is given. A reaper run
        /// before the hold began left one sample, with nothing suspended yet.
        fn pass_hold(sleeper: Option<&Sleeper>) -> (TempDir, OstromPaths) {
            let root = tempdir().expect("state root");
            let paths = OstromPaths {
                config: root.path().to_path_buf(),
                state: root.path().to_path_buf(),
            };
            let mut fact = json!({"owner": OWNER, "run_id": RUN});
            let mut lease = json!({
                "owner": OWNER,
                "started_at": STARTED,
                "expires_at": STARTED + 120,
            });
            if let Some(sleeper) = sleeper {
                let identity = read_process_identity(sleeper.0.id())
                    .expect("read the process")
                    .expect("the process is running");
                for record in [&mut fact, &mut lease] {
                    record["pid"] = json!(identity.pid);
                    record["process_group_id"] = json!(identity.process_group_id);
                    record["process_start_time"] = json!(identity.start_time);
                }
            }
            let row = json!({
                "ts": timestamp(STARTED),
                "kind": "pass-started",
                "fact": fact,
                "narration": {},
            });
            fs::write(paths.trace_file(), format!("{row}\n")).expect("write the trace");
            fs::write(paths.state.join("builder-pass.lease"), lease.to_string())
                .expect("write the pass lease");
            let timeline = timeline_path(&paths.state);
            fs::create_dir_all(timeline.parent().expect("timeline directory"))
                .expect("create the reaping directory");
            fs::write(
                timeline,
                json!({"boot_id": "fixture-boot", "samples": [[STARTED - 10, 0]]}).to_string(),
            )
            .expect("write the earlier sample");
            (root, paths)
        }

        fn reap(paths: &OstromPaths, at: u64, suspended: Result<u64, String>) -> Vec<ReapedHold> {
            reap_stalled_holds_with(paths, &clock(at), &Suspended(suspended), "up", None, None)
        }

        fn pass_ended(paths: &OstromPaths) -> Vec<Value> {
            fs::read_to_string(paths.trace_file())
                .expect("read the trace")
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).expect("trace row"))
                .filter(|row| row["kind"] == "pass-ended")
                .map(|row| row["fact"]["reason"].clone())
                .collect()
        }

        /// Three hours asleep is not three hours silent: the resumed pass is
        /// not reaped on resume, nor five minutes later, once a reaper run has
        /// already seen the suspend.
        #[test]
        fn a_three_hour_suspend_with_no_real_stall_reaps_nothing() {
            let mut sleeper = Sleeper::start();
            let (_root, paths) = pass_hold(Some(&sleeper));
            let reaped = [
                reap(&paths, NOW, Ok(THREE_HOURS)).len(),
                reap(&paths, NOW + 300, Ok(THREE_HOURS)).len(),
            ];
            assert_eq!(
                (reaped, sleeper.running(), pass_ended(&paths)),
                ([0, 0], true, Vec::<Value>::new())
            );
        }

        /// The same three hours with the machine awake is a stall, and it is
        /// reaped.
        #[test]
        fn a_real_stall_of_the_same_length_is_still_reaped() {
            let mut sleeper = Sleeper::start();
            let (_root, paths) = pass_hold(Some(&sleeper));
            let reaped = reap(&paths, NOW, Ok(0))
                .into_iter()
                .map(|hold| (hold.reason, hold.stopped))
                .collect::<Vec<_>>();
            assert_eq!(
                (reaped, sleeper.running(), pass_ended(&paths)),
                (
                    vec![("stalled".to_owned(), true)],
                    false,
                    vec![json!("stalled")]
                )
            );
        }

        /// When the suspended time cannot be read, nothing is judged: the
        /// stall above is left running, nothing is recorded against it, and
        /// the reason is where doctor reads it.
        #[test]
        fn unreadable_suspended_time_skips_reaping_and_records_why() {
            let mut sleeper = Sleeper::start();
            let (_root, paths) = pass_hold(Some(&sleeper));
            let reaped = reap(&paths, NOW, Err("fixture: no clock".to_owned())).len();
            let findings = reaper_findings(&paths);
            assert_eq!(
                (
                    reaped,
                    sleeper.running(),
                    pass_ended(&paths),
                    findings.len(),
                    findings.iter().any(|finding| finding
                        .contains("the time this machine spent suspended could not be read")
                        && finding.contains("fixture: no clock")),
                ),
                (0, true, Vec::<Value>::new(), 1, true),
                "{findings:?}"
            );
        }

        /// A pass lease that names no process lapses by its expiry alone, and
        /// the machine sleeping is not that: it is closed only when the time
        /// awake has passed its expiry.
        #[test]
        fn a_lease_naming_no_process_does_not_lapse_while_the_machine_sleeps() {
            let (_asleep_root, asleep) = pass_hold(None);
            let (_awake_root, awake) = pass_hold(None);
            assert_eq!(
                (
                    reap(&asleep, NOW, Ok(THREE_HOURS)).len(),
                    pass_ended(&asleep),
                    reap(&awake, NOW, Ok(0)).len(),
                    pass_ended(&awake),
                ),
                (
                    0,
                    Vec::<Value>::new(),
                    1,
                    vec![json!("exited-without-terminal")]
                )
            );
        }
    }
}
