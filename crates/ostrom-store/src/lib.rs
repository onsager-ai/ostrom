//! Filesystem implementation and compatibility readers.

mod app_token;
mod check_store;
mod dispatch;
mod file_store;
mod lease;
mod leaves;
mod migration;
mod parity;
mod pass_state;
mod paths;
mod plan;
mod queue;
mod selection;
mod sweep;
mod trace;

pub use app_token::AppTokenError;
pub use check_store::JsonlCheckStore;
pub use dispatch::{DispatchError, DispatchOutcome, DispatchRequest, run_dispatch};
pub use file_store::JsonlSweepStore;
pub use lease::{LeaseRecord, read_lease, write_lease};
pub use leaves::{
    AuditError, AuditOptions, ExcuseError, LocalDriftError, audit, grant_excuse, list_excuses,
    local_drift,
};
pub use migration::{MigrationOutcome, migrate};
pub use parity::{ParityError, SweepParityOptions, SweepParityOutcome, run_sweep_parity};
pub use pass_state::{PassState, read_pass_state, write_pass_state};
pub use paths::OstromPaths;
pub use plan::{
    AssessmentDeriver, AssessmentInput, ExecutableAssessmentDeriver, GoalPlan, PlanDocument,
    PlanError, PlanFault, PlanOptions, PlanRanking, PlanSweep, UnavailableAssessmentDeriver,
    run_plan,
};
pub use queue::{QueueDocument, list_queue_json, read_queue, write_queue};
pub use selection::{
    PlanApplication, SelectAction, SelectError, SelectOutcome, SelectRequest, encode_selection,
    run_selection,
};
pub use sweep::{
    PublishTarget, RepositorySnapshot, SweepError, SweepFixture, SweepMode, SweepOptions,
    SweepOutcome, acquire_org_from_github, encode_org_snapshots, load_config,
    load_config_or_defaults, run_sweep,
};
pub use trace::{
    MalformedTraceRow, TraceAppend, TraceFactRecord, TraceRead, append_trace, read_trace,
};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("could not resolve Ostrom directories")]
    PathsUnavailable,
    #[error("{operation} failed for {path}: {source}")]
    Io {
        operation: &'static str,
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("malformed queue row {line}: {message}")]
    MalformedQueue { line: usize, message: String },
    #[error("malformed trace record: {message}")]
    MalformedTrace { message: String },
    #[error("malformed lease {name}: {message}")]
    MalformedLease { name: String, message: String },
    #[error("malformed pass state for {role}: {message}")]
    MalformedPassState { role: String, message: String },
    #[error("trace record is {bytes} bytes; maximum is 4096")]
    TraceTooLarge { bytes: usize },
    #[error("migration refused: held lease {name} owned by {owner}")]
    LeaseHeld { name: String, owner: String },
    #[error("migration source and destination overlap: {0}")]
    MigrationOverlap(String),
    #[error("migration destination already contains different data: {0}")]
    MigrationConflict(String),
    #[error("could not parse secrets.yaml during key migration: {0}")]
    Secrets(String),
}

pub(crate) fn io_error(
    operation: &'static str,
    path: &std::path::Path,
    source: std::io::Error,
) -> StoreError {
    StoreError::Io {
        operation,
        path: path.display().to_string(),
        source,
    }
}

#[cfg(unix)]
pub(crate) fn set_private_file_mode(path: &std::path::Path) -> Result<(), StoreError> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|error| io_error("set private file mode", path, error))
}

#[cfg(not(unix))]
pub(crate) fn set_private_file_mode(_path: &std::path::Path) -> Result<(), StoreError> {
    Ok(())
}
