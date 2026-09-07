//! Ethogram lifecycle emission through Umwelt's durable sink.

use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use ethogram::{
    EventDraft, PayloadExtension, RunFinishedPayload, RunKind, RunOutcome, RunStartedPayload,
    RunUsage,
};
use serde::Serialize;
use thiserror::Error;
use umwelt_runtime::{FileSink, Sink, SinkFault};

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
    sink: FileSink,
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
        clock: Clock,
        start: RunEventStart,
    ) -> Result<Self, RunEventError> {
        let sink = FileSink::new(paths.runs_dir());
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

    fn write_terminal(&mut self) -> Result<(), RunEventError> {
        if self.finished {
            return Ok(());
        }
        let elapsed = self
            .clock
            .now()
            .signed_duration_since(self.started_at)
            .num_milliseconds()
            .max(0);
        let payload = RunFinishedPayload {
            outcome: self.outcome,
            reason: self.reason.clone(),
            truncated: None,
            cost_usd: self.cost_usd,
            usage: self.usage.clone(),
            duration_ms: u64::try_from(elapsed).unwrap_or(u64::MAX),
            estimated: None,
            extra: PayloadExtension::new(),
        };
        self.sink
            .append(&self.run_id, draft("run.finished", payload)?)?;
        self.finished = true;
        Ok(())
    }
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
}
