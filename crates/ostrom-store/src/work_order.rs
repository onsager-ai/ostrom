//! Durable schema-version 1 work orders shared by the shell and native
//! dispatchers.
//!
//! The schema is intentionally exact: a native consumer must be able to read
//! orders created before cutover without guessing which ad-hoc fields matter.
//! New branch names are item-derived, while validation keeps accepting every
//! historically valid version 1 branch so an existing order can be retargeted.

use std::{
    collections::HashSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use chrono::Utc;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::set_private_file_mode;

const DEFAULT_COST_CEILING_USD: &str = "20";
const DEFAULT_TOKEN_CEILING: &str = "500000";
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
    #[error("ostrom work order: could not write {0}")]
    Write(String),
}

impl WorkOrderError {
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::LiveLease(_) | Self::InFlight(_) => 3,
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
    if state_root
        .join(format!("implementer-item-{hash}.lease"))
        .exists()
    {
        return Err(WorkOrderError::LiveLease(target.display().to_string()));
    }
    if target.is_file() && prior_order_is_in_flight(&target, &state_root.join("sprint.jsonl")) {
        return Err(WorkOrderError::InFlight(target.display().to_string()));
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
    let Some(order_id) = fs::read_to_string(order_path)
        .ok()
        .and_then(|contents| serde_json::from_str::<Value>(&contents).ok())
        .and_then(|order| order.get("order_id").cloned())
        .and_then(|value| value.as_str().map(str::to_owned))
    else {
        return false;
    };
    let Ok(trace) = fs::read_to_string(trace_path) else {
        return false;
    };
    let rows = trace
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|row| row.is_object())
        .collect::<Vec<_>>();
    rows.iter().any(|row| {
        row.get("kind").and_then(Value::as_str) == Some("work-dispatched")
            && row.pointer("/fact/order_id").and_then(Value::as_str) == Some(&order_id)
    }) && !rows.iter().any(|row| {
        matches!(
            row.get("kind").and_then(Value::as_str),
            Some("work-completed" | "work-failed")
        ) && row.pointer("/fact/order_id").and_then(Value::as_str) == Some(&order_id)
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
