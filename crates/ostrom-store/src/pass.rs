use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};
use thiserror::Error;

use crate::{
    LeaseActionError, OstromPaths, OwnedLease, PassState, TraceAppend, append_trace, read_lease,
    read_pass_state, read_trace, write_pass_state,
};

const MAX_TURNS: &str = "200";
const DEFAULT_DAILY_CAP_USD: f64 = 50.0;
const DEFAULT_LEASE_TTL_SECONDS: u64 = 3_600;

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

    const fn prompt(self) -> &'static str {
        match self {
            Self::Builder => "/ostrom:work",
            Self::Gatekeeper => "/ostrom:gatekeep",
        }
    }

    const fn permission_mode(self) -> &'static str {
        match self {
            Self::Builder => "auto",
            Self::Gatekeeper => "manual",
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
    pub role: PassRole,
    pub claude_bin: PathBuf,
    pub signals: SignalFlags,
    pub supervisor_pid: Option<u32>,
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
        role: &'static str,
        message: String,
        code: i32,
    },
    #[error("ostrom {0} pass: another pass already holds {0}-pass.lease; skipping")]
    Held(&'static str),
    #[error("ostrom {0} pass: loop is disarmed")]
    Disarmed(&'static str),
}

impl PassError {
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::Failed { code, .. } => *code,
            Self::Held(_) | Self::Disarmed(_) => 0,
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
}

impl PassGuard {
    fn finish(&mut self) -> Result<(), PassError> {
        let mut failure = None;
        if self.started {
            let outcome = self.outcome.clone().unwrap_or_else(|| {
                if thread::panicking() {
                    "failed".to_owned()
                } else {
                    "completed".to_owned()
                }
            });
            let now = wall_epoch_seconds();
            let mut fact = Map::new();
            fact.insert("owner".to_owned(), json!(self.owner));
            fact.insert("outcome".to_owned(), json!(outcome));
            fact.insert(
                "cost_usd".to_owned(),
                self.cost_usd.map_or(Value::Null, |cost| json!(cost)),
            );
            // A pinned trace clock pins the duration too. `MANDATE_TRACE_TIME`
            // exists so a run's recorded output is reproducible, and a wall-clock
            // duration defeats that: the recorded-parity fixture compares the
            // trace byte for byte, so the same pass emitted 0 on an idle machine
            // and 1 under CI load, failing intermittently and for no real reason.
            // Under a pinned clock no time passes, which is the honest reading of
            // "the clock is fixed" rather than a special case for tests.
            fact.insert(
                "duration_seconds".to_owned(),
                json!(if pinned_trace_time().is_some() {
                    0
                } else {
                    now.saturating_sub(self.started_epoch)
                }),
            );
            if let Some(reason) = &self.reason {
                fact.insert("reason".to_owned(), json!(reason));
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

impl Drop for PassGuard {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

pub fn run_pass(request: &PassRequest) -> Result<(), PassError> {
    validate_arm(request)?;
    fs::create_dir_all(&request.paths.state).map_err(|error| {
        PassError::failed(
            request.role,
            format!("could not create state directory: {error}"),
            1,
        )
    })?;
    let lease_now = lease_epoch_seconds();
    let started_epoch = wall_epoch_seconds();
    let lease_name = format!("{}-pass.lease", request.role.name());
    let ttl = positive_env("MANDATE_LEASE_TTL_SECONDS").unwrap_or(DEFAULT_LEASE_TTL_SECONDS);

    let prior = read_pass_state(&request.paths.state, request.role.name())
        .map_err(|error| PassError::failed(request.role, error.to_string(), 1))?;
    let mut state = prior.unwrap_or_else(|| PassState {
        role_id: generated_role_id(),
        wake: 0,
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
            return Err(PassError::Held(request.role.name()));
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

    let trace_time = pass_trace_time();
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

    if env::var_os("OSTROM_PASS_TEST_PANIC").is_some() {
        panic!("requested pass panic fixture");
    }
    check_signal(request, &mut guard, None)?;
    let settings = request
        .paths
        .state
        .join("roles")
        .join(format!("{}.settings.json", request.role.name()));
    if !settings.is_file() {
        guard.outcome = Some("failed".to_owned());
        return Err(PassError::failed(
            request.role,
            format!("{} missing", settings.display()),
            1,
        ));
    }
    if !is_executable_file(&request.claude_bin) {
        guard.outcome = Some("failed".to_owned());
        return Err(PassError::failed(
            request.role,
            format!("{} is not marked executable", request.claude_bin.display()),
            1,
        ));
    }
    if daily_spend(&request.paths, &pass_day()) >= daily_cap() {
        guard.outcome = Some("no-op".to_owned());
        guard.reason = Some("daily-cap".to_owned());
        guard.finish()?;
        return Ok(());
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
        DateTime::<Utc>::from(SystemTime::now()).format("%Y%m%dT%H%M%SZ")
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
            request.role.permission_mode(),
            "--output-format",
            "stream-json",
            "--verbose",
            "--max-turns",
            MAX_TURNS,
            request.role.prompt(),
        ])
        .stdout(Stdio::from(output))
        .stderr(Stdio::from(error_output));
    set_process_group(&mut command);
    let mut child = command.spawn().map_err(|error| {
        PassError::failed(request.role, format!("could not start Claude: {error}"), 1)
    })?;
    guard.child_spawned = true;
    let status = wait_for_child(request, &mut guard, &mut child)?;
    guard.cost_usd = read_cost(&log);
    reconcile_outcome(&mut guard, watermark, status);
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
    if expiry.timestamp() <= wall_epoch_seconds() as i64 {
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
) -> Result<ExitStatus, PassError> {
    loop {
        if let Some(status) = child.try_wait().map_err(|error| {
            PassError::failed(
                request.role,
                format!("could not wait for Claude: {error}"),
                1,
            )
        })? {
            kill_remaining_process_group(child.id());
            return Ok(status);
        }
        if let Err(error) = check_signal(request, guard, Some(child)) {
            let _ = child.wait();
            return Err(error);
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn check_signal(
    request: &PassRequest,
    guard: &mut PassGuard,
    child: Option<&mut Child>,
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
    if let Some(child) = child {
        terminate_child_process_group(child, Duration::from_secs(5));
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
        request.role,
        format!("received SIG{name}"),
        code,
    ))
}

fn reconcile_outcome(guard: &mut PassGuard, watermark: usize, status: ExitStatus) {
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

fn pass_day() -> String {
    let clock = env::var("MANDATE_NOW_EPOCH")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .and_then(|value| DateTime::<Utc>::from_timestamp(value, 0));
    clock
        .unwrap_or_else(|| DateTime::<Utc>::from(SystemTime::now()))
        .format("%Y-%m-%d")
        .to_string()
}

fn daily_cap() -> f64 {
    env::var("MANDATE_DAILY_CAP_USD")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite())
        .unwrap_or(DEFAULT_DAILY_CAP_USD)
}

fn read_cost(path: &Path) -> Option<f64> {
    fs::read_to_string(path).ok().and_then(|contents| {
        contents
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .fold(None, |cost, event| {
                if event.get("type").and_then(Value::as_str) == Some("result") {
                    event.get("total_cost_usd").and_then(Value::as_f64).or(cost)
                } else {
                    cost
                }
            })
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
    paths.sort_by(|left, right| right.0.cmp(&left.0));
    for (_, path) in paths.into_iter().skip(30) {
        let _ = fs::remove_file(path);
    }
}

/// The pinned trace instant, if one is set. Empty means unset, matching the
/// shell's `${VAR:-default}` semantics used everywhere else in this crate.
fn pinned_trace_time() -> Option<String> {
    env::var("MANDATE_TRACE_TIME")
        .ok()
        .filter(|value| !value.is_empty())
}

fn pass_trace_time() -> String {
    if let Ok(value) = env::var("MANDATE_TRACE_TIME") {
        if !value.is_empty() {
            return value;
        }
    }
    let clock = env::var("MANDATE_NOW_EPOCH")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .and_then(|value| DateTime::<Utc>::from_timestamp(value, 0));
    clock
        .unwrap_or_else(|| DateTime::<Utc>::from(SystemTime::now()))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

fn generated_role_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    format!("{:08x}", nanos ^ std::process::id())
}

fn lease_epoch_seconds() -> u64 {
    env::var("MANDATE_LEASE_NOW_EPOCH")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        })
}

fn wall_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn positive_env(name: &str) -> Option<u64> {
    env::var(name)
        .ok()
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
