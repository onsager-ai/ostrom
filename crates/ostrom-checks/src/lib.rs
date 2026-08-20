//! Exact mechanical-action and judged-harness registration.
//!
//! Providers own a whole domain, enumerate its verbs, and turn an opaque
//! authored `with` map into a prepared action. The registration API exposes
//! no check basis: `agent` remains the only judged domain and is reserved.
//! Judged execution uses the separate [`JudgmentRegistry`], preserving that
//! reservation while resolving `agent/*` verbs as harness names.

mod command;
mod doctor;
mod http;
mod judgment;
mod plugin_surface;
mod process;
mod registry;
mod required_check_selectors;
mod role_allowlists;
mod shell_retirement;
mod skill_version_bump;

pub use command::CommandProvider;
pub use doctor::{
    DOCTOR_CHECKS, DoctorOptions, DoctorProvider, DoctorResult, DoctorStatus, run_doctor,
    run_doctor_check,
};
pub use http::HttpProvider;
pub use judgment::{
    ClaudeHarness, HarnessRequest, JudgmentHarness, JudgmentOutcome, JudgmentRegistry,
    PreparedJudgment,
};
pub use plugin_surface::{PluginSurfaceReport, PluginSurfaceViolation, check_plugin_surface};
pub use registry::{
    ActionFault, ActionOutcome, ActionProvider, ActionRegistry, PreparedAction, PreparedCheck,
};
pub use required_check_selectors::{
    RequiredCheckSelectorReport, RequiredCheckSelectorViolation, check_required_check_selectors,
    resolve_repository_name,
};
pub use role_allowlists::{
    RoleAllowlistReport, RoleAllowlistViolation, check_modeled_role_allowlists,
    check_role_allowlists,
};
pub use shell_retirement::{ShellFile, ShellRetirementReport, check_shell_retirement};
pub use skill_version_bump::{
    SkillVersionBumpError, SkillVersionBumpReport, VersionBumpViolation, check_skill_version_bump,
};
