//! Named agent runners shared by the coordinating loop and implementer.

use std::{
    collections::BTreeMap,
    env,
    ffi::{OsStr, OsString},
    fs,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    sync::Arc,
    thread,
    time::Duration,
};

use ostrom_core::ResolvedLoopCeilings;
use thiserror::Error;

use crate::{SignalFlags, environment};

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
}

#[derive(Debug, Clone)]
pub struct OrchestratorRunRequest {
    pub prompt: String,
    pub model: String,
    pub profile: PathBuf,
    pub permission_mode: String,
    pub ceilings: ResolvedLoopCeilings,
    pub transcript: PathBuf,
    pub signals: SignalFlags,
    pub supervisor_pid: Option<u32>,
    pub termination_grace: Duration,
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
pub enum RunOutcome {
    Exited(ExitStatus),
    Terminated(RunTermination),
    Error(ActionFault),
}

impl RunOutcome {
    #[must_use]
    pub fn status(&self) -> Option<ExitStatus> {
        match self {
            Self::Exited(status) => Some(*status),
            Self::Terminated(_) | Self::Error(_) => None,
        }
    }
}

pub trait AgentRunner: Harness {
    fn prepare(&self) -> Result<RunnerLaunch, ActionFault> {
        Ok(RunnerLaunch::new(Vec::new()))
    }

    fn run(&self, request: &RunRequest) -> RunOutcome;
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

    pub fn prepare(&self, name: &str) -> Result<RunnerLaunch, ActionFault> {
        self.get(name)
            .ok_or_else(|| ActionFault::new("unregistered_harness", None))?
            .prepare()
    }

    #[must_use]
    pub fn run(&self, name: &str, request: &RunRequest) -> RunOutcome {
        self.get(name).map_or_else(
            || RunOutcome::Error(ActionFault::new("unregistered_harness", None)),
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
}

impl CodexHarness {
    #[must_use]
    pub fn new(
        executable: impl Into<PathBuf>,
        version: impl Into<String>,
        default_model: impl Into<String>,
    ) -> Self {
        Self {
            executable: executable.into(),
            version: version.into(),
            default_model: default_model.into(),
        }
    }

    #[must_use]
    pub fn from_environment() -> Self {
        Self::new(
            environment::CODEX_BIN
                .value_os()
                .map_or_else(|| PathBuf::from("codex"), PathBuf::from),
            "codex-cli",
            "default",
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
        let node = NodeResolver::from_environment().resolve().ok_or_else(|| {
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
}

impl AgentRunner for CodexHarness {
    fn prepare(&self) -> Result<RunnerLaunch, ActionFault> {
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

    fn run(&self, request: &RunRequest) -> RunOutcome {
        let RunRequest::Implementer(request) = request else {
            return RunOutcome::Error(ActionFault::new("runner_kind_mismatch", None));
        };
        if !request.offline || request.token_ceiling == 0 {
            return RunOutcome::Error(ActionFault::new("runner_policy", None));
        }
        let (executable, _, path) = match self.resolved() {
            Ok(resolved) => resolved,
            Err(error) => return RunOutcome::Error(error),
        };
        let events = match fs::File::create(&request.transcript) {
            Ok(events) => events,
            Err(error) => {
                return RunOutcome::Error(ActionFault::new("runner_io", Some(error.to_string())));
            }
        };
        let errors = match events.try_clone() {
            Ok(errors) => errors,
            Err(error) => {
                return RunOutcome::Error(ActionFault::new("runner_io", Some(error.to_string())));
            }
        };
        let input = match fs::File::open(&request.prompt) {
            Ok(input) => input,
            Err(error) => {
                return RunOutcome::Error(ActionFault::new("runner_io", Some(error.to_string())));
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
        configure_agent_process_group(&mut command);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return RunOutcome::Error(ActionFault::new(
                    "runner_unavailable",
                    Some(format!("could not start Codex: {error}")),
                ));
            }
        };
        wait_for_agent_child(
            &mut child,
            &request.signals,
            request.supervisor_pid,
            request.termination_grace,
        )
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
    if !crate::pass::is_executable_file(candidate) {
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
    fn from_environment() -> Self {
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

        let standalone = environment::OSTROM_NODE_FALLBACKS.value_os().map_or_else(
            || {
                let mut paths = vec![
                    PathBuf::from("/usr/local/bin/node"),
                    PathBuf::from("/opt/homebrew/bin/node"),
                ];
                if let Some(home) = &home {
                    paths.push(home.join(".local/bin/node"));
                }
                paths
            },
            |paths| {
                paths
                    .to_string_lossy()
                    .split_whitespace()
                    .map(PathBuf::from)
                    .collect()
            },
        );

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
pub fn configure_agent_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(not(unix))]
pub fn configure_agent_process_group(_command: &mut Command) {}

/// Wait for an agent subprocess with the transcript-safe TERM/grace/KILL policy.
pub fn wait_for_agent_child(
    child: &mut std::process::Child,
    signals: &SignalFlags,
    supervisor_pid: Option<u32>,
    termination_grace: Duration,
) -> RunOutcome {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                crate::pass::kill_remaining_process_group(child.id());
                return RunOutcome::Exited(status);
            }
            Ok(None) => {}
            Err(error) => {
                return RunOutcome::Error(ActionFault::new(
                    "runner_io",
                    Some(error.to_string()),
                ));
            }
        }
        let signal = signals.take_pending();
        let orphaned = supervisor_pid.is_some_and(|pid| !crate::pass::process_alive(pid));
        if signal.is_some() || orphaned {
            let signal = signal.unwrap_or("TERM");
            let termination_signal =
                crate::pass::terminate_child_process_group(child, termination_grace);
            let _ = child.wait();
            return RunOutcome::Terminated(RunTermination {
                signal,
                termination_signal,
            });
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use tempfile::tempdir;

    use super::*;

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
    }

    impl AgentRunner for FixtureRunner {
        fn run(&self, request: &RunRequest) -> RunOutcome {
            assert!(matches!(request, RunRequest::Implementer(_)));
            self.ran.store(true, Ordering::SeqCst);
            RunOutcome::Error(ActionFault::new("fixture-finished", None))
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
            matches!(outcome, RunOutcome::Error(ref fault) if fault.name() == "fixture-finished")
        );
        assert!(ran.load(Ordering::SeqCst));
    }

    #[cfg(unix)]
    #[test]
    fn codex_implementer_argv_pins_the_offline_sandbox() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempdir().expect("Codex runner fixture");
        let executable = root.path().join("codex-fixture");
        fs::write(
            &executable,
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >\"$0.args\"\n",
        )
        .expect("write Codex fixture");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))
            .expect("make Codex fixture executable");
        fs::create_dir(root.path().join("worktree")).expect("create fixture worktree");
        fs::write(root.path().join("prompt.md"), "fixture prompt\n").expect("write fixture prompt");
        let runner = CodexHarness::new(&executable, "fixture-v1", "fixture-model");

        let outcome = runner.run(&implementer_request(root.path()));

        assert!(outcome.status().is_some_and(|status| status.success()));
        let arguments = fs::read_to_string(executable.with_extension("args"))
            .expect("read captured Codex arguments")
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert!(arguments.windows(2).any(|arguments| {
            arguments == ["-c", "sandbox_workspace_write.network_access=false"]
        }));
        assert!(
            !arguments
                .iter()
                .any(|argument| { argument == "sandbox_workspace_write.network_access=true" })
        );
    }
}
