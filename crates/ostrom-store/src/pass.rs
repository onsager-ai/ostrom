use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use chrono::DateTime;
use serde_json::{Map, Value, json};
use thiserror::Error;

use crate::{
    Clock, LeaseActionError, OstromPaths, OwnedLease, PassState, TraceAppend, append_trace,
    environment, read_pass_state, read_trace, write_pass_state,
};

pub const MAX_TURNS: &str = "200";
const DEFAULT_DAILY_CAP_USD: f64 = 50.0;
const DEFAULT_LEASE_TTL_SECONDS: u64 = 3_600;

#[derive(Debug, Clone)]
pub struct PassRequest {
    pub paths: OstromPaths,
    pub actor: String,
    pub signals: SignalFlags,
    pub supervisor_pid: Option<u32>,
    pub clock: Clock,
}

#[derive(Debug, Clone, Default)]
pub struct PassDispatch {
    pub transcript: Option<PathBuf>,
    pub exit_code: i32,
    pub error: Option<String>,
    pub run_signature: Option<String>,
    pub queue_count: Option<usize>,
    pub dispatchable_count: Option<usize>,
    pub skipped_unchanged: bool,
}

#[derive(Debug, Clone, Default)]
pub struct SignalFlags {
    hup: Arc<AtomicBool>,
    int: Arc<AtomicBool>,
    term: Arc<AtomicBool>,
}

impl SignalFlags {
    #[must_use]
    pub fn hup_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.hup)
    }

    #[must_use]
    pub fn int_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.int)
    }

    #[must_use]
    pub fn term_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.term)
    }

    pub fn take_pending(&self) -> Option<&'static str> {
        if self.term.swap(false, Ordering::SeqCst) {
            Some("TERM")
        } else if self.int.swap(false, Ordering::SeqCst) {
            Some("INT")
        } else if self.hup.swap(false, Ordering::SeqCst) {
            Some("HUP")
        } else {
            None
        }
    }
}

#[derive(Debug, Error)]
pub enum PassError {
    #[error("ostrom {role} pass: {message}")]
    Failed {
        role: String,
        message: String,
        code: i32,
    },
    #[error("ostrom {0} pass: another pass already holds {0}-pass.lease; skipping")]
    Held(String),
    #[error("ostrom {0} pass: loop is disarmed")]
    Disarmed(String),
}

impl PassError {
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::Failed { code, .. } => *code,
            Self::Held(_) | Self::Disarmed(_) => 0,
        }
    }

    fn failed(role: &str, message: impl Into<String>, code: i32) -> Self {
        Self::Failed {
            role: role.to_owned(),
            message: message.into(),
            code,
        }
    }
}

struct PassGuard {
    actor: String,
    owner: String,
    paths: OstromPaths,
    lease: OwnedLease,
    started_epoch: u64,
    trace_time: String,
    started: bool,
    outcome: Option<String>,
    reason: Option<String>,
    cost_usd: Option<f64>,
    clock: Clock,
    run_signature: Option<String>,
    queue_count: Option<usize>,
    dispatchable_count: Option<usize>,
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
        let mut failure = None;
        if self.started {
            let outcome = terminal_outcome(self.outcome.clone(), thread::panicking());
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
            if let Some(signature) = &self.run_signature {
                fact.insert("run_signature".to_owned(), json!(signature));
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
                    &self.actor,
                    format!("could not append pass-ended: {error}"),
                    1,
                ));
            }
            self.started = false;
        }
        if self.lease.release().is_err() && failure.is_none() {
            failure = Some(PassError::failed(
                &self.actor,
                "could not release pass lease",
                1,
            ));
        }
        failure.map_or(Ok(()), Err)
    }
}

impl Drop for PassGuard {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

pub fn run_pass(
    request: &PassRequest,
    dispatch: impl FnOnce(Option<&str>) -> PassDispatch,
) -> Result<(), PassError> {
    validate_arm(request)?;
    fs::create_dir_all(&request.paths.state).map_err(|error| {
        PassError::failed(
            &request.actor,
            format!("could not create state directory: {error}"),
            1,
        )
    })?;
    let lease_now = request.clock.epoch_seconds();
    let started_epoch = request.clock.epoch_seconds();
    let lease_name = format!("{}-pass.lease", request.actor);
    let ttl =
        positive_env(environment::MANDATE_LEASE_TTL_SECONDS).unwrap_or(DEFAULT_LEASE_TTL_SECONDS);

    let prior = read_pass_state(&request.paths.state, &request.actor)
        .map_err(|error| PassError::failed(&request.actor, error.to_string(), 1))?;
    let mut state = prior.unwrap_or_else(|| PassState {
        role_id: generated_role_id(&request.clock),
        wake: 0,
        run_signature: None,
    });
    let next_wake = state.wake.saturating_add(1);
    let owner = format!("{}-{}-wake{next_wake}", request.actor, state.role_id);
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
            return Err(PassError::Held(request.actor.clone()));
        }
        Err(error) => {
            return Err(PassError::failed(
                &request.actor,
                format!("could not acquire {lease_name}: {error:?}"),
                1,
            ));
        }
    };
    state.wake = next_wake;
    write_pass_state(&request.paths.state, &request.actor, &state)
        .map_err(|error| PassError::failed(&request.actor, error.to_string(), 1))?;

    let trace_time = request.clock.timestamp();
    let mut guard = PassGuard {
        actor: request.actor.clone(),
        owner: owner.clone(),
        paths: request.paths.clone(),
        lease,
        started_epoch,
        trace_time,
        started: false,
        outcome: None,
        reason: None,
        cost_usd: None,
        clock: request.clock.clone(),
        run_signature: None,
        queue_count: None,
        dispatchable_count: None,
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
            &request.actor,
            format!("could not append pass-started: {error}"),
            1,
        )
    })?;
    guard.started = true;
    check_signal(request, &mut guard)?;
    if daily_spend(&request.paths, &request.clock.date()) >= daily_cap() {
        guard.outcome = Some("no-op".to_owned());
        guard.reason = Some("daily-cap".to_owned());
        guard.finish()?;
        return Ok(());
    }
    let result = dispatch(state.run_signature.as_deref());
    guard.run_signature.clone_from(&result.run_signature);
    guard.queue_count = result.queue_count;
    guard.dispatchable_count = result.dispatchable_count;
    if result.skipped_unchanged {
        guard.outcome = Some("no-op".to_owned());
        guard.reason = Some("run-signature-unchanged".to_owned());
        guard.cost_usd = Some(0.0);
    } else if let Some(transcript) = result.transcript.as_deref() {
        let summary = read_transcript(transcript);
        guard.cost_usd = summary.cost_usd;
        guard.outcome = Some(if summary.permission_denied {
            "permission-denied".to_owned()
        } else if result.exit_code == 0 {
            "completed".to_owned()
        } else {
            "failed".to_owned()
        });
    } else {
        guard.outcome = Some(if result.exit_code == 0 {
            "completed".to_owned()
        } else {
            "fault".to_owned()
        });
    }
    if result.exit_code == 0
        && !matches!(guard.outcome.as_deref(), Some("failed" | "permission-denied" | "fault"))
        && let Some(signature) = &result.run_signature
    {
        state.run_signature = Some(signature.clone());
        if let Err(error) = write_pass_state(&request.paths.state, &request.actor, &state) {
            guard.outcome = Some("failed".to_owned());
            return Err(PassError::failed(&request.actor, error.to_string(), 1));
        }
    }
    guard.finish()?;
    if result.exit_code == 0 {
        Ok(())
    } else {
        Err(PassError::failed(
            &request.actor,
            result.error.unwrap_or_else(|| "dispatch failed".to_owned()),
            result.exit_code,
        ))
    }
}

fn validate_arm(request: &PassRequest) -> Result<(), PassError> {
    let path = request.paths.state.join("loop-armed");
    let Ok(contents) = fs::read_to_string(&path) else {
        return Err(PassError::Disarmed(request.actor.clone()));
    };
    if contents.is_empty() {
        return Ok(());
    }
    let value = contents.strip_suffix('\n').unwrap_or(&contents);
    if value.contains('\n') {
        return Err(PassError::Disarmed(request.actor.clone()));
    }
    if !valid_arm_expiry(value) {
        return Err(PassError::Disarmed(request.actor.clone()));
    }
    let expiry = DateTime::parse_from_rfc3339(value)
        .map_err(|_| PassError::Disarmed(request.actor.clone()))?;
    if expiry.timestamp() <= request.clock.epoch_seconds() as i64 {
        return Err(PassError::Disarmed(request.actor.clone()));
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

fn check_signal(
    request: &PassRequest,
    guard: &mut PassGuard,
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
    Err(PassError::failed(
        &request.actor,
        format!("received SIG{name}"),
        code,
    ))
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
