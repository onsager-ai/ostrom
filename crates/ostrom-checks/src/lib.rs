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
mod protocol;
mod registry;
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
pub use protocol::{
    EMBEDDED_ASSETS, EmbeddedAsset, InstallEntry, InstallReport, InstallStatus, ProtocolError,
    VerificationEntry, VerificationReport, VerificationStatus, install as install_protocol,
    resolve_harness_root, resolve_harness_root_from, verify as verify_protocol,
};
pub use registry::{
    ActionFault, ActionOutcome, ActionProvider, ActionRegistry, PreparedAction, PreparedCheck,
};
pub use shell_retirement::{ShellFile, ShellRetirementReport, check_shell_retirement};
pub use skill_version_bump::{
    SkillVersionBumpError, SkillVersionBumpReport, VersionBumpViolation, check_skill_version_bump,
};
