//! Filesystem implementation and compatibility readers.

mod app_token;
mod check_store;
mod clock;
mod dispatch;
pub mod environment;
mod event_store;
mod file_store;
mod gate;
mod hooks;
mod implement;
mod lease;
mod leaves;
mod migration;
mod parity;
mod pass;
mod pass_state;
mod paths;
mod plan;
mod policy;
mod policy_signature;
mod publish;
mod queue;
mod repair;
mod replay;
mod selection;
mod selector;
mod sweep;
mod trace;
mod work_order;

pub use app_token::{AppTokenError, CredentialCommandError, credential_output};
pub use check_store::JsonlCheckStore;
pub use clock::Clock;
pub use dispatch::{DispatchError, DispatchOutcome, DispatchRequest, run_dispatch};
pub use environment::{ENVIRONMENT_VARIABLES, EnvironmentClass, EnvironmentVariable};
pub use event_store::JsonlEventStore;
pub use file_store::JsonlSweepStore;
pub use gate::{GateError, GateOptions, GateOutput, run_gate};
pub use hooks::{DigestOptions, HookOutput, render_constitution, render_digest};
pub use implement::{ImplementError, ImplementRequest, run_implement};
pub use lease::{
    LeaseActionError, LeaseRecord, OwnedLease, acquire_lease, lease_status, read_lease,
    release_lease, validate_lease_name, write_lease,
};
pub use leaves::{
    AuditError, AuditOptions, ExcuseError, LocalDriftError, audit, grant_excuse, list_excuses,
    local_drift,
};
pub use migration::{MigrationOutcome, migrate};
pub use parity::{ParityError, SweepParityOptions, SweepParityOutcome, run_sweep_parity};
pub use pass::{PassError, PassRequest, PassRole, SignalFlags, run_pass};
pub use pass_state::{PassState, read_pass_state, write_pass_state};
pub use paths::OstromPaths;
pub use plan::{
    AssessmentDeriver, AssessmentDeriverError, AssessmentHarness, AssessmentInput,
    ExecutableAssessmentDeriver, GoalPlan, HarnessAssessmentDeriver, PlanDocument, PlanError,
    PlanFault, PlanOptions, PlanRanking, PlanSweep, UnavailableAssessmentDeriver, run_plan,
};
pub use policy::{
    PolicyBundle, PolicyExplanation, RequirementExplanation, RuleExplanation, SelectorProjection,
};
pub use policy_signature::{PolicySignatureError, sign_policy_manifest, verify_policy_manifest};
pub use publish::{PublishDestination, PublishError};
pub use queue::{
    QueueActionError, QueueDecision, QueueDocument, decide_queue_item, lint_queue_state,
    list_queue_json, read_queue, write_queue,
};
pub use repair::{RepairOptions, RepairOutput, run_repair_prs};
pub use replay::{ReplayError, ReplayOptions, replay};
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
    MalformedTraceRow, TraceActionError, TraceAppend, TraceFactRecord, TraceRead, TraceView,
    append_trace, append_trace_checked, read_trace, read_trace_json,
};
pub use work_order::{
    ClearedWorkOrder, CreatedWorkOrder, WorkOrderError, branch_name, clear_work_order,
    create_work_order, finalize_exited_implementer, item_hash, validate_work_order_file,
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
    #[error("event store: {0}")]
    Event(#[from] ostrom_core::EventStoreFault),
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
