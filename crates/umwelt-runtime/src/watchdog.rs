//! Cap enforcement over normalised harness events and a monotonic clock.
//!
//! Idle time means time since the last observed event, but it does not advance
//! while a tool call is in flight. Suspension is reference-counted rather than
//! boolean: every `agent.tool_use` increments the count, every
//! `agent.tool_result` decrements it, and idle timing resumes only after all
//! concurrent calls have closed. The decrement saturates at zero so an
//! unpaired result cannot cancel a later, genuine suspension. Every event
//! resets idle, including a sub-agent event observed while its parent's tool
//! call is open, because that event is evidence of activity.
//!
//! This protection necessarily trusts the harness's claim that work is in
//! flight. If a tool hangs and never emits its result, idle remains suspended
//! indefinitely; only a wall cap bounds that run. An operator who configures
//! an idle cap without a wall cap is therefore less protected than the idle
//! cap's name may suggest.
//!
//! `costUsd` and `usage` on `agent.completed` are cumulative for the session
//! named by `sessionId`, as the harness reports them at that moment; they are
//! not per-frame increments. A consumer takes the maximum cost and the maximum
//! of each usage field across completions sharing a session, then sums those
//! maxima across distinct sessions. `turns` and `durationMs` are instead
//! per-invocation values, so turns continue to be summed across completions.
//!
//! A completion is attributed first to its own `sessionId`, or otherwise to
//! the `sessionId` on the most recent preceding `agent.started`. The field is
//! optional, so this fallback is permanent. If neither event supplies an id,
//! that completion is its own session and is summed with the others. Folding
//! an unattributable completion into another session would silently discard a
//! cost, so summing is the only safe attribution rule without an identity,
//! even though over-counting can kill work that was inside its budget — the
//! failure that known-session folding prevents.

use std::{
    collections::HashMap,
    process::Child,
    time::{Duration, Instant},
};

use ethogram::{
    AGENT_COMPLETED, AGENT_STARTED, AGENT_TOOL_RESULT, AGENT_TOOL_USE, AGENT_WARNING,
    AgentCompletedPayload, AgentStartedPayload, AgentWarningPayload, Event, EventDraft,
    PayloadExtension, RUN_FINISHED, RunFinishedPayload, RunOutcome, RunUsage,
};
use serde_json::Value;
use thiserror::Error;

use crate::{
    agent::RunCaps,
    process_control,
    sink::{Sink, SinkFault},
};

const MICRODOLLARS_PER_DOLLAR: f64 = 1_000_000.0;

/// A monotonic clock used by cap state machines.
///
/// Values are durations from an arbitrary epoch. They need only be comparable
/// with earlier values returned by the same clock.
pub trait Clock {
    fn now(&self) -> Duration;
}

/// The production monotonic clock.
#[derive(Debug)]
pub struct SystemClock {
    epoch: Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self {
            epoch: Instant::now(),
        }
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.epoch.elapsed()
    }
}

/// A cap with a deterministic precedence relative to other caps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cap {
    Wall,
    Idle,
    Turns,
    Tokens,
    Cost,
}

impl Cap {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Wall => "wall",
            Self::Idle => "idle",
            Self::Turns => "turns",
            Self::Tokens => "tokens",
            Self::Cost => "cost",
        }
    }
}

/// The measured value that caused a cap to trip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapMeasurement {
    Milliseconds(u64),
    Count(u64),
    Microdollars(u64),
}

/// A single terminal cap decision made by the watchdog.
#[derive(Debug, PartialEq)]
pub struct CapTrip {
    cap: Cap,
    measured: CapMeasurement,
    limit: CapMeasurement,
    finished: RunFinishedPayload,
    kill_grace: Duration,
}

impl CapTrip {
    #[must_use]
    pub const fn cap(&self) -> Cap {
        self.cap
    }

    #[must_use]
    pub const fn measured(&self) -> CapMeasurement {
        self.measured
    }

    #[must_use]
    pub const fn limit(&self) -> CapMeasurement {
        self.limit
    }

    #[must_use]
    pub fn finished(&self) -> &RunFinishedPayload {
        &self.finished
    }

    /// Terminate the child's process group and append its sole terminal event.
    ///
    /// The child must have been spawned as its process-group leader. The
    /// existing process-control path sends `SIGTERM`, waits the configured
    /// grace period, and sends `SIGKILL` if the group remains alive.
    pub fn terminate_and_report(
        self,
        child: &mut Child,
        sink: &impl Sink,
        run: &str,
    ) -> Result<Event, SinkFault> {
        process_control::terminate_child_process_group(child, self.kill_grace);
        let _ = child.wait();
        sink.append(run, finished_draft(self.finished))
    }

    #[must_use]
    pub fn into_finished_draft(self) -> EventDraft {
        finished_draft(self.finished)
    }
}

/// A malformed watchdog configuration or normalised observation.
#[derive(Debug, Error)]
pub enum WatchdogError {
    #[error(
        "cost must be a finite, non-negative number of US dollars representable in micro-dollars"
    )]
    InvalidCost,
    #[error("invalid {event_type} observation: {source}")]
    InvalidObservation {
        event_type: String,
        #[source]
        source: serde_json::Error,
    },
}

/// Cap state for one run, driven only by observed events and an injected clock.
pub struct CapsWatchdog<C> {
    caps: RunCaps,
    cost_cap_microusd: Option<u64>,
    clock: C,
    started_at: Duration,
    last_observed_at: Duration,
    open_tool_calls: u64,
    turns: u64,
    last_started_session_id: Option<String>,
    sessions: HashMap<SessionKey, SessionObservation>,
    usage_unit: Option<String>,
    tripped: bool,
}

impl<C: Clock> CapsWatchdog<C> {
    pub fn new(caps: RunCaps, clock: C) -> Result<Self, WatchdogError> {
        let cost_cap_microusd = caps.cost_usd.map(dollars_to_microusd).transpose()?;
        let now = clock.now();
        Ok(Self {
            caps,
            cost_cap_microusd,
            clock,
            started_at: now,
            last_observed_at: now,
            open_tool_calls: 0,
            turns: 0,
            last_started_session_id: None,
            sessions: HashMap::new(),
            usage_unit: None,
            tripped: false,
        })
    }

    /// Start a run's watchdog and emit any configuration warning immediately.
    pub fn start(caps: RunCaps, clock: C, sink: &impl Sink, run: &str) -> Result<Self, StartError> {
        let watchdog = Self::new(caps, clock)?;
        if let Some(warning) = watchdog.start_warning() {
            sink.append(run, warning)?;
        }
        Ok(watchdog)
    }

    /// Return the warning required when idle is the run's only time bound.
    #[must_use]
    pub fn start_warning(&self) -> Option<EventDraft> {
        (self.caps.idle_ms.is_some() && self.caps.wall_ms.is_none()).then(|| {
            warning_draft(
                "the idle cap does not bound a hanging tool call; a tool call that never reports a result suspends idle indefinitely, so configure a wall cap to bound that run",
            )
        })
    }

    /// Observe one already-normalised ethogram event.
    ///
    /// Every event resets idle. Only typed `agent.completed` fields contribute
    /// turns, tokens, and cost; raw harness output is never interpreted here.
    pub fn observe(&mut self, event: &Event) -> Result<Option<CapTrip>, WatchdogError> {
        if self.tripped {
            return Ok(None);
        }

        self.last_observed_at = self.clock.now();
        match event.event_type.as_str() {
            AGENT_TOOL_USE => {
                self.open_tool_calls = self.open_tool_calls.saturating_add(1);
            }
            AGENT_TOOL_RESULT => {
                self.open_tool_calls = self.open_tool_calls.saturating_sub(1);
            }
            AGENT_STARTED => self.observe_started(event)?,
            AGENT_COMPLETED => self.observe_completed(event)?,
            _ => {}
        }
        Ok(self.evaluate())
    }

    /// Evaluate clock-derived caps without observing a new event.
    pub fn check(&mut self) -> Option<CapTrip> {
        self.evaluate()
    }

    fn observe_started(&mut self, event: &Event) -> Result<(), WatchdogError> {
        let started = serde_json::from_value::<AgentStartedPayload>(event.payload.clone())
            .map_err(|source| WatchdogError::InvalidObservation {
                event_type: event.event_type.clone(),
                source,
            })?;
        self.last_started_session_id = started.session_id;
        Ok(())
    }

    fn observe_completed(&mut self, event: &Event) -> Result<(), WatchdogError> {
        let completed = serde_json::from_value::<AgentCompletedPayload>(event.payload.clone())
            .map_err(|source| WatchdogError::InvalidObservation {
                event_type: event.event_type.clone(),
                source,
            })?;
        self.turns = self.turns.saturating_add(completed.turns.unwrap_or(0));
        let cost_microusd = completed.cost_usd.map(dollars_to_microusd).transpose()?;
        let session_key = completed
            .session_id
            .or_else(|| self.last_started_session_id.clone())
            .map_or_else(
                || SessionKey::Unattributed(self.sessions.len()),
                SessionKey::Named,
            );
        if let Some(unit) = completed
            .usage
            .as_ref()
            .and_then(|usage| usage.unit.as_ref())
        {
            self.usage_unit = Some(unit.clone());
        }
        let session = self.sessions.entry(session_key).or_default();
        max_optional(&mut session.cost_microusd, cost_microusd);
        if let Some(usage) = completed.usage {
            session.usage.add(usage);
        }
        Ok(())
    }

    fn observed_totals(&self) -> ObservedTotals {
        let mut totals = ObservedTotals {
            usage: UsageLowerBound {
                unit: self.usage_unit.clone(),
                ..UsageLowerBound::default()
            },
            ..ObservedTotals::default()
        };
        for session in self.sessions.values() {
            sum_optional(&mut totals.cost_microusd, session.cost_microusd);
            sum_optional(&mut totals.usage.input_tokens, session.usage.input_tokens);
            sum_optional(&mut totals.usage.output_tokens, session.usage.output_tokens);
            sum_optional(
                &mut totals.usage.cache_read_tokens,
                session.usage.cache_read_tokens,
            );
            sum_optional(
                &mut totals.usage.cache_creation_tokens,
                session.usage.cache_creation_tokens,
            );
        }
        totals
    }

    fn evaluate(&mut self) -> Option<CapTrip> {
        if self.tripped {
            return None;
        }

        let now = self.clock.now();
        let wall_ms = duration_ms(now.saturating_sub(self.started_at));
        let idle_ms = (self.open_tool_calls == 0)
            .then(|| duration_ms(now.saturating_sub(self.last_observed_at)));
        let totals = self.observed_totals();
        let tokens = totals.usage.total_tokens();
        let cost_microusd = totals.cost_microusd.unwrap_or(0);

        // When several caps reach their limit at the same observation, this
        // order is the contract: wall, idle, turns, tokens, then cost.
        let decision = self
            .caps
            .wall_ms
            .filter(|limit| wall_ms >= *limit)
            .map(|limit| {
                (
                    Cap::Wall,
                    CapMeasurement::Milliseconds(wall_ms),
                    CapMeasurement::Milliseconds(limit),
                )
            })
            .or_else(|| {
                self.caps.idle_ms.and_then(|limit| {
                    idle_ms.filter(|value| *value >= limit).map(|value| {
                        (
                            Cap::Idle,
                            CapMeasurement::Milliseconds(value),
                            CapMeasurement::Milliseconds(limit),
                        )
                    })
                })
            })
            .or_else(|| {
                self.caps
                    .turns
                    .filter(|limit| self.turns >= *limit)
                    .map(|limit| {
                        (
                            Cap::Turns,
                            CapMeasurement::Count(self.turns),
                            CapMeasurement::Count(limit),
                        )
                    })
            })
            .or_else(|| {
                self.caps
                    .tokens
                    .filter(|limit| tokens >= *limit)
                    .map(|limit| {
                        (
                            Cap::Tokens,
                            CapMeasurement::Count(tokens),
                            CapMeasurement::Count(limit),
                        )
                    })
            })
            .or_else(|| {
                self.cost_cap_microusd
                    .filter(|limit| cost_microusd >= *limit)
                    .map(|limit| {
                        (
                            Cap::Cost,
                            CapMeasurement::Microdollars(cost_microusd),
                            CapMeasurement::Microdollars(limit),
                        )
                    })
            });

        decision.map(|(cap, measured, limit)| {
            self.tripped = true;
            CapTrip {
                cap,
                measured,
                limit,
                finished: self.finished_payload(cap, measured, limit, wall_ms),
                kill_grace: Duration::from_millis(self.caps.kill_grace_ms),
            }
        })
    }

    fn finished_payload(
        &self,
        cap: Cap,
        measured: CapMeasurement,
        limit: CapMeasurement,
        duration_ms: u64,
    ) -> RunFinishedPayload {
        let totals = self.observed_totals();
        RunFinishedPayload {
            outcome: match cap {
                Cap::Wall | Cap::Idle => RunOutcome::TimedOut,
                Cap::Turns | Cap::Tokens | Cap::Cost => RunOutcome::Capped,
            },
            reason: Some(reason(cap, measured, limit)),
            truncated: Some(false),
            cost_usd: totals.cost_microusd.map(micros_to_dollars),
            usage: totals.usage.to_wire(),
            duration_ms,
            estimated: Some(true),
            extra: PayloadExtension::new(),
        }
    }
}

#[derive(Debug, Hash, PartialEq, Eq)]
enum SessionKey {
    Named(String),
    Unattributed(usize),
}

#[derive(Default)]
struct SessionObservation {
    cost_microusd: Option<u64>,
    usage: UsageLowerBound,
}

#[derive(Default)]
struct ObservedTotals {
    cost_microusd: Option<u64>,
    usage: UsageLowerBound,
}

#[derive(Debug, Error)]
pub enum StartError {
    #[error(transparent)]
    Watchdog(#[from] WatchdogError),
    #[error(transparent)]
    Sink(#[from] SinkFault),
}

#[derive(Default)]
struct UsageLowerBound {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_creation_tokens: Option<u64>,
    unit: Option<String>,
}

impl UsageLowerBound {
    fn add(&mut self, usage: RunUsage) {
        max_optional(&mut self.input_tokens, usage.input_tokens);
        max_optional(&mut self.output_tokens, usage.output_tokens);
        max_optional(&mut self.cache_read_tokens, usage.cache_read_tokens);
        max_optional(&mut self.cache_creation_tokens, usage.cache_creation_tokens);
        if usage.unit.is_some() {
            self.unit = usage.unit;
        }
    }

    fn total_tokens(&self) -> u64 {
        [
            self.input_tokens,
            self.output_tokens,
            self.cache_read_tokens,
            self.cache_creation_tokens,
        ]
        .into_iter()
        .flatten()
        .fold(0, u64::saturating_add)
    }

    fn to_wire(&self) -> Option<RunUsage> {
        let observed = self.input_tokens.is_some()
            || self.output_tokens.is_some()
            || self.cache_read_tokens.is_some()
            || self.cache_creation_tokens.is_some();
        observed.then(|| RunUsage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_read_tokens: self.cache_read_tokens,
            cache_creation_tokens: self.cache_creation_tokens,
            unit: self.unit.clone(),
            extra: PayloadExtension::new(),
        })
    }
}

fn max_optional(maximum: &mut Option<u64>, observation: Option<u64>) {
    if let Some(observation) = observation {
        *maximum = Some(maximum.unwrap_or(0).max(observation));
    }
}

fn sum_optional(total: &mut Option<u64>, value: Option<u64>) {
    if let Some(value) = value {
        *total = Some(total.unwrap_or(0).saturating_add(value));
    }
}

fn dollars_to_microusd(dollars: f64) -> Result<u64, WatchdogError> {
    let microdollars = dollars * MICRODOLLARS_PER_DOLLAR;
    if !dollars.is_finite() || dollars.is_sign_negative() || microdollars > u64::MAX as f64 {
        return Err(WatchdogError::InvalidCost);
    }
    Ok(microdollars.round() as u64)
}

fn micros_to_dollars(microdollars: u64) -> f64 {
    microdollars as f64 / MICRODOLLARS_PER_DOLLAR
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn reason(cap: Cap, measured: CapMeasurement, limit: CapMeasurement) -> String {
    match (measured, limit) {
        (CapMeasurement::Milliseconds(measured), CapMeasurement::Milliseconds(limit)) => {
            format!(
                "{} cap tripped at {measured} ms (limit {limit} ms)",
                cap.name()
            )
        }
        (CapMeasurement::Count(measured), CapMeasurement::Count(limit)) => {
            format!("{} cap tripped at {measured} (limit {limit})", cap.name())
        }
        (CapMeasurement::Microdollars(measured), CapMeasurement::Microdollars(limit)) => format!(
            "{} cap tripped at {:.6} USD (limit {:.6} USD)",
            cap.name(),
            micros_to_dollars(measured),
            micros_to_dollars(limit)
        ),
        _ => unreachable!("cap measurement and limit use the same unit"),
    }
}

fn warning_draft(message: &str) -> EventDraft {
    EventDraft {
        event_type: AGENT_WARNING.to_owned(),
        payload: typed_payload(AgentWarningPayload {
            stage: Some("start".to_owned()),
            message: message.to_owned(),
            extra: PayloadExtension::new(),
        }),
        captured_at: None,
    }
}

fn finished_draft(payload: RunFinishedPayload) -> EventDraft {
    EventDraft {
        event_type: RUN_FINISHED.to_owned(),
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

    use ethogram::{EVENT_SCHEMA_VERSION, StampFields, stamp};
    use serde_json::json;

    use super::*;

    #[derive(Clone, Default)]
    struct ManualClock(Arc<Cell<Duration>>);

    impl ManualClock {
        fn advance(&self, milliseconds: u64) {
            self.0
                .set(self.0.get() + Duration::from_millis(milliseconds));
        }
    }

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

    fn caps() -> RunCaps {
        RunCaps {
            kill_grace_ms: 0,
            ..RunCaps::default()
        }
    }

    fn event(run: &str, event_type: &str, payload: Value) -> Event {
        Event {
            v: EVENT_SCHEMA_VERSION,
            event_type: event_type.to_owned(),
            run_id: run.to_owned(),
            seq: 1,
            ts: "2030-01-02T03:04:05.000Z".to_owned(),
            payload,
            captured_at: None,
        }
    }

    fn ordinary_event(run: &str, event_type: &str) -> Event {
        event(run, event_type, json!({ "fixture": true }))
    }

    fn completed(turns: Option<u64>, tokens: Option<u64>, cost_usd: Option<f64>) -> Event {
        completed_for_session(
            None,
            turns,
            cost_usd,
            tokens.map(|input_tokens| RunUsage {
                input_tokens: Some(input_tokens),
                output_tokens: None,
                cache_read_tokens: None,
                cache_creation_tokens: None,
                unit: None,
                extra: PayloadExtension::new(),
            }),
            None,
        )
    }

    fn completed_for_session(
        session_id: Option<&str>,
        turns: Option<u64>,
        cost_usd: Option<f64>,
        usage: Option<RunUsage>,
        duration_ms: Option<u64>,
    ) -> Event {
        event(
            "run",
            AGENT_COMPLETED,
            typed_payload(AgentCompletedPayload {
                stage: Some("finish".to_owned()),
                turns,
                session_id: session_id.map(str::to_owned),
                cost_usd,
                model: None,
                usage,
                duration_ms,
                estimated: None,
                extra: PayloadExtension::new(),
            }),
        )
    }

    fn started(session_id: Option<&str>) -> Event {
        event(
            "run",
            AGENT_STARTED,
            typed_payload(AgentStartedPayload {
                stage: Some("start".to_owned()),
                model: None,
                session_id: session_id.map(str::to_owned),
                pid: None,
                extra: PayloadExtension::new(),
            }),
        )
    }

    fn usage(
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cache_read_tokens: Option<u64>,
        cache_creation_tokens: Option<u64>,
    ) -> RunUsage {
        RunUsage {
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_creation_tokens,
            unit: Some("tokens".to_owned()),
            extra: PayloadExtension::new(),
        }
    }

    fn finished(trip: CapTrip) -> RunFinishedPayload {
        serde_json::from_value(trip.into_finished_draft().payload)
            .expect("typed run.finished payload")
    }

    fn assert_named(trip: &CapTrip, cap: Cap) {
        assert_eq!(trip.cap(), cap);
        assert!(
            trip.finished()
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains(&format!("{} cap", cap.name())))
        );
    }

    #[test]
    fn wall_cap_trips_in_isolation_and_names_itself() {
        let clock = ManualClock::default();
        let mut watchdog = CapsWatchdog::new(
            RunCaps {
                wall_ms: Some(10),
                ..caps()
            },
            clock.clone(),
        )
        .expect("watchdog");

        clock.advance(10);
        let trip = watchdog.check().expect("wall cap trip");

        assert_named(&trip, Cap::Wall);
        assert_eq!(trip.measured(), CapMeasurement::Milliseconds(10));
        assert_eq!(trip.finished().outcome, RunOutcome::TimedOut);
        assert_eq!(trip.into_finished_draft().event_type, RUN_FINISHED);
    }

    #[test]
    fn idle_cap_trips_in_isolation_and_names_itself() {
        let clock = ManualClock::default();
        let mut watchdog = CapsWatchdog::new(
            RunCaps {
                idle_ms: Some(20),
                ..caps()
            },
            clock.clone(),
        )
        .expect("watchdog");

        clock.advance(20);
        let trip = watchdog.check().expect("idle cap trip");

        assert_named(&trip, Cap::Idle);
        assert_eq!(trip.measured(), CapMeasurement::Milliseconds(20));
        assert_eq!(trip.finished().outcome, RunOutcome::TimedOut);
        assert_eq!(trip.into_finished_draft().event_type, RUN_FINISHED);
    }

    #[test]
    fn turns_cap_trips_in_isolation_and_names_itself() {
        let clock = ManualClock::default();
        let mut watchdog = CapsWatchdog::new(
            RunCaps {
                turns: Some(2),
                ..caps()
            },
            clock,
        )
        .expect("watchdog");

        let trip = watchdog
            .observe(&completed(Some(2), None, None))
            .expect("valid event")
            .expect("turn cap trip");

        assert_named(&trip, Cap::Turns);
        assert_eq!(trip.measured(), CapMeasurement::Count(2));
        assert_eq!(trip.finished().outcome, RunOutcome::Capped);
        assert_eq!(trip.into_finished_draft().event_type, RUN_FINISHED);
    }

    #[test]
    fn tokens_cap_trips_in_isolation_and_names_itself() {
        let clock = ManualClock::default();
        let mut watchdog = CapsWatchdog::new(
            RunCaps {
                tokens: Some(30),
                ..caps()
            },
            clock,
        )
        .expect("watchdog");

        let trip = watchdog
            .observe(&completed(None, Some(30), None))
            .expect("valid event")
            .expect("token cap trip");

        assert_named(&trip, Cap::Tokens);
        assert_eq!(trip.measured(), CapMeasurement::Count(30));
        assert_eq!(trip.finished().outcome, RunOutcome::Capped);
        assert_eq!(trip.into_finished_draft().event_type, RUN_FINISHED);
    }

    #[test]
    fn cost_cap_trips_in_isolation_and_names_itself() {
        let clock = ManualClock::default();
        let mut watchdog = CapsWatchdog::new(
            RunCaps {
                cost_usd: Some(0.3),
                ..caps()
            },
            clock,
        )
        .expect("watchdog");

        assert!(
            watchdog
                .observe(&completed(None, None, Some(0.1)))
                .expect("first cost")
                .is_none()
        );
        let trip = watchdog
            .observe(&completed(None, None, Some(0.2)))
            .expect("second cost")
            .expect("cost cap trip");

        assert_named(&trip, Cap::Cost);
        assert_eq!(trip.measured(), CapMeasurement::Microdollars(300_000));
        assert_eq!(trip.finished().outcome, RunOutcome::Capped);
        assert_eq!(trip.into_finished_draft().event_type, RUN_FINISHED);
    }

    #[test]
    fn repeated_session_total_does_not_trip_cost_cap() {
        let clock = ManualClock::default();
        let mut watchdog = CapsWatchdog::new(
            RunCaps {
                cost_usd: Some(0.15),
                ..caps()
            },
            clock,
        )
        .expect("watchdog");
        let session_id = "1a425592-726e-4137-9dee-13ec62709123";
        let first = event(
            "run",
            AGENT_COMPLETED,
            typed_payload(AgentCompletedPayload {
                stage: Some("finish".to_owned()),
                turns: Some(3),
                session_id: Some(session_id.to_owned()),
                cost_usd: Some(0.097_651_900_000_000_01),
                model: None,
                usage: None,
                duration_ms: Some(12_357),
                estimated: None,
                extra: PayloadExtension::new(),
            }),
        );
        let second = event(
            "run",
            AGENT_COMPLETED,
            typed_payload(AgentCompletedPayload {
                stage: Some("finish".to_owned()),
                turns: Some(1),
                session_id: Some(session_id.to_owned()),
                cost_usd: Some(0.097_651_900_000_000_01),
                model: None,
                usage: None,
                duration_ms: Some(1_664),
                estimated: None,
                extra: PayloadExtension::new(),
            }),
        );

        assert!(
            watchdog
                .observe(&first)
                .expect("first completion")
                .is_none()
        );
        assert!(
            watchdog
                .observe(&second)
                .expect("second completion")
                .is_none()
        );
    }

    #[test]
    fn repeated_session_cost_is_folded_to_the_maximum() {
        let clock = ManualClock::default();
        let mut watchdog = CapsWatchdog::new(
            RunCaps {
                wall_ms: Some(1),
                ..caps()
            },
            clock.clone(),
        )
        .expect("watchdog");

        watchdog
            .observe(&completed_for_session(
                Some("session"),
                None,
                Some(0.097_651_900_000_000_01),
                None,
                None,
            ))
            .expect("first completion");
        watchdog
            .observe(&completed_for_session(
                Some("session"),
                None,
                Some(0.097_651_900_000_000_01),
                None,
                None,
            ))
            .expect("second completion");
        clock.advance(1);

        let payload = finished(watchdog.check().expect("wall cap trip"));
        assert_eq!(
            dollars_to_microusd(payload.cost_usd.expect("observed cost")).expect("valid cost"),
            97_652
        );
    }

    #[test]
    fn completion_falls_back_to_most_recent_started_session() {
        let clock = ManualClock::default();
        let mut watchdog = CapsWatchdog::new(
            RunCaps {
                wall_ms: Some(1),
                ..caps()
            },
            clock.clone(),
        )
        .expect("watchdog");
        watchdog
            .observe(&started(Some("session")))
            .expect("session start");

        for (turns, duration_ms) in [(3, 12_357), (1, 1_664)] {
            watchdog
                .observe(&completed_for_session(
                    None,
                    Some(turns),
                    Some(0.097_651_900_000_000_01),
                    None,
                    Some(duration_ms),
                ))
                .expect("completion");
        }
        clock.advance(1);

        let payload = finished(watchdog.check().expect("wall cap trip"));
        assert_eq!(
            dollars_to_microusd(payload.cost_usd.expect("observed cost")).expect("valid cost"),
            97_652
        );
    }

    #[test]
    fn distinct_session_costs_are_summed() {
        let mut watchdog = CapsWatchdog::new(
            RunCaps {
                cost_usd: Some(0.15),
                ..caps()
            },
            ManualClock::default(),
        )
        .expect("watchdog");
        watchdog
            .observe(&started(Some("session-a")))
            .expect("session start");
        watchdog
            .observe(&completed_for_session(
                Some("session-a"),
                None,
                Some(0.097_651_900_000_000_01),
                None,
                None,
            ))
            .expect("first session");

        let trip = watchdog
            .observe(&completed_for_session(
                Some("session-b"),
                None,
                Some(0.097_651_900_000_000_01),
                None,
                None,
            ))
            .expect("second session")
            .expect("cost cap trip");

        assert_eq!(trip.measured(), CapMeasurement::Microdollars(195_304));
        assert_eq!(
            dollars_to_microusd(trip.finished().cost_usd.expect("observed cost"))
                .expect("valid cost"),
            195_304
        );
    }

    #[test]
    fn unattributable_completion_is_its_own_summed_session() {
        let mut watchdog = CapsWatchdog::new(
            RunCaps {
                cost_usd: Some(0.15),
                ..caps()
            },
            ManualClock::default(),
        )
        .expect("watchdog");
        watchdog
            .observe(&completed_for_session(
                Some("known-session"),
                None,
                Some(0.1),
                None,
                None,
            ))
            .expect("known session");

        let trip = watchdog
            .observe(&completed_for_session(None, None, Some(0.05), None, None))
            .expect("unattributable completion")
            .expect("cost cap trip");

        assert_eq!(trip.measured(), CapMeasurement::Microdollars(150_000));
    }

    #[test]
    fn turns_remain_summed_within_a_session() {
        let mut watchdog = CapsWatchdog::new(
            RunCaps {
                turns: Some(4),
                ..caps()
            },
            ManualClock::default(),
        )
        .expect("watchdog");
        watchdog
            .observe(&completed_for_session(
                Some("session"),
                Some(3),
                None,
                None,
                None,
            ))
            .expect("first invocation");

        let trip = watchdog
            .observe(&completed_for_session(
                Some("session"),
                Some(1),
                None,
                None,
                None,
            ))
            .expect("second invocation")
            .expect("turn cap trip");

        assert_eq!(trip.measured(), CapMeasurement::Count(4));
    }

    #[test]
    fn usage_takes_field_maxima_within_sessions_and_sums_across_them() {
        let mut watchdog = CapsWatchdog::new(
            RunCaps {
                tokens: Some(33),
                ..caps()
            },
            ManualClock::default(),
        )
        .expect("watchdog");
        watchdog
            .observe(&completed_for_session(
                Some("session-a"),
                None,
                None,
                Some(usage(Some(10), Some(5), Some(7), Some(1))),
                None,
            ))
            .expect("first session total");
        watchdog
            .observe(&completed_for_session(
                Some("session-a"),
                None,
                None,
                Some(usage(Some(8), Some(6), None, Some(2))),
                None,
            ))
            .expect("updated session total");

        let trip = watchdog
            .observe(&completed_for_session(
                Some("session-b"),
                None,
                None,
                Some(usage(Some(4), Some(3), Some(1), None)),
                None,
            ))
            .expect("second session total")
            .expect("token cap trip");

        assert_eq!(trip.measured(), CapMeasurement::Count(33));
        let folded = trip.finished().usage.as_ref().expect("observed usage");
        assert_eq!(folded.input_tokens, Some(14));
        assert_eq!(folded.output_tokens, Some(9));
        assert_eq!(folded.cache_read_tokens, Some(8));
        assert_eq!(folded.cache_creation_tokens, Some(2));
    }

    #[test]
    fn idle_does_not_trip_during_a_tool_call_longer_than_its_bound() {
        let clock = ManualClock::default();
        let mut watchdog = CapsWatchdog::new(
            RunCaps {
                idle_ms: Some(10),
                ..caps()
            },
            clock.clone(),
        )
        .expect("watchdog");
        watchdog
            .observe(&ordinary_event("run", AGENT_TOOL_USE))
            .expect("tool use");

        clock.advance(100);

        assert!(watchdog.check().is_none());
    }

    #[test]
    fn idle_resumes_only_after_the_last_concurrent_tool_call_closes() {
        let clock = ManualClock::default();
        let mut watchdog = CapsWatchdog::new(
            RunCaps {
                idle_ms: Some(10),
                ..caps()
            },
            clock.clone(),
        )
        .expect("watchdog");
        watchdog
            .observe(&ordinary_event("run", AGENT_TOOL_USE))
            .expect("first tool use");
        watchdog
            .observe(&ordinary_event("run", AGENT_TOOL_USE))
            .expect("second tool use");
        clock.advance(100);
        watchdog
            .observe(&ordinary_event("run", AGENT_TOOL_RESULT))
            .expect("first tool result");
        clock.advance(100);
        assert!(watchdog.check().is_none());

        watchdog
            .observe(&ordinary_event("run", AGENT_TOOL_RESULT))
            .expect("second tool result");
        clock.advance(10);

        assert_eq!(watchdog.check().expect("idle resumed").cap(), Cap::Idle);
    }

    #[test]
    fn unpaired_tool_result_cannot_cancel_a_later_suspension() {
        let clock = ManualClock::default();
        let mut watchdog = CapsWatchdog::new(
            RunCaps {
                idle_ms: Some(10),
                ..caps()
            },
            clock.clone(),
        )
        .expect("watchdog");
        watchdog
            .observe(&ordinary_event("run", AGENT_TOOL_RESULT))
            .expect("unpaired result");
        watchdog
            .observe(&ordinary_event("run", AGENT_TOOL_USE))
            .expect("genuine tool use");

        clock.advance(100);

        assert!(watchdog.check().is_none());
    }

    #[test]
    fn subagent_event_inside_parent_tool_call_resets_idle_timer() {
        let clock = ManualClock::default();
        let mut watchdog = CapsWatchdog::new(
            RunCaps {
                idle_ms: Some(10),
                ..caps()
            },
            clock.clone(),
        )
        .expect("watchdog");
        watchdog
            .observe(&ordinary_event("parent", AGENT_TOOL_USE))
            .expect("parent tool use");
        clock.advance(7);

        watchdog
            .observe(&ordinary_event("subagent", "agent.text"))
            .expect("subagent activity");

        assert_eq!(watchdog.last_observed_at, Duration::from_millis(7));
        assert_eq!(watchdog.open_tool_calls, 1);
    }

    #[test]
    fn cap_kill_reports_observed_nonzero_usage_as_an_estimated_lower_bound() {
        let clock = ManualClock::default();
        let mut watchdog = CapsWatchdog::new(
            RunCaps {
                tokens: Some(5),
                ..caps()
            },
            clock,
        )
        .expect("watchdog");

        let trip = watchdog
            .observe(&completed(None, Some(5), None))
            .expect("valid event")
            .expect("token cap trip");
        let payload = finished(trip);

        assert_eq!(payload.estimated, Some(true));
        assert_eq!(payload.usage.expect("observed usage").input_tokens, Some(5));
    }

    #[test]
    fn simultaneous_caps_emit_one_outcome_with_documented_precedence() {
        let clock = ManualClock::default();
        let mut watchdog = CapsWatchdog::new(
            RunCaps {
                wall_ms: Some(10),
                idle_ms: Some(10),
                ..caps()
            },
            clock.clone(),
        )
        .expect("watchdog");
        clock.advance(10);

        let outcomes = [watchdog.check(), watchdog.check()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();

        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].cap(), Cap::Wall);
    }

    #[test]
    fn idle_without_wall_emits_a_run_start_warning() {
        let sink = MemorySink::default();
        CapsWatchdog::start(
            RunCaps {
                idle_ms: Some(10),
                ..caps()
            },
            ManualClock::default(),
            &sink,
            "run",
        )
        .expect("watchdog start");

        let events = sink.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, AGENT_WARNING);
        let warning = serde_json::from_value::<AgentWarningPayload>(events[0].payload.clone())
            .expect("typed warning");
        assert!(
            warning
                .message
                .contains("does not bound a hanging tool call")
        );
    }

    #[test]
    fn idle_with_wall_emits_no_run_start_warning() {
        let sink = MemorySink::default();
        CapsWatchdog::start(
            RunCaps {
                wall_ms: Some(100),
                idle_ms: Some(10),
                ..caps()
            },
            ManualClock::default(),
            &sink,
            "run",
        )
        .expect("watchdog start");

        assert!(sink.events().is_empty());
    }
}
