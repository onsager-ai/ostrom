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
        if let Some(home) = env::var_os("OSTROM_HOME") {
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
