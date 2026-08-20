//! Durable schema-version 1 work orders shared by the shell and native
//! dispatchers.
//!
//! The schema is intentionally exact: a native consumer must be able to read
//! orders created before cutover without guessing which ad-hoc fields matter.
//! New branch names are item-derived, while validation keeps accepting every
//! historically valid version 1 branch so an existing order can be retargeted.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use chrono::{DateTime, Utc};
use ostrom_core::WorkOrder;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::{
    TraceAppend, TraceFactRecord, append_trace, read_lease, read_trace, set_private_file_mode,
};

const DEFAULT_COST_CEILING_USD: &str = "20";
const DEFAULT_TOKEN_CEILING: &str = "500000";
// These are the dispatcher's existing runtime ceilings. Keeping the lease and
// order staleness calculations here gives both users one source of truth.
const WEIGHTED_TOKENS_PER_RUNTIME_SECOND: u64 = 100;
const RUNTIME_SECONDS_PER_COST_USD: f64 = 240.0;
const IMPLEMENTER_LEASE_MARGIN_SECONDS: u64 = 5 * 60;
const CANDIDATE_KEYS: &[&str] = &[
    "acceptance_criteria",
    "branch_name",
    "constraints",
    "item_id",
    "item_ref",
    "repository",
    "schema_version",
    "spec",
];
const ORDER_KEYS: &[&str] = &[
    "acceptance_criteria",
    "branch_name",
    "constraints",
    "cost_ceiling_usd",
    "created_at",
    "item_id",
    "item_ref",
    "order_id",
    "repository",
    "schema_version",
    "spec",
    "token_ceiling",
];

#[derive(Debug, Error)]
pub enum WorkOrderError {
    #[error("ostrom work order: candidate is not a file")]
    CandidateNotFile,
    #[error("ostrom work order: candidate does not match schema_version 1")]
    InvalidCandidate,
    #[error("ostrom work order: cost ceiling must be a positive number")]
    InvalidCostCeiling,
    #[error("ostrom work order: token ceiling must be a positive integer")]
    InvalidTokenCeiling,
    #[error("ostrom work order: {0} is not a file")]
    OrderNotFile(String),
    #[error("ostrom work order: invalid schema_version 1 work order at {0}")]
    InvalidOrder(String),
    #[error("ostrom work order: item has a live implementer lease; refusing to replace {0}")]
    LiveLease(String),
    #[error("ostrom work order: prior order is still in flight; refusing to replace {0}")]
    InFlight(String),
    #[error("ostrom work order: no in-flight order matches {0}")]
    NoMatchingInFlight(String),
    #[error("ostrom work order: multiple in-flight orders match {0}; use an order id")]
    AmbiguousInFlight(String),
    #[error("ostrom work order: order is still running; refusing to clear {0}")]
    StillRunning(String),
    #[error(
        "ostrom work order: unit state is unknown and the order is not stale; refusing to clear {0}"
    )]
    UnitStateUnknown(String),
    #[error("ostrom work order: could not write {0}")]
    Write(String),
}

impl WorkOrderError {
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::LiveLease(_)
            | Self::InFlight(_)
            | Self::NoMatchingInFlight(_)
            | Self::AmbiguousInFlight(_)
            | Self::StillRunning(_)
            | Self::UnitStateUnknown(_) => 3,
            Self::CandidateNotFile
            | Self::InvalidCandidate
            | Self::InvalidCostCeiling
            | Self::InvalidTokenCeiling
            | Self::OrderNotFile(_)
            | Self::InvalidOrder(_)
            | Self::Write(_) => 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedWorkOrder {
    pub target: PathBuf,
    pub branch_warning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClearedWorkOrder {
    pub order_id: String,
    pub item_id: String,
}

#[derive(Debug, Clone)]
pub(crate) struct InFlightOrder {
    pub ts: String,
    pub item_id: String,
    pub order_id: String,
    pub unit_name: String,
    pub backend: String,
    pub cost_ceiling_usd: f64,
    pub token_ceiling: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnitLiveness {
    Live,
    NotLive,
    Unknown,
}

#[derive(Debug, Clone)]
struct UnitObservation {
    liveness: UnitLiveness,
    exit_code: Option<i32>,
    detail: String,
}

#[must_use]
pub fn item_hash(item_id: &str) -> String {
    format!("{:x}", Sha256::digest(item_id.as_bytes()))
}

#[must_use]
pub fn branch_name(item_id: &str) -> String {
    let hash = item_hash(item_id);
    let suffix = item_id.rsplit('#').next().unwrap_or_default();
    if !suffix.is_empty() && suffix.chars().all(|character| character.is_ascii_digit()) {
        format!("ostrom/{suffix}-{}", &hash[..12])
    } else {
        format!("ostrom/item-{}", &hash[..20])
    }
}

pub fn validate_work_order_file(path: &Path) -> Result<(), WorkOrderError> {
    if !path.is_file() {
        return Err(WorkOrderError::OrderNotFile(path.display().to_string()));
    }
    let value = fs::read_to_string(path)
        .ok()
        .and_then(|contents| serde_json::from_str::<Value>(&contents).ok())
        .ok_or_else(|| WorkOrderError::InvalidOrder(path.display().to_string()))?;
    if validate_order(&value) {
        Ok(())
    } else {
        Err(WorkOrderError::InvalidOrder(path.display().to_string()))
    }
}

pub fn create_work_order(
    state_root: &Path,
    candidate_path: &Path,
    created_at: Option<&str>,
    cost_ceiling: Option<&str>,
    token_ceiling: Option<&str>,
) -> Result<CreatedWorkOrder, WorkOrderError> {
    if !candidate_path.is_file() {
        return Err(WorkOrderError::CandidateNotFile);
    }
    let contents =
        fs::read_to_string(candidate_path).map_err(|_| WorkOrderError::InvalidCandidate)?;
    let mut candidate: Value =
        serde_json::from_str(&contents).map_err(|_| WorkOrderError::InvalidCandidate)?;
    if !validate_candidate(&candidate) {
        return Err(WorkOrderError::InvalidCandidate);
    }
    let cost = parse_positive_number(cost_ceiling.unwrap_or(DEFAULT_COST_CEILING_USD))
        .ok_or(WorkOrderError::InvalidCostCeiling)?;
    let tokens = parse_positive_integer(token_ceiling.unwrap_or(DEFAULT_TOKEN_CEILING))
        .ok_or(WorkOrderError::InvalidTokenCeiling)?;
    let item_id = candidate["item_id"]
        .as_str()
        .expect("validated item id")
        .to_owned();
    let hash = item_hash(&item_id);
    let deterministic_branch = branch_name(&item_id);
    let supplied_branch = candidate["branch_name"]
        .as_str()
        .expect("validated branch")
        .to_owned();
    let branch_warning = (supplied_branch != deterministic_branch).then(|| {
        format!(
            "ostrom work order: overwriting candidate branch_name '{supplied_branch}' with item-derived '{deterministic_branch}'"
        )
    });
    let timestamp = created_at.map_or_else(
        || {
            chrono::DateTime::<Utc>::from(SystemTime::now())
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string()
        },
        str::to_owned,
    );
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let order_id = format!(
        "{:x}",
        Sha256::digest(format!(
            "{item_id}\n{timestamp}\n{}-{nonce}\n",
            std::process::id()
        ))
    );
    let orders_dir = state_root.join("work-orders");
    let target = orders_dir.join(format!("{hash}.json"));
    fs::create_dir_all(&orders_dir)
        .map_err(|_| WorkOrderError::Write(target.display().to_string()))?;
    if target.is_file() {
        let prior_order_id = read_order(&target).map(|order| order.order_id);
        if let Some(order_id) = prior_order_id.as_deref() {
            reap_stale_work_orders_matching(state_root, Some(order_id))?;
        }
        if prior_order_is_in_flight(&target, &state_root.join("sprint.jsonl")) {
            return Err(WorkOrderError::InFlight(target.display().to_string()));
        }
    }
    remove_expired_lease(
        &state_root.join(format!("implementer-item-{hash}.lease")),
        current_epoch(),
    );
    if state_root
        .join(format!("implementer-item-{hash}.lease"))
        .exists()
    {
        return Err(WorkOrderError::LiveLease(target.display().to_string()));
    }

    let object = candidate
        .as_object_mut()
        .expect("validated candidate is an object");
    object.insert(
        "branch_name".to_owned(),
        Value::String(deterministic_branch),
    );
    object.insert("order_id".to_owned(), Value::String(order_id));
    object.insert("created_at".to_owned(), Value::String(timestamp));
    object.insert("cost_ceiling_usd".to_owned(), cost);
    object.insert("token_ceiling".to_owned(), tokens);
    if !validate_order(&candidate) {
        return Err(WorkOrderError::InvalidOrder(target.display().to_string()));
    }
    let mut temporary = NamedTempFile::new_in(&orders_dir)
        .map_err(|_| WorkOrderError::Write(target.display().to_string()))?;
    set_private_file_mode(temporary.path())
        .map_err(|_| WorkOrderError::Write(target.display().to_string()))?;
    serde_json::to_writer(&mut temporary, &candidate)
        .map_err(|_| WorkOrderError::Write(target.display().to_string()))?;
    temporary
        .write_all(b"\n")
        .and_then(|()| temporary.flush())
        .map_err(|_| WorkOrderError::Write(target.display().to_string()))?;
    temporary
        .persist(&target)
        .map_err(|_| WorkOrderError::Write(target.display().to_string()))?;
    Ok(CreatedWorkOrder {
        target,
        branch_warning,
    })
}

fn validate_candidate(value: &Value) -> bool {
    let Some(object) = exact_object(value, CANDIDATE_KEYS) else {
        return false;
    };
    common_fields_valid(object)
}

fn validate_order(value: &Value) -> bool {
    let Some(object) = exact_object(value, ORDER_KEYS) else {
        return false;
    };
    common_fields_valid(object)
        && object
            .get("order_id")
            .and_then(Value::as_str)
            .is_some_and(|id| {
                id.len() == 64
                    && id.chars().all(|character| {
                        character.is_ascii_hexdigit() && !character.is_ascii_uppercase()
                    })
            })
        && object
            .get("created_at")
            .and_then(Value::as_str)
            .is_some_and(valid_created_at)
        && object
            .get("cost_ceiling_usd")
            .and_then(Value::as_f64)
            .is_some_and(|value| value > 0.0)
        && object
            .get("token_ceiling")
            .and_then(Value::as_f64)
            .is_some_and(|value| value > 0.0 && value.fract() == 0.0)
}

fn exact_object<'a>(value: &'a Value, expected: &[&str]) -> Option<&'a Map<String, Value>> {
    let object = value.as_object()?;
    let keys = object.keys().map(String::as_str).collect::<HashSet<_>>();
    (keys.len() == expected.len() && expected.iter().all(|key| keys.contains(key)))
        .then_some(object)
}

fn common_fields_valid(object: &Map<String, Value>) -> bool {
    object.get("schema_version").and_then(Value::as_i64) == Some(1)
        && nonempty_string(object.get("item_id"))
        && object
            .get("repository")
            .and_then(Value::as_str)
            .is_some_and(valid_repository)
        && nonempty_string(object.get("item_ref"))
        && object
            .get("branch_name")
            .and_then(Value::as_str)
            .is_some_and(valid_branch_name)
        && nonempty_string(object.get("spec"))
        && string_array(object.get("acceptance_criteria"), true)
        && string_array(object.get("constraints"), false)
}

fn nonempty_string(value: Option<&Value>) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty())
}

fn string_array(value: Option<&Value>, require_nonempty: bool) -> bool {
    value.and_then(Value::as_array).is_some_and(|values| {
        (!require_nonempty || !values.is_empty())
            && values.iter().all(|value| nonempty_string(Some(value)))
    })
}

fn valid_repository(repository: &str) -> bool {
    let mut parts = repository.split('/');
    matches!((parts.next(), parts.next(), parts.next()), (Some(owner), Some(name), None)
        if !owner.is_empty() && !name.is_empty() && !repository.chars().any(char::is_whitespace))
}

fn valid_branch_name(branch: &str) -> bool {
    !branch.contains("..")
        && branch
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphanumeric())
        && branch.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '/' | '-')
        })
}

fn valid_created_at(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 20
        && [4, 7, 10, 13, 16, 19]
            .into_iter()
            .zip(*b"--T::Z")
            .all(|(index, expected)| bytes[index] == expected)
        && bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19) || byte.is_ascii_digit()
        })
}

fn parse_positive_number(value: &str) -> Option<Value> {
    let value = serde_json::from_str::<Value>(value).ok()?;
    value.as_f64().filter(|number| *number > 0.0)?;
    Some(value)
}

fn parse_positive_integer(value: &str) -> Option<Value> {
    let value = serde_json::from_str::<Value>(value).ok()?;
    value
        .as_f64()
        .filter(|number| *number > 0.0 && number.fract() == 0.0)?;
    Some(value)
}

fn prior_order_is_in_flight(order_path: &Path, trace_path: &Path) -> bool {
    let Some(order_id) = read_order(order_path).map(|order| order.order_id) else {
        return false;
    };
    in_flight_orders(trace_path)
        .is_ok_and(|orders| orders.iter().any(|order| order.order_id == order_id))
}

fn read_order(path: &Path) -> Option<WorkOrder> {
    fs::read(path)
        .ok()
        .and_then(|bytes| WorkOrder::from_json(&bytes).ok())
}

pub(crate) fn implementer_lease_ttl(order: &WorkOrder) -> u64 {
    implementer_lease_ttl_from_ceilings(order.cost(), order.tokens())
}

pub(crate) fn implementer_lease_ttl_from_ceilings(cost: f64, tokens: u64) -> u64 {
    let token_seconds = tokens.div_ceil(WEIGHTED_TOKENS_PER_RUNTIME_SECOND);
    let cost_seconds = (cost * RUNTIME_SECONDS_PER_COST_USD).ceil() as u64;
    token_seconds
        .max(cost_seconds)
        .max(1)
        .saturating_add(IMPLEMENTER_LEASE_MARGIN_SECONDS)
}

fn dispatch_fact(row: &TraceFactRecord) -> Option<InFlightOrder> {
    (row.kind == "work-dispatched").then_some(())?;
    Some(InFlightOrder {
        ts: row.ts.clone(),
        item_id: row.fact.get("item_id")?.as_str()?.to_owned(),
        order_id: row.fact.get("order_id")?.as_str()?.to_owned(),
        unit_name: row.fact.get("unit_name")?.as_str()?.to_owned(),
        backend: row
            .fact
            .get("backend")
            .and_then(Value::as_str)
            .unwrap_or("systemd")
            .to_owned(),
        cost_ceiling_usd: row.fact.get("cost_ceiling_usd").and_then(Value::as_f64)?,
        token_ceiling: row.fact.get("token_ceiling").and_then(Value::as_u64)?,
    })
}

pub(crate) fn in_flight_orders(trace_path: &Path) -> Result<Vec<InFlightOrder>, WorkOrderError> {
    let trace = read_trace(trace_path)
        .map_err(|_| WorkOrderError::Write(trace_path.display().to_string()))?;
    let rows = trace
        .rows
        .into_iter()
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    let terminal = rows
        .iter()
        .filter(|row| matches!(row.kind.as_str(), "work-completed" | "work-failed"))
        .filter_map(|row| row.fact.get("order_id").and_then(Value::as_str))
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let mut dispatched = BTreeMap::new();
    for row in &rows {
        if let Some(order) = dispatch_fact(row) {
            dispatched.insert(order.order_id.clone(), order);
        }
    }
    Ok(dispatched
        .into_values()
        .filter(|order| !terminal.contains(&order.order_id))
        .collect())
}

pub(crate) fn reap_stale_work_orders(
    state_root: &Path,
) -> Result<Vec<ClearedWorkOrder>, WorkOrderError> {
    reap_stale_work_orders_matching(state_root, None)
}

fn reap_stale_work_orders_matching(
    state_root: &Path,
    order_id: Option<&str>,
) -> Result<Vec<ClearedWorkOrder>, WorkOrderError> {
    let trace_path = state_root.join("sprint.jsonl");
    let now = current_epoch();
    let mut reaped = Vec::new();
    for order in in_flight_orders(&trace_path)? {
        if order_id.is_some_and(|expected| order.order_id != expected)
            || !order_is_stale(&order, now)
        {
            continue;
        }
        let observation = observe_unit(&order);
        if observation.liveness == UnitLiveness::Live {
            continue;
        }
        if append_terminal_failure(
            state_root,
            &order,
            "stale-order-reaped",
            &observation.detail,
            observation.exit_code,
            None,
            true,
        )? {
            reaped.push(ClearedWorkOrder {
                order_id: order.order_id,
                item_id: order.item_id,
            });
        }
    }
    Ok(reaped)
}

pub fn clear_work_order(
    state_root: &Path,
    identifier: &str,
) -> Result<ClearedWorkOrder, WorkOrderError> {
    let trace_path = state_root.join("sprint.jsonl");
    let mut matches = in_flight_orders(&trace_path)?
        .into_iter()
        .filter(|order| order.order_id == identifier || order.item_id == identifier)
        .collect::<Vec<_>>();
    if matches.is_empty() {
        return Err(WorkOrderError::NoMatchingInFlight(identifier.to_owned()));
    }
    if matches.len() > 1 {
        return Err(WorkOrderError::AmbiguousInFlight(identifier.to_owned()));
    }
    let order = matches.pop().expect("one matching order");
    let observation = observe_unit(&order);
    match observation.liveness {
        UnitLiveness::Live => {
            return Err(WorkOrderError::StillRunning(order.order_id));
        }
        UnitLiveness::Unknown if !order_is_stale(&order, current_epoch()) => {
            return Err(WorkOrderError::UnitStateUnknown(order.order_id));
        }
        UnitLiveness::NotLive | UnitLiveness::Unknown => {}
    }
    append_terminal_failure(
        state_root,
        &order,
        "operator-reaped",
        &observation.detail,
        observation.exit_code,
        None,
        true,
    )?;
    Ok(ClearedWorkOrder {
        order_id: order.order_id,
        item_id: order.item_id,
    })
}

pub fn finalize_exited_implementer(
    state_root: &Path,
    order_path: &Path,
    unit_name: &str,
    exit_code: Option<i32>,
    signal: Option<i32>,
) -> Result<bool, WorkOrderError> {
    let order_id = read_order(order_path).map(|order| order.order_id);
    let trace_path = state_root.join("sprint.jsonl");
    let Some(order) = in_flight_orders(&trace_path)?.into_iter().find(|order| {
        order_id
            .as_deref()
            .is_some_and(|expected| order.order_id == expected)
            || (order_id.is_none() && order.unit_name == unit_name)
    }) else {
        return Ok(false);
    };
    let detail = match (exit_code, signal) {
        (Some(code), _) => format!("implementer worker exited with code {code}"),
        (_, Some(signal)) => format!("implementer worker was killed by signal {signal}"),
        _ => "implementer worker exited without a status code".to_owned(),
    };
    append_terminal_failure(
        state_root,
        &order,
        "unit-exit-without-terminal",
        &detail,
        exit_code,
        signal,
        false,
    )
}

fn order_is_stale(order: &InFlightOrder, now: u64) -> bool {
    let Some(dispatched_at) = DateTime::parse_from_rfc3339(&order.ts)
        .ok()
        .and_then(|time| u64::try_from(time.timestamp()).ok())
    else {
        return false;
    };
    now >= dispatched_at.saturating_add(implementer_lease_ttl_from_ceilings(
        order.cost_ceiling_usd,
        order.token_ceiling,
    ))
}

fn observe_unit(order: &InFlightOrder) -> UnitObservation {
    if order.backend != "systemd" {
        return UnitObservation {
            liveness: UnitLiveness::Unknown,
            exit_code: None,
            detail: format!("cannot inspect unsupported backend {}", order.backend),
        };
    }
    let executable = env::var_os("MANDATE_SYSTEMCTL_BIN")
        .map_or_else(|| PathBuf::from("systemctl"), PathBuf::from);
    let service = if order.unit_name.ends_with(".service") {
        order.unit_name.clone()
    } else {
        format!("{}.service", order.unit_name)
    };
    let output = Command::new(executable)
        .args([
            "--user",
            "show",
            &service,
            "--property=ActiveState",
            "--property=ExecMainCode",
            "--property=ExecMainStatus",
        ])
        .output();
    let Ok(output) = output else {
        return UnitObservation {
            liveness: UnitLiveness::Unknown,
            exit_code: None,
            detail: "could not invoke systemctl".to_owned(),
        };
    };
    if output.status.code() == Some(4) {
        return UnitObservation {
            liveness: UnitLiveness::NotLive,
            exit_code: None,
            detail: "systemd unit does not exist".to_owned(),
        };
    }
    if !output.status.success() {
        return UnitObservation {
            liveness: UnitLiveness::Unknown,
            exit_code: None,
            detail: format!("systemctl exited with {}", output.status),
        };
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let properties = stdout
        .lines()
        .filter_map(|line| line.split_once('='))
        .collect::<BTreeMap<_, _>>();
    let state = properties.get("ActiveState").copied().unwrap_or_default();
    if ["active", "activating", "reloading", "deactivating"].contains(&state) {
        return UnitObservation {
            liveness: UnitLiveness::Live,
            exit_code: None,
            detail: format!("systemd unit is {state}"),
        };
    }
    if state.is_empty() {
        return UnitObservation {
            liveness: UnitLiveness::NotLive,
            exit_code: None,
            detail: "systemd unit does not exist".to_owned(),
        };
    }
    let exited_normally = matches!(properties.get("ExecMainCode"), Some(&"exited") | Some(&"1"));
    let exit_code = exited_normally
        .then(|| properties.get("ExecMainStatus")?.parse::<i32>().ok())
        .flatten();
    UnitObservation {
        liveness: UnitLiveness::NotLive,
        exit_code,
        detail: format!("systemd unit is {state}"),
    }
}

#[allow(clippy::too_many_arguments)]
fn append_terminal_failure(
    state_root: &Path,
    order: &InFlightOrder,
    reason: &str,
    message: &str,
    exit_code: Option<i32>,
    signal: Option<i32>,
    reaped: bool,
) -> Result<bool, WorkOrderError> {
    let trace_path = state_root.join("sprint.jsonl");
    if !in_flight_orders(&trace_path)?
        .iter()
        .any(|candidate| candidate.order_id == order.order_id)
    {
        release_matching_lease(state_root, order)?;
        return Ok(false);
    }
    let now = current_epoch();
    let dispatched_at = DateTime::parse_from_rfc3339(&order.ts)
        .ok()
        .and_then(|time| u64::try_from(time.timestamp()).ok())
        .unwrap_or(now);
    let repository = order
        .item_id
        .rsplit_once('#')
        .map(|(repository, _)| repository);
    let fact = Map::from_iter([
        ("schema_version".to_owned(), Value::from(1)),
        ("item_id".to_owned(), Value::String(order.item_id.clone())),
        ("order_id".to_owned(), Value::String(order.order_id.clone())),
        (
            "unit_name".to_owned(),
            Value::String(order.unit_name.clone()),
        ),
        ("backend".to_owned(), Value::String(order.backend.clone())),
        (
            "repository".to_owned(),
            repository.map_or(Value::Null, |value| Value::String(value.to_owned())),
        ),
        (
            "cost_ceiling_usd".to_owned(),
            Value::from(order.cost_ceiling_usd),
        ),
        ("token_ceiling".to_owned(), Value::from(order.token_ceiling)),
        ("weighted_tokens".to_owned(), Value::from(0)),
        ("cost_usd".to_owned(), Value::from(0)),
        (
            "duration_seconds".to_owned(),
            Value::from(now.saturating_sub(dispatched_at)),
        ),
        ("pr_url".to_owned(), Value::Null),
        ("reason".to_owned(), Value::String(reason.to_owned())),
        ("message".to_owned(), Value::String(message.to_owned())),
        (
            "exit_code".to_owned(),
            exit_code.map_or(Value::Null, Value::from),
        ),
        (
            "termination_signal".to_owned(),
            signal.map_or(Value::Null, |value| Value::String(format!("SIG{value}"))),
        ),
        ("reaped".to_owned(), Value::Bool(reaped)),
        (
            "usage".to_owned(),
            serde_json::json!({
                "input_tokens": 0,
                "cached_input_tokens": 0,
                "output_tokens": 0,
                "reasoning_output_tokens": 0
            }),
        ),
    ]);
    append_trace(
        &trace_path,
        &TraceAppend {
            ts: trace_time(),
            kind: "work-failed".to_owned(),
            fact,
            narration: Map::new(),
        },
    )
    .map_err(|_| WorkOrderError::Write(trace_path.display().to_string()))?;
    release_matching_lease(state_root, order)?;
    Ok(true)
}

fn release_matching_lease(state_root: &Path, order: &InFlightOrder) -> Result<(), WorkOrderError> {
    let path = state_root.join(format!(
        "implementer-item-{}.lease",
        item_hash(&order.item_id)
    ));
    if read_lease(&path)
        .ok()
        .flatten()
        .is_some_and(|lease| lease.owner == order.unit_name)
    {
        fs::remove_file(&path).map_err(|_| WorkOrderError::Write(path.display().to_string()))?;
    }
    Ok(())
}

fn remove_expired_lease(path: &Path, now: u64) {
    if read_lease(path)
        .ok()
        .flatten()
        .is_some_and(|lease| lease.expires_at <= now)
    {
        let _ = fs::remove_file(path);
    }
}

fn current_epoch() -> u64 {
    env::var("MANDATE_LEASE_NOW_EPOCH")
        .or_else(|_| env::var("MANDATE_NOW_EPOCH"))
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        })
}

fn trace_time() -> String {
    env::var("MANDATE_TRACE_TIME").unwrap_or_else(|_| {
        DateTime::<Utc>::from(SystemTime::now())
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::{branch_name, item_hash, validate_work_order_file};
    use std::path::Path;

    #[test]
    fn identifiers_match_recorded_shell_values() {
        assert_eq!(
            item_hash("placeholder-org/alpha#42"),
            "6cebcc7b2a63688956531e4da720ea1bd4b63e88030ba0c147d95ab0d87b5e77"
        );
        assert_eq!(
            branch_name("placeholder-org/alpha#42"),
            "ostrom/42-6cebcc7b2a63"
        );
    }

    #[test]
    fn validates_a_bash_era_order() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../ostrom-cli/tests/fixtures/leaves/state-writing/work-order.bash-era.json");
        validate_work_order_file(&fixture).expect("Bash-era order remains valid");
    }
}
