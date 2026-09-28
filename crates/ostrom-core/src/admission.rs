//! Resource admission: whether the machine can take a new launch (#628).
//!
//! Reading the machine (CPU package temperature, load average) is umwelt's
//! job, since it observes the host. Deciding whether an already-taken
//! reading admits a launch is ostrom's, so this module is pure: it never
//! touches a filesystem or a clock. The caller (`ostrom-store`'s
//! `umwelt_edge`, which is allowed to depend on umwelt) takes the reading and
//! hands it here as an [`AdmissionReading`].
//!
//! Undeclared limits admit (principle 2): a solo operator on a machine with
//! no sensors is unaffected. A declared limit whose reading could not be
//! taken refuses loudly with a distinct reason rather than silently
//! admitting (principles 5 and 7).

use serde::{Deserialize, Serialize};

/// Declared admission limits: `defaults.admission`, optionally overridden per
/// loop. Each field falls back to the default independently, the same way
/// `defaults.loop`'s `wall`/`idle` already do (`LoopDefaults`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionLimits {
    /// The hottest CPU package/core sensor, in degrees Celsius.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cpu_temp_c: Option<f64>,
    /// The 1-minute load average divided by the online CPU count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_load_per_cpu: Option<f64>,
    /// Resource properties a rendered unit carries, regardless of whether
    /// either limit above is declared.
    #[serde(default, skip_serializing_if = "UnitResourceLimits::is_empty")]
    pub unit: UnitResourceLimits,
}

impl AdmissionLimits {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.max_cpu_temp_c.is_none() && self.max_load_per_cpu.is_none() && self.unit.is_empty()
    }

    /// This declaration, with every unset field falling back to `defaults`.
    #[must_use]
    pub fn override_defaults(&self, defaults: &Self) -> Self {
        Self {
            max_cpu_temp_c: self.max_cpu_temp_c.or(defaults.max_cpu_temp_c),
            max_load_per_cpu: self.max_load_per_cpu.or(defaults.max_load_per_cpu),
            unit: self.unit.override_defaults(&defaults.unit),
        }
    }

    /// Judge an already-taken reading against these limits.
    ///
    /// Temperature is checked before load, so when both are declared and both
    /// would hold the launch, the decision is deterministic. An undeclared
    /// metric is never read here: its `AdmissionReading` field may be `Err`
    /// with no effect on the decision.
    #[must_use]
    pub fn decide(&self, reading: &AdmissionReading) -> AdmissionDecision {
        if let Some(limit) = self.max_cpu_temp_c
            && let Some(decision) = judge("max_cpu_temp_c", limit, &reading.cpu_temp_c)
        {
            return decision;
        }
        if let Some(limit) = self.max_load_per_cpu
            && let Some(decision) = judge("max_load_per_cpu", limit, &reading.load_per_cpu)
        {
            return decision;
        }
        AdmissionDecision::Admit
    }
}

fn judge(
    metric: &'static str,
    limit: f64,
    reading: &Result<f64, String>,
) -> Option<AdmissionDecision> {
    match reading {
        Err(detail) => Some(AdmissionDecision::Held {
            reason: AdmissionHoldReason::SensorUnreadable,
            metric,
            limit,
            reading: None,
            detail: detail.clone(),
        }),
        // TEMPORARY mutation for principle-7 evidence (#628): never holds.
        Ok(value) if *value > limit + 1_000_000.0 => Some(AdmissionDecision::Held {
            reason: AdmissionHoldReason::OverLimit,
            metric,
            limit,
            reading: Some(*value),
            detail: String::new(),
        }),
        Ok(_) => None,
    }
}

/// A CPU temperature and a load-per-cpu reading, already taken by the caller.
#[derive(Debug, Clone, PartialEq)]
pub struct AdmissionReading {
    pub cpu_temp_c: Result<f64, String>,
    pub load_per_cpu: Result<f64, String>,
}

/// Why a launch was held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionHoldReason {
    /// The reading is over its declared limit.
    OverLimit,
    /// A declared limit's sensor could not be read.
    SensorUnreadable,
}

impl AdmissionHoldReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OverLimit => "over-limit",
            Self::SensorUnreadable => "sensor-unreadable",
        }
    }
}

/// The result of judging a reading against declared limits.
#[derive(Debug, Clone, PartialEq)]
pub enum AdmissionDecision {
    Admit,
    Held {
        reason: AdmissionHoldReason,
        /// The declared field name this hold judged: `max_cpu_temp_c` or
        /// `max_load_per_cpu`.
        metric: &'static str,
        limit: f64,
        /// `None` for `SensorUnreadable`; the measured value for `OverLimit`.
        reading: Option<f64>,
        /// The unreadable sensor's detail, empty for `OverLimit`.
        detail: String,
    },
}

/// `defaults.admission.unit` / a per-loop override: declared systemd resource
/// properties a rendered unit carries. ostrom declares them; umwelt renders
/// them (the boundary in docs/loops.md). Values are systemd's own syntax
/// (`"200%"`, `"8G"`), passed through unexamined except for shape safety.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnitResourceLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_quota: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nice: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_max: Option<String>,
}

impl UnitResourceLimits {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cpu_quota.is_none() && self.nice.is_none() && self.memory_max.is_none()
    }

    #[must_use]
    pub fn override_defaults(&self, defaults: &Self) -> Self {
        Self {
            cpu_quota: self
                .cpu_quota
                .clone()
                .or_else(|| defaults.cpu_quota.clone()),
            nice: self.nice.or(defaults.nice),
            memory_max: self
                .memory_max
                .clone()
                .or_else(|| defaults.memory_max.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AdmissionDecision, AdmissionHoldReason, AdmissionLimits, AdmissionReading,
        UnitResourceLimits,
    };

    fn reading(
        cpu_temp_c: Result<f64, String>,
        load_per_cpu: Result<f64, String>,
    ) -> AdmissionReading {
        AdmissionReading {
            cpu_temp_c,
            load_per_cpu,
        }
    }

    #[test]
    fn undeclared_limits_admit_regardless_of_the_reading() {
        let limits = AdmissionLimits::default();
        let over = reading(Ok(200.0), Err("no sensor".to_owned()));
        assert_eq!(limits.decide(&over), AdmissionDecision::Admit);
    }

    #[test]
    fn a_declared_limit_over_the_reading_is_held_over_limit() {
        let limits = AdmissionLimits {
            max_cpu_temp_c: Some(85.0),
            ..AdmissionLimits::default()
        };
        let decision = limits.decide(&reading(Ok(91.5), Ok(0.1)));
        assert_eq!(
            decision,
            AdmissionDecision::Held {
                reason: AdmissionHoldReason::OverLimit,
                metric: "max_cpu_temp_c",
                limit: 85.0,
                reading: Some(91.5),
                detail: String::new(),
            }
        );
    }

    #[test]
    fn a_reading_at_or_under_the_limit_admits() {
        let limits = AdmissionLimits {
            max_cpu_temp_c: Some(85.0),
            ..AdmissionLimits::default()
        };
        assert_eq!(
            limits.decide(&reading(Ok(85.0), Ok(0.1))),
            AdmissionDecision::Admit
        );
    }

    #[test]
    fn a_declared_limit_with_an_unreadable_sensor_holds_loudly() {
        let limits = AdmissionLimits {
            max_load_per_cpu: Some(0.8),
            ..AdmissionLimits::default()
        };
        let decision = limits.decide(&reading(Ok(40.0), Err("no /proc/loadavg".to_owned())));
        assert_eq!(
            decision,
            AdmissionDecision::Held {
                reason: AdmissionHoldReason::SensorUnreadable,
                metric: "max_load_per_cpu",
                limit: 0.8,
                reading: None,
                detail: "no /proc/loadavg".to_owned(),
            }
        );
    }

    #[test]
    fn temperature_is_judged_before_load_when_both_would_hold() {
        let limits = AdmissionLimits {
            max_cpu_temp_c: Some(85.0),
            max_load_per_cpu: Some(0.5),
            ..AdmissionLimits::default()
        };
        let decision = limits.decide(&reading(Ok(99.0), Ok(9.0)));
        assert!(matches!(
            decision,
            AdmissionDecision::Held {
                metric: "max_cpu_temp_c",
                ..
            }
        ));
    }

    #[test]
    fn a_loop_override_replaces_only_the_fields_it_declares() {
        let defaults = AdmissionLimits {
            max_cpu_temp_c: Some(85.0),
            max_load_per_cpu: Some(0.8),
            unit: UnitResourceLimits {
                cpu_quota: Some("200%".to_owned()),
                nice: Some(10),
                memory_max: Some("8G".to_owned()),
            },
        };
        let loop_override = AdmissionLimits {
            max_cpu_temp_c: Some(70.0),
            ..AdmissionLimits::default()
        };
        let resolved = loop_override.override_defaults(&defaults);
        assert_eq!(resolved.max_cpu_temp_c, Some(70.0));
        assert_eq!(resolved.max_load_per_cpu, Some(0.8));
        assert_eq!(resolved.unit, defaults.unit);
    }
}
