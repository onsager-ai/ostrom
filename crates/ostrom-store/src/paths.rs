use std::{env, path::PathBuf};

use directories::ProjectDirs;

use crate::StoreError;

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
        if let Some(home) =
            env::var_os("OSTROM_HOME").filter(|home| !home.to_string_lossy().trim().is_empty())
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
}

#[cfg(test)]
mod tests {
    use std::{env, process::Command};

    use super::OstromPaths;

    const EMPTY_HOME_CHILD: &str = "OSTROM_TEST_EMPTY_HOME_CHILD";

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
}
