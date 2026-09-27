//! Wall and idle caps for one run, and the defaults that apply when a manifest
//! declares none (#619).
//!
//! These constants are the one place the defaults live. `ostrom pass`,
//! `ostrom implement`, the generated loop units, the stall reaper, `ostrom ps`
//! and `ostrom doctor` all read them from here, so a default cannot drift
//! between the process that enforces it and the surfaces that report it.
//!
//! An undeclared cap is a default, never "unbounded": a run that stops moving
//! otherwise holds its item until someone notices. Neither kind of run gets an
//! idle default, because an idle cap alone is weaker than a wall cap: a hung
//! tool call suspends idle timing indefinitely, and only a wall cap bounds it.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// The wall cap of an implementer run when the manifest declares none.
pub const DEFAULT_IMPLEMENTER_WALL_SECONDS: u64 = 4 * 60 * 60;

/// The wall cap of a pass (and of any loop run) when the manifest declares
/// none. It equals the fixed `TimeoutStartSec` loop units carried before
/// #619, so an undeclared loop keeps the bound it already had. It must also
/// leave `MINIMUM_PASS_WORK_SECONDS` after the sweep lease wait ceiling, which
/// `every_loop_services_timeout_agrees_with_the_sweep_lease_ceiling` checks
/// against the rendered units.
pub const DEFAULT_PASS_WALL_SECONDS: u64 = 30 * 60;

/// The grace between `SIGTERM` and `SIGKILL` when a run is stopped, and the
/// margin the stall threshold of a run with no idle cap adds to its wall cap
/// so the in-process watchdog fires first. A systemd unit's own bound adds
/// more (`IMPLEMENTER_UNIT_RUNTIME_MARGIN_SECONDS`,
/// `LOOP_UNIT_TIMEOUT_MARGIN_SECONDS`).
pub const RUN_TERMINATION_GRACE_SECONDS: u64 = 5;

/// The margin a systemd implementer unit's `RuntimeMaxSec` adds to the wall cap
/// (#635 N2). `RuntimeMaxSec` counts from the unit's start, while the
/// implementer's wall cap counts from after its preflight (source resolution,
/// fetch, worktree), and once the cap trips the run still needs its
/// termination grace to stop Codex and write its `wall-cap` row. Five seconds
/// left systemd's signal racing that row; two minutes leaves the in-process
/// watchdog comfortably first.
pub const IMPLEMENTER_UNIT_RUNTIME_MARGIN_SECONDS: u64 = 120;

/// The margin a rendered loop unit's `TimeoutStartSec` adds to the loop's wall
/// cap (#637, carried over from #635 N2). It is the same race: the timeout
/// counts from the unit's start, before `ostrom loop run` has started the pass
/// and its watchdog, and once the wall cap trips the pass still needs its
/// termination grace to stop its harness and write its own `pass-ended`. Five
/// seconds left systemd's `SIGTERM` racing that row.
pub const LOOP_UNIT_TIMEOUT_MARGIN_SECONDS: u64 = 5; // MUTATION A

/// The cost ceiling of one run, in US dollars, when nothing declares one: the
/// default a work order is created with, and what the stall reaper charges a
/// reaped pass that declared no ceiling of its own. One value, so the two
/// fallbacks cannot disagree (principle 6). It is a per-run figure on purpose:
/// charging a reaped pass the whole daily cap would hold every other pass for
/// the rest of the day.
pub const DEFAULT_RUN_COST_CEILING_USD: f64 = 20.0;

/// A positive duration such as `90m`, `2s`, `4h` or `1d`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapDuration {
    value: String,
    seconds: u64,
}

impl CapDuration {
    #[must_use]
    pub const fn as_seconds(&self) -> u64 {
        self.seconds
    }
}

impl fmt::Display for CapDuration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.value)
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("`wall` and `idle` must be positive durations such as 90m, 2h or 30s")]
pub struct CapDurationError;

impl FromStr for CapDuration {
    type Err = CapDurationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let split = value
            .find(|character: char| !character.is_ascii_digit())
            .ok_or(CapDurationError)?;
        let (amount, unit) = value.split_at(split);
        let amount = amount.parse::<u64>().map_err(|_| CapDurationError)?;
        let multiplier = match unit {
            "s" => 1,
            "m" => 60,
            "h" => 3_600,
            "d" => 86_400,
            _ => return Err(CapDurationError),
        };
        let seconds = amount
            .checked_mul(multiplier)
            .filter(|seconds| *seconds > 0)
            .ok_or(CapDurationError)?;
        Ok(Self {
            value: value.to_owned(),
            seconds,
        })
    }
}

impl Serialize for CapDuration {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.value)
    }
}

impl<'de> Deserialize<'de> for CapDuration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// The wall and idle caps one run is started with, after defaults applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedRunCaps {
    pub wall_seconds: u64,
    /// False when `wall_seconds` is the default rather than a declaration.
    pub wall_declared: bool,
    pub idle_seconds: Option<u64>,
}

impl ResolvedRunCaps {
    #[must_use]
    pub fn resolve(
        wall: Option<&CapDuration>,
        idle: Option<&CapDuration>,
        default_wall_seconds: u64,
    ) -> Self {
        Self {
            wall_seconds: wall.map_or(default_wall_seconds, CapDuration::as_seconds),
            wall_declared: wall.is_some(),
            idle_seconds: idle.map(CapDuration::as_seconds),
        }
    }

    /// The caps of a pass no manifest bounds.
    #[must_use]
    pub const fn pass_default() -> Self {
        Self {
            wall_seconds: DEFAULT_PASS_WALL_SECONDS,
            wall_declared: false,
            idle_seconds: None,
        }
    }

    /// The caps of an implementer no manifest bounds.
    #[must_use]
    pub const fn implementer_default() -> Self {
        Self {
            wall_seconds: DEFAULT_IMPLEMENTER_WALL_SECONDS,
            wall_declared: false,
            idle_seconds: None,
        }
    }

    /// How long a live run may go without progress before it is stalled: its
    /// idle cap, or with none its wall cap plus the termination grace, a
    /// backstop for a watchdog that itself died.
    #[must_use]
    pub const fn stall_threshold_seconds(&self) -> u64 {
        match self.idle_seconds {
            Some(idle) => idle,
            None => self
                .wall_seconds
                .saturating_add(RUN_TERMINATION_GRACE_SECONDS),
        }
    }

    /// The `TimeoutStartSec` of a rendered loop unit: the wall cap plus
    /// [`LOOP_UNIT_TIMEOUT_MARGIN_SECONDS`].
    #[must_use]
    pub const fn loop_unit_timeout_seconds(&self) -> u64 {
        self.wall_seconds
            .saturating_add(LOOP_UNIT_TIMEOUT_MARGIN_SECONDS)
    }

    /// The `RuntimeMaxSec` of a systemd implementer unit: the wall cap plus
    /// [`IMPLEMENTER_UNIT_RUNTIME_MARGIN_SECONDS`].
    #[must_use]
    pub const fn implementer_unit_runtime_seconds(&self) -> u64 {
        self.wall_seconds
            .saturating_add(IMPLEMENTER_UNIT_RUNTIME_MARGIN_SECONDS)
    }

    /// `4h (default)` or `90m`, for `ostrom ps` and `ostrom doctor`.
    #[must_use]
    pub fn render_wall(&self) -> String {
        let rendered = render_seconds(self.wall_seconds);
        if self.wall_declared {
            rendered
        } else {
            format!("{rendered} (default)")
        }
    }

    /// `-` when no idle cap applies.
    #[must_use]
    pub fn render_idle(&self) -> String {
        self.idle_seconds
            .map_or_else(|| "-".to_owned(), render_seconds)
    }
}

/// The largest whole unit that renders `seconds` exactly.
#[must_use]
pub fn render_seconds(seconds: u64) -> String {
    for (unit, size) in [("d", 86_400), ("h", 3_600), ("m", 60)] {
        if seconds >= size && seconds / size * size == seconds {
            return format!("{}{unit}", seconds / size);
        }
    }
    format!("{seconds}s")
}

#[cfg(test)]
mod tests {
    use super::{CapDuration, ResolvedRunCaps, render_seconds};

    #[test]
    fn durations_parse_and_render_in_whole_units() {
        let parsed = ["2s", "90m", "4h", "1d"]
            .map(|value| value.parse::<CapDuration>().map(|value| value.as_seconds()));
        assert_eq!(parsed, [Ok(2), Ok(5_400), Ok(14_400), Ok(86_400)]);
        for invalid in ["0s", "90", "m", "5w", "-1m", ""] {
            assert!(invalid.parse::<CapDuration>().is_err(), "{invalid}");
        }
        assert_eq!(
            [5_400, 14_400, 1_805, 1_800].map(render_seconds),
            ["90m", "4h", "1805s", "30m"]
        );
        assert_eq!(
            ResolvedRunCaps::implementer_default().render_wall(),
            "4h (default)"
        );
    }
}
