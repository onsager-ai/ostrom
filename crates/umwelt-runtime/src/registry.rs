use std::{
    ffi::OsString,
    path::PathBuf,
    process::{Command, ExitStatus},
    time::{Duration, SystemTime},
};

use chrono::{DateTime, Utc};

use crate::{
    ActionFault,
    process::{ProcessResult, run_bounded},
};

/// A fully resolved check process. It contains only what the harness needs to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckAction {
    pub command: PathBuf,
    pub arguments: Vec<OsString>,
    pub environment: Vec<(OsString, OsString)>,
    pub working_directory: Option<PathBuf>,
    pub timeout: Duration,
}

impl CheckAction {
    #[must_use]
    pub fn new(command: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self {
            command: command.into(),
            arguments: Vec::new(),
            environment: Vec::new(),
            working_directory: None,
            timeout,
        }
    }
}

/// Captured facts from one check process execution.
///
/// An Umwelt receipt deliberately does not say what the exit status means.
#[derive(Debug, PartialEq, Eq)]
pub struct CheckReceipt {
    pub status: Option<ExitStatus>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub fault: Option<ActionFault>,
}

#[must_use]
pub fn execute_check_action(action: &CheckAction) -> CheckReceipt {
    let started_at = DateTime::<Utc>::from(SystemTime::now());
    let mut command = Command::new(&action.command);
    command
        .args(&action.arguments)
        .envs(action.environment.iter().map(|(name, value)| (name, value)));
    if let Some(working_directory) = &action.working_directory {
        command.current_dir(working_directory);
    }

    let result = run_bounded(&mut command, action.timeout);
    let completed_at = DateTime::<Utc>::from(SystemTime::now());
    match result {
        ProcessResult::Completed {
            status,
            stdout,
            stderr,
        } => CheckReceipt {
            status: Some(status),
            stdout,
            stderr,
            started_at,
            completed_at,
            fault: None,
        },
        ProcessResult::SpawnFailed => fault_receipt(
            started_at,
            completed_at,
            ActionFault::new("check_spawn_failed", None),
        ),
        ProcessResult::WaitFailed => fault_receipt(
            started_at,
            completed_at,
            ActionFault::new("check_wait_failed", None),
        ),
        ProcessResult::TimedOut => fault_receipt(
            started_at,
            completed_at,
            ActionFault::new("check_timed_out", None),
        ),
    }
}

fn fault_receipt(
    started_at: DateTime<Utc>,
    completed_at: DateTime<Utc>,
    fault: ActionFault,
) -> CheckReceipt {
    CheckReceipt {
        status: None,
        stdout: Vec::new(),
        stderr: Vec::new(),
        started_at,
        completed_at,
        fault: Some(fault),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tempfile::tempdir;

    use super::*;

    #[cfg(unix)]
    #[test]
    fn resolved_action_captures_process_facts_without_interpreting_them() {
        let root = tempdir().expect("fixture directory");
        let mut action = CheckAction::new("sh", Duration::from_secs(1));
        action.arguments = vec![
            "-c".into(),
            "printf '%s:%s' \"$FIXTURE\" \"$(pwd)\"; printf 'problem' >&2; exit 7".into(),
        ];
        action.environment = vec![("FIXTURE".into(), "observed".into())];
        action.working_directory = Some(root.path().to_path_buf());

        let receipt = execute_check_action(&action);

        assert_eq!(receipt.status.and_then(|status| status.code()), Some(7));
        assert_eq!(
            String::from_utf8(receipt.stdout).expect("stdout text"),
            format!("observed:{}", root.path().display())
        );
        assert_eq!(receipt.stderr, b"problem");
        assert_eq!(receipt.fault, None);
        assert!(receipt.completed_at >= receipt.started_at);
    }

    #[test]
    fn unspawnable_action_is_a_capture_fault() {
        let root = tempdir().expect("fixture directory");
        let action = CheckAction::new(root.path().join("absent"), Duration::from_secs(1));

        let receipt = execute_check_action(&action);

        assert_eq!(
            receipt.fault.as_ref().map(ActionFault::name),
            Some("check_spawn_failed")
        );
        assert!(receipt.status.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn timeout_is_recorded_as_a_capture_fault() {
        let mut action = CheckAction::new("sh", Duration::from_millis(10));
        action.arguments = vec!["-c".into(), "sleep 1".into()];

        let receipt = execute_check_action(&action);

        assert_eq!(
            receipt.fault.as_ref().map(ActionFault::name),
            Some("check_timed_out")
        );
        assert!(receipt.status.is_none());
    }
}
