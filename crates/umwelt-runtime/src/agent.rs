//! Named agent runners shared by the coordinating loop and implementer.

use std::{
    collections::BTreeMap,
    env,
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{environment, process_control};

pub mod claude;

/// Resource ceilings already resolved by an Umwelt consumer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoopCeilings {
    pub concurrent: Option<u64>,
    pub spend_usd: Option<f64>,
    pub tokens: Option<u64>,
}

/// Resource caps applied to one harness run.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RunCaps {
    /// Maximum elapsed wall-clock time for the run, in milliseconds.
    pub wall_ms: Option<u64>,
    /// Maximum idle time, in milliseconds; idle timing is suspended while a tool call is in flight.
    pub idle_ms: Option<u64>,
    /// Maximum number of agent turns completed by the run.
    pub turns: Option<u64>,
    /// Maximum cumulative token usage reported for the run.
    pub tokens: Option<u64>,
    /// Maximum cumulative cost reported for the run, in US dollars.
    pub cost_usd: Option<f64>,
    /// Grace period, in milliseconds, before cap enforcement force-kills the process group.
    pub kill_grace_ms: u64,
}

impl Default for RunCaps {
    fn default() -> Self {
        Self {
            wall_ms: None,
            idle_ms: None,
            turns: None,
            tokens: None,
            cost_usd: None,
            kill_grace_ms: 10_000,
        }
    }
}

impl RunCaps {
    /// Return the subset of per-run caps that the current ethogram wire can carry.
    #[must_use]
    pub fn to_wire(&self) -> ethogram::RunCeilings {
        // Kill grace is permanently excluded: it is a local enforcement detail,
        // not information that any wire consumer needs.
        //
        // Idle and turn caps are omitted for now only because this ethogram pin
        // cannot carry them. This exhaustive literal must fail to compile when
        // ethogram adds those fields so their mappings are added deliberately.
        ethogram::RunCeilings {
            cost_usd: self.cost_usd,
            tokens: self.tokens,
            wall_ms: self.wall_ms,
            extra: serde_json::Map::new(),
        }
    }
}

/// Per-cap enforcement support declared by a harness.
///
/// There is deliberately no [`Default`] implementation: every harness must
/// opt in to each claim rather than accidentally advertising a cap it ignores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapSupport {
    wall: bool,
    idle: bool,
    turns: bool,
    tokens: bool,
    cost: bool,
}

impl CapSupport {
    #[must_use]
    pub const fn none() -> Self {
        Self {
            wall: false,
            idle: false,
            turns: false,
            tokens: false,
            cost: false,
        }
    }

    #[must_use]
    pub const fn with_wall(mut self) -> Self {
        self.wall = true;
        self
    }

    #[must_use]
    pub const fn with_idle(mut self) -> Self {
        self.idle = true;
        self
    }

    #[must_use]
    pub const fn with_turns(mut self) -> Self {
        self.turns = true;
        self
    }

    #[must_use]
    pub const fn with_tokens(mut self) -> Self {
        self.tokens = true;
        self
    }

    #[must_use]
    pub const fn with_cost(mut self) -> Self {
        self.cost = true;
        self
    }

    #[must_use]
    pub const fn wall(self) -> bool {
        self.wall
    }

    #[must_use]
    pub const fn idle(self) -> bool {
        self.idle
    }

    #[must_use]
    pub const fn turns(self) -> bool {
        self.turns
    }

    #[must_use]
    pub const fn tokens(self) -> bool {
        self.tokens
    }

    #[must_use]
    pub const fn cost(self) -> bool {
        self.cost
    }
}

/// Signals observed by a running harness process.
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

/// Maximum turns passed to the shipped Claude spawn adapter.
pub const PASS_MAX_TURNS: &str = "200";

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{name}")]
pub struct ActionFault {
    name: &'static str,
    detail: Option<String>,
}

impl ActionFault {
    #[must_use]
    pub fn new(name: &'static str, detail: Option<String>) -> Self {
        Self { name, detail }
    }

    #[must_use]
    pub fn name(&self) -> &'static str {
        self.name
    }

    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }
}

pub trait Harness: Send + Sync {
    fn name(&self) -> &'static str;
    fn version(&self) -> &str;
    fn default_model(&self) -> &str;
    fn enforceable_caps(&self) -> CapSupport;
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrchestratorRunRequest {
    pub prompt: String,
    pub model: String,
    pub profile: PathBuf,
    pub permission_mode: String,
    pub ceilings: LoopCeilings,
    pub transcript: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ImplementerRunRequest {
    pub prompt: PathBuf,
    pub worktree: PathBuf,
    pub result: PathBuf,
    pub transcript: PathBuf,
    pub token_ceiling: u64,
    pub offline: bool,
    pub signals: SignalFlags,
    pub supervisor_pid: Option<u32>,
    pub termination_grace: Duration,
}

#[derive(Debug, Clone)]
pub enum RunRequest {
    Orchestrator(OrchestratorRunRequest),
    Implementer(ImplementerRunRequest),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerLaunch {
    environment: Vec<(OsString, OsString)>,
}

impl RunnerLaunch {
    #[must_use]
    pub fn new(environment: Vec<(OsString, OsString)>) -> Self {
        Self { environment }
    }

    #[must_use]
    pub fn environment(&self) -> &[(OsString, OsString)] {
        &self.environment
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunTermination {
    pub signal: &'static str,
    pub termination_signal: Option<String>,
}

#[derive(Debug)]
pub enum ProcessOutcome {
    Exited(ExitStatus),
    Terminated(RunTermination),
    Error(ActionFault),
}

impl ProcessOutcome {
    #[must_use]
    pub fn status(&self) -> Option<ExitStatus> {
        match self {
            Self::Exited(status) => Some(*status),
            Self::Terminated(_) | Self::Error(_) => None,
        }
    }
}

pub trait AgentRunner: Harness {
    /// Prepare a harness to enforce the requested caps.
    ///
    /// Refusal and the watchdog's run-start warning answer different problems.
    /// This refuses a cap the harness cannot enforce at all. An enforceable idle
    /// cap is accepted, but [`crate::watchdog::CapsWatchdog::start`] warns when
    /// no wall cap bounds the tool-hang case that idle intentionally suspends.
    fn prepare(&self, caps: &RunCaps) -> Result<RunnerLaunch, ActionFault> {
        refuse_unenforceable_caps(self.name(), self.enforceable_caps(), caps)?;
        Ok(RunnerLaunch::new(Vec::new()))
    }

    fn run(&self, request: &RunRequest) -> ProcessOutcome;
}

#[derive(Default)]
pub struct AgentRegistry {
    runners: BTreeMap<String, Arc<dyn AgentRunner>>,
}

impl AgentRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn core(runner: impl AgentRunner + 'static) -> Result<Self, ActionFault> {
        let mut registry = Self::new();
        registry.register(runner)?;
        Ok(registry)
    }

    pub fn register(&mut self, runner: impl AgentRunner + 'static) -> Result<(), ActionFault> {
        let name = runner.name();
        if !valid_component(name)
            || runner.version().is_empty()
            || runner.default_model().is_empty()
        {
            return Err(ActionFault::new("invalid_harness_registration", None));
        }
        let key = format!("agent/{name}");
        if self.runners.contains_key(&key) {
            return Err(ActionFault::new("ambiguous_harness", None));
        }
        self.runners.insert(key, Arc::new(runner));
        Ok(())
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<Arc<dyn AgentRunner>> {
        self.runners.get(name).cloned()
    }

    pub fn prepare(&self, name: &str, caps: &RunCaps) -> Result<RunnerLaunch, ActionFault> {
        self.get(name)
            .ok_or_else(|| ActionFault::new("unregistered_harness", None))?
            .prepare(caps)
    }

    #[must_use]
    pub fn run(&self, name: &str, request: &RunRequest) -> ProcessOutcome {
        self.get(name).map_or_else(
            || ProcessOutcome::Error(ActionFault::new("unregistered_harness", None)),
            |runner| runner.run(request),
        )
    }
}

fn valid_component(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// The shipped offline implementer runner.
pub struct CodexHarness {
    executable: PathBuf,
    version: String,
    default_model: String,
    node_fallbacks: Vec<PathBuf>,
}

impl CodexHarness {
    #[must_use]
    pub fn new(
        executable: impl Into<PathBuf>,
        version: impl Into<String>,
        default_model: impl Into<String>,
        node_fallbacks: Vec<PathBuf>,
    ) -> Self {
        Self {
            executable: executable.into(),
            version: version.into(),
            default_model: default_model.into(),
            node_fallbacks,
        }
    }

    #[must_use]
    pub fn from_environment(node_fallbacks: Vec<PathBuf>) -> Self {
        Self::new(
            environment::CODEX_BIN
                .value_os()
                .map_or_else(|| PathBuf::from("codex"), PathBuf::from),
            "codex-cli",
            "default",
            node_fallbacks,
        )
    }

    fn resolved(&self) -> Result<(PathBuf, PathBuf, OsString), ActionFault> {
        let executable = resolve_executable(&self.executable).ok_or_else(|| {
            ActionFault::new(
                "runner_unavailable",
                Some(format!(
                    "Codex is unavailable: {} was not found",
                    self.executable.display()
                )),
            )
        })?;
        let node = NodeResolver::from_environment(self.node_fallbacks.clone())
            .resolve()
            .ok_or_else(|| {
                ActionFault::new(
                    "runner_unavailable",
                    Some(format!(
                        "Codex is unavailable: Node.js could not be resolved for {}",
                        executable.display()
                    )),
                )
            })?;
        let inherited_path = environment::PATH
            .value_os()
            .unwrap_or_else(|| OsString::from("/usr/local/bin:/usr/bin:/bin"));
        let mut paths = vec![
            node.parent()
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf(),
        ];
        paths.extend(env::split_paths(&inherited_path));
        let path = env::join_paths(paths)
            .map_err(|error| ActionFault::new("runner_unavailable", Some(error.to_string())))?;
        Ok((executable, node, path))
    }
}

impl Harness for CodexHarness {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn version(&self) -> &str {
        &self.version
    }

    fn default_model(&self) -> &str {
        &self.default_model
    }

    fn enforceable_caps(&self) -> CapSupport {
        // Wall time is measured from the runtime's own monotonic clock, so it
        // never depends on a harness event. The codex `exec --json` schema is
        // otherwise unverified: chreode's normaliser and fixture explicitly
        // guess its tool, turn, and usage events. Until a real capture proves
        // those boundaries, Codex cannot honestly claim idle, turns, tokens,
        // or cost support.
        CapSupport::none().with_wall()
    }
}

impl AgentRunner for CodexHarness {
    fn prepare(&self, caps: &RunCaps) -> Result<RunnerLaunch, ActionFault> {
        refuse_unenforceable_caps(self.name(), self.enforceable_caps(), caps)?;
        let (executable, node, path) = self.resolved()?;
        if !Command::new(&executable)
            .arg("--version")
            .env("PATH", &path)
            .output()
            .is_ok_and(|output| output.status.success())
        {
            return Err(ActionFault::new(
                "runner_unavailable",
                Some(format!(
                    "Codex is unavailable: {} cannot execute with resolved Node {}",
                    executable.display(),
                    node.display()
                )),
            ));
        }
        Ok(RunnerLaunch::new(vec![
            (OsString::from("CODEX_BIN"), executable.into_os_string()),
            (OsString::from("PATH"), path),
        ]))
    }

    fn run(&self, request: &RunRequest) -> ProcessOutcome {
        let RunRequest::Implementer(request) = request else {
            return ProcessOutcome::Error(ActionFault::new("runner_kind_mismatch", None));
        };
        if !request.offline || request.token_ceiling == 0 {
            return ProcessOutcome::Error(ActionFault::new("runner_policy", None));
        }
        let (executable, _, path) = match self.resolved() {
            Ok(resolved) => resolved,
            Err(error) => return ProcessOutcome::Error(error),
        };
        let events = match fs::File::create(&request.transcript) {
            Ok(events) => events,
            Err(error) => {
                return ProcessOutcome::Error(ActionFault::new(
                    "runner_io",
                    Some(error.to_string()),
                ));
            }
        };
        let errors = match events.try_clone() {
            Ok(errors) => errors,
            Err(error) => {
                return ProcessOutcome::Error(ActionFault::new(
                    "runner_io",
                    Some(error.to_string()),
                ));
            }
        };
        let input = match fs::File::open(&request.prompt) {
            Ok(input) => input,
            Err(error) => {
                return ProcessOutcome::Error(ActionFault::new(
                    "runner_io",
                    Some(error.to_string()),
                ));
            }
        };
        let mut command = Command::new(executable);
        // This literal is deliberately pinned: implementer work is offline.
        command
            .args([
                "exec",
                "--json",
                "-C",
                &request.worktree.display().to_string(),
                "-s",
                "workspace-write",
                "-c",
                "approval_policy=\"never\"",
                "-c",
                "sandbox_workspace_write.network_access=false",
                "-c",
                "web_search=\"disabled\"",
                "-o",
                &request.result.display().to_string(),
            ])
            .env("PATH", path)
            .stdin(Stdio::from(input))
            .stdout(Stdio::from(events))
            .stderr(Stdio::from(errors));
        set_process_group(&mut command);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return ProcessOutcome::Error(ActionFault::new(
                    "runner_unavailable",
                    Some(format!("could not start Codex: {error}")),
                ));
            }
        };
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    process_control::kill_remaining_process_group(child.id());
                    return ProcessOutcome::Exited(status);
                }
                Ok(None) => {}
                Err(error) => {
                    return ProcessOutcome::Error(ActionFault::new(
                        "runner_io",
                        Some(error.to_string()),
                    ));
                }
            }
            let signal = request.signals.take_pending();
            let orphaned = request
                .supervisor_pid
                .is_some_and(|pid| !process_control::process_alive(pid));
            if signal.is_some() || orphaned {
                let signal = signal.unwrap_or("TERM");
                let termination_signal = process_control::terminate_child_process_group(
                    &mut child,
                    request.termination_grace,
                );
                let _ = child.wait();
                return ProcessOutcome::Terminated(RunTermination {
                    signal,
                    termination_signal,
                });
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}

fn refuse_unenforceable_caps(
    harness: &str,
    support: CapSupport,
    caps: &RunCaps,
) -> Result<(), ActionFault> {
    let mut unsupported = Vec::new();
    if caps.wall_ms.is_some() && !support.wall() {
        unsupported.push("wall");
    }
    if caps.idle_ms.is_some() && !support.idle() {
        unsupported.push("idle");
    }
    if caps.turns.is_some() && !support.turns() {
        unsupported.push("turns");
    }
    if caps.tokens.is_some() && !support.tokens() {
        unsupported.push("tokens");
    }
    if caps.cost_usd.is_some() && !support.cost() {
        unsupported.push("cost");
    }
    if unsupported.is_empty() {
        Ok(())
    } else {
        Err(ActionFault::new(
            "unsupported_run_caps",
            Some(format!(
                "{harness} cannot enforce requested caps: {}",
                unsupported.join(", ")
            )),
        ))
    }
}

fn resolve_executable(command: &Path) -> Option<PathBuf> {
    if command.components().count() > 1 {
        absolute_executable(command)
    } else {
        find_on_path(command).or_else(|| find_in_nvm(command))
    }
}

pub(crate) fn absolute_executable(candidate: &Path) -> Option<PathBuf> {
    if !process_control::is_executable_file(candidate) {
        return None;
    }
    if candidate.is_absolute() {
        Some(candidate.to_path_buf())
    } else {
        candidate.canonicalize().ok()
    }
}

fn find_on_path(command: &Path) -> Option<PathBuf> {
    find_on_path_in(command, environment::PATH.value_os().as_deref())
}

fn find_on_path_in(command: &Path, path: Option<&OsStr>) -> Option<PathBuf> {
    env::split_paths(path?).find_map(|directory| absolute_executable(&directory.join(command)))
}

fn find_in_nvm(command: &Path) -> Option<PathBuf> {
    let home = nonempty_env_path(environment::HOME);
    let nvm = env_path_or_home(environment::NVM_DIR, home.as_deref(), ".nvm")?;
    find_in_nvm_root(command, &nvm)
}

pub(crate) fn find_in_nvm_root(command: &Path, nvm: &Path) -> Option<PathBuf> {
    let default = fs::read_to_string(nvm.join("alias/default"))
        .ok()?
        .lines()
        .next()?
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    let version = default.strip_prefix('v').unwrap_or(&default);
    let parts = version.split('.').collect::<Vec<_>>();
    if parts.len() == 3 && parts.iter().all(|part| is_ascii_number(part)) {
        return absolute_executable(
            &nvm.join("versions/node")
                .join(format!("v{version}"))
                .join("bin")
                .join(command),
        );
    }
    if parts.len() != 1 || !is_ascii_number(version) {
        return None;
    }

    let prefix = format!("v{version}.");
    let mut candidates = fs::read_dir(nvm.join("versions/node"))
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let suffix = name.to_str()?.strip_prefix(&prefix)?;
            let (minor, patch) = suffix.split_once('.')?;
            if patch.contains('.') || !is_ascii_number(minor) || !is_ascii_number(patch) {
                return None;
            }
            Some((
                entry.path().join("bin").join(command),
                minor.parse::<u64>().ok()?,
                patch.parse::<u64>().ok()?,
            ))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| left.0.cmp(&right.0));

    let mut best = None;
    let mut best_version = None;
    for (candidate, minor, patch) in candidates {
        if best_version.is_none_or(|current| (minor, patch) > current)
            && let Some(candidate) = absolute_executable(&candidate)
        {
            best = Some(candidate);
            best_version = Some((minor, patch));
        }
    }
    best
}

fn is_ascii_number(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn nonempty_env_path(variable: environment::EnvironmentVariable) -> Option<PathBuf> {
    variable
        .value_os()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn env_path_or_home(
    variable: environment::EnvironmentVariable,
    home: Option<&Path>,
    home_suffix: &str,
) -> Option<PathBuf> {
    nonempty_env_path(variable).or_else(|| home.map(|path| path.join(home_suffix)))
}

#[derive(Debug)]
pub(crate) struct NodeResolver {
    pub(crate) path: Option<OsString>,
    pub(crate) nvm_dir: Option<PathBuf>,
    pub(crate) fnm_dirs: Vec<PathBuf>,
    pub(crate) volta_home: Option<PathBuf>,
    pub(crate) asdf_data_dir: Option<PathBuf>,
    pub(crate) standalone: Vec<PathBuf>,
}

impl NodeResolver {
    fn from_environment(standalone: Vec<PathBuf>) -> Self {
        let home = nonempty_env_path(environment::HOME);
        let mut fnm_dirs = Vec::new();
        if let Some(directory) =
            env_path_or_home(environment::FNM_DIR, home.as_deref(), ".local/share/fnm")
        {
            fnm_dirs.push(directory);
        }
        if let Some(home) = &home {
            fnm_dirs.push(home.join(".fnm"));
        }

        Self {
            path: environment::PATH.value_os(),
            nvm_dir: env_path_or_home(environment::NVM_DIR, home.as_deref(), ".nvm"),
            fnm_dirs,
            volta_home: env_path_or_home(environment::VOLTA_HOME, home.as_deref(), ".volta"),
            asdf_data_dir: env_path_or_home(environment::ASDF_DATA_DIR, home.as_deref(), ".asdf"),
            standalone,
        }
    }

    pub(crate) fn resolve(&self) -> Option<PathBuf> {
        let command = Path::new("node");
        find_on_path_in(command, self.path.as_deref())
            .or_else(|| {
                self.nvm_dir
                    .as_deref()
                    .and_then(|directory| find_in_nvm_root(command, directory))
            })
            .or_else(|| {
                self.fnm_dirs.iter().find_map(|directory| {
                    absolute_executable(&directory.join("aliases/default/bin/node"))
                })
            })
            .or_else(|| {
                self.volta_home
                    .as_deref()
                    .and_then(|directory| absolute_executable(&directory.join("bin/node")))
            })
            .or_else(|| {
                self.asdf_data_dir
                    .as_deref()
                    .and_then(|directory| absolute_executable(&directory.join("shims/node")))
            })
            .or_else(|| {
                self.standalone
                    .iter()
                    .find_map(|candidate| absolute_executable(candidate))
            })
    }
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
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn run_caps_wire_round_trips_carried_fields() {
        let caps = RunCaps {
            wall_ms: Some(567),
            idle_ms: None,
            turns: None,
            tokens: Some(234),
            cost_usd: Some(1.25),
            kill_grace_ms: 10_000,
        };
        let wire = caps.to_wire();
        let json = serde_json::to_string(&wire).expect("serialise wire ceilings");
        let round_tripped =
            serde_json::from_str::<ethogram::RunCeilings>(&json).expect("parse wire ceilings");

        assert_eq!(round_tripped, wire);
        assert_eq!(
            round_tripped,
            ethogram::RunCeilings {
                cost_usd: Some(1.25),
                tokens: Some(234),
                wall_ms: Some(567),
                extra: serde_json::Map::new(),
            }
        );
    }

    #[test]
    fn kill_grace_is_permanently_absent_from_wire() {
        let caps = RunCaps {
            wall_ms: Some(101),
            tokens: Some(202),
            cost_usd: Some(3.5),
            kill_grace_ms: 42_123,
            ..RunCaps::default()
        };
        let value = serde_json::to_value(caps.to_wire()).expect("serialise wire ceilings");
        let object = value.as_object().expect("wire ceilings object");

        // This absence is permanent: kill grace is an enforcement detail, not
        // a wire cap awaiting upstream support.
        assert!(!object.is_empty());
        assert!(object.keys().all(|key| {
            let key = key.to_ascii_lowercase();
            !key.contains("kill") && !key.contains("grace")
        }));
    }

    #[test]
    fn idle_and_turns_are_absent_only_until_ethogram_can_carry_them() {
        let caps = RunCaps {
            wall_ms: Some(123),
            idle_ms: Some(456),
            turns: Some(789),
            tokens: Some(234),
            cost_usd: Some(1.25),
            ..RunCaps::default()
        };
        let value = serde_json::to_value(caps.to_wire()).expect("serialise wire ceilings");
        let object = value.as_object().expect("wire ceilings object");

        // This assertion is expected to change when ethogram adds idleMs and
        // turns; unlike kill grace, these are absent only because of today's wire.
        assert!(!object.is_empty());
        assert!(!object.contains_key("idleMs"));
        assert!(!object.contains_key("turns"));
    }

    #[test]
    fn none_caps_are_omitted_from_wire_instead_of_sent_as_null() {
        let json =
            serde_json::to_string(&RunCaps::default().to_wire()).expect("serialise wire ceilings");

        assert_eq!(json, "{}");
    }

    #[test]
    fn run_caps_default_is_ten_seconds_of_grace_and_no_caps() {
        assert_eq!(
            RunCaps::default(),
            RunCaps {
                wall_ms: None,
                idle_ms: None,
                turns: None,
                tokens: None,
                cost_usd: None,
                kill_grace_ms: 10_000,
            }
        );
    }

    struct FixtureRunner {
        name: &'static str,
        ran: Arc<AtomicBool>,
    }

    impl Harness for FixtureRunner {
        fn name(&self) -> &'static str {
            self.name
        }

        fn version(&self) -> &str {
            "fixture-v1"
        }

        fn default_model(&self) -> &str {
            "fixture-model"
        }

        fn enforceable_caps(&self) -> CapSupport {
            CapSupport::none().with_wall().with_tokens()
        }
    }

    impl AgentRunner for FixtureRunner {
        fn run(&self, request: &RunRequest) -> ProcessOutcome {
            assert!(matches!(request, RunRequest::Implementer(_)));
            self.ran.store(true, Ordering::SeqCst);
            ProcessOutcome::Error(ActionFault::new("fixture-finished", None))
        }
    }

    fn implementer_request(root: &Path) -> RunRequest {
        RunRequest::Implementer(ImplementerRunRequest {
            prompt: root.join("prompt.md"),
            worktree: root.join("worktree"),
            result: root.join("result.md"),
            transcript: root.join("events.jsonl"),
            token_ceiling: 100,
            offline: true,
            signals: SignalFlags::default(),
            supervisor_pid: None,
            termination_grace: Duration::from_secs(1),
        })
    }

    #[test]
    fn codex_is_registered_under_the_default_implementer_name() {
        let registry = AgentRegistry::core(CodexHarness::new(
            "codex-fixture",
            "fixture-v1",
            "fixture-model",
            Vec::new(),
        ))
        .expect("register Codex fixture");
        assert_eq!(
            registry
                .get("agent/codex")
                .expect("resolve default implementer")
                .name(),
            "codex"
        );
    }

    #[test]
    fn harness_refuses_every_requested_cap_it_cannot_enforce() {
        let runner = FixtureRunner {
            name: "fixture",
            ran: Arc::new(AtomicBool::new(false)),
        };
        let caps = RunCaps {
            idle_ms: Some(1_000),
            turns: Some(2),
            cost_usd: Some(0.5),
            ..RunCaps::default()
        };

        let error = runner
            .prepare(&caps)
            .expect_err("unsupported caps must be refused");

        assert_eq!(error.name(), "unsupported_run_caps");
        assert_eq!(
            error.detail(),
            Some("fixture cannot enforce requested caps: idle, turns, cost")
        );
    }

    #[test]
    fn harness_without_idle_support_refuses_an_idle_cap() {
        let runner = FixtureRunner {
            name: "fixture",
            ran: Arc::new(AtomicBool::new(false)),
        };
        let caps = RunCaps {
            idle_ms: Some(1_000),
            ..RunCaps::default()
        };

        let error = runner
            .prepare(&caps)
            .expect_err("idle cap must require idle support");

        assert_eq!(error.name(), "unsupported_run_caps");
        assert_eq!(
            error.detail(),
            Some("fixture cannot enforce requested caps: idle")
        );
    }

    #[test]
    fn harness_accepts_caps_it_can_enforce() {
        let runner = FixtureRunner {
            name: "fixture",
            ran: Arc::new(AtomicBool::new(false)),
        };
        let caps = RunCaps {
            wall_ms: Some(1_000),
            tokens: Some(2_000),
            ..RunCaps::default()
        };

        let launch = runner
            .prepare(&caps)
            .expect("supported caps must be accepted");

        assert!(launch.environment().is_empty());
    }

    #[test]
    fn codex_declares_only_runtime_clock_cap_support() {
        let codex = CodexHarness::new("codex", "fixture-v1", "fixture-model", Vec::new());

        // The `exec --json` schema is unverified, so any claim beyond the
        // runtime's own wall clock would be a guess.
        assert_eq!(codex.enforceable_caps(), CapSupport::none().with_wall());
    }

    #[test]
    fn named_handoff_runs_a_second_registered_implementer() {
        let ran = Arc::new(AtomicBool::new(false));
        let mut registry = AgentRegistry::core(FixtureRunner {
            name: "codex",
            ran: Arc::new(AtomicBool::new(false)),
        })
        .expect("register default fixture");
        registry
            .register(FixtureRunner {
                name: "fixture",
                ran: Arc::clone(&ran),
            })
            .expect("register alternate fixture");
        let root = tempdir().expect("runner request root");

        let outcome = registry.run("agent/fixture", &implementer_request(root.path()));

        assert!(
            matches!(outcome, ProcessOutcome::Error(ref fault) if fault.name() == "fixture-finished")
        );
        assert!(ran.load(Ordering::SeqCst));
    }

    #[cfg(unix)]
    #[test]
    fn codex_implementer_argv_pins_the_offline_sandbox() {
        let root = tempdir().expect("Codex runner fixture");
        fs::create_dir(root.path().join("worktree")).expect("create fixture worktree");
        fs::write(root.path().join("prompt.md"), "fixture prompt\n").expect("write fixture prompt");
        let runner = CodexHarness::new("/bin/echo", "fixture-v1", "fixture-model", Vec::new());

        let outcome = runner.run(&implementer_request(root.path()));

        assert!(
            outcome.status().is_some_and(|status| status.success()),
            "Codex fixture outcome: {outcome:?}"
        );
        let arguments = fs::read_to_string(root.path().join("events.jsonl"))
            .expect("read captured Codex arguments");
        assert!(arguments.contains("-c sandbox_workspace_write.network_access=false"));
        assert!(!arguments.contains("sandbox_workspace_write.network_access=true"));
    }
}
