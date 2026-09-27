//! Open holds: which run holds which item, read from local files only (#618).
//!
//! A hold opens with the record that claims it and closes with that record's
//! terminal row. An implementer is held from `work-dispatched` until a
//! `work-completed` or `work-failed` with the same `order_id`; a pass is held
//! from `pass-started` until a `pass-ended` with the same `owner`. Those are
//! the same rules dispatch applies to its own in-flight count.
//!
//! Nothing here calls GitHub, a service manager or anything off the machine:
//! the trace, the lease files and the run event logs are the whole input, so a
//! solo operator and a scheduler read the same holdings.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use chrono::DateTime;
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use umwelt_runtime::{FileSink, Source as _};

use crate::{Clock, LeaseRecord, OstromPaths, TraceFactRecord, read_lease, read_trace};

#[derive(Debug, Error)]
pub enum HoldingsError {
    #[error("ostrom ps: could not read the trace: {0}")]
    Trace(String),
}

/// What holds the item: an implementer run a dispatch recorded, or a pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum HoldingKind {
    Implementer,
    Pass,
}

/// The state of the lease whose owner is the hold's owner. `None` when no
/// lease file names that owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LeaseState {
    Live,
    Expired,
}

impl LeaseState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Expired => "expired",
        }
    }
}

/// One open hold. Every field a record may lack is `None`, never a guess:
/// a `work-dispatched` written before #618 has no `run_id` or `runner`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Holding {
    pub kind: HoldingKind,
    pub run_id: Option<String>,
    pub runner: Option<String>,
    pub item: Option<String>,
    pub order_id: Option<String>,
    /// The implementer's unit name or the pass's owner: the lease owner.
    pub owner: Option<String>,
    /// The `ts` of the record that opened the hold.
    pub started_at: String,
    pub age_seconds: Option<u64>,
    /// The `ts` of the newest event in the run's `events.jsonl`.
    pub last_event_at: Option<String>,
    pub lease: Option<LeaseState>,
}

/// Every open hold, oldest first.
pub fn open_holdings(paths: &OstromPaths, clock: &Clock) -> Result<Vec<Holding>, HoldingsError> {
    let rows = read_trace(&paths.trace_file())
        .map_err(|error| HoldingsError::Trace(error.to_string()))?
        .rows
        .into_iter()
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    // MUTATION (principle 7, #618): terminal rows no longer close a hold.
    let _terminal_orders = rows
        .iter()
        .filter(|row| matches!(row.kind.as_str(), "work-completed" | "work-failed"))
        .filter_map(|row| fact_str(row, "order_id"))
        .collect::<BTreeSet<_>>();
    let ended_owners = rows
        .iter()
        .filter(|row| row.kind == "pass-ended")
        .filter_map(|row| fact_str(row, "owner"))
        .collect::<BTreeSet<_>>();
    // A repeated opener for the same order or owner is one hold; the latest
    // record describes it, as it does for dispatch's in-flight view.
    let mut latest = BTreeMap::<(HoldingKind, &str), usize>::new();
    for (index, row) in rows.iter().enumerate() {
        match row.kind.as_str() {
            "work-dispatched" => {
                if let Some(order_id) = fact_str(row, "order_id") {
                    latest.insert((HoldingKind::Implementer, order_id), index);
                }
            }
            "pass-started" => {
                if let Some(owner) = fact_str(row, "owner")
                    && !ended_owners.contains(owner)
                {
                    latest.insert((HoldingKind::Pass, owner), index);
                }
            }
            _ => {}
        }
    }
    let mut indices = latest.into_values().collect::<Vec<_>>();
    indices.sort_unstable();

    let now = clock.epoch_seconds();
    let leases = read_leases(&paths.state);
    let events = FileSink::new(paths.runs_dir());
    Ok(indices
        .into_iter()
        .map(|index| {
            let row = &rows[index];
            let implementer = row.kind == "work-dispatched";
            let run_id = fact_string(row, "run_id");
            let owner = fact_string(row, if implementer { "unit_name" } else { "owner" });
            // A pass holds no single item and names no registered runner.
            let implementer_field = |key: &str| {
                if implementer {
                    fact_string(row, key)
                } else {
                    None
                }
            };
            Holding {
                kind: if implementer {
                    HoldingKind::Implementer
                } else {
                    HoldingKind::Pass
                },
                runner: implementer_field("runner"),
                item: implementer_field("item_id"),
                order_id: implementer_field("order_id"),
                started_at: row.ts.clone(),
                age_seconds: age_seconds(&row.ts, now),
                last_event_at: run_id
                    .as_deref()
                    .and_then(|run_id| last_event_at(&events, run_id)),
                lease: owner
                    .as_deref()
                    .and_then(|owner| lease_state(&leases, owner, now)),
                run_id,
                owner,
            }
        })
        .collect())
}

fn fact_str<'a>(row: &'a TraceFactRecord, key: &str) -> Option<&'a str> {
    row.fact
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
}

fn fact_string(row: &TraceFactRecord, key: &str) -> Option<String> {
    fact_str(row, key).map(str::to_owned)
}

fn age_seconds(ts: &str, now: u64) -> Option<u64> {
    let opened = u64::try_from(DateTime::parse_from_rfc3339(ts).ok()?.timestamp()).ok()?;
    Some(now.saturating_sub(opened))
}

fn last_event_at(events: &FileSink, run_id: &str) -> Option<String> {
    events
        .read_from(run_id, 0)
        .ok()?
        .last()
        .map(|event| event.ts.clone())
}

fn read_leases(state: &Path) -> Vec<LeaseRecord> {
    let Ok(entries) = fs::read_dir(state) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "lease")
        })
        .filter_map(|path| read_lease(&path).ok().flatten())
        .collect()
}

// MUTATION (principle 7, #618): the liveness check is reverted; every lease reads live.
fn lease_state(leases: &[LeaseRecord], owner: &str, _now: u64) -> Option<LeaseState> {
    leases
        .iter()
        .find(|lease| lease.owner == owner)
        .map(|_lease| LeaseState::Live)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use chrono::{DateTime, Utc};
    use ethogram::{RunKind, RunOutcome};
    use serde_json::json;

    use super::{HoldingKind, LeaseState, open_holdings};
    use crate::{Clock, OstromPaths, RunEventGuard, RunEventStart};

    fn fixture() -> (tempfile::TempDir, OstromPaths, Clock) {
        let root = tempfile::tempdir().expect("holdings fixture");
        let paths = OstromPaths {
            config: root.path().to_path_buf(),
            state: root.path().to_path_buf(),
        };
        let now = DateTime::parse_from_rfc3339("2026-09-27T12:00:00Z")
            .expect("fixed instant")
            .with_timezone(&Utc);
        (root, paths, Clock::fixed(now))
    }

    fn write_rows(paths: &OstromPaths, rows: &[serde_json::Value]) {
        let mut text = String::new();
        for row in rows {
            text.push_str(&row.to_string());
            text.push('\n');
        }
        fs::write(paths.trace_file(), text).expect("write trace");
    }

    fn dispatched(order_id: &str, run_id: Option<&str>) -> serde_json::Value {
        let mut fact = json!({
            "schema_version": 1,
            "item_id": "placeholder-org/alpha#7",
            "order_id": order_id,
            "unit_name": format!("ostrom-implementer-{order_id}"),
            "backend": "systemd",
            "cost_ceiling_usd": 1,
            "token_ceiling": 1000
        });
        if let Some(run_id) = run_id {
            fact["run_id"] = json!(run_id);
            fact["runner"] = json!("agent/codex");
        }
        json!({"ts": "2026-09-27T11:00:00Z", "kind": "work-dispatched", "fact": fact, "narration": {}})
    }

    #[test]
    fn a_terminal_row_closes_the_hold_it_names_and_no_other() {
        let (_root, paths, clock) = fixture();
        write_rows(
            &paths,
            &[
                dispatched("order-a", Some("run-a")),
                dispatched("order-b", Some("run-b")),
                json!({"ts": "2026-09-27T11:30:00Z", "kind": "work-failed", "fact": {"order_id": "order-a"}, "narration": {}}),
            ],
        );

        let holdings = open_holdings(&paths, &clock).expect("read holdings");

        assert_eq!(holdings.len(), 1, "{holdings:?}");
        assert_eq!(holdings[0].kind, HoldingKind::Implementer);
        assert_eq!(holdings[0].order_id.as_deref(), Some("order-b"));
        assert_eq!(holdings[0].run_id.as_deref(), Some("run-b"));
        assert_eq!(holdings[0].runner.as_deref(), Some("agent/codex"));
        assert_eq!(holdings[0].age_seconds, Some(3_600));
    }

    #[test]
    fn a_record_from_before_run_ids_is_still_a_hold_with_unknown_fields() {
        let (_root, paths, clock) = fixture();
        write_rows(&paths, &[dispatched("order-old", None)]);

        let holdings = open_holdings(&paths, &clock).expect("read holdings");

        assert_eq!(holdings.len(), 1);
        assert_eq!(holdings[0].run_id, None);
        assert_eq!(holdings[0].runner, None);
        assert_eq!(holdings[0].last_event_at, None);
        assert_eq!(holdings[0].lease, None);
    }

    #[test]
    fn lease_state_follows_the_lease_that_names_the_owner() {
        let (_root, paths, clock) = fixture();
        write_rows(
            &paths,
            &[
                dispatched("order-live", Some("run-live")),
                dispatched("order-expired", Some("run-expired")),
            ],
        );
        let now = clock.epoch_seconds();
        fs::write(
            paths.state.join("implementer-item-live.lease"),
            json!({"owner": "ostrom-implementer-order-live", "started_at": now - 10, "expires_at": now + 600}).to_string(),
        )
        .expect("write live lease");
        fs::write(
            paths.state.join("implementer-item-expired.lease"),
            json!({"owner": "ostrom-implementer-order-expired", "started_at": now - 600, "expires_at": now - 10}).to_string(),
        )
        .expect("write expired lease");

        let holdings = open_holdings(&paths, &clock).expect("read holdings");

        let lease = |order: &str| {
            holdings
                .iter()
                .find(|holding| holding.order_id.as_deref() == Some(order))
                .and_then(|holding| holding.lease)
        };
        assert_eq!(lease("order-live"), Some(LeaseState::Live));
        assert_eq!(lease("order-expired"), Some(LeaseState::Expired));
    }

    #[test]
    fn the_last_event_is_read_from_the_run_named_by_the_hold() {
        let (_root, paths, clock) = fixture();
        let run_id = "builder-placeholder-run";
        let mut events = RunEventGuard::start(
            &paths,
            None,
            false,
            clock.clone(),
            RunEventStart {
                run_id: run_id.to_owned(),
                kind: RunKind::Handoff,
                actor: "builder".to_owned(),
                harness: "claude".to_owned(),
                model: None,
                schedule: None,
                repository: None,
                repositories: None,
                work_order: None,
                ceilings: None,
            },
        )
        .expect("start run events");
        events
            .finish(RunOutcome::Completed, None, None, None)
            .expect("finish run events");
        let written = umwelt_runtime::FileSink::new(paths.runs_dir());
        let newest = umwelt_runtime::Source::read_from(&written, run_id, 0)
            .expect("read written events")
            .last()
            .expect("an event was written")
            .ts
            .clone();
        write_rows(
            &paths,
            &[
                json!({"ts": "2026-09-27T11:59:00Z", "kind": "pass-started", "fact": {"owner": "builder-a1b2c3d4-wake1", "run_id": run_id}, "narration": {}}),
                json!({"ts": "2026-09-27T11:59:00Z", "kind": "pass-started", "fact": {"owner": "builder-a1b2c3d4-wake0", "run_id": "ended-run"}, "narration": {}}),
                json!({"ts": "2026-09-27T11:59:30Z", "kind": "pass-ended", "fact": {"owner": "builder-a1b2c3d4-wake0", "outcome": "completed"}, "narration": {}}),
            ],
        );

        let holdings = open_holdings(&paths, &clock).expect("read holdings");

        assert_eq!(holdings.len(), 1, "{holdings:?}");
        assert_eq!(holdings[0].kind, HoldingKind::Pass);
        assert_eq!(holdings[0].run_id.as_deref(), Some(run_id));
        assert_eq!(holdings[0].last_event_at.as_deref(), Some(newest.as_str()));
        assert_eq!(holdings[0].item, None);
    }
}
