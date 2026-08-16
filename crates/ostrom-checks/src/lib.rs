//! Exact mechanical-action registration and the built-in check providers.
//!
//! Providers own a whole domain, enumerate its verbs, and turn an opaque
//! authored `with` map into a prepared action. The registration API exposes
//! no check basis: `agent` remains the only judged domain and is reserved.

mod command;
mod doctor;
mod http;
mod process;
mod registry;

pub use command::CommandProvider;
pub use doctor::{DOCTOR_CHECKS, DoctorProvider};
pub use http::HttpProvider;
pub use registry::{
    ActionFault, ActionOutcome, ActionProvider, ActionRegistry, PreparedAction, PreparedCheck,
};
