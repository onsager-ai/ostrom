//! Process and harness runtime shared by Umwelt consumers.

pub mod agent;
pub mod command;
pub mod control;
mod environment;
pub mod follower;
pub mod loop_units;
pub mod operation_settings;
pub mod pass_state;
pub mod process;
pub mod process_control;
pub mod registry;
pub mod sink;
pub mod trace;
pub mod watchdog;

pub use agent::claude::ClaudeHarness;
pub use agent::{
    ActionFault, AgentRegistry, AgentRunner, CapSupport, CodexHarness, Harness,
    ImplementerRunRequest, LoopCeilings, OrchestratorRunRequest, ProcessOutcome, RunCaps,
    RunRequest, RunTermination, RunnerLaunch, SignalFlags,
};
pub use command::CommandProvider;
pub use control::{
    ClaudeSessionResumer, ControlError, ProcessExit, ResumeError, ResumedSession, RunControl,
    SessionResumer,
};
pub use follower::{
    FollowExit, FollowPoll, FollowState, FollowStatus, LIFETIME_CAP as FOLLOW_LIFETIME_CAP,
    POLL_INTERVAL as FOLLOW_POLL_INTERVAL, follow,
};
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
pub use sink::{FileSink, Sink, SinkFault, Source, SourceFault, run_directory_name};
pub use trace::{TraceAppend, TraceAppendError, TraceFactRecord, append_trace};
pub use watchdog::{
    Cap, CapMeasurement, CapTrip, CapsWatchdog, Clock, StartError, SystemClock, WatchdogError,
};
