//! Interrupt and queued-steer control for one harness run.

use std::{
    collections::VecDeque,
    fs::OpenOptions,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::Duration,
};

use ethogram::{
    CONTROL_APPLIED, CONTROL_REQUESTED, ControlAppliedPayload, ControlAppliedReason, ControlKind,
    ControlRequestedPayload, EventDraft, MAX_EXCERPT_SCALARS, PayloadExtension, RUN_FINISHED,
    RunFinishedPayload, excerpt, validate,
};
use serde_json::Value;
use thiserror::Error;

use crate::{
    agent::{PASS_MAX_TURNS, RunCaps},
    process_control,
    sink::{Sink, SinkFault},
    watchdog::{CapsWatchdog, Clock},
};

/// Failure to start a harness session resume.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum ResumeError {
    /// This harness cannot resume a session headlessly.
    #[error("the harness cannot resume headlessly")]
    Unsupported,
    /// The harness refused or failed to start the resume.
    #[error("{0}")]
    Harness(String),
}

/// Starts the next headless turn of an existing harness session.
pub trait SessionResumer: Send + Sync {
    /// Start `text` as the next user turn of `session_id`.
    fn resume(&self, session_id: &str, text: &str) -> Result<ResumedSession, ResumeError>;
}

/// Proof that a resumed harness process has started.
#[derive(Debug)]
pub struct ResumedSession {
    child: Option<Child>,
}

impl ResumedSession {
    fn child(child: Child) -> Self {
        Self { child: Some(child) }
    }

    /// Record a successful start managed outside this process.
    #[must_use]
    pub const fn externally_managed() -> Self {
        Self { child: None }
    }
}

/// Claude Code's headless session resumer.
#[derive(Debug)]
pub struct ClaudeSessionResumer {
    executable: PathBuf,
    profile: PathBuf,
    permission_mode: String,
    transcript: PathBuf,
}

impl ClaudeSessionResumer {
    #[must_use]
    pub fn new(
        executable: impl Into<PathBuf>,
        profile: impl Into<PathBuf>,
        permission_mode: impl Into<String>,
        transcript: impl Into<PathBuf>,
    ) -> Self {
        Self {
            executable: executable.into(),
            profile: profile.into(),
            permission_mode: permission_mode.into(),
            transcript: transcript.into(),
        }
    }

    fn arguments(&self, session_id: &str, text: &str) -> Vec<String> {
        vec![
            "--print".to_owned(),
            "--resume".to_owned(),
            session_id.to_owned(),
            "--settings".to_owned(),
            self.profile.display().to_string(),
            "--permission-mode".to_owned(),
            self.permission_mode.clone(),
            "--output-format".to_owned(),
            "stream-json".to_owned(),
            "--verbose".to_owned(),
            "--max-turns".to_owned(),
            PASS_MAX_TURNS.to_owned(),
            text.to_owned(),
        ]
    }
}

impl SessionResumer for ClaudeSessionResumer {
    fn resume(&self, session_id: &str, text: &str) -> Result<ResumedSession, ResumeError> {
        let output = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.transcript)
            .map_err(|error| ResumeError::Harness(error.to_string()))?;
        let error_output = output
            .try_clone()
            .map_err(|error| ResumeError::Harness(error.to_string()))?;
        let mut command = Command::new(&self.executable);
        command
            .args(self.arguments(session_id, text))
            .stdout(Stdio::from(output))
            .stderr(Stdio::from(error_output));
        process_control::set_process_group(&mut command);

        // Never resume while the process is live and never add
        // `--fork-session`: `claude --resume <id>` starts a copy when the
        // session is already running, which would fork the work and bill for
        // both. That is why steer queues until normal process exit.
        command
            .spawn()
            .map(ResumedSession::child)
            .map_err(|error| ResumeError::Harness(error.to_string()))
    }
}

/// Whether the just-ended harness process exited normally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessExit {
    Normal,
    Abnormal,
}

/// A refused or malformed request, or a failure to record its events.
#[derive(Debug, Error)]
pub enum ControlError {
    /// The request is not valid for this runtime.
    #[error("invalid control request: {0}")]
    InvalidRequest(String),
    #[error("expected a {expected} control request")]
    WrongKind { expected: &'static str },
    #[error("a live process handle is required to interrupt a live run")]
    MissingLiveProcess,
    /// The run has already emitted its terminal event, so its log is closed.
    #[error("run has already emitted its terminal event")]
    NotLive,
    #[error(transparent)]
    Sink(#[from] SinkFault),
}

#[derive(Debug)]
struct QueuedSteer {
    control_id: String,
    text: String,
}

/// Control state for one run, retained across every queued resume.
///
/// Keeping one instance across resumes preserves the `runId`; the caller also
/// keeps the same watchdog, so all ceilings continue accumulating across turns.
pub struct RunControl<R> {
    run_id: String,
    session_id: Option<String>,
    kill_grace: Duration,
    resumer: R,
    live: bool,
    terminal_emitted: bool,
    pending_steers: VecDeque<QueuedSteer>,
    resumed_child: Option<Child>,
}

impl<R: SessionResumer> RunControl<R> {
    #[must_use]
    pub fn new(
        run_id: impl Into<String>,
        session_id: Option<String>,
        caps: RunCaps,
        resumer: R,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            session_id,
            kill_grace: Duration::from_millis(caps.kill_grace_ms),
            resumer,
            live: true,
            terminal_emitted: false,
            pending_steers: VecDeque::new(),
            resumed_child: None,
        }
    }

    #[must_use]
    pub const fn is_live(&self) -> bool {
        self.live
    }

    #[must_use]
    pub fn pending_steers(&self) -> usize {
        self.pending_steers.len()
    }

    /// Take ownership of the process started by the last successful resume.
    pub fn take_resumed_child(&mut self) -> Option<Child> {
        self.resumed_child.take()
    }

    /// Request an interrupt, terminating the process group when the run is live.
    pub fn interrupt<C: Clock>(
        &mut self,
        request: ControlRequestedPayload,
        child: Option<&mut Child>,
        watchdog: &CapsWatchdog<C>,
        sink: &impl Sink,
    ) -> Result<(), ControlError> {
        self.interrupt_with(request, watchdog, sink, |grace| {
            let child = child.ok_or(ControlError::MissingLiveProcess)?;
            process_control::terminate_child_process_group(child, grace);
            let _ = child.wait();
            Ok(())
        })
    }

    fn interrupt_with<C, F>(
        &mut self,
        request: ControlRequestedPayload,
        watchdog: &CapsWatchdog<C>,
        sink: &impl Sink,
        terminate: F,
    ) -> Result<(), ControlError>
    where
        C: Clock,
        F: FnOnce(Duration) -> Result<(), ControlError>,
    {
        // A non-live run may still have an open log and can carry a not-live
        // event answer. Once terminal has been emitted, every consumer has
        // stopped at it, so the refusal is synchronous and appends nothing.
        if self.terminal_emitted {
            return Err(ControlError::NotLive);
        }
        validate_request(&request)?;
        if request.kind != ControlKind::Interrupt {
            return Err(ControlError::WrongKind {
                expected: "interrupt",
            });
        }
        sink.append(&self.run_id, requested_draft(request.clone()))?;
        if !self.live {
            sink.append(
                &self.run_id,
                applied_draft(
                    &request.control_id,
                    false,
                    Some(ControlAppliedReason::NotLive),
                    None,
                ),
            )?;
            return Ok(());
        }

        terminate(self.kill_grace)?;
        self.live = false;
        sink.append(
            &self.run_id,
            applied_draft(
                &request.control_id,
                true,
                None,
                watchdog.most_recent_open_tool_call(),
            ),
        )?;
        self.reject_pending_steers(sink)?;
        sink.append(&self.run_id, finished_draft(watchdog.interrupted_payload()))?;
        self.terminal_emitted = true;
        Ok(())
    }

    /// Queue a steer for the next normal process boundary.
    pub fn steer(
        &mut self,
        request: ControlRequestedPayload,
        sink: &impl Sink,
    ) -> Result<(), ControlError> {
        // Do not conflate process liveness with log liveness: pre-terminal
        // not-live answers belong in the log, post-terminal refusals do not.
        if self.terminal_emitted {
            return Err(ControlError::NotLive);
        }
        validate_request(&request)?;
        if request.kind != ControlKind::Steer {
            return Err(ControlError::WrongKind { expected: "steer" });
        }
        sink.append(&self.run_id, requested_draft(request.clone()))?;
        if !self.live {
            sink.append(
                &self.run_id,
                applied_draft(
                    &request.control_id,
                    false,
                    Some(ControlAppliedReason::NotLive),
                    None,
                ),
            )?;
            return Ok(());
        }

        self.pending_steers.push_back(QueuedSteer {
            control_id: request.control_id,
            text: request.text.unwrap_or_default(),
        });
        Ok(())
    }

    /// Handle a process exit and either land one queued steer or finish the run.
    pub fn process_exited(
        &mut self,
        exit: ProcessExit,
        finished: RunFinishedPayload,
        sink: &impl Sink,
    ) -> Result<(), ControlError> {
        if self.terminal_emitted {
            return Ok(());
        }

        self.live = false;
        let Some(steer) = self.pending_steers.pop_front() else {
            sink.append(&self.run_id, finished_draft(finished))?;
            self.terminal_emitted = true;
            return Ok(());
        };

        if exit == ProcessExit::Normal {
            let result = self
                .session_id
                .as_deref()
                .map_or(Err(ResumeError::Unsupported), |session_id| {
                    self.resumer.resume(session_id, &steer.text)
                });
            match result {
                Ok(started) => {
                    self.resumed_child = started.child;
                    self.live = true;
                    sink.append(
                        &self.run_id,
                        applied_draft(&steer.control_id, true, None, None),
                    )?;
                    return Ok(());
                }
                Err(ResumeError::Unsupported) => {
                    sink.append(
                        &self.run_id,
                        applied_draft(
                            &steer.control_id,
                            false,
                            Some(ControlAppliedReason::Unsupported),
                            None,
                        ),
                    )?;
                }
                Err(ResumeError::Harness(reason)) => {
                    sink.append(
                        &self.run_id,
                        applied_draft(
                            &steer.control_id,
                            false,
                            Some(ControlAppliedReason::Unknown(reason)),
                            None,
                        ),
                    )?;
                }
            }
        } else {
            sink.append(
                &self.run_id,
                applied_draft(
                    &steer.control_id,
                    false,
                    Some(ControlAppliedReason::NotLive),
                    None,
                ),
            )?;
        }

        self.reject_pending_steers(sink)?;
        sink.append(&self.run_id, finished_draft(finished))?;
        self.terminal_emitted = true;
        Ok(())
    }

    fn reject_pending_steers(&mut self, sink: &impl Sink) -> Result<(), ControlError> {
        while let Some(queued) = self.pending_steers.pop_front() {
            sink.append(
                &self.run_id,
                applied_draft(
                    &queued.control_id,
                    false,
                    Some(ControlAppliedReason::NotLive),
                    None,
                ),
            )?;
        }
        Ok(())
    }
}

fn validate_request(request: &ControlRequestedPayload) -> Result<(), ControlError> {
    validate(CONTROL_REQUESTED, request)
        .map_err(|error| ControlError::InvalidRequest(error.to_string()))
}

fn requested_draft(payload: ControlRequestedPayload) -> EventDraft {
    draft(CONTROL_REQUESTED, payload)
}

/// Record a reason, bounding a harness's own diagnostic words as the unfamiliar
/// string case. `Unknown` is the open union's sanctioned slot for bounded producer
/// prose, not a place to smuggle a value that has a typed member. Under principle 2,
/// excerpt once here at the producer; the sink's `validate` is the second line of
/// defence. Typed reasons carry no free text and never carry a truncation flag.
fn applied_draft(
    control_id: &str,
    ok: bool,
    reason: Option<ControlAppliedReason>,
    landed_in: Option<&str>,
) -> EventDraft {
    let (reason, truncated) = match reason {
        Some(ControlAppliedReason::Unknown(diagnostic)) => {
            let bounded = excerpt(&diagnostic, MAX_EXCERPT_SCALARS);
            (
                Some(ControlAppliedReason::Unknown(bounded.text)),
                bounded.truncated.then_some(true),
            )
        }
        reason => (reason, None),
    };
    draft(
        CONTROL_APPLIED,
        ControlAppliedPayload {
            control_id: control_id.to_owned(),
            ok,
            reason,
            truncated,
            landed_in: landed_in.map(str::to_owned),
            extra: PayloadExtension::new(),
        },
    )
}

fn finished_draft(payload: RunFinishedPayload) -> EventDraft {
    draft(RUN_FINISHED, payload)
}

fn draft(event_type: &str, payload: impl serde::Serialize) -> EventDraft {
    EventDraft {
        event_type: event_type.to_owned(),
        payload: typed_payload(payload),
        captured_at: None,
    }
}

fn typed_payload(payload: impl serde::Serialize) -> Value {
    serde_json::to_value(payload).expect("ethogram payload serializes")
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        sync::{Arc, Mutex},
    };

    use ethogram::{
        AGENT_TOOL_RESULT, AGENT_TOOL_USE, AgentToolResultPayload, AgentToolUsePayload,
        EVENT_SCHEMA_VERSION, Event, RunOutcome, StampFields, stamp,
    };

    use super::*;

    #[derive(Clone, Default)]
    struct ManualClock(Arc<Cell<Duration>>);

    impl Clock for ManualClock {
        fn now(&self) -> Duration {
            self.0.get()
        }
    }

    #[derive(Default)]
    struct MemorySink(Mutex<Vec<Event>>);

    impl MemorySink {
        fn events(&self) -> Vec<Event> {
            self.0.lock().expect("memory sink lock").clone()
        }
    }

    impl Sink for MemorySink {
        fn append(&self, run: &str, draft: EventDraft) -> Result<Event, SinkFault> {
            let mut events = self.0.lock().expect("memory sink lock");
            let event = stamp(
                draft,
                StampFields {
                    run_id: run.to_owned(),
                    seq: u64::try_from(events.len()).expect("fixture sequence") + 1,
                    ts: "2030-01-02T03:04:05.000Z".to_owned(),
                },
            );
            events.push(event.clone());
            Ok(event)
        }

        fn forward(&self, event: Event) -> Result<(), SinkFault> {
            self.0.lock().expect("memory sink lock").push(event);
            Ok(())
        }

        fn last_seq(&self, _run: &str) -> Result<u64, SinkFault> {
            Ok(
                u64::try_from(self.0.lock().expect("memory sink lock").len())
                    .expect("fixture sequence"),
            )
        }
    }

    #[derive(Clone)]
    struct RecordingResumer {
        calls: Arc<Mutex<Vec<(String, String)>>>,
        error: Option<ResumeError>,
    }

    impl RecordingResumer {
        fn succeeding() -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                error: None,
            }
        }

        fn unsupported() -> Self {
            Self {
                calls: Arc::new(Mutex::new(Vec::new())),
                error: Some(ResumeError::Unsupported),
            }
        }

        fn calls(&self) -> Vec<(String, String)> {
            self.calls.lock().expect("resumer calls lock").clone()
        }
    }

    impl SessionResumer for RecordingResumer {
        fn resume(&self, session_id: &str, text: &str) -> Result<ResumedSession, ResumeError> {
            self.calls
                .lock()
                .expect("resumer calls lock")
                .push((session_id.to_owned(), text.to_owned()));
            self.error
                .clone()
                .map_or_else(|| Ok(ResumedSession::externally_managed()), Err)
        }
    }

    fn request(control_id: &str, kind: ControlKind, text: Option<&str>) -> ControlRequestedPayload {
        ControlRequestedPayload {
            control_id: control_id.to_owned(),
            kind,
            decision_id: None,
            option_id: None,
            text: text.map(str::to_owned),
            truncated: text.map(|_| false),
            by: "operator".to_owned(),
            extra: PayloadExtension::new(),
        }
    }

    fn finished(outcome: RunOutcome) -> RunFinishedPayload {
        RunFinishedPayload {
            outcome,
            reason: None,
            truncated: None,
            cost_usd: None,
            usage: None,
            duration_ms: 12,
            estimated: None,
            extra: PayloadExtension::new(),
        }
    }

    fn event(event_type: &str, payload: impl serde::Serialize) -> Event {
        Event {
            v: EVENT_SCHEMA_VERSION,
            event_type: event_type.to_owned(),
            run_id: "run-1".to_owned(),
            seq: 1,
            ts: "2030-01-02T03:04:05.000Z".to_owned(),
            payload: typed_payload(payload),
            captured_at: None,
        }
    }

    fn tool_use(tool_use_id: &str) -> Event {
        event(
            AGENT_TOOL_USE,
            AgentToolUsePayload {
                stage: Some("act".to_owned()),
                tool: "fixture".to_owned(),
                input_excerpt: None,
                truncated: None,
                tool_use_id: Some(tool_use_id.to_owned()),
                parent_tool_use_id: None,
                extra: PayloadExtension::new(),
            },
        )
    }

    fn tool_result(tool_use_id: &str) -> Event {
        event(
            AGENT_TOOL_RESULT,
            AgentToolResultPayload {
                stage: Some("act".to_owned()),
                tool: "fixture".to_owned(),
                is_error: Some(false),
                result_excerpt: None,
                truncated: None,
                tool_use_id: Some(tool_use_id.to_owned()),
                parent_tool_use_id: None,
                extra: PayloadExtension::new(),
            },
        )
    }

    fn watchdog() -> CapsWatchdog<ManualClock> {
        CapsWatchdog::new(RunCaps::default(), ManualClock::default()).expect("watchdog")
    }

    fn payload<T: serde::de::DeserializeOwned>(event: &Event) -> T {
        serde_json::from_value(event.payload.clone()).expect("typed payload")
    }

    #[test]
    fn interrupt_emits_requested_applied_and_finished_in_order() {
        let sink = MemorySink::default();
        let watchdog = watchdog();
        let observed_grace = Cell::new(None);
        let mut control = RunControl::new(
            "run-1",
            Some("session-1".to_owned()),
            RunCaps::default(),
            RecordingResumer::succeeding(),
        );

        control
            .interrupt_with(
                request("control-1", ControlKind::Interrupt, None),
                &watchdog,
                &sink,
                |grace| {
                    observed_grace.set(Some(grace));
                    Ok(())
                },
            )
            .expect("interrupt");

        assert_eq!(observed_grace.get(), Some(Duration::from_secs(10)));
        let events = sink.events();
        assert_eq!(
            events
                .iter()
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>(),
            [CONTROL_REQUESTED, CONTROL_APPLIED, RUN_FINISHED]
        );
        let applied: ControlAppliedPayload = payload(&events[1]);
        assert!(applied.ok);
        let finished: RunFinishedPayload = payload(&events[2]);
        assert_eq!(finished.outcome, RunOutcome::Interrupted);
    }

    #[test]
    fn process_exit_after_interrupt_does_not_emit_a_second_terminal() {
        let sink = MemorySink::default();
        let watchdog = watchdog();
        let mut control = RunControl::new(
            "run-1",
            Some("session-1".to_owned()),
            RunCaps::default(),
            RecordingResumer::succeeding(),
        );

        control
            .interrupt_with(
                request("control-1", ControlKind::Interrupt, None),
                &watchdog,
                &sink,
                |_| Ok(()),
            )
            .expect("interrupt");
        control
            .process_exited(ProcessExit::Abnormal, finished(RunOutcome::Failed), &sink)
            .expect("observe interrupted child exit");

        assert_eq!(
            sink.events()
                .iter()
                .filter(|event| event.event_type == RUN_FINISHED)
                .count(),
            1
        );
    }

    #[test]
    fn interrupt_rejects_queued_steers_before_finishing() {
        let sink = MemorySink::default();
        let watchdog = watchdog();
        let mut control = RunControl::new(
            "run-1",
            Some("session-1".to_owned()),
            RunCaps::default(),
            RecordingResumer::succeeding(),
        );
        control
            .steer(
                request("steer-1", ControlKind::Steer, Some("next text")),
                &sink,
            )
            .expect("queue steer");

        control
            .interrupt_with(
                request("interrupt-1", ControlKind::Interrupt, None),
                &watchdog,
                &sink,
                |_| Ok(()),
            )
            .expect("interrupt");

        let events = sink.events();
        assert_eq!(
            events
                .iter()
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>(),
            [
                CONTROL_REQUESTED,
                CONTROL_REQUESTED,
                CONTROL_APPLIED,
                CONTROL_APPLIED,
                RUN_FINISHED,
            ]
        );
        let interrupt_applied: ControlAppliedPayload = payload(&events[2]);
        assert_eq!(interrupt_applied.control_id, "interrupt-1");
        assert!(interrupt_applied.ok);
        let steer_applied: ControlAppliedPayload = payload(&events[3]);
        assert_eq!(steer_applied.control_id, "steer-1");
        assert!(!steer_applied.ok);
        assert_eq!(steer_applied.reason, Some(ControlAppliedReason::NotLive));
    }

    #[test]
    fn interrupt_lands_in_most_recent_still_open_tool_call() {
        let sink = MemorySink::default();
        let mut watchdog = watchdog();
        watchdog.observe(&tool_use("tool-1")).expect("first use");
        watchdog.observe(&tool_use("tool-2")).expect("second use");
        watchdog.observe(&tool_use("tool-3")).expect("third use");
        watchdog
            .observe(&tool_result("tool-3"))
            .expect("third result");
        let mut control = RunControl::new(
            "run-1",
            Some("session-1".to_owned()),
            RunCaps::default(),
            RecordingResumer::succeeding(),
        );

        control
            .interrupt_with(
                request("control-1", ControlKind::Interrupt, None),
                &watchdog,
                &sink,
                |_| Ok(()),
            )
            .expect("interrupt");

        let applied: ControlAppliedPayload = payload(&sink.events()[1]);
        assert_eq!(applied.landed_in.as_deref(), Some("tool-2"));
    }

    #[test]
    fn interrupt_outside_a_tool_call_has_no_landed_in() {
        let sink = MemorySink::default();
        let watchdog = watchdog();
        let mut control = RunControl::new(
            "run-1",
            Some("session-1".to_owned()),
            RunCaps::default(),
            RecordingResumer::succeeding(),
        );

        control
            .interrupt_with(
                request("control-1", ControlKind::Interrupt, None),
                &watchdog,
                &sink,
                |_| Ok(()),
            )
            .expect("interrupt");

        let applied: ControlAppliedPayload = payload(&sink.events()[1]);
        assert_eq!(applied.landed_in, None);
    }

    #[test]
    fn interrupt_against_non_live_run_fails_without_another_finished_event() {
        let sink = MemorySink::default();
        let watchdog = watchdog();
        let mut control = RunControl::new(
            "run-1",
            Some("session-1".to_owned()),
            RunCaps::default(),
            RecordingResumer::succeeding(),
        );
        control.live = false;

        control
            .interrupt_with(
                request("control-1", ControlKind::Interrupt, None),
                &watchdog,
                &sink,
                |_| panic!("non-live interrupt must not terminate"),
            )
            .expect("non-live reply");

        let events = sink.events();
        assert_eq!(events.len(), 2);
        assert!(!events.iter().any(|event| event.event_type == RUN_FINISHED));
        let applied: ControlAppliedPayload = payload(&events[1]);
        assert!(!applied.ok);
        assert_eq!(applied.reason, Some(ControlAppliedReason::NotLive));
    }

    #[test]
    fn steer_while_live_queues_without_spawning_or_applying() {
        let sink = MemorySink::default();
        let resumer = RecordingResumer::succeeding();
        let calls = resumer.clone();
        let mut control = RunControl::new(
            "run-1",
            Some("session-1".to_owned()),
            RunCaps::default(),
            resumer,
        );

        control
            .steer(
                request("control-1", ControlKind::Steer, Some("take the next turn")),
                &sink,
            )
            .expect("queue steer");

        assert_eq!(calls.calls(), []);
        assert_eq!(control.pending_steers(), 1);
        assert_eq!(
            sink.events()
                .iter()
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>(),
            [CONTROL_REQUESTED]
        );
    }

    #[test]
    fn unknown_control_kind_is_rejected_without_appending() {
        let sink = MemorySink::default();
        let mut control = RunControl::new(
            "run-1",
            Some("session-1".to_owned()),
            RunCaps::default(),
            RecordingResumer::succeeding(),
        );

        let error = control
            .steer(
                request(
                    "control-1",
                    ControlKind::Unknown("teleport".to_owned()),
                    None,
                ),
                &sink,
            )
            .expect_err("unknown kind must be rejected");

        let ControlError::InvalidRequest(message) = error else {
            panic!("expected request validation error");
        };
        assert!(message.contains("unknown value: teleport"));
        assert!(sink.events().is_empty());
    }

    #[test]
    fn steer_without_text_is_rejected_without_appending() {
        let sink = MemorySink::default();
        let mut control = RunControl::new(
            "run-1",
            Some("session-1".to_owned()),
            RunCaps::default(),
            RecordingResumer::succeeding(),
        );

        let error = control
            .steer(request("control-1", ControlKind::Steer, None), &sink)
            .expect_err("textless steer must be rejected");

        assert!(matches!(error, ControlError::InvalidRequest(_)));
        assert!(sink.events().is_empty());
    }

    #[test]
    fn steer_with_empty_text_is_rejected_without_appending() {
        let sink = MemorySink::default();
        let mut control = RunControl::new(
            "run-1",
            Some("session-1".to_owned()),
            RunCaps::default(),
            RecordingResumer::succeeding(),
        );

        let error = control
            .steer(request("control-1", ControlKind::Steer, Some("")), &sink)
            .expect_err("empty steer must be rejected");

        assert!(matches!(error, ControlError::InvalidRequest(_)));
        assert!(sink.events().is_empty());
    }

    #[test]
    fn normal_exit_with_queued_steer_resumes_same_session_then_applies() {
        let sink = MemorySink::default();
        let resumer = RecordingResumer::succeeding();
        let calls = resumer.clone();
        let mut control = RunControl::new(
            "run-1",
            Some("session-1".to_owned()),
            RunCaps::default(),
            resumer,
        );
        control
            .steer(
                request("control-1", ControlKind::Steer, Some("next text")),
                &sink,
            )
            .expect("queue steer");

        control
            .process_exited(ProcessExit::Normal, finished(RunOutcome::Completed), &sink)
            .expect("resume queued steer");

        assert_eq!(
            calls.calls(),
            [("session-1".to_owned(), "next text".to_owned())]
        );
        let events = sink.events();
        assert_eq!(
            events
                .iter()
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>(),
            [CONTROL_REQUESTED, CONTROL_APPLIED]
        );
        let applied: ControlAppliedPayload = payload(&events[1]);
        assert!(applied.ok);
        assert!(control.is_live());
    }

    #[test]
    fn controls_after_finished_return_not_live_without_appending() {
        let sink = MemorySink::default();
        let resumer = RecordingResumer::succeeding();
        let calls = resumer.clone();
        let mut control = RunControl::new(
            "run-1",
            Some("session-1".to_owned()),
            RunCaps::default(),
            resumer,
        );
        control
            .process_exited(ProcessExit::Normal, finished(RunOutcome::Completed), &sink)
            .expect("finish run");
        let event_count = sink.events().len();

        let interrupt_error = control
            .interrupt(
                request("interrupt-1", ControlKind::Interrupt, None),
                None,
                &watchdog(),
                &sink,
            )
            .expect_err("terminal interrupt must be refused synchronously");
        let steer_error = control
            .steer(
                request("steer-1", ControlKind::Steer, Some("too late")),
                &sink,
            )
            .expect_err("terminal steer must be refused synchronously");

        assert!(matches!(interrupt_error, ControlError::NotLive));
        assert!(matches!(steer_error, ControlError::NotLive));
        assert_eq!(calls.calls(), []);
        let events = sink.events();
        assert_eq!(events.len(), event_count);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, RUN_FINISHED);
    }

    #[test]
    fn unsupported_resume_is_reported_and_run_finishes() {
        let sink = MemorySink::default();
        let resumer = RecordingResumer::unsupported();
        let mut control = RunControl::new(
            "run-1",
            Some("session-1".to_owned()),
            RunCaps::default(),
            resumer,
        );
        control
            .steer(
                request("control-1", ControlKind::Steer, Some("next text")),
                &sink,
            )
            .expect("queue steer");

        control
            .process_exited(ProcessExit::Normal, finished(RunOutcome::Completed), &sink)
            .expect("unsupported reply");

        let events = sink.events();
        assert_eq!(
            events
                .iter()
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>(),
            [CONTROL_REQUESTED, CONTROL_APPLIED, RUN_FINISHED]
        );
        let applied: ControlAppliedPayload = payload(&events[1]);
        assert!(!applied.ok);
        assert_eq!(applied.reason, Some(ControlAppliedReason::Unsupported));
        assert!(!control.is_live());
    }

    #[test]
    fn harness_resume_failure_reports_excerpted_diagnostic_with_truncation() {
        let sink = MemorySink::default();
        let diagnostic = "harness refused to resume:".repeat(1_000);
        assert_eq!(diagnostic.len(), 26_000);
        let resumer = RecordingResumer {
            error: Some(ResumeError::Harness(diagnostic.clone())),
            ..RecordingResumer::succeeding()
        };
        let error = resumer
            .resume("session-1", "next text")
            .expect_err("resume fails");
        assert_eq!(error, ResumeError::Harness(diagnostic.clone()));
        let mut control = RunControl::new(
            "run-1",
            Some("session-1".to_owned()),
            RunCaps::default(),
            resumer,
        );
        control
            .steer(
                request("control-1", ControlKind::Steer, Some("next text")),
                &sink,
            )
            .expect("queue steer");

        control
            .process_exited(ProcessExit::Normal, finished(RunOutcome::Completed), &sink)
            .expect("harness failure reply");

        let events = sink.events();
        assert_eq!(
            events
                .iter()
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>(),
            [CONTROL_REQUESTED, CONTROL_APPLIED, RUN_FINISHED]
        );
        let applied: ControlAppliedPayload = payload(&events[1]);
        assert!(!applied.ok);
        let Some(ControlAppliedReason::Unknown(retained)) = &applied.reason else {
            panic!("expected the harness diagnostic as an unknown reason");
        };
        assert_eq!(retained.chars().count(), MAX_EXCERPT_SCALARS);
        assert_eq!(
            retained,
            &diagnostic
                .chars()
                .take(MAX_EXCERPT_SCALARS)
                .collect::<String>()
        );
        assert_eq!(applied.truncated, Some(true));
        assert!(applied.extra.is_empty());
        validate(CONTROL_APPLIED, &applied).expect("valid bounded harness diagnostic");
        assert_eq!(error, ResumeError::Harness(diagnostic));
        assert!(!control.is_live());
    }

    #[test]
    fn harness_resume_failure_reports_short_diagnostic_without_truncation() {
        let sink = MemorySink::default();
        let diagnostic = "harness could not open the session";
        assert!(diagnostic.chars().count() < MAX_EXCERPT_SCALARS);
        let resumer = RecordingResumer {
            error: Some(ResumeError::Harness(diagnostic.to_owned())),
            ..RecordingResumer::succeeding()
        };
        let mut control = RunControl::new(
            "run-1",
            Some("session-1".to_owned()),
            RunCaps::default(),
            resumer,
        );
        control
            .steer(
                request("control-1", ControlKind::Steer, Some("next text")),
                &sink,
            )
            .expect("queue steer");
        control
            .process_exited(ProcessExit::Normal, finished(RunOutcome::Completed), &sink)
            .expect("harness failure reply");

        let events = sink.events();
        let applied: ControlAppliedPayload = payload(&events[1]);
        assert!(!applied.ok);
        assert_eq!(
            applied.reason,
            Some(ControlAppliedReason::Unknown(diagnostic.to_owned()))
        );
        assert_eq!(applied.truncated, None);
        assert!(events[1].payload.get("truncated").is_none());
        validate(CONTROL_APPLIED, &applied).expect("valid short harness diagnostic");
        assert!(!control.is_live());
    }

    #[test]
    fn abnormal_exit_prevents_a_queued_steer_from_landing() {
        let sink = MemorySink::default();
        let resumer = RecordingResumer::succeeding();
        let calls = resumer.clone();
        let mut control = RunControl::new(
            "run-1",
            Some("session-1".to_owned()),
            RunCaps::default(),
            resumer,
        );
        control
            .steer(
                request("control-1", ControlKind::Steer, Some("next text")),
                &sink,
            )
            .expect("queue steer");

        control
            .process_exited(ProcessExit::Abnormal, finished(RunOutcome::Failed), &sink)
            .expect("abnormal exit");

        assert_eq!(calls.calls(), []);
        let applied: ControlAppliedPayload = payload(&sink.events()[1]);
        assert_eq!(applied.reason, Some(ControlAppliedReason::NotLive));
    }

    #[test]
    fn claude_resume_arguments_reuse_identity_without_forking() {
        let resumer =
            ClaudeSessionResumer::new("claude", "profile.json", "auto", "transcript.ndjson");

        let arguments = resumer.arguments("session-1", "next text");

        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--resume", "session-1"])
        );
        assert!(
            !arguments
                .iter()
                .any(|argument| argument == "--fork-session")
        );
        assert_eq!(arguments.last().map(String::as_str), Some("next text"));
    }
}
