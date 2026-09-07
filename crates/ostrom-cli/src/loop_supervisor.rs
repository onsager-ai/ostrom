use std::{
    collections::BTreeSet,
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use chrono::Local;
use ostrom_core::{PolicyManifest, ResolvedLoopCeilings};
use ostrom_store::{Clock, OstromPaths, read_trace};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::policy_version::{self, CurrentPolicyVersion};

const STATE_SCHEMA_VERSION: u64 = 1;
// The five-minute reconciler timer should normally observe a slot almost
// immediately. Two hours tolerates login delay, suspend/resume, and several
// missed timer firings, while refusing to replay daily work after a long host
// outage (the failure a limitless "latest slot" lookup would introduce).
const SLOT_STALE_AFTER_SECONDS: i64 = 2 * 60 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum LoopStatus {
    Starting,
    Running,
    Completed,
    Failed,
    Stopped,
    Stale,
    Inconclusive,
}

impl LoopStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
            Self::Stale => "stale:slot_age_exceeded",
            Self::Inconclusive => "inconclusive",
        }
    }

    const fn may_be_alive(self) -> bool {
        matches!(self, Self::Starting | Self::Running)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LoopRunState {
    schema_version: u64,
    name: String,
    version: String,
    schedule_slot: String,
    status: LoopStatus,
    pid: Option<u32>,
    started_at: String,
    finished_at: Option<String>,
    reason: Option<String>,
}

impl LoopRunState {
    fn starting(name: &str, version: &str, schedule_slot: &str, clock: &Clock) -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            name: name.to_owned(),
            version: version.to_owned(),
            schedule_slot: schedule_slot.to_owned(),
            status: LoopStatus::Starting,
            pid: None,
            started_at: clock.timestamp(),
            finished_at: None,
            reason: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Consumption {
    concurrent: Measurement<u64>,
    spend_usd: Measurement<f64>,
    tokens: Measurement<u64>,
}

#[derive(Debug, Clone, Copy, Default)]
enum Measurement<T> {
    Measured(T),
    Unknown(&'static str),
    #[default]
    Unset,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct UpSummary {
    pub started: usize,
    pub stopped: usize,
    pub unchanged: usize,
    pub not_due: usize,
    pub stale: usize,
}

#[derive(Debug, Error)]
pub(crate) enum LoopSupervisorError {
    #[error(transparent)]
    Current(#[from] policy_version::CurrentPolicyError),
    #[error("loop `{name}` is unlaunchable: {cause}")]
    Unlaunchable { name: String, cause: String },
    #[error("loop `{name}` state at `{}` is unreadable: {source}", path.display())]
    ReadState {
        name: String,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("loop `{name}` state at `{}` is invalid: {source}", path.display())]
    ParseState {
        name: String,
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("loop `{name}` state could not be written at `{}`: {source}", path.display())]
    WriteState {
        name: String,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("loop `{name}` is inconclusive:{cause}")]
    Inconclusive { name: String, cause: &'static str },
    #[error("unknown loop `{0}` in the current policy version")]
    UnknownLoop(String),
    #[error("loop `{name}` has no log at `{}`", path.display())]
    LogMissing { name: String, path: PathBuf },
    #[error("loop `{name}` log at `{}` is unreadable: {source}", path.display())]
    LogUnreadable {
        name: String,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

pub(crate) fn reconcile(
    paths: &OstromPaths,
    clock: &Clock,
    executable: &Path,
) -> Result<UpSummary, LoopSupervisorError> {
    let current = policy_version::load_current(paths)?;
    fs::create_dir_all(paths.loop_runs_dir()).map_err(|source| {
        LoopSupervisorError::Unlaunchable {
            name: "reconciler".to_owned(),
            cause: format!(
                "state_directory_unavailable: {}: {source}",
                paths.loop_runs_dir().display()
            ),
        }
    })?;
    let mut summary = stop_obsolete(paths, clock, &current)?;
    let consumption = measure_consumption(paths, clock);
    let local_now = clock.now().with_timezone(&Local);

    for name in current.manifest.loops.keys() {
        let resolved = current.manifest.resolve_loop(name).map_err(|error| {
            LoopSupervisorError::Unlaunchable {
                name: name.clone(),
                cause: error.to_string(),
            }
        })?;
        let Some(slot) = resolved.every.activation_slot(&local_now) else {
            summary.not_due += 1;
            continue;
        };
        if let Some(existing) = read_state(paths, name)?
            && existing.version == current.digest
            && existing.schedule_slot == slot.identity
            && existing.status != LoopStatus::Inconclusive
        {
            summary.unchanged += 1;
            continue;
        }
        if slot.age.num_seconds() > SLOT_STALE_AFTER_SECONDS {
            let reason = format!(
                "slot_age_exceeded age_seconds={} bound_seconds={SLOT_STALE_AFTER_SECONDS}",
                slot.age.num_seconds()
            );
            let mut state = LoopRunState::starting(name, &current.digest, &slot.identity, clock);
            state.status = LoopStatus::Stale;
            state.finished_at = Some(clock.timestamp());
            state.reason = Some(reason.clone());
            write_state(paths, &state)?;
            append_log(
                paths,
                name,
                &format!(
                    "{} stale slot={} {reason}\n",
                    clock.timestamp(),
                    slot.identity
                ),
            )?;
            summary.stale += 1;
            continue;
        }
        if let Some(reason) = exceeded_reason(consumption, resolved.ceilings, name)? {
            let mut state = LoopRunState::starting(name, &current.digest, &slot.identity, clock);
            state.status = LoopStatus::Stopped;
            state.finished_at = Some(clock.timestamp());
            state.reason = Some(reason.clone());
            write_state(paths, &state)?;
            append_log(
                paths,
                name,
                &format!("{} stopped: {reason}\n", clock.timestamp()),
            )?;
            summary.stopped += 1;
            continue;
        }
        launch_worker(
            paths,
            clock,
            executable,
            name,
            &current.digest,
            &slot.identity,
        )?;
        summary.started += 1;
    }
    Ok(summary)
}

pub(crate) fn render_ps(paths: &OstromPaths, clock: &Clock) -> Result<String, LoopSupervisorError> {
    let current = policy_version::load_current(paths)?;
    let consumption = measure_consumption(paths, clock);
    let mut output = String::new();
    for name in current.manifest.loops.keys() {
        let resolved = current.manifest.resolve_loop(name).map_err(|error| {
            LoopSupervisorError::Unlaunchable {
                name: name.clone(),
                cause: error.to_string(),
            }
        })?;
        let status = read_state(paths, name)?
            .filter(|state| state.version == current.digest)
            .map_or("stopped", |state| state.status.as_str());
        let concurrent = render_u64(consumption.concurrent, resolved.ceilings.concurrent);
        let spend = render_spend(consumption.spend_usd, resolved.ceilings.spend_usd);
        let tokens = render_u64(consumption.tokens, resolved.ceilings.tokens);
        output.push_str(&format!(
            "{name}  {status}  {concurrent}  {spend}  {tokens} tokens\n"
        ));
    }
    Ok(output)
}

pub(crate) fn read_logs(paths: &OstromPaths, name: &str) -> Result<Vec<u8>, LoopSupervisorError> {
    let current = policy_version::load_current(paths)?;
    if !current.manifest.loops.contains_key(name) {
        return Err(LoopSupervisorError::UnknownLoop(name.to_owned()));
    }
    let path = paths.loop_run_log_file(name);
    fs::read(&path).map_err(|source| match source.kind() {
        io::ErrorKind::NotFound => LoopSupervisorError::LogMissing {
            name: name.to_owned(),
            path,
        },
        _ => LoopSupervisorError::LogUnreadable {
            name: name.to_owned(),
            path,
            source,
        },
    })
}

pub(crate) fn worker_started(
    paths: &OstromPaths,
    name: &str,
    version: &str,
    schedule_slot: &str,
    clock: &Clock,
) -> Result<PolicyManifest, LoopSupervisorError> {
    let current = policy_version::load_current(paths)?;
    if current.digest != version {
        return Err(LoopSupervisorError::Unlaunchable {
            name: name.to_owned(),
            cause: "current_version_changed".to_owned(),
        });
    }
    if !current.manifest.loops.contains_key(name) {
        return Err(LoopSupervisorError::UnknownLoop(name.to_owned()));
    }
    let mut state = read_state(paths, name)?
        .unwrap_or_else(|| LoopRunState::starting(name, version, schedule_slot, clock));
    if state.version != version || state.schedule_slot != schedule_slot {
        return Err(LoopSupervisorError::Unlaunchable {
            name: name.to_owned(),
            cause: "activation_state_changed".to_owned(),
        });
    }
    state.status = LoopStatus::Running;
    state.pid = Some(std::process::id());
    write_state(paths, &state)?;
    Ok(current.manifest)
}

pub(crate) fn worker_finished(
    paths: &OstromPaths,
    name: &str,
    succeeded: bool,
    reason: Option<String>,
    clock: &Clock,
) -> Result<(), LoopSupervisorError> {
    let Some(mut state) = read_state(paths, name)? else {
        return Err(LoopSupervisorError::Unlaunchable {
            name: name.to_owned(),
            cause: "activation_state_missing".to_owned(),
        });
    };
    state.status = if succeeded {
        LoopStatus::Completed
    } else {
        LoopStatus::Failed
    };
    state.pid = None;
    state.finished_at = Some(clock.timestamp());
    state.reason = reason;
    write_state(paths, &state)
}

fn stop_obsolete(
    paths: &OstromPaths,
    clock: &Clock,
    current: &CurrentPolicyVersion,
) -> Result<UpSummary, LoopSupervisorError> {
    let mut summary = UpSummary::default();
    let entries = fs::read_dir(paths.loop_runs_dir()).map_err(|source| {
        LoopSupervisorError::Unlaunchable {
            name: "reconciler".to_owned(),
            cause: format!("state_directory_unreadable: {source}"),
        }
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| LoopSupervisorError::Unlaunchable {
            name: "reconciler".to_owned(),
            cause: format!("state_directory_unreadable: {source}"),
        })?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Some(mut state) = read_state(paths, name)? else {
            continue;
        };
        let declared = current.manifest.loops.contains_key(name);
        if state.status.may_be_alive() && (!declared || state.version != current.digest) {
            let Some(pid) = state.pid else {
                return Err(LoopSupervisorError::Inconclusive {
                    name: name.to_owned(),
                    cause: "process_identity_missing",
                });
            };
            terminate_process_group(name, pid)?;
            state.status = LoopStatus::Stopped;
            state.pid = None;
            state.finished_at = Some(clock.timestamp());
            state.reason = Some(if declared {
                "version_changed".to_owned()
            } else {
                "undeclared_by_current_policy".to_owned()
            });
            write_state(paths, &state)?;
            summary.stopped += 1;
        }
    }
    Ok(summary)
}

fn launch_worker(
    paths: &OstromPaths,
    clock: &Clock,
    executable: &Path,
    name: &str,
    version: &str,
    schedule_slot: &str,
) -> Result<(), LoopSupervisorError> {
    let mut state = LoopRunState::starting(name, version, schedule_slot, clock);
    write_state(paths, &state)?;
    append_log(
        paths,
        name,
        &format!(
            "{} starting version={version} slot={schedule_slot}\n",
            clock.timestamp()
        ),
    )?;
    let log_path = paths.loop_run_log_file(name);
    let stdout = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|source| LoopSupervisorError::Unlaunchable {
            name: name.to_owned(),
            cause: format!("log_unavailable: {}: {source}", log_path.display()),
        })?;
    let stderr = stdout
        .try_clone()
        .map_err(|source| LoopSupervisorError::Unlaunchable {
            name: name.to_owned(),
            cause: format!("log_unavailable: {}: {source}", log_path.display()),
        })?;
    let mut command = Command::new(executable);
    command
        .arg("__loop-worker")
        .arg(name)
        .arg(version)
        .arg(schedule_slot)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    configure_process_group(&mut command);
    if let Err(source) = command.spawn() {
        let cause = format!("process_spawn_failed: {source}");
        state.status = LoopStatus::Failed;
        state.finished_at = Some(clock.timestamp());
        state.reason = Some(cause.clone());
        write_state(paths, &state)?;
        return Err(LoopSupervisorError::Unlaunchable {
            name: name.to_owned(),
            cause,
        });
    }
    Ok(())
}

fn read_state(
    paths: &OstromPaths,
    name: &str,
) -> Result<Option<LoopRunState>, LoopSupervisorError> {
    let path = paths.loop_run_state_file(name);
    let source = match fs::read(&path) {
        Ok(source) => source,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(LoopSupervisorError::ReadState {
                name: name.to_owned(),
                path,
                source,
            });
        }
    };
    let state = serde_json::from_slice::<LoopRunState>(&source).map_err(|source| {
        LoopSupervisorError::ParseState {
            name: name.to_owned(),
            path: path.clone(),
            source,
        }
    })?;
    if state.schema_version != STATE_SCHEMA_VERSION || state.name != name {
        return Err(LoopSupervisorError::Unlaunchable {
            name: name.to_owned(),
            cause: "state_identity_invalid".to_owned(),
        });
    }
    Ok(Some(state))
}

fn write_state(paths: &OstromPaths, state: &LoopRunState) -> Result<(), LoopSupervisorError> {
    fs::create_dir_all(paths.loop_runs_dir()).map_err(|source| {
        LoopSupervisorError::WriteState {
            name: state.name.clone(),
            path: paths.loop_runs_dir(),
            source,
        }
    })?;
    let path = paths.loop_run_state_file(&state.name);
    let mut temporary = NamedTempFile::new_in(paths.loop_runs_dir()).map_err(|source| {
        LoopSupervisorError::WriteState {
            name: state.name.clone(),
            path: path.clone(),
            source,
        }
    })?;
    serde_json::to_writer(&mut temporary, state).map_err(|source| {
        LoopSupervisorError::WriteState {
            name: state.name.clone(),
            path: path.clone(),
            source: io::Error::other(source),
        }
    })?;
    temporary
        .write_all(b"\n")
        .map_err(|source| LoopSupervisorError::WriteState {
            name: state.name.clone(),
            path: path.clone(),
            source,
        })?;
    temporary
        .persist(&path)
        .map_err(|error| LoopSupervisorError::WriteState {
            name: state.name.clone(),
            path,
            source: error.error,
        })?;
    Ok(())
}

fn append_log(paths: &OstromPaths, name: &str, message: &str) -> Result<(), LoopSupervisorError> {
    fs::create_dir_all(paths.loop_runs_dir()).map_err(|source| {
        LoopSupervisorError::Unlaunchable {
            name: name.to_owned(),
            cause: format!("log_directory_unavailable: {source}"),
        }
    })?;
    let path = paths.loop_run_log_file(name);
    let mut log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|source| LoopSupervisorError::Unlaunchable {
            name: name.to_owned(),
            cause: format!("log_unavailable: {}: {source}", path.display()),
        })?;
    log.write_all(message.as_bytes())
        .map_err(|source| LoopSupervisorError::Unlaunchable {
            name: name.to_owned(),
            cause: format!("log_unavailable: {}: {source}", path.display()),
        })
}

fn measure_consumption(paths: &OstromPaths, clock: &Clock) -> Consumption {
    let trace = match read_trace(&paths.trace_file()) {
        Ok(trace) => trace,
        Err(_) => {
            return Consumption {
                concurrent: Measurement::Unknown("trace_unreadable"),
                spend_usd: Measurement::Unknown("trace_unreadable"),
                tokens: Measurement::Unknown("trace_unreadable"),
            };
        }
    };
    if trace.rows.iter().any(Result::is_err) {
        return Consumption {
            concurrent: Measurement::Unknown("trace_malformed"),
            spend_usd: Measurement::Unknown("trace_malformed"),
            tokens: Measurement::Unknown("trace_malformed"),
        };
    }
    let rows = trace
        .rows
        .into_iter()
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    let mut active = BTreeSet::new();
    let mut concurrent_unknown = false;
    let mut spend = 0.0;
    let mut spend_unknown = false;
    let mut tokens = 0_u64;
    let day = clock.date();
    for row in rows {
        let order_id = row.fact.get("order_id").and_then(serde_json::Value::as_str);
        match row.kind.as_str() {
            "work-dispatched" => match order_id {
                Some(order_id) => {
                    active.insert(order_id.to_owned());
                }
                None => concurrent_unknown = true,
            },
            "work-completed" | "work-failed" => match order_id {
                Some(order_id) => {
                    active.remove(order_id);
                }
                None => concurrent_unknown = true,
            },
            "pass-ended" => {}
            _ => continue,
        }
        if !row.ts.starts_with(&day) {
            continue;
        }
        if matches!(
            row.kind.as_str(),
            "pass-ended" | "work-completed" | "work-failed"
        ) {
            match row.fact.get("cost_usd").and_then(serde_json::Value::as_f64) {
                Some(value) if value.is_finite() && value >= 0.0 => spend += value,
                Some(_) | None => spend_unknown = true,
            }
            if let Some(value) = row.fact.get("weighted_tokens") {
                match value.as_u64() {
                    Some(value) => tokens = tokens.saturating_add(value),
                    None => {
                        return Consumption {
                            concurrent: Measurement::Unknown("token_measurement_invalid"),
                            spend_usd: Measurement::Unknown("token_measurement_invalid"),
                            tokens: Measurement::Unknown("token_measurement_invalid"),
                        };
                    }
                }
            }
        }
    }
    Consumption {
        concurrent: if concurrent_unknown {
            Measurement::Unknown("concurrency_not_measured")
        } else {
            Measurement::Measured(u64::try_from(active.len()).unwrap_or(u64::MAX))
        },
        spend_usd: if spend_unknown {
            Measurement::Unknown("spend_not_measured")
        } else {
            Measurement::Measured(spend)
        },
        tokens: Measurement::Measured(tokens),
    }
}

fn exceeded_reason(
    consumption: Consumption,
    ceilings: ResolvedLoopCeilings,
    name: &str,
) -> Result<Option<String>, LoopSupervisorError> {
    if let Some(ceiling) = ceilings.concurrent {
        match consumption.concurrent {
            Measurement::Measured(value) if value >= ceiling => {
                return Ok(Some(format!(
                    "ceiling_exceeded:concurrent {value}/{ceiling}"
                )));
            }
            Measurement::Unknown(cause) => {
                return Err(LoopSupervisorError::Inconclusive {
                    name: name.to_owned(),
                    cause,
                });
            }
            Measurement::Measured(_) | Measurement::Unset => {}
        }
    }
    if let Some(ceiling) = ceilings.spend_usd {
        match consumption.spend_usd {
            Measurement::Measured(value) if value >= ceiling => {
                return Ok(Some(format!(
                    "ceiling_exceeded:spend_usd {}/{}",
                    render_number(value),
                    render_number(ceiling)
                )));
            }
            Measurement::Unknown(cause) => {
                return Err(LoopSupervisorError::Inconclusive {
                    name: name.to_owned(),
                    cause,
                });
            }
            Measurement::Measured(_) | Measurement::Unset => {}
        }
    }
    if let Some(ceiling) = ceilings.tokens {
        match consumption.tokens {
            Measurement::Measured(value) if value >= ceiling => {
                return Ok(Some(format!("ceiling_exceeded:tokens {value}/{ceiling}")));
            }
            Measurement::Unknown(cause) => {
                return Err(LoopSupervisorError::Inconclusive {
                    name: name.to_owned(),
                    cause,
                });
            }
            Measurement::Measured(_) | Measurement::Unset => {}
        }
    }
    Ok(None)
}

fn render_u64(measurement: Measurement<u64>, ceiling: Option<u64>) -> String {
    let used = match measurement {
        Measurement::Measured(value) => value.to_string(),
        Measurement::Unknown(cause) => format!("unknown:{cause}"),
        Measurement::Unset => "unknown:not_measured".to_owned(),
    };
    let ceiling = ceiling.map_or_else(|| "unbounded".to_owned(), |value| value.to_string());
    format!("{used}/{ceiling}")
}

fn render_spend(measurement: Measurement<f64>, ceiling: Option<f64>) -> String {
    let used = match measurement {
        Measurement::Measured(value) => format!("${value:.2}"),
        Measurement::Unknown(cause) => format!("unknown:{cause}"),
        Measurement::Unset => "unknown:not_measured".to_owned(),
    };
    let ceiling = ceiling.map_or_else(
        || "unbounded".to_owned(),
        |value| format!("${}", render_number(value)),
    );
    format!("{used}/{ceiling}")
}

fn render_number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        value.to_string()
    }
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

#[cfg(unix)]
fn terminate_process_group(name: &str, pid: u32) -> Result<(), LoopSupervisorError> {
    let status = Command::new("kill")
        .args(["-TERM", "--", &format!("-{pid}")])
        .status()
        .map_err(|_| LoopSupervisorError::Inconclusive {
            name: name.to_owned(),
            cause: "process_control_unavailable",
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(LoopSupervisorError::Inconclusive {
            name: name.to_owned(),
            cause: "process_termination_unconfirmed",
        })
    }
}

#[cfg(not(unix))]
fn terminate_process_group(name: &str, _pid: u32) -> Result<(), LoopSupervisorError> {
    Err(LoopSupervisorError::Inconclusive {
        name: name.to_owned(),
        cause: "process_control_unavailable",
    })
}
