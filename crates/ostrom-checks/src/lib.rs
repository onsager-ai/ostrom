//! Exact mechanical-action and judged-harness registration.
//!
//! Providers own a whole domain, enumerate its verbs, and turn an opaque
//! authored `with` map into a prepared action. The registration API exposes
//! no check basis: `agent` remains the only judged domain and is reserved.
//! Judged execution uses the separate [`JudgmentRegistry`], preserving that
//! reservation while resolving `agent/*` verbs as harness names.

mod command;
mod doctor;
mod github;
mod http;
mod judgment;
mod loop_units;
mod operation_settings;
mod plugin_surface;
mod process;
mod registry;
mod role_allowlists;
mod shell_retirement;
mod skill_version_bump;

pub use command::CommandProvider;
pub use doctor::{
    DOCTOR_CHECKS, DoctorOptions, DoctorProvider, DoctorResult, DoctorStatus, run_doctor,
    run_doctor_check,
};
pub use github::GitHubProvider;
pub use http::HttpProvider;
pub use judgment::{
    ClaudeHarness, HarnessRequest, JudgmentHarness, JudgmentOutcome, JudgmentRegistry,
    PreparedJudgment,
};
pub use loop_units::{
    LoopUnit, LoopUnitDrift, LoopUnitError, check_loop_units_drift, generate_loop_units,
    loop_execstart_is_not_shell, render_loop_units,
};
pub use operation_settings::{
    OperationSettingsDrift, OperationSettingsError, RoleSkillOperations, SkillOperationViolation,
    check_operation_settings_drift, check_skill_operation_grants, generate_operation_settings,
};
pub use ostrom_store::{
    ActionFault, AgentRegistry, AgentRunner, CodexHarness, Harness, ImplementerRunRequest,
    OrchestratorRunRequest, RunOutcome, RunRequest, RunTermination, RunnerLaunch,
};
pub use plugin_surface::{PluginSurfaceReport, PluginSurfaceViolation, check_plugin_surface};
pub use registry::{ActionOutcome, ActionProvider, ActionRegistry, PreparedAction, PreparedCheck};
pub use role_allowlists::{
    RoleAllowlistReport, RoleAllowlistViolation, check_modeled_role_allowlists,
    check_role_allowlists,
};
pub use shell_retirement::{ShellFile, ShellRetirementReport, check_shell_retirement};
pub use skill_version_bump::{
    SkillVersionBumpError, SkillVersionBumpReport, VersionBumpViolation, check_skill_version_bump,
};
