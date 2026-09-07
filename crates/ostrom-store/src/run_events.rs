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
use serde::Serialize;
use thiserror::Error;
use umwelt_runtime::{
    CapTrip, CapsWatchdog, ControlError, FileSink, ProcessExit, RunControl, SessionResumer, Sink,
    SinkFault, watchdog::Clock as WatchdogClock,
};

use crate::{Clock, OstromPaths};

static RUN_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Error)]
pub enum RunEventError {
    #[error("run event sink: {0}")]
    Sink(#[from] SinkFault),
    #[error("run event payload: {0}")]
    Payload(#[from] serde_json::Error),
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
    use tempfile::tempdir;
    use umwelt_runtime::{FileSink, Source};

    use super::{RunEventGuard, RunEventStart};
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
