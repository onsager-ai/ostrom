//! Process and harness runtime shared by Umwelt consumers.

pub mod agent;
pub mod command;
mod environment;
pub mod loop_units;
pub mod operation_settings;
pub mod pass_state;
pub mod process;
mod process_control;
pub mod registry;
pub mod trace;

pub use agent::claude::ClaudeHarness;
pub use agent::{
    ActionFault, AgentRegistry, AgentRunner, CodexHarness, Harness, ImplementerRunRequest,
    OrchestratorRunRequest, RunCeilings, RunOutcome, RunRequest, RunTermination, RunnerLaunch,
    SignalFlags,
};
pub use command::CommandProvider;
pub use loop_units::{
    CeilingEnvironmentNames, LoopUnit, LoopUnitDeclaration, LoopUnitDrift, LoopUnitError,
    LoopUnitGeneratorConfig, check_loop_units_drift, generate_loop_units,
    loop_execstart_is_not_shell, render_loop_units,
};
pub use operation_settings::{
    HarnessProfile, OperationSettingsDrift, OperationSettingsError, ResolvedOperationSettings,
    check_operation_settings_drift, generate_operation_settings,
};
pub use pass_state::{PassState, PassStateError, read_pass_state, write_pass_state};
pub use registry::{CheckAction, CheckReceipt, execute_check_action};
pub use trace::{TraceAppend, TraceAppendError, TraceFactRecord, append_trace};
