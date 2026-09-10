//! Replay-then-follow state and its polling driver.

use std::thread;
use std::time::Duration;

use ethogram::{Event, RUN_FINISHED};

use crate::sink::{Source, SourceFault};
use crate::watchdog::Clock;

/// Delay between reads while a followed run remains live.
pub const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Maximum lifetime of one follow connection before its caller must reconnect.
pub const LIFETIME_CAP: Duration = Duration::from_secs(60 * 60);

/// Status after applying one source poll to a [`FollowState`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FollowStatus {
    /// The run is still live and should be polled again.
    Following,
    /// The returned batch includes the run's stored `run.finished` event.
    Terminal,
    /// The lifetime cap was reached; reconnect with the last handled sequence.
    ReconnectToResume,
}

/// Events and status produced from one source poll.
#[derive(Clone, Debug, PartialEq)]
pub struct FollowPoll {
    /// Events to deliver before acting on `status`.
    pub events: Vec<Event>,
    /// Whether to poll again, close as terminal, or reconnect to resume.
    pub status: FollowStatus,
}

/// Final non-fault outcome from [`follow`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FollowExit {
    /// The source contained `run.finished` and every queued event was delivered.
    Terminal,
    /// The run was still live at the lifetime cap and may be resumed by sequence.
    ReconnectToResume,
}

/// Timing-independent replay-then-follow state for one run.
///
/// Tests drive this type with a fake implementation of the watchdog's shared
/// [`Clock`] trait. It never manufactures a terminal event: only a stored event
/// whose type is `run.finished` produces [`FollowStatus::Terminal`].
pub struct FollowState<C> {
    clock: C,
    started_at: Duration,
    after: u64,
    terminal: bool,
}

impl<C: Clock> FollowState<C> {
    /// Begin following strictly after `after`.
    pub fn new(after: u64, clock: C) -> Self {
        let started_at = clock.now();
        Self {
            clock,
            started_at,
            after,
            terminal: false,
        }
    }

    /// Highest sequence consumed so far, passed verbatim to the next source read.
    #[must_use]
    pub const fn after(&self) -> u64 {
        self.after
    }

    /// Apply one source poll and return all events plus its resulting status.
    ///
    /// A source fault is returned unchanged. Terminal detection happens before
    /// the lifetime check so a terminal event observed at the deadline remains a
    /// terminal exit, and every event in that poll remains available to flush.
    pub fn poll(
        &mut self,
        poll: Result<Vec<Event>, SourceFault>,
    ) -> Result<FollowPoll, SourceFault> {
        let events = poll?;

        if self.terminal {
            return Ok(FollowPoll {
                events: Vec::new(),
                status: FollowStatus::Terminal,
            });
        }

        for event in &events {
            self.after = event.seq;
            if event.event_type == RUN_FINISHED {
                self.terminal = true;
            }
        }

        let status = if self.terminal {
            FollowStatus::Terminal
        } else if self.clock.now().saturating_sub(self.started_at) >= LIFETIME_CAP {
            FollowStatus::ReconnectToResume
        } else {
            FollowStatus::Following
        };

        Ok(FollowPoll { events, status })
    }
}

/// Replay and then follow a run, delivering each complete poll before exiting.
///
/// The driver sleeps only between live polls. On `run.finished`, it invokes
/// `deliver` for every queued event before returning [`FollowExit::Terminal`].
/// At the lifetime cap it likewise flushes that poll, then returns the distinct
/// reconnect outcome. Source faults are surfaced as `Err`.
pub fn follow<S, C, F>(
    source: &S,
    run: &str,
    after: u64,
    clock: C,
    mut deliver: F,
) -> Result<FollowExit, SourceFault>
where
    S: Source + ?Sized,
    C: Clock,
    F: FnMut(Event),
{
    let mut state = FollowState::new(after, clock);
    loop {
        let batch = state.poll(source.read_from(run, state.after()))?;
        for event in batch.events {
            deliver(event);
        }
        match batch.status {
            FollowStatus::Following => thread::sleep(POLL_INTERVAL),
            FollowStatus::Terminal => return Ok(FollowExit::Terminal),
            FollowStatus::ReconnectToResume => return Ok(FollowExit::ReconnectToResume),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use ethogram::{EVENT_SCHEMA_VERSION, Event};
    use serde_json::json;

    use super::*;

    struct FixtureSource {
        events: Vec<Event>,
    }

    impl Source for FixtureSource {
        fn read_from(&self, _run: &str, after: u64) -> Result<Vec<Event>, SourceFault> {
            Ok(self
                .events
                .iter()
                .filter(|event| event.seq > after)
                .cloned()
                .collect())
        }
    }

    struct FaultSource(SourceFault);

    impl Source for FaultSource {
        fn read_from(&self, _run: &str, _after: u64) -> Result<Vec<Event>, SourceFault> {
            Err(self.0.clone())
        }
    }

    #[derive(Clone, Default)]
    struct FakeClock(Rc<Cell<Duration>>);

    impl FakeClock {
        fn advance(&self, duration: Duration) {
            self.0.set(self.0.get() + duration);
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> Duration {
            self.0.get()
        }
    }

    #[derive(Default)]
    struct ExpiringClock(Cell<bool>);

    impl Clock for ExpiringClock {
        fn now(&self) -> Duration {
            if self.0.replace(true) {
                LIFETIME_CAP
            } else {
                Duration::ZERO
            }
        }
    }

    fn event(seq: u64, event_type: &str) -> Event {
        Event {
            v: EVENT_SCHEMA_VERSION,
            event_type: event_type.to_owned(),
            run_id: "run".to_owned(),
            seq,
            ts: format!("2026-09-07T00:00:{seq:02}.000Z"),
            payload: json!({ "opaque": true }),
            captured_at: None,
        }
    }

    #[test]
    fn terminal_poll_keeps_every_queued_event_for_flushing() {
        let mut state = FollowState::new(0, FakeClock::default());
        let queued = vec![event(1, "test.first"), event(2, RUN_FINISHED)];

        let poll = state.poll(Ok(queued.clone())).expect("terminal poll");

        assert_eq!(poll.events, queued);
        assert_eq!(poll.status, FollowStatus::Terminal);
        assert_eq!(state.after(), 2);
    }

    #[test]
    fn driver_flushes_the_terminal_batch_before_closing() {
        let queued = vec![event(1, "test.first"), event(2, RUN_FINISHED)];
        let source = FixtureSource {
            events: queued.clone(),
        };
        let mut delivered = Vec::new();

        let exit = follow(&source, "run", 0, FakeClock::default(), |event| {
            delivered.push(event);
        })
        .expect("follow terminal run");

        assert_eq!(exit, FollowExit::Terminal);
        assert_eq!(delivered, queued);
    }

    #[test]
    fn driver_reports_reconnect_for_a_run_without_a_terminal_event() {
        let only = event(1, "test.only");
        let source = FixtureSource {
            events: vec![only.clone()],
        };
        let mut delivered = Vec::new();

        let exit = follow(&source, "run", 0, ExpiringClock::default(), |event| {
            delivered.push(event);
        })
        .expect("follow live run to lifetime cap");

        assert_eq!(exit, FollowExit::ReconnectToResume);
        assert_eq!(delivered, vec![only]);
        assert!(
            delivered
                .iter()
                .all(|event| event.event_type != RUN_FINISHED)
        );
    }

    #[test]
    fn driver_surfaces_source_faults() {
        let fault = SourceFault::Gap {
            expected: 1,
            got: 2,
        };
        let source = FaultSource(fault.clone());

        assert_eq!(
            follow(&source, "run", 0, FakeClock::default(), |_| {}),
            Err(fault)
        );
    }

    #[test]
    fn lifetime_cap_is_distinguishable_from_terminal() {
        let clock = FakeClock::default();
        let mut state = FollowState::new(0, clock.clone());
        clock.advance(LIFETIME_CAP);

        let poll = state.poll(Ok(Vec::new())).expect("lifetime poll");

        assert!(poll.events.is_empty());
        assert_eq!(poll.status, FollowStatus::ReconnectToResume);
    }

    #[test]
    fn missing_terminal_event_is_not_invented_and_ages_out() {
        let clock = FakeClock::default();
        let mut state = FollowState::new(0, clock.clone());
        let first = state
            .poll(Ok(vec![event(1, "test.only")]))
            .expect("live poll");
        assert_eq!(first.status, FollowStatus::Following);
        assert!(
            first
                .events
                .iter()
                .all(|event| event.event_type != RUN_FINISHED)
        );

        clock.advance(LIFETIME_CAP);
        let expired = state.poll(Ok(Vec::new())).expect("expired poll");
        assert_eq!(expired.status, FollowStatus::ReconnectToResume);
        assert!(expired.events.is_empty());
    }

    #[test]
    fn source_fault_is_surfaced_unchanged() {
        let mut state = FollowState::new(0, FakeClock::default());
        let fault = SourceFault::Gap {
            expected: 1,
            got: 2,
        };

        assert_eq!(state.poll(Err(fault.clone())), Err(fault));
        assert_eq!(state.after(), 0);
    }
}
