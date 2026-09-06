use std::{path::PathBuf, time::Duration};

use crate::{ActionFault, CheckAction, process::invalid_parameters};

/// Prepares shell-backed check processes without assigning meaning to their output.
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

    pub fn prepare(
        &self,
        script: impl Into<String>,
        timeout: Duration,
    ) -> Result<CheckAction, ActionFault> {
        let script = script.into();
        if script.is_empty() || timeout.is_zero() {
            return Err(invalid_parameters());
        }
        let mut action = CheckAction::new(&self.shell, timeout);
        action.arguments = vec!["-c".into(), script.into()];
        Ok(action)
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::Duration};

    use tempfile::tempdir;

    use super::CommandProvider;
    use crate::execute_check_action;

    fn execute(script: &str, provider: CommandProvider) -> crate::CheckReceipt {
        let action = provider
            .prepare(script, Duration::from_secs(1))
            .expect("prepared command");
        execute_check_action(&action)
    }

    #[test]
    fn exit_one_is_captured_verbatim() {
        let receipt = execute("exit 1", CommandProvider::default());
        assert_eq!(receipt.status.and_then(|status| status.code()), Some(1));
        assert_eq!(receipt.fault, None);
    }

    #[test]
    fn non_predicate_exit_is_captured_verbatim() {
        let receipt = execute("exit 7", CommandProvider::default());
        assert_eq!(receipt.status.and_then(|status| status.code()), Some(7));
        assert_eq!(receipt.fault, None);
    }

    #[test]
    fn syntax_error_remains_captured_output() {
        let receipt = execute("python3 -c 'if'", CommandProvider::default());
        assert!(receipt.status.is_some());
        assert!(String::from_utf8_lossy(&receipt.stderr).contains("SyntaxError"));
        assert_eq!(receipt.fault, None);
    }

    #[test]
    fn runtime_crash_remains_captured_output() {
        let receipt = execute(
            "python3 -c 'raise RuntimeError(\"placeholder crash\")'",
            CommandProvider::default(),
        );
        assert!(receipt.status.is_some());
        assert!(String::from_utf8_lossy(&receipt.stderr).contains("RuntimeError"));
        assert_eq!(receipt.fault, None);
    }

    #[test]
    fn missing_script_command_remains_a_shell_exit() {
        let receipt = execute("missing-fixture-command", CommandProvider::default());
        assert_eq!(receipt.status.and_then(|status| status.code()), Some(127));
        assert_eq!(receipt.fault, None);
    }

    #[test]
    fn absent_shell_is_a_spawn_fault() {
        let fixture = tempdir().expect("fixture directory");
        let absent_shell = PathBuf::from(fixture.path()).join("absent-shell");
        let receipt = execute("exit 0", CommandProvider::with_shell(absent_shell));
        assert_eq!(
            receipt.fault.as_ref().map(crate::ActionFault::name),
            Some("check_spawn_failed")
        );
    }

    #[test]
    fn command_timeout_is_a_capture_fault() {
        let action = CommandProvider::default()
            .prepare("sleep 1", Duration::from_millis(10))
            .expect("prepared command");
        let receipt = execute_check_action(&action);
        assert_eq!(
            receipt.fault.as_ref().map(crate::ActionFault::name),
            Some("check_timed_out")
        );
    }
}
