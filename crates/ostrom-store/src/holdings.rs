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
    let terminal_orders = rows
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
                if let Some(order_id) = fact_str(row, "order_id")
                    && !terminal_orders.contains(order_id)
                {
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
