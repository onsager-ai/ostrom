use std::path::PathBuf;

use directories::ProjectDirs;

use crate::{StoreError, environment};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OstromPaths {
    pub config: PathBuf,
    pub state: PathBuf,
}

impl OstromPaths {
    /// Resolve the XDG locations. `OSTROM_HOME` intentionally collapses both
    /// roots to one explicit directory: test processes cannot accidentally
    /// fall through to an operator's home if their fixture is incomplete.
    pub fn resolve() -> Result<Self, StoreError> {
        if let Some(home) = environment::OSTROM_HOME
            .value_os()
            .filter(|home| !home.to_string_lossy().trim().is_empty())
        {
            let home = PathBuf::from(home);
            return Ok(Self {
                config: home.clone(),
                state: home,
            });
        }
        let dirs =
            ProjectDirs::from("ai", "onsager", "ostrom").ok_or(StoreError::PathsUnavailable)?;
        let state = dirs
            .state_dir()
            .ok_or(StoreError::PathsUnavailable)?
            .to_path_buf();
        Ok(Self {
            config: dirs.config_dir().to_path_buf(),
            state,
        })
    }

    #[must_use]
    pub fn queue_file(&self) -> PathBuf {
        self.state.join("queue.jsonl")
    }

    #[must_use]
    pub fn trace_file(&self) -> PathBuf {
        self.state.join("sprint.jsonl")
    }

    /// Immutable merge facts observed across every sweep generation.
    #[must_use]
    pub fn merge_file(&self) -> PathBuf {
        self.state.join("merge.jsonl")
    }

    #[must_use]
    pub fn sweep_state_file(&self) -> PathBuf {
        self.state.join("state.json")
    }

    #[must_use]
    pub fn previous_sweep_dir(&self) -> PathBuf {
        self.state.join("previous")
    }

    #[must_use]
    pub fn selector_events_file(&self) -> PathBuf {
        self.state.join("selector-events.jsonl")
    }

    #[must_use]
    pub fn work_orders_dir(&self) -> PathBuf {
        self.state.join("work-orders")
    }

    #[must_use]
    pub fn sweep_journal_file(&self) -> PathBuf {
        self.state.join("sweep-passes.jsonl")
    }

    #[must_use]
    pub fn check_journal_file(&self) -> PathBuf {
        self.state.join("check-runs.jsonl")
    }

    #[must_use]
    pub fn event_journal_file(&self) -> PathBuf {
        self.state.join("events.jsonl")
    }

    /// Credentials stay outside layered policy configuration. The explicit
    /// override is needed by isolated runtimes that cannot place a secret in
    /// the config root, and an empty override retains the established default.
    #[must_use]
    pub fn secrets_file(&self) -> PathBuf {
        resolve_secrets_file(&self.config, environment::MANDATE_SECRETS_FILE.value_os())
    }
}

fn resolve_secrets_file(
    config: &std::path::Path,
    override_path: Option<std::ffi::OsString>,
) -> PathBuf {
    override_path
        .filter(|path| !path.is_empty())
        .map_or_else(|| config.join("secrets.yaml"), PathBuf::from)
}

#[cfg(test)]
mod tests {
    use std::{env, path::PathBuf, process::Command};

    use super::{OstromPaths, resolve_secrets_file};

    const EMPTY_HOME_CHILD: &str = "OSTROM_TEST_EMPTY_HOME_CHILD";
    const SECRETS_OVERRIDE_CHILD: &str = "OSTROM_TEST_SECRETS_OVERRIDE_CHILD";

    #[test]
    fn empty_ostrom_home_does_not_resolve_relative_path() {
        if env::var_os(EMPTY_HOME_CHILD).is_some() {
            let paths = OstromPaths::resolve().expect("resolve fallback paths");
            assert!(paths.config.is_absolute());
            assert!(paths.state.is_absolute());
            return;
        }

        let status = Command::new(env::current_exe().expect("current test executable"))
            .env("OSTROM_HOME", "")
            .env(EMPTY_HOME_CHILD, "1")
            .args([
                "--exact",
                "paths::tests::empty_ostrom_home_does_not_resolve_relative_path",
                "--nocapture",
            ])
            .status()
            .expect("run empty OSTROM_HOME test subprocess");
        assert!(status.success());
    }

    #[test]
    fn secrets_file_honors_environment_override_and_defaults_to_config() {
        if let Some(expected) = env::var_os(SECRETS_OVERRIDE_CHILD) {
            let paths = OstromPaths::resolve().expect("resolve explicit test paths");
            assert_eq!(paths.secrets_file(), PathBuf::from(expected));
            return;
        }

        let fixture = tempfile::tempdir().expect("temporary OSTROM_HOME");
        let config = fixture.path().join("config");
        assert_eq!(
            resolve_secrets_file(&config, None),
            config.join("secrets.yaml")
        );

        let override_path = fixture.path().join("isolated/secrets.yaml");
        let status = Command::new(env::current_exe().expect("current test executable"))
            .env("OSTROM_HOME", fixture.path())
            .env("MANDATE_SECRETS_FILE", &override_path)
            .env(SECRETS_OVERRIDE_CHILD, &override_path)
            .args([
                "--exact",
                "paths::tests::secrets_file_honors_environment_override_and_defaults_to_config",
                "--nocapture",
            ])
            .status()
            .expect("run secrets override test subprocess");
        assert!(status.success());
    }
}
