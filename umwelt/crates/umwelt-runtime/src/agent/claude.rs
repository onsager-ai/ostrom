use std::{
    fs,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
};

use serde_json::{Value, json};

use super::{
    ActionFault, AgentRunner, CapSupport, Harness, ImplementerRunRequest, OrchestratorRunRequest,
    PASS_MAX_TURNS, ProcessOutcome, RunRequest, supervise_implementer,
};
use crate::process_control;

/// The oldest Claude Code release whose flags and sandbox settings the
/// implementer boundary relies on: `--restricted`, `--permission-prompts`, and
/// `sandbox.network.strictAllowlist`. An older release could ignore a setting
/// it does not know, so the implementer mode refuses it instead.
pub const CLAUDE_IMPLEMENTER_MINIMUM_VERSION: (u64, u64, u64) = (2, 1, 259);

/// The settings profile an implementer run is started with, written beside its
/// transcript and never inside the worktree it edits.
pub const CLAUDE_IMPLEMENTER_SETTINGS_FILE: &str = "claude-implementer.settings.json";

const SETTINGS_SCHEMA: &str = "https://json.schemastore.org/claude-code-settings.json";

/// The only tools an implementer run is given. `WebFetch` and `WebSearch` are
/// absent, and denied again below, so the file tools and sandboxed commands
/// are all it has.
const IMPLEMENTER_TOOLS: &str = "Bash,Read,Edit,Write,Glob,Grep";

/// The settings profile of a Claude implementer run: the equivalent of Codex's
/// `workspace-write` sandbox with network access off.
///
/// Every shell command runs in Claude Code's OS sandbox, which may write only
/// the working directory (the worktree) and its per-user temp directory, and
/// which reaches no host at all: no domain is allowed and the strict allowlist
/// denies, rather than asks about, every other. The sandbox refuses to start
/// rather than run unsandboxed, and a command it blocks is never retried
/// outside it. Bash is allowed only by `autoAllowBashIfSandboxed`, so if this
/// profile were ever not applied, shell commands would need an approval that
/// `--permission-prompts none` never gives.
#[must_use]
pub fn implementer_settings() -> Value {
    json!({
        "$schema": SETTINGS_SCHEMA,
        "permissions": {
            "deny": ["WebFetch", "WebSearch"],
        },
        "sandbox": {
            "enabled": true,
            "failIfUnavailable": true,
            "autoAllowBashIfSandboxed": true,
            "allowUnsandboxedCommands": false,
            "network": {
                "allowedDomains": [],
                "strictAllowlist": true,
            },
        },
    })
}

/// The argv of a Claude implementer run, after the executable. The prompt is
/// read from stdin and the run's working directory is the worktree.
///
/// `--restricted` confines the file tools to the working directory, ignores
/// user, project and local settings (a repository's own `.claude/` cannot widen
/// the profile), and refuses `bypassPermissions`. `acceptEdits` approves edits
/// inside the working directory; `--permission-prompts none` denies everything
/// else that would ask. No MCP server is loaded.
#[must_use]
pub fn implementer_arguments(
    settings: &Path,
    model: Option<&str>,
    effort: Option<&str>,
) -> Vec<String> {
    let mut arguments = vec![
        "--print".to_owned(),
        "--restricted".to_owned(),
        "--tools".to_owned(),
        IMPLEMENTER_TOOLS.to_owned(),
        "--disallowed-tools".to_owned(),
        "WebFetch,WebSearch".to_owned(),
        "--strict-mcp-config".to_owned(),
        "--settings".to_owned(),
        settings.display().to_string(),
        "--permission-mode".to_owned(),
        "acceptEdits".to_owned(),
        "--permission-prompts".to_owned(),
        "none".to_owned(),
        "--output-format".to_owned(),
        "stream-json".to_owned(),
        "--verbose".to_owned(),
        "--max-turns".to_owned(),
        PASS_MAX_TURNS.to_owned(),
    ];
    if let Some(model) = model {
        arguments.push(format!("--model={model}"));
    }
    if let Some(effort) = effort {
        arguments.push(format!("--effort={effort}"));
    }
    arguments
}

/// Where a Claude implementer run started with `arguments` and `settings` in
/// `worktree` may write, or `None` when nothing in them confines the file
/// tools. Read from the rendered argv and profile, not restated, so anything
/// that widens either widens this too.
#[must_use]
pub fn implementer_write_roots(
    arguments: &[String],
    settings: &Value,
    worktree: &Path,
) -> Option<Vec<PathBuf>> {
    if !arguments.iter().any(|argument| argument == "--restricted") {
        return None;
    }
    let mut roots = vec![lexical(worktree)];
    let mut arguments = arguments.iter();
    while let Some(argument) = arguments.next() {
        if argument == "--add-dir" {
            roots.extend(arguments.next().map(|path| lexical(Path::new(path))));
        } else if let Some(path) = argument.strip_prefix("--add-dir=") {
            roots.push(lexical(Path::new(path)));
        }
    }
    for pointer in [
        "/sandbox/filesystem/allowWrite",
        "/permissions/additionalDirectories",
    ] {
        match settings.pointer(pointer) {
            None => {}
            Some(Value::Array(paths)) => {
                for path in paths {
                    roots.push(lexical(Path::new(path.as_str()?)));
                }
            }
            Some(_) => return None,
        }
    }
    Some(roots)
}

/// Whether `path` lies under one of `roots`, compared lexically so a `..`
/// cannot climb out of a root.
#[must_use]
pub fn write_permitted(roots: &[PathBuf], path: &Path) -> bool {
    let path = lexical(path);
    roots.iter().any(|root| path.starts_with(root))
}

fn lexical(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn parse_version(output: &str) -> Option<(u64, u64, u64)> {
    let version = output.split_whitespace().next()?;
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((major, minor, patch))
}

/// Spawn adapter for the `agent/claude` harness.
pub struct ClaudeHarness {
    executable: PathBuf,
    version: String,
    default_model: String,
}

impl ClaudeHarness {
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
}

impl Harness for ClaudeHarness {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn version(&self) -> &str {
        &self.version
    }

    fn default_model(&self) -> &str {
        &self.default_model
    }

    fn enforceable_caps(&self) -> CapSupport {
        // Wall time comes from the runtime's monotonic clock. Claude's verified
        // `stream-json` output exposes assistant turn boundaries, tool use and
        // matching tool results, and per-message token usage, so wall, idle,
        // turns, and tokens have observable enforcement points. Total cost is
        // reported only by the terminal result; that makes cost enforcement
        // end-only, but still truthful.
        CapSupport::none()
            .with_wall()
            .with_idle()
            .with_turns()
            .with_tokens()
            .with_cost()
    }
}

impl AgentRunner for ClaudeHarness {
    fn run(&self, request: &RunRequest) -> ProcessOutcome {
        match request {
            RunRequest::Orchestrator(request) => self.run_orchestrator(request),
            RunRequest::Implementer(request) => self.run_implementer(request),
        }
    }
}

impl ClaudeHarness {
    fn run_orchestrator(&self, request: &OrchestratorRunRequest) -> ProcessOutcome {
        let output = match fs::File::create(&request.transcript) {
            Ok(output) => output,
            Err(error) => {
                return ProcessOutcome::Error(ActionFault::new(
                    "runner_io",
                    Some(error.to_string()),
                ));
            }
        };
        let error_output = match output.try_clone() {
            Ok(error_output) => error_output,
            Err(error) => {
                return ProcessOutcome::Error(ActionFault::new(
                    "runner_io",
                    Some(error.to_string()),
                ));
            }
        };
        let mut command = Command::new(&self.executable);
        command
            .args([
                "--print",
                "--settings",
                &request.profile.display().to_string(),
                "--permission-mode",
                &request.permission_mode,
                "--output-format",
                "stream-json",
                "--verbose",
                "--max-turns",
                PASS_MAX_TURNS,
                &request.prompt,
            ])
            .stdout(Stdio::from(output))
            .stderr(Stdio::from(error_output));
        process_control::set_process_group(&mut command);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return ProcessOutcome::Error(ActionFault::new(
                    "runner_unavailable",
                    Some(error.to_string()),
                ));
            }
        };
        match child.wait() {
            Ok(status) => ProcessOutcome::Exited(status),
            Err(error) => {
                ProcessOutcome::Error(ActionFault::new("runner_io", Some(error.to_string())))
            }
        }
    }

    /// Run one implementer order inside the worktree under
    /// [`implementer_settings`], the way the Codex harness does: the prompt
    /// file on stdin, the stream on the transcript, the final answer in the
    /// result file, and the process group stopped on a forwarded signal.
    fn run_implementer(&self, request: &ImplementerRunRequest) -> ProcessOutcome {
        if !request.offline || request.token_ceiling == 0 {
            return ProcessOutcome::Error(ActionFault::new("runner_policy", None));
        }
        if let Err(fault) = self.check_implementer_version() {
            return ProcessOutcome::Error(fault);
        }
        let io = |error: std::io::Error| {
            ProcessOutcome::Error(ActionFault::new("runner_io", Some(error.to_string())))
        };
        let settings = request
            .transcript
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(CLAUDE_IMPLEMENTER_SETTINGS_FILE);
        let mut profile = match serde_json::to_string_pretty(&implementer_settings()) {
            Ok(profile) => profile,
            Err(error) => {
                return ProcessOutcome::Error(ActionFault::new(
                    "runner_io",
                    Some(error.to_string()),
                ));
            }
        };
        profile.push('\n');
        if let Err(error) = fs::write(&settings, profile) {
            return io(error);
        }
        let events = match fs::File::create(&request.transcript) {
            Ok(events) => events,
            Err(error) => return io(error),
        };
        let errors = match events.try_clone() {
            Ok(errors) => errors,
            Err(error) => return io(error),
        };
        let input = match fs::File::open(&request.prompt) {
            Ok(input) => input,
            Err(error) => return io(error),
        };
        let mut command = Command::new(&self.executable);
        command
            .args(implementer_arguments(
                &settings,
                request.model.as_deref(),
                request.effort.as_deref(),
            ))
            .current_dir(&request.worktree)
            .envs(request.environment.iter().cloned())
            .stdin(Stdio::from(input))
            .stdout(Stdio::from(events))
            .stderr(Stdio::from(errors));
        process_control::set_process_group(&mut command);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return ProcessOutcome::Error(ActionFault::new(
                    "runner_unavailable",
                    Some(format!("could not start Claude: {error}")),
                ));
            }
        };
        request.spawned.notify(child.id());
        let outcome = supervise_implementer(&mut child, request);
        if outcome.status().is_some_and(|status| status.success())
            && let Some(result) = final_result(&request.transcript)
            && let Err(error) = fs::write(&request.result, result)
        {
            return io(error);
        }
        outcome
    }

    fn check_implementer_version(&self) -> Result<(), ActionFault> {
        let output = Command::new(&self.executable)
            .arg("--version")
            .stdin(Stdio::null())
            .output()
            .map_err(|error| {
                ActionFault::new(
                    "runner_unavailable",
                    Some(format!("could not start Claude: {error}")),
                )
            })?;
        let reported = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let version = if output.status.success() {
            parse_version(&reported)
        } else {
            None
        };
        match version {
            Some(version) if version >= CLAUDE_IMPLEMENTER_MINIMUM_VERSION => Ok(()),
            _ => {
                let (major, minor, patch) = CLAUDE_IMPLEMENTER_MINIMUM_VERSION;
                Err(ActionFault::new(
                    "runner_policy",
                    Some(format!(
                        "Claude Code `{reported}` cannot run an implementer: the sandbox profile needs {major}.{minor}.{patch} or later"
                    )),
                ))
            }
        }
    }
}

/// The text of the stream's final `result` event, which is what Codex's `-o`
/// writes for its own last message.
fn final_result(transcript: &Path) -> Option<String> {
    let contents = fs::read_to_string(transcript).ok()?;
    contents.lines().rev().find_map(|line| {
        let event = serde_json::from_str::<Value>(line).ok()?;
        if event.get("type")?.as_str()? != "result" {
            return None;
        }
        event.get("result")?.as_str().map(str::to_owned)
    })
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };

    use tempfile::tempdir;

    use super::*;
    use crate::agent::{
        AgentRegistry, ImplementerRunRequest, LoopCeilings, OrchestratorRunRequest, SignalFlags,
    };

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
            CapSupport::none().with_wall()
        }
    }

    impl AgentRunner for FixtureRunner {
        fn run(&self, request: &RunRequest) -> ProcessOutcome {
            assert!(matches!(request, RunRequest::Implementer(_)));
            self.ran.store(true, Ordering::SeqCst);
            ProcessOutcome::Error(ActionFault::new("fixture-finished", None))
        }
    }

    #[cfg(unix)]
    #[test]
    fn claude_agent_runner_preserves_the_pass_argv_contract() {
        let fixture = tempdir().expect("fixture directory");
        let profile = fixture.path().join("roles/builder.settings.json");
        let transcript = fixture.path().join("transcript.jsonl");
        let prompt = "Resolve the declared operation prompt.";
        let request = RunRequest::Orchestrator(OrchestratorRunRequest {
            prompt: prompt.to_owned(),
            model: "fixture-model".to_owned(),
            profile: profile.clone(),
            permission_mode: "auto".to_owned(),
            ceilings: LoopCeilings::default(),
            transcript,
        });

        let outcome =
            ClaudeHarness::new("/bin/echo", "claude-fixture-v1", "fixture-model").run(&request);
        assert!(outcome.status().is_some_and(|status| status.success()));
        let observed = fs::read_to_string(fixture.path().join("transcript.jsonl"))
            .expect("read observed runner arguments");
        assert_eq!(
            observed,
            format!(
                "--print --settings {} --permission-mode auto --output-format stream-json --verbose --max-turns {} {prompt}\n",
                profile.display(),
                PASS_MAX_TURNS,
            )
        );
    }

    #[test]
    fn claude_declares_all_stream_observable_cap_support() {
        let claude = ClaudeHarness::new("claude", "fixture-v1", "fixture-model");

        // Verified `stream-json` emits the tool, turn, and usage boundaries
        // required by the stream-derived caps; wall uses the runtime clock.
        assert_eq!(
            claude.enforceable_caps(),
            CapSupport::none()
                .with_wall()
                .with_idle()
                .with_turns()
                .with_tokens()
                .with_cost()
        );
    }

    #[test]
    fn agent_registry_resolves_named_runners_and_rejects_duplicates() {
        let mut registry = AgentRegistry::core(ClaudeHarness::new(
            "claude",
            "claude-fixture-v1",
            "fixture-model",
        ))
        .expect("core agent registry");
        registry
            .register(FixtureRunner {
                name: "codex",
                ran: Arc::new(AtomicBool::new(false)),
            })
            .expect("register second runner");

        assert_eq!(
            registry
                .get("agent/claude")
                .expect("resolve Claude runner")
                .name(),
            "claude"
        );
        assert_eq!(
            registry
                .get("agent/codex")
                .expect("resolve codex runner")
                .name(),
            "codex"
        );
        let error = registry
            .register(FixtureRunner {
                name: "codex",
                ran: Arc::new(AtomicBool::new(false)),
            })
            .expect_err("duplicate runner must be rejected");
        assert_eq!(error.name(), "ambiguous_harness");
    }

    fn implementer_request(root: &Path, offline: bool) -> ImplementerRunRequest {
        ImplementerRunRequest {
            prompt: root.join("prompt.md"),
            worktree: root.join("worktree"),
            result: root.join("run/result.md"),
            transcript: root.join("run/transcript.jsonl"),
            token_ceiling: 1,
            offline,
            model: Some("fixture-model-x".to_owned()),
            effort: Some("high".to_owned()),
            signals: SignalFlags::default(),
            supervisor_pid: None,
            termination_grace: std::time::Duration::from_secs(1),
            environment: Vec::new(),
            spawned: crate::SpawnObserver::default(),
        }
    }

    #[cfg(unix)]
    fn stub(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;

        fs::write(path, format!("#!/bin/sh\n{body}\n")).expect("write Claude stub");
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod Claude stub");
    }

    #[test]
    fn an_implementer_request_that_is_not_offline_is_refused_without_spawning() {
        let fixture = tempdir().expect("fixture directory");
        let request = RunRequest::Implementer(implementer_request(fixture.path(), false));
        let outcome = ClaudeHarness::new("missing", "fixture-v1", "fixture-model").run(&request);
        assert!(matches!(
            outcome,
            ProcessOutcome::Error(ref fault) if fault.name() == "runner_policy"
        ));
    }

    /// #626: the implementer profile confines writes to the worktree. The
    /// roots are read from the argv and profile the harness actually renders,
    /// so an `--add-dir`, an `allowWrite` or a dropped `--restricted` makes a
    /// write outside the worktree permitted and this test fail.
    #[test]
    fn the_implementer_profile_refuses_a_write_outside_the_worktree() {
        let worktree = Path::new("/state/implementer-worktrees/placeholder");
        let arguments = implementer_arguments(
            Path::new("/state/implementer-runs/placeholder/claude-implementer.settings.json"),
            Some("fixture-model"),
            Some("high"),
        );
        let roots = implementer_write_roots(&arguments, &implementer_settings(), worktree)
            .expect("the file tools are confined");
        assert_eq!(roots, [worktree.to_path_buf()]);
        assert!(write_permitted(&roots, &worktree.join("src/lib.rs")));
        for outside in [
            PathBuf::from("/etc/passwd"),
            PathBuf::from("/state/implementer-worktrees/other/src/lib.rs"),
            PathBuf::from("/state/implementer-runs/placeholder/claude-implementer.settings.json"),
            worktree.join("../other/escape.rs"),
            worktree.join("../../sprint.jsonl"),
        ] {
            assert!(
                !write_permitted(&roots, &outside),
                "{} must be refused",
                outside.display()
            );
        }

        let mut widened = arguments.clone();
        widened.extend(["--add-dir".to_owned(), "/".to_owned()]);
        let roots = implementer_write_roots(&widened, &implementer_settings(), worktree)
            .expect("still restricted");
        assert!(write_permitted(&roots, Path::new("/etc/passwd")));
        let mut allow_write = implementer_settings();
        allow_write["sandbox"]["filesystem"] = json!({"allowWrite": ["/etc"]});
        let roots =
            implementer_write_roots(&arguments, &allow_write, worktree).expect("still restricted");
        assert!(write_permitted(&roots, Path::new("/etc/passwd")));
        let unrestricted = arguments
            .iter()
            .filter(|argument| *argument != "--restricted")
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            implementer_write_roots(&unrestricted, &implementer_settings(), worktree),
            None
        );
    }

    /// #626: network is denied by default, matching Codex's `offline`.
    #[test]
    fn the_implementer_profile_denies_network_and_every_escape_from_the_sandbox() {
        let settings = implementer_settings();
        assert_eq!(settings["sandbox"]["enabled"], json!(true));
        assert_eq!(settings["sandbox"]["failIfUnavailable"], json!(true));
        assert_eq!(
            settings["sandbox"]["allowUnsandboxedCommands"],
            json!(false)
        );
        assert_eq!(settings["sandbox"]["network"]["allowedDomains"], json!([]));
        assert_eq!(
            settings["sandbox"]["network"]["strictAllowlist"],
            json!(true)
        );
        assert!(settings["sandbox"].get("excludedCommands").is_none());
        assert!(
            settings["sandbox"]["network"]
                .get("allowUnixSockets")
                .is_none()
        );
        assert!(
            settings["sandbox"]["network"]
                .get("allowAllUnixSockets")
                .is_none()
        );
        assert_eq!(
            settings["permissions"]["deny"],
            json!(["WebFetch", "WebSearch"])
        );
        assert!(settings["permissions"].get("allow").is_none());

        let arguments = implementer_arguments(Path::new("profile.json"), None, None);
        let pair = |flag: &str| {
            arguments
                .iter()
                .position(|argument| argument == flag)
                .and_then(|index| arguments.get(index + 1))
                .map(String::as_str)
        };
        for flag in ["--print", "--restricted", "--strict-mcp-config"] {
            assert!(arguments.iter().any(|argument| argument == flag), "{flag}");
        }
        assert_eq!(pair("--settings"), Some("profile.json"));
        assert_eq!(pair("--permission-mode"), Some("acceptEdits"));
        assert_eq!(pair("--permission-prompts"), Some("none"));
        assert_eq!(pair("--disallowed-tools"), Some("WebFetch,WebSearch"));
        let tools = pair("--tools")
            .expect("tools are named")
            .split(',')
            .collect::<Vec<_>>();
        assert!(!tools.contains(&"WebFetch") && !tools.contains(&"WebSearch"));
        assert!(!arguments.iter().any(|argument| {
            argument.contains("dangerously")
                || argument.contains("bypassPermissions")
                || argument.starts_with("--add-dir")
                || argument.starts_with("--mcp-config")
        }));
    }

    #[cfg(unix)]
    #[test]
    fn a_claude_implementer_runs_in_the_worktree_under_the_generated_profile() {
        let fixture = tempdir().expect("fixture directory");
        let root = fixture.path();
        fs::create_dir_all(root.join("worktree")).expect("create worktree");
        fs::create_dir_all(root.join("run")).expect("create run directory");
        fs::write(root.join("prompt.md"), "Implement the placeholder.\n").expect("write prompt");
        let claude = root.join("claude-stub");
        let observed = root.join("observed");
        stub(
            &claude,
            &format!(
                concat!(
                    "if [ \"$1\" = --version ]; then echo '2.1.283 (Claude Code)'; exit 0; fi\n",
                    "pwd >'{0}.cwd'\n",
                    "printf '%s\\n' \"$@\" >'{0}.argv'\n",
                    "cat >'{0}.stdin'\n",
                    "echo '{{\"type\":\"system\",\"subtype\":\"init\"}}'\n",
                    "echo '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"placeholder done\"}}'\n",
                ),
                observed.display()
            ),
        );
        let request = implementer_request(root, true);

        let outcome = ClaudeHarness::new(&claude, "claude-fixture-v1", "fixture-model")
            .run(&RunRequest::Implementer(request.clone()));

        assert!(
            outcome.status().is_some_and(|status| status.success()),
            "{outcome:?}"
        );
        let settings = root.join("run").join(CLAUDE_IMPLEMENTER_SETTINGS_FILE);
        let read = |suffix: &str| {
            fs::read_to_string(format!("{}.{suffix}", observed.display())).expect("observation")
        };
        assert_eq!(
            Path::new(read("cwd").trim()).canonicalize().expect("cwd"),
            root.join("worktree").canonicalize().expect("worktree")
        );
        assert_eq!(
            read("argv").lines().collect::<Vec<_>>(),
            implementer_arguments(&settings, Some("fixture-model-x"), Some("high"))
        );
        assert_eq!(read("stdin"), "Implement the placeholder.\n");
        let profile: Value = serde_json::from_str(
            &fs::read_to_string(&settings).expect("the profile is written beside the transcript"),
        )
        .expect("profile JSON");
        assert_eq!(profile, implementer_settings());
        assert_eq!(
            fs::read_to_string(&request.result).expect("result file"),
            "placeholder done"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_claude_release_older_than_the_profile_needs_is_refused_before_it_runs() {
        let fixture = tempdir().expect("fixture directory");
        let root = fixture.path();
        fs::create_dir_all(root.join("worktree")).expect("create worktree");
        fs::create_dir_all(root.join("run")).expect("create run directory");
        fs::write(root.join("prompt.md"), "placeholder\n").expect("write prompt");
        let claude = root.join("claude-stub");
        let ran = root.join("ran");
        stub(
            &claude,
            &format!(
                "if [ \"$1\" = --version ]; then echo '2.1.100 (Claude Code)'; exit 0; fi\ntouch '{}'",
                ran.display()
            ),
        );

        let outcome = ClaudeHarness::new(&claude, "claude-fixture-v1", "fixture-model")
            .run(&RunRequest::Implementer(implementer_request(root, true)));

        assert!(
            matches!(outcome, ProcessOutcome::Error(ref fault) if fault.name() == "runner_policy"),
            "{outcome:?}"
        );
        assert!(!ran.exists(), "an older release must never be started");
        assert_eq!(parse_version("2.1.283 (Claude Code)"), Some((2, 1, 283)));
        assert_eq!(parse_version("not a version"), None);
    }
}
