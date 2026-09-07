//! Ethogram lifecycle emission through Umwelt's durable sink.

use std::{
    fs::File,
    io::Write,
    path::Path,
    process::Child,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

#[cfg(unix)]
use std::fs::OpenOptions;

use chrono::{DateTime, Utc};
use ethogram::{
    ControlRequestedPayload, Event, EventDraft, PayloadExtension, RunFinishedPayload, RunKind,
    RunOutcome, RunStartedPayload, RunUsage,
};
use ethogram_decisions::{
    DecisionDossier, DecisionKind, DecisionRequestedPayload,
    PayloadExtension as DecisionPayloadExtension,
};
use ostrom_core::{DecisionOption, Dossier, WriteDisposition};
use serde::Serialize;
use thiserror::Error;
use umwelt_runtime::{
    CapTrip, CapsWatchdog, ControlError, FileSink, ProcessExit, RunControl, SessionResumer, Sink,
    SinkFault, Source as _, SourceFault, watchdog::Clock as WatchdogClock,
};

use crate::{Clock, OstromPaths};

static RUN_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Error)]
pub enum RunEventError {
    #[error("run event sink: {0}")]
    Sink(#[from] SinkFault),
    #[error("run event source: {0}")]
    Source(#[from] SourceFault),
    #[error("run event payload: {0}")]
    Payload(#[from] serde_json::Error),
    #[error("decision id {0} was reused with different content")]
    DecisionConflict(String),
}

pub(crate) const SWEEP_RUN_ID: &str = "sweep";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecisionRequest {
    pub decision_id: String,
    pub kind: DecisionKind,
    pub dossier: Dossier,
    pub options: Vec<DecisionOption>,
    pub subject: String,
}

/// The sweep is a durable relay rather than a child process. Its stable run
/// remains open so a later queue command can apply an answer on the run that
/// owns the request.
pub(crate) struct SweepDecisionEmitter {
    sink: RunEventSink,
}

impl SweepDecisionEmitter {
    pub(crate) fn new(paths: &OstromPaths) -> Result<Self, RunEventError> {
        let sink = RunEventSink::new(&paths.runs_dir(), None, false);
        if sink.last_seq(SWEEP_RUN_ID)? == 0 {
            let payload = RunStartedPayload {
                kind: RunKind::Relay,
                actor: "sweep".to_owned(),
                harness: "ostrom".to_owned(),
                model: None,
                parent_run_id: None,
                parent_tool_use_id: None,
                schedule: None,
                repository: None,
                work_order: None,
                ceilings: None,
                extra: PayloadExtension::new(),
            };
            sink.append(SWEEP_RUN_ID, draft("run.started", payload)?)?;
        }
        Ok(Self { sink })
    }

    pub(crate) fn request(
        &self,
        request: &DecisionRequest,
    ) -> Result<WriteDisposition, RunEventError> {
        let draft = decision_request_draft(request)?;
        for event in self.sink.durable.read_from(SWEEP_RUN_ID, 0)? {
            if event.event_type != ethogram_decisions::DECISION_REQUESTED
                || event
                    .payload
                    .get("decisionId")
                    .and_then(serde_json::Value::as_str)
                    != Some(request.decision_id.as_str())
            {
                continue;
            }
            if event.payload == draft.payload {
                return Ok(WriteDisposition::Unchanged);
            }
            return Err(RunEventError::DecisionConflict(request.decision_id.clone()));
        }
        self.sink.append(SWEEP_RUN_ID, draft)?;
        Ok(WriteDisposition::Written)
    }
}

#[derive(Debug)]
pub struct RunEventStart {
    pub run_id: String,
    pub kind: RunKind,
    pub actor: String,
    pub harness: String,
    pub model: Option<String>,
    pub schedule: Option<String>,
    pub repository: Option<String>,
    pub work_order: Option<String>,
    pub ceilings: Option<ethogram::RunCeilings>,
}

/// A started run which writes exactly one terminal event, including on unwind.
pub struct RunEventGuard {
    sink: RunEventSink,
    run_id: String,
    started_at: DateTime<Utc>,
    clock: Clock,
    outcome: RunOutcome,
    reason: Option<String>,
    cost_usd: Option<f64>,
    usage: Option<RunUsage>,
    finished: bool,
}

impl RunEventGuard {
    pub fn start(
        paths: &OstromPaths,
        events_fd: Option<u32>,
        facts_only: bool,
        clock: Clock,
        start: RunEventStart,
    ) -> Result<Self, RunEventError> {
        let sink = RunEventSink::new(&paths.runs_dir(), events_fd, facts_only);
        let payload = RunStartedPayload {
            kind: start.kind,
            actor: start.actor,
            harness: start.harness,
            model: start.model,
            parent_run_id: None,
            parent_tool_use_id: None,
            schedule: start.schedule,
            repository: start.repository,
            work_order: start.work_order,
            ceilings: start.ceilings,
            extra: PayloadExtension::new(),
        };
        sink.append(&start.run_id, draft("run.started", payload)?)?;
        Ok(Self {
            sink,
            run_id: start.run_id,
            started_at: clock.now(),
            clock,
            outcome: RunOutcome::Failed,
            reason: Some("run-failed".to_owned()),
            cost_usd: None,
            usage: None,
            finished: false,
        })
    }

    pub fn finish(
        &mut self,
        outcome: RunOutcome,
        reason: Option<String>,
        cost_usd: Option<f64>,
        usage: Option<RunUsage>,
    ) -> Result<(), RunEventError> {
        self.outcome = outcome;
        self.reason = reason;
        self.cost_usd = cost_usd;
        self.usage = usage;
        self.write_terminal()
    }

    pub(crate) fn append(&self, draft: EventDraft) -> Result<Event, RunEventError> {
        self.sink.append(&self.run_id, draft).map_err(Into::into)
    }

    #[must_use]
    pub(crate) fn sink(&self) -> &RunEventSink {
        &self.sink
    }

    #[must_use]
    pub(crate) fn run_id(&self) -> &str {
        &self.run_id
    }

    pub(crate) fn process_exited<R: SessionResumer>(
        &mut self,
        control: &mut RunControl<R>,
        exit: ProcessExit,
        outcome: RunOutcome,
        reason: Option<String>,
        cost_usd: Option<f64>,
        usage: Option<RunUsage>,
    ) -> Result<(), ControlError> {
        if self.finished {
            return Ok(());
        }
        self.outcome = outcome;
        self.reason = reason;
        self.cost_usd = cost_usd;
        self.usage = usage;
        control.process_exited(exit, self.finished_payload(), &self.sink)?;
        self.finished = true;
        Ok(())
    }

    pub(crate) fn interrupt<R: SessionResumer, C: WatchdogClock>(
        &mut self,
        control: &mut RunControl<R>,
        request: ControlRequestedPayload,
        child: Option<&mut Child>,
        watchdog: &CapsWatchdog<C>,
    ) -> Result<(), ControlError> {
        control.interrupt(request, child, watchdog, &self.sink)?;
        self.finished = true;
        Ok(())
    }

    pub(crate) fn terminate_cap(
        &mut self,
        trip: CapTrip,
        child: &mut Child,
    ) -> Result<Event, SinkFault> {
        let event = trip.terminate_and_report(child, &self.sink, &self.run_id)?;
        self.finished = true;
        Ok(event)
    }

    fn write_terminal(&mut self) -> Result<(), RunEventError> {
        if self.finished {
            return Ok(());
        }
        let payload = self.finished_payload();
        self.sink
            .append(&self.run_id, draft("run.finished", payload)?)?;
        self.finished = true;
        Ok(())
    }

    fn finished_payload(&self) -> RunFinishedPayload {
        let elapsed = self
            .clock
            .now()
            .signed_duration_since(self.started_at)
            .num_milliseconds()
            .max(0);
        RunFinishedPayload {
            outcome: self.outcome.clone(),
            reason: self.reason.clone(),
            truncated: None,
            cost_usd: self.cost_usd,
            usage: self.usage.clone(),
            duration_ms: u64::try_from(elapsed).unwrap_or(u64::MAX),
            estimated: None,
            extra: PayloadExtension::new(),
        }
    }
}

pub(crate) struct RunEventSink {
    durable: FileSink,
    facts_only: bool,
    live: Mutex<LiveEventSink>,
}

struct LiveEventSink {
    live: Option<File>,
    live_fd: Option<u32>,
    live_fault: Option<String>,
}

impl RunEventSink {
    fn new(root: &Path, live_fd: Option<u32>, facts_only: bool) -> Self {
        let mut live = LiveEventSink {
            live: None,
            live_fd,
            live_fault: None,
        };
        if let Some(fd) = live_fd {
            match open_fd(fd) {
                Ok(file) => live.live = Some(file),
                Err(error) => {
                    live.record_fault(format!("could not open events fd {fd}: {error}"));
                }
            }
        }
        Self {
            durable: FileSink::new(root),
            facts_only,
            live: Mutex::new(live),
        }
    }

    fn mirror(&self, event: &Event) {
        if self.facts_only
            && !(event.event_type.starts_with("run.") || event.event_type.starts_with("control."))
        {
            return;
        }
        let Ok(mut state) = self.live.lock() else {
            eprintln!("ostrom observability: events fd lock was poisoned");
            return;
        };
        if let Some(live) = &mut state.live {
            let result = ethogram::serialise_event(event)
                .map_err(std::io::Error::other)
                .and_then(|serialised| live.write_all(serialised.as_bytes()))
                .and_then(|()| live.write_all(b"\n"))
                .and_then(|()| live.flush());
            if let Err(error) = result {
                let fd = state.live_fd.unwrap_or_default();
                state.live = None;
                state.record_fault(format!("could not write events fd {fd}: {error}"));
            }
        }
    }
}

impl LiveEventSink {
    fn record_fault(&mut self, message: String) {
        eprintln!("ostrom observability: {message}");
        self.live_fault = Some(message);
    }
}

impl Sink for RunEventSink {
    fn append(&self, run: &str, draft: EventDraft) -> Result<Event, SinkFault> {
        let event = self.durable.append(run, draft)?;
        self.mirror(&event);
        Ok(event)
    }

    fn forward(&self, event: Event) -> Result<(), SinkFault> {
        self.durable.forward(event.clone())?;
        self.mirror(&event);
        Ok(())
    }

    fn last_seq(&self, run: &str) -> Result<u64, SinkFault> {
        self.durable.last_seq(run)
    }
}

#[cfg(target_os = "linux")]
fn open_fd(fd: u32) -> std::io::Result<File> {
    OpenOptions::new()
        .append(true)
        .open(format!("/proc/self/fd/{fd}"))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn open_fd(fd: u32) -> std::io::Result<File> {
    OpenOptions::new()
        .append(true)
        .open(format!("/dev/fd/{fd}"))
}

#[cfg(not(unix))]
fn open_fd(_fd: u32) -> std::io::Result<File> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "file descriptors are unsupported on this platform",
    ))
}

impl Drop for RunEventGuard {
    fn drop(&mut self) {
        if let Err(error) = self.write_terminal() {
            eprintln!(
                "ostrom observability: could not finish {}: {error}",
                self.run_id
            );
        }
    }
}

#[must_use]
pub fn generated_run_id(prefix: &str, clock: &Clock) -> String {
    let sequence = RUN_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!(
        "{prefix}-{}-{}-{sequence}",
        clock.now().format("%Y%m%dT%H%M%S%3fZ"),
        std::process::id(),
    )
}

fn decision_request_draft(request: &DecisionRequest) -> Result<EventDraft, serde_json::Error> {
    let mut truncated = false;
    let mut bounded = |value: &str| {
        let excerpt = ethogram_decisions::excerpt(value, ethogram_decisions::MAX_EXCERPT_SCALARS);
        truncated |= excerpt.truncated;
        excerpt.text
    };
    let question = bounded(&request.dossier.question);
    let options_ruled_out = request
        .dossier
        .options_ruled_out
        .iter()
        .map(|value| bounded(value))
        .collect();
    let recommended_action = bounded(&request.dossier.recommended_action);
    let blast_radius = bounded(&request.dossier.blast_radius);
    let options = request
        .options
        .iter()
        .map(|option| ethogram_decisions::DecisionOption {
            id: option.id.clone(),
            label: bounded(&option.label),
            extra: DecisionPayloadExtension::new(),
        })
        .collect();
    let payload = serde_json::to_value(DecisionRequestedPayload {
        decision_id: request.decision_id.clone(),
        kind: request.kind.clone(),
        dossier: DecisionDossier {
            question,
            options_ruled_out,
            recommended_action,
            blast_radius,
            truncated: truncated.then_some(true),
            extra: DecisionPayloadExtension::new(),
        },
        options,
        subject: Some(request.subject.clone()),
        expires_at: None,
        on_timeout: None,
        extra: DecisionPayloadExtension::new(),
    })?;
    ethogram_decisions::validate(ethogram_decisions::DECISION_REQUESTED, &payload)?;
    Ok(EventDraft {
        event_type: ethogram_decisions::DECISION_REQUESTED.to_owned(),
        payload,
        captured_at: None,
    })
}

fn draft(payload_type: &str, payload: impl Serialize) -> Result<EventDraft, serde_json::Error> {
    Ok(EventDraft {
        event_type: payload_type.to_owned(),
        payload: serde_json::to_value(payload)?,
        captured_at: None,
    })
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::{fs, fs::OpenOptions, os::fd::AsRawFd};

    use chrono::{TimeZone, Utc};
    use ethogram::{RunFinishedPayload, RunKind, RunOutcome, RunStartedPayload};
    use ethogram_decisions::{DecisionKind, DecisionRequestedPayload};
    use ostrom_core::{DecisionOption, Dossier, WriteDisposition};
    use tempfile::tempdir;
    use umwelt_runtime::{FileSink, Source};

    use super::{
        DecisionRequest, RunEventError, RunEventGuard, RunEventStart, SWEEP_RUN_ID,
        SweepDecisionEmitter,
    };
    use crate::{Clock, OstromPaths};

    fn fixture() -> (tempfile::TempDir, OstromPaths, Clock) {
        let root = tempdir().expect("temporary run event fixture");
        let paths = OstromPaths {
            config: root.path().to_path_buf(),
            state: root.path().to_path_buf(),
        };
        let clock = Clock::fixed(
            Utc.with_ymd_and_hms(2030, 1, 2, 3, 4, 5)
                .single()
                .expect("fixture time"),
        );
        (root, paths, clock)
    }

    fn decision_request(decision_id: &str, subject: &str) -> DecisionRequest {
        DecisionRequest {
            decision_id: decision_id.to_owned(),
            kind: DecisionKind::Tripwire,
            dossier: Dossier {
                question: format!("May {subject} proceed?"),
                options_ruled_out: vec!["automatic progress".to_owned()],
                recommended_action: "review the evidence".to_owned(),
                blast_radius: format!("{subject} only"),
            },
            options: ["approve", "reject", "defer"]
                .into_iter()
                .map(|option| DecisionOption {
                    id: option.to_owned(),
                    label: option.to_owned(),
                })
                .collect(),
            subject: subject.to_owned(),
        }
    }

    #[test]
    fn sweep_decisions_cross_the_typed_edge_without_a_timeout() {
        let (_root, paths, _clock) = fixture();
        let emitter = SweepDecisionEmitter::new(&paths).expect("open sweep decision emitter");
        assert_eq!(
            emitter
                .request(&decision_request(
                    "decision-fixture",
                    "synthetic/project#42"
                ))
                .expect("append decision request"),
            WriteDisposition::Written
        );

        let events = FileSink::new(paths.runs_dir())
            .read_from(SWEEP_RUN_ID, 0)
            .expect("read sweep events");
        let decision = events
            .iter()
            .find(|event| event.event_type == ethogram_decisions::DECISION_REQUESTED)
            .expect("decision event");
        ethogram_decisions::validate(&decision.event_type, &decision.payload)
            .expect("emitted decision validates against the current SDK");
        let payload: DecisionRequestedPayload =
            serde_json::from_value(decision.payload.clone()).expect("typed decision payload");
        assert_eq!(payload.kind, DecisionKind::Tripwire);
        assert_eq!(payload.subject.as_deref(), Some("synthetic/project#42"));
        assert_eq!(
            payload
                .options
                .iter()
                .map(|option| option.id.as_str())
                .collect::<Vec<_>>(),
            ["approve", "reject", "defer"]
        );
        assert!(payload.on_timeout.is_none());
        assert!(
            !decision
                .payload
                .as_object()
                .unwrap()
                .contains_key("onTimeout")
        );
    }

    #[test]
    fn sweep_decision_identity_is_idempotent_and_conflicts_are_loud() {
        let (_root, paths, _clock) = fixture();
        let emitter = SweepDecisionEmitter::new(&paths).expect("open sweep decision emitter");
        let request = decision_request("stable-decision", "synthetic/project#42");
        assert_eq!(
            emitter.request(&request).expect("first request"),
            WriteDisposition::Written
        );
        assert_eq!(
            emitter.request(&request).expect("identical retry"),
            WriteDisposition::Unchanged
        );

        let conflicting = decision_request("stable-decision", "synthetic/project#43");
        assert!(matches!(
            emitter.request(&conflicting),
            Err(RunEventError::DecisionConflict(id)) if id == "stable-decision"
        ));
        let decisions = FileSink::new(paths.runs_dir())
            .read_from(SWEEP_RUN_ID, 0)
            .expect("read sweep events")
            .into_iter()
            .filter(|event| event.event_type == ethogram_decisions::DECISION_REQUESTED)
            .count();
        assert_eq!(decisions, 1);
    }

    #[test]
    fn lifecycle_events_are_typed_and_terminal() {
        let (_root, paths, clock) = fixture();
        let mut run = RunEventGuard::start(
            &paths,
            None,
            false,
            clock,
            RunEventStart {
                run_id: "builder-fixture".to_owned(),
                kind: RunKind::Loop,
                actor: "builder".to_owned(),
                harness: "claude".to_owned(),
                model: None,
                schedule: None,
                repository: None,
                work_order: None,
                ceilings: None,
            },
        )
        .expect("start lifecycle");
        run.finish(RunOutcome::Completed, None, Some(1.25), None)
            .expect("finish lifecycle");

        let events = FileSink::new(paths.runs_dir())
            .read_from("builder-fixture", 0)
            .expect("read lifecycle");
        assert_eq!(events.len(), 2);
        let started: RunStartedPayload =
            serde_json::from_value(events[0].payload.clone()).expect("typed started payload");
        let finished: RunFinishedPayload =
            serde_json::from_value(events[1].payload.clone()).expect("typed finished payload");
        assert_eq!(started.kind, RunKind::Loop);
        assert_eq!(started.actor, "builder");
        assert_eq!(finished.outcome, RunOutcome::Completed);
        assert_eq!(finished.cost_usd, Some(1.25));
        assert_eq!(finished.duration_ms, 0);
    }

    #[test]
    fn dropping_a_started_run_records_a_failed_terminal_event() {
        let (_root, paths, clock) = fixture();
        drop(
            RunEventGuard::start(
                &paths,
                None,
                false,
                clock,
                RunEventStart {
                    run_id: "abandoned-fixture".to_owned(),
                    kind: RunKind::Handoff,
                    actor: "builder".to_owned(),
                    harness: "codex".to_owned(),
                    model: None,
                    schedule: None,
                    repository: None,
                    work_order: None,
                    ceilings: None,
                },
            )
            .expect("start lifecycle"),
        );

        let events = FileSink::new(paths.runs_dir())
            .read_from("abandoned-fixture", 0)
            .expect("read lifecycle");
        assert_eq!(events.len(), 2);
        let finished: RunFinishedPayload =
            serde_json::from_value(events[1].payload.clone()).expect("typed finished payload");
        assert_eq!(finished.outcome, RunOutcome::Failed);
        assert_eq!(finished.reason.as_deref(), Some("run-failed"));
    }

    #[cfg(unix)]
    #[test]
    fn live_descriptor_bytes_are_identical_to_durable_bytes() {
        let (root, paths, clock) = fixture();
        let live_path = root.path().join("live.jsonl");
        let live = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&live_path)
            .expect("open live event target");
        let mut run = RunEventGuard::start(
            &paths,
            Some(u32::try_from(live.as_raw_fd()).expect("non-negative event descriptor")),
            false,
            clock,
            RunEventStart {
                run_id: "mirrored-fixture".to_owned(),
                kind: RunKind::Loop,
                actor: "builder".to_owned(),
                harness: "claude".to_owned(),
                model: None,
                schedule: None,
                repository: None,
                work_order: None,
                ceilings: None,
            },
        )
        .expect("start mirrored lifecycle");
        run.finish(RunOutcome::Completed, None, None, None)
            .expect("finish mirrored lifecycle");
        drop(run);
        drop(live);

        let durable_path = paths
            .runs_dir()
            .join(umwelt_runtime::run_directory_name("mirrored-fixture"))
            .join("events.jsonl");
        let durable = fs::read(durable_path).expect("read durable events");
        let mirrored = fs::read(live_path).expect("read mirrored events");
        assert_eq!(mirrored, durable);
        assert_eq!(durable.iter().filter(|byte| **byte == b'\n').count(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn a_closed_live_descriptor_does_not_stop_the_durable_run() {
        let (root, paths, clock) = fixture();
        let closed = OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.path().join("closed.jsonl"))
            .expect("open descriptor to close");
        let fd = u32::try_from(closed.as_raw_fd()).expect("non-negative event descriptor");
        drop(closed);

        let mut run = RunEventGuard::start(
            &paths,
            Some(fd),
            false,
            clock,
            RunEventStart {
                run_id: "closed-fd-fixture".to_owned(),
                kind: RunKind::Loop,
                actor: "builder".to_owned(),
                harness: "claude".to_owned(),
                model: None,
                schedule: None,
                repository: None,
                work_order: None,
                ceilings: None,
            },
        )
        .expect("a closed live descriptor cannot prevent run.started");
        assert!(
            run.sink
                .live
                .lock()
                .expect("live event sink lock")
                .live_fault
                .is_some()
        );
        run.finish(RunOutcome::Completed, None, None, None)
            .expect("a closed live descriptor cannot prevent run.finished");

        let events = FileSink::new(paths.runs_dir())
            .read_from("closed-fd-fixture", 0)
            .expect("read durable lifecycle");
        assert_eq!(events.len(), 2);
    }
}
