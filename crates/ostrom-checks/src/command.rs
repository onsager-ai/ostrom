use std::{collections::BTreeMap, path::PathBuf, process::Command};

use ostrom_core::ActionDefinition;
use serde_json::{Value, json};

use crate::{
    ActionFault, ActionOutcome, ActionProvider, PreparedAction,
    process::{ProcessResult, exact_keys, invalid_parameters, parameter_timeout, run_bounded},
};

pub struct CommandProvider {
    shell: PathBuf,
}

impl Default for CommandProvider {
    fn default() -> Self {
        Self {
            shell: PathBuf::from("sh"),
        }
    }
}

impl CommandProvider {
    #[must_use]
    pub fn with_shell(shell: impl Into<PathBuf>) -> Self {
        Self {
            shell: shell.into(),
        }
    }
}

impl ActionProvider for CommandProvider {
    fn domain(&self) -> &'static str {
        "cmd"
    }

    fn verbs(&self) -> &'static [&'static str] {
        &["run"]
    }

    fn action_definition(&self, verb: &str) -> Option<ActionDefinition> {
        (verb == "run").then(|| ActionDefinition {
            uses: "cmd/run".to_owned(),
            producer: "ostrom-cmd".to_owned(),
            default_fresh_for_seconds: 300,
            definition: json!({
                "parameters": ["script", "timeout"],
                "timeout_default": "30s",
                "exit": "zero passes; non-zero fails"
            }),
            source_revision: "cmd-run-v1".to_owned(),
        })
    }

    fn prepare(
        &self,
        verb: &str,
        parameters: &BTreeMap<String, Value>,
    ) -> Result<Box<dyn PreparedAction>, ActionFault> {
        if verb != "run" || !exact_keys(parameters, &["script", "timeout"]) {
            return Err(invalid_parameters());
        }
        let script = parameters
            .get("script")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(invalid_parameters)?;
        let timeout = parameter_timeout(parameters.get("timeout"))?;
        Ok(Box::new(CommandAction {
            shell: self.shell.clone(),
            script: script.to_owned(),
            timeout,
        }))
    }
}

struct CommandAction {
    shell: PathBuf,
    script: String,
    timeout: std::time::Duration,
}

impl PreparedAction for CommandAction {
    fn execute(&self) -> ActionOutcome {
        let mut command = Command::new(&self.shell);
        command.arg("-c").arg(&self.script);
        match run_bounded(&mut command, self.timeout) {
            ProcessResult::Completed { status, .. } if status.success() => ActionOutcome::Pass,
            ProcessResult::Completed { status, stderr, .. }
                if status.code().is_none()
                    || matches!(status.code(), Some(126 | 127))
                    || String::from_utf8_lossy(&stderr).contains("SyntaxError") =>
            {
                ActionOutcome::Inconclusive(ActionFault::new("cmd_harness_error", None))
            }
            ProcessResult::Completed { .. } => ActionOutcome::Fail,
            ProcessResult::TimedOut => {
                ActionOutcome::Inconclusive(ActionFault::new("cmd_timeout", None))
            }
            ProcessResult::SpawnFailed | ProcessResult::WaitFailed => {
                ActionOutcome::Inconclusive(ActionFault::new("cmd_execute_error", None))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ostrom_core::{Catalogue, CatalogueEnumeration, CheckDocument, CheckVerdict};
    use tempfile::tempdir;

    use super::CommandProvider;
    use crate::ActionRegistry;

    fn execute(script: &str, provider: CommandProvider) -> ostrom_core::CheckReceipt {
        let yaml = format!(
            "checks_version: 1\nchecks:\n  command:\n    uses: cmd/run\n    with:\n      script: {script:?}\n      timeout: 1s\n"
        );
        let enumeration = CatalogueEnumeration {
            catalogues: vec![Catalogue {
                document: CheckDocument::from_yaml(&yaml).expect("command fixture"),
            }],
            complete: true,
        };
        let mut registry = ActionRegistry::new();
        registry.register(provider).expect("command provider");
        registry
            .prepare("command", &enumeration)
            .expect("prepared command")
            .execute("command-attempt")
    }

    #[test]
    fn an_explicit_nonzero_claim_is_a_fail() {
        let receipt = execute("exit 7", CommandProvider::default());
        assert_eq!(receipt.verdict, Some(CheckVerdict::Fail));
        assert_eq!(receipt.error, None);
    }

    #[test]
    fn the_loop_not_burning_syntax_error_is_inconclusive() {
        let receipt = execute("python3 -c 'if'", CommandProvider::default());
        assert_eq!(receipt.verdict, Some(CheckVerdict::Inconclusive));
        assert_eq!(receipt.error, None);
    }

    #[test]
    fn a_missing_script_command_is_inconclusive() {
        let receipt = execute("missing-fixture-command", CommandProvider::default());
        assert_eq!(receipt.verdict, Some(CheckVerdict::Inconclusive));
        assert_eq!(receipt.error, None);
    }

    #[test]
    fn a_command_that_cannot_be_executed_is_an_error() {
        let fixture = tempdir().expect("fixture directory");
        let absent_shell = PathBuf::from(fixture.path()).join("absent-shell");
        let receipt = execute("exit 0", CommandProvider::with_shell(absent_shell));
        assert_eq!(receipt.verdict, Some(CheckVerdict::Inconclusive));
        assert_eq!(receipt.error, None);
    }

    #[test]
    fn command_timeout_is_an_error() {
        let yaml = "checks_version: 1\nchecks:\n  command:\n    uses: cmd/run\n    with:\n      script: sleep 1\n      timeout: 10ms\n";
        let enumeration = CatalogueEnumeration {
            catalogues: vec![Catalogue {
                document: CheckDocument::from_yaml(yaml).expect("timeout fixture"),
            }],
            complete: true,
        };
        let mut registry = ActionRegistry::new();
        registry
            .register(CommandProvider::default())
            .expect("command provider");
        let receipt = registry
            .prepare("command", &enumeration)
            .expect("prepared command")
            .execute("timeout-attempt");
        assert_eq!(receipt.verdict, Some(CheckVerdict::Inconclusive));
        assert_eq!(receipt.error, None);
    }
}
