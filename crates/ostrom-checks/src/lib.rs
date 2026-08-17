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
mod process;
mod registry;

pub use command::CommandProvider;
pub use doctor::{DOCTOR_CHECKS, DoctorProvider};
pub use http::HttpProvider;
pub use judgment::{
    ClaudeHarness, HarnessRequest, JudgmentHarness, JudgmentOutcome, JudgmentRegistry,
    PreparedJudgment,
};
pub use registry::{
    ActionFault, ActionOutcome, ActionProvider, ActionRegistry, PreparedAction, PreparedCheck,
};
