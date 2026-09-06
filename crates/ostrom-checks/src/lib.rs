//! Exact mechanical-action and judged-harness registration.
//!
//! Providers own a whole domain, enumerate its verbs, and turn an opaque
//! authored `with` map into a prepared action. The registration API exposes
//! no check basis: `agent` remains the only judged domain and is reserved.
//! Judged execution uses the separate [`JudgmentRegistry`], preserving that
//! reservation while resolving `agent/*` verbs as harness names.

mod doctor;
mod github;
mod http;
mod judgment;
pub mod umwelt_edge;

pub use doctor::{
    DOCTOR_CHECKS, DoctorOptions, DoctorProvider, DoctorResult, DoctorStatus, run_doctor,
    run_doctor_check,
};
pub use github::GitHubProvider;
pub use http::HttpProvider;
pub use judgment::{
    HarnessRequest, JudgmentHarness, JudgmentOutcome, JudgmentRegistry, PreparedJudgment,
};
pub use umwelt_edge::{
    ActionOutcome, ActionProvider, ActionRegistry, ClaudeJudgmentHarness, LoopUnitError,
    OperationSettingsError, PreparedAction, PreparedCheck, check_loop_units_drift,
    check_operation_settings_drift, generate_loop_units, generate_operation_settings,
    loop_execstart_is_not_shell, render_loop_units, resolved_operation_settings,
};
pub use umwelt_runtime::{
    ActionFault, AgentRegistry, AgentRunner, CeilingEnvironmentNames, CheckAction, CheckReceipt,
    ClaudeHarness, CodexHarness, CommandProvider, Harness, HarnessProfile, ImplementerRunRequest,
    LoopUnit, LoopUnitDeclaration, LoopUnitDrift, LoopUnitGeneratorConfig, OperationSettingsDrift,
    OrchestratorRunRequest, ResolvedOperationSettings, RunCeilings, RunOutcome, RunRequest,
    RunTermination, RunnerLaunch,
};
