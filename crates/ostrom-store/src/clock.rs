use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, SecondsFormat, Utc};

/// The sole wall-clock source for Ostrom workflow decisions and records.
///
/// Production constructs a realtime clock at the process boundary and threads
/// it through requests. Tests that need a stable instant inject a fixed clock
/// directly instead of changing process-global environment state.
#[derive(Clone, Debug)]
pub struct Clock {
    source: ClockSource,
}

#[derive(Clone, Debug)]
enum ClockSource {
    Realtime,
    Fixed(DateTime<Utc>),
}

impl Clock {
    #[must_use]
    pub const fn realtime() -> Self {
        Self {
            source: ClockSource::Realtime,
        }
    }

    #[must_use]
    pub const fn fixed(now: DateTime<Utc>) -> Self {
        Self {
            source: ClockSource::Fixed(now),
        }
    }

    #[must_use]
    pub fn now(&self) -> DateTime<Utc> {
        match self.source {
            ClockSource::Realtime => DateTime::<Utc>::from(SystemTime::now()),
            ClockSource::Fixed(now) => now,
        }
    }

    #[must_use]
    pub fn epoch_seconds(&self) -> u64 {
        match self.source {
            ClockSource::Realtime => SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            ClockSource::Fixed(now) => u64::try_from(now.timestamp()).unwrap_or_default(),
        }
    }

    #[must_use]
    pub fn timestamp(&self) -> String {
        self.now().to_rfc3339_opts(SecondsFormat::Secs, true)
    }

    #[must_use]
    pub fn date(&self) -> String {
        self.now().format("%Y-%m-%d").to_string()
    }

    #[must_use]
    pub const fn is_fixed(&self) -> bool {
        matches!(self.source, ClockSource::Fixed(_))
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::realtime()
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};

    use super::Clock;

    #[test]
    fn fixed_clock_exposes_one_instant_in_every_representation() {
        let now = DateTime::parse_from_rfc3339("2026-08-01T01:02:03Z")
            .expect("fixed timestamp")
            .with_timezone(&Utc);
        let clock = Clock::fixed(now);

        assert_eq!(clock.now(), now);
        assert_eq!(clock.epoch_seconds(), 1_785_546_123);
        assert_eq!(clock.timestamp(), "2026-08-01T01:02:03Z");
        assert_eq!(clock.date(), "2026-08-01");
    }
}
