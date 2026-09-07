use std::{
    fs,
    path::PathBuf,
    process::{Command, Stdio},
};

use super::{
    ActionFault, AgentRunner, CapSupport, Harness, PASS_MAX_TURNS, ProcessOutcome, RunRequest,
    set_process_group,
};

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
        let RunRequest::Orchestrator(request) = request else {
            return ProcessOutcome::Error(ActionFault::new("runner_kind_mismatch", None));
        };
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
        set_process_group(&mut command);
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

    #[test]
    fn runner_kind_mismatch_is_reported_without_spawning() {
        let fixture = tempdir().expect("fixture directory");
        let request = RunRequest::Implementer(ImplementerRunRequest {
            prompt: fixture.path().join("prompt.md"),
            worktree: fixture.path().join("worktree"),
            result: fixture.path().join("result.md"),
            transcript: fixture.path().join("transcript.jsonl"),
            token_ceiling: 1,
            offline: true,
            signals: SignalFlags::default(),
            supervisor_pid: None,
            termination_grace: std::time::Duration::from_secs(1),
        });
        let outcome = ClaudeHarness::new("missing", "fixture-v1", "fixture-model").run(&request);
        assert!(matches!(
            outcome,
            ProcessOutcome::Error(ref fault) if fault.name() == "runner_kind_mismatch"
        ));
    }
}
