use std::{
    collections::HashSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use serde_json::Value;
use thiserror::Error;

use crate::{StoreError, io_error, set_private_file_mode};

const REQUIRED: &[&str] = &["id", "kind", "mandate", "opened", "ref", "repo", "state"];
const ALLOWED: &[&str] = &[
    "age_days",
    "aged_out",
    "blocked_by",
    "classification",
    "id",
    "item_type",
    "kind",
    "mandate",
    "matched_selector",
    "needs_judgment",
    "opened",
    "ref",
    "repo",
    "state",
    "semantic_derivation",
    "title",
];

/// A legacy queue row remains an ordered JSON object so serialization can
/// reproduce `jq -c` exactly. This compatibility type intentionally lives in
/// the file adapter: the portable core queue record has no narration-bearing
/// `mandate.reason` slot.
#[derive(Debug, Clone, PartialEq)]
pub struct QueueDocument(Value);

impl QueueDocument {
    pub fn from_value(value: Value) -> Result<Self, StoreError> {
        validate_queue(&value)
            .map_err(|message| StoreError::MalformedQueue { line: 0, message })?;
        Ok(Self(value))
    }

    #[must_use]
    pub fn value(&self) -> &Value {
        &self.0
    }

    fn state(&self) -> Option<&str> {
        self.0.get("state").and_then(Value::as_str)
    }

    fn string(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(Value::as_str)
    }
}

#[derive(Debug, Error)]
pub enum QueueActionError {
    #[error("mandate queue: cannot read {path}")]
    CannotRead { path: String },
    #[error("mandate lint: no sweep state at {path}")]
    NoSweepState { path: String },
    #[error("mandate queue: no pending or deferred item with id {0}")]
    UnknownItem(String),
    #[error("mandate queue: {0} is paused; CI drift cannot mint a handoff token")]
    Paused(String),
    #[error("mandate queue: cannot write {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: StoreError,
    },
    #[error("mandate queue: malformed state at {path}")]
    MalformedState { path: String },
}

impl QueueActionError {
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::UnknownItem(_) => 3,
            Self::Paused(_) => 4,
            Self::CannotRead { .. }
            | Self::NoSweepState { .. }
            | Self::Write { .. }
            | Self::MalformedState { .. } => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueDecision {
    Approve,
    Reject,
    Defer,
}

impl TryFrom<Value> for QueueDocument {
    type Error = StoreError;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        Self::from_value(value)
    }
}

pub fn read_queue(path: &Path) -> Result<Vec<QueueDocument>, StoreError> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let contents = fs::read_to_string(path).map_err(|error| io_error("read", path, error))?;
    contents
        .lines()
        .enumerate()
        .map(|(index, line)| {
            let number = index + 1;
            let value: Value =
                serde_json::from_str(line).map_err(|error| StoreError::MalformedQueue {
                    line: number,
                    message: error.to_string(),
                })?;
            validate_queue(&value).map_err(|message| StoreError::MalformedQueue {
                line: number,
                message,
            })?;
            Ok(QueueDocument(value))
        })
        .collect()
}

pub fn list_queue_json(path: &Path) -> Result<Vec<u8>, StoreError> {
    let mut output = Vec::new();
    for row in read_queue(path)? {
        if matches!(row.state(), Some("pending" | "deferred")) {
            serde_json::to_writer(&mut output, row.value())
                .expect("writing JSON to memory cannot fail");
            output.push(b'\n');
        }
    }
    Ok(output)
}

pub fn lint_queue_state(path: &Path) -> Result<Vec<u8>, QueueActionError> {
    if !path.metadata().is_ok_and(|metadata| metadata.len() > 0) {
        return Err(QueueActionError::NoSweepState {
            path: path.display().to_string(),
        });
    }
    let contents = fs::read_to_string(path).map_err(|_| QueueActionError::MalformedState {
        path: path.display().to_string(),
    })?;
    let state: Value =
        serde_json::from_str(&contents).map_err(|_| QueueActionError::MalformedState {
            path: path.display().to_string(),
        })?;
    let selectors = state
        .get("dead_selectors")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut output = Vec::new();
    for selector in selectors {
        let Some(source) = selector.get("source").and_then(Value::as_str) else {
            return Err(QueueActionError::MalformedState {
                path: path.display().to_string(),
            });
        };
        let Some(value) = selector.get("selector").and_then(Value::as_str) else {
            return Err(QueueActionError::MalformedState {
                path: path.display().to_string(),
            });
        };
        if let Some(repository) = selector.get("repo").and_then(Value::as_str) {
            writeln!(
                &mut output,
                "{repository}: unmatched in last sweep — {source} {value}"
            )
            .expect("writing to memory cannot fail");
        } else {
            writeln!(&mut output, "unmatched in last sweep — {source} {value}")
                .expect("writing to memory cannot fail");
        }
    }
    Ok(output)
}

pub fn decide_queue_item(
    queue_path: &Path,
    state_path: &Path,
    events_path: &Path,
    id: &str,
    decision: QueueDecision,
    event_time: Option<&str>,
) -> Result<Vec<u8>, QueueActionError> {
    let mut rows = read_queue(queue_path).map_err(|_| QueueActionError::CannotRead {
        path: queue_path.display().to_string(),
    })?;
    let index = rows
        .iter()
        .position(|row| {
            row.string("id") == Some(id) && matches!(row.state(), Some("pending" | "deferred"))
        })
        .ok_or_else(|| QueueActionError::UnknownItem(id.to_owned()))?;
    let original = rows[index].value().clone();
    let repository = rows[index].string("repo").unwrap_or_default().to_owned();

    if decision == QueueDecision::Approve && repository_is_paused(state_path, &repository) {
        return Err(QueueActionError::Paused(repository));
    }

    match decision {
        QueueDecision::Approve => rows[index].0["state"] = Value::String("approved".to_owned()),
        QueueDecision::Defer => rows[index].0["state"] = Value::String("deferred".to_owned()),
        QueueDecision::Reject => {
            rows.remove(index);
        }
    }
    write_queue(queue_path, &rows).map_err(|source| QueueActionError::Write {
        path: queue_path.display().to_string(),
        source,
    })?;

    if decision == QueueDecision::Reject {
        append_selector_event(state_path, events_path, id, &repository, event_time)?;
    }

    let mut rendered = original;
    if decision == QueueDecision::Approve {
        rendered["state"] = Value::String("approved".to_owned());
    } else if decision == QueueDecision::Defer {
        rendered["state"] = Value::String("deferred".to_owned());
    }
    let mut output = serde_json::to_vec(&rendered).expect("JSON value serializes");
    output.push(b'\n');
    if decision == QueueDecision::Approve {
        let reference = rendered
            .get("ref")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mandate = rendered
            .pointer("/mandate/reason")
            .and_then(Value::as_str)
            .or_else(|| rendered.get("mandate").and_then(Value::as_str))
            .unwrap_or_default();
        writeln!(
            &mut output,
            "HANDOFF {repository} {reference} — invoke /handoff with approval token mandate:{id}; mandate: {mandate}"
        )
        .expect("writing to memory cannot fail");
    }
    Ok(output)
}

fn repository_is_paused(path: &Path, repository: &str) -> bool {
    let Ok(contents) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(state) = serde_json::from_str::<Value>(&contents) else {
        return false;
    };
    state
        .pointer(&format!(
            "/repos/{}/policy/paused",
            repository.replace('~', "~0").replace('/', "~1")
        ))
        .and_then(Value::as_bool)
        == Some(true)
}

fn append_selector_event(
    state_path: &Path,
    events_path: &Path,
    id: &str,
    repository: &str,
    event_time: Option<&str>,
) -> Result<(), QueueActionError> {
    let lookup = fs::read_to_string(state_path)
        .ok()
        .and_then(|contents| serde_json::from_str::<Value>(&contents).ok())
        .and_then(|state| {
            state
                .get("repos")?
                .get(repository)?
                .get("items")?
                .get(id)
                .cloned()
        })
        .unwrap_or(Value::Null);
    let timestamp = event_time.map_or_else(
        || {
            chrono::DateTime::<chrono::Utc>::from(std::time::SystemTime::now())
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string()
        },
        str::to_owned,
    );
    let event = serde_json::json!({
        "ts": timestamp,
        "id": id,
        "decision": "reject",
        "matched_selector": lookup.get("matched_selector").cloned().unwrap_or(Value::Null),
        "classification": lookup.get("classification").cloned().unwrap_or(Value::Null),
    });
    if let Some(parent) = events_path.parent() {
        fs::create_dir_all(parent).map_err(|source| QueueActionError::Write {
            path: events_path.display().to_string(),
            source: io_error("create selector events directory", parent, source),
        })?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(events_path)
        .map_err(|source| QueueActionError::Write {
            path: events_path.display().to_string(),
            source: io_error("open selector events", events_path, source),
        })?;
    set_private_file_mode(events_path).map_err(|source| QueueActionError::Write {
        path: events_path.display().to_string(),
        source,
    })?;
    let mut bytes = serde_json::to_vec(&event).expect("JSON value serializes");
    bytes.push(b'\n');
    file.write_all(&bytes)
        .map_err(|source| QueueActionError::Write {
            path: events_path.display().to_string(),
            source: io_error("append selector event", events_path, source),
        })
}

pub fn write_queue(path: &Path, rows: &[QueueDocument]) -> Result<(), StoreError> {
    let mut bytes = Vec::new();
    for row in rows {
        validate_queue(row.value())
            .map_err(|message| StoreError::MalformedQueue { line: 0, message })?;
        serde_json::to_writer(&mut bytes, row.value()).expect("writing JSON to memory cannot fail");
        bytes.push(b'\n');
    }
    if fs::read(path).is_ok_and(|current| current == bytes) {
        return Ok(());
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| io_error("create directory", parent, error))?;
    let temporary = temporary_path(path);
    let mut file = fs::File::create(&temporary)
        .map_err(|error| io_error("create temporary queue", &temporary, error))?;
    set_private_file_mode(&temporary)?;
    file.write_all(&bytes)
        .map_err(|error| io_error("write temporary queue", &temporary, error))?;
    file.sync_all()
        .map_err(|error| io_error("sync temporary queue", &temporary, error))?;
    fs::rename(&temporary, path).map_err(|error| io_error("install queue", path, error))
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map_or_else(|| "queue".into(), std::ffi::OsString::from);
    name.push(format!(".tmp.{}", std::process::id()));
    path.with_file_name(name)
}

fn validate_queue(value: &Value) -> Result<(), String> {
    let object = value
        .as_object()
        .ok_or_else(|| "row must be a JSON object".to_owned())?;
    let keys: HashSet<&str> = object.keys().map(String::as_str).collect();
    if let Some(missing) = REQUIRED.iter().find(|key| !keys.contains(**key)) {
        return Err(format!("missing required field {missing}"));
    }
    if let Some(extra) = keys.iter().find(|key| !ALLOWED.contains(key)) {
        return Err(format!("unknown field {extra}"));
    }
    require_string(object, "id")?;
    require_string(object, "repo")?;
    if object
        .get("item_type")
        .is_some_and(|value| !matches!(value.as_str(), Some("issue" | "pull_request")))
    {
        return Err("item_type is not recognized".to_owned());
    }
    let reference = require_string(object, "ref")?;
    let kind = require_string(object, "kind")?;
    let issue_reference = reference.strip_prefix('#').is_some_and(|digits| {
        !digits.is_empty() && digits.chars().all(|character| character.is_ascii_digit())
    });
    let branch_reference = kind == "unexplained-write"
        && reference.strip_prefix('@').is_some_and(|branch| {
            !branch.is_empty()
                && !branch
                    .chars()
                    .any(|character| character.is_whitespace() || character.is_control())
        });
    if !issue_reference && !branch_reference {
        return Err("ref must have the shape #N or an unexplained-write @branch".to_owned());
    }
    if !matches!(
        kind,
        "tripwire"
            | "decision"
            | "moved"
            | "stuck"
            | "drift"
            | "parked"
            | "merge-gate-fault"
            | "unexplained-write"
    ) {
        return Err("kind is not recognized".to_owned());
    }
    let state = require_string(object, "state")?;
    if !matches!(state, "pending" | "approved" | "deferred") {
        return Err("state is not recognized".to_owned());
    }
    require_string(object, "opened")?;
    if object
        .get("age_days")
        .is_some_and(|value| value.as_u64().is_none())
    {
        return Err("age_days must be a non-negative integer".to_owned());
    }
    for field in ["aged_out", "needs_judgment"] {
        if object.get(field).is_some_and(|value| !value.is_boolean()) {
            return Err(format!("{field} must be boolean"));
        }
    }
    if let Some(blocked) = object.get("blocked_by") {
        let Some(blocked) = blocked.as_array() else {
            return Err("blocked_by must be an array".to_owned());
        };
        if blocked
            .iter()
            .any(|item| item.as_str().is_none_or(|value| !valid_blocked_by(value)))
        {
            return Err("blocked_by entries must have the shape owner/repo#N".to_owned());
        }
    }
    if object
        .get("title")
        .is_some_and(|title| title.as_str().is_none_or(str::is_empty))
    {
        return Err("title must be a non-empty string".to_owned());
    }
    validate_semantic_fields(object)?;
    Ok(())
}

fn validate_semantic_fields(object: &serde_json::Map<String, Value>) -> Result<(), String> {
    let fields_present = ["semantic_derivation", "classification", "matched_selector"]
        .map(|field| object.contains_key(field));
    if fields_present.iter().all(|present| !present) {
        return Ok(());
    }
    if !fields_present.iter().all(|present| *present) {
        return Err(
            "semantic_derivation, classification, and matched_selector must appear together"
                .to_owned(),
        );
    }
    object
        .get("classification")
        .and_then(Value::as_str)
        .filter(|value| {
            matches!(
                *value,
                "delegated" | "excluded" | "unclassified" | "reserved" | "tripwire"
            )
        })
        .ok_or_else(|| "classification is not recognized".to_owned())?;
    object
        .get("matched_selector")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "matched_selector must be a non-empty string".to_owned())?;
    let semantic = object
        .get("semantic_derivation")
        .and_then(Value::as_object)
        .ok_or_else(|| "semantic_derivation must be an object".to_owned())?;
    let findings = semantic
        .get("findings")
        .and_then(Value::as_array)
        .ok_or_else(|| "semantic_derivation.findings must be an array".to_owned())?;
    for finding in findings {
        validate_semantic_finding(finding)?;
    }
    if let Some(authority) = semantic.get("authority").filter(|value| !value.is_null()) {
        let authority = authority
            .as_object()
            .ok_or_else(|| "semantic_derivation.authority must be null or an object".to_owned())?;
        let authority_classification = authority
            .get("classification")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !matches!(
            authority_classification,
            "unclassified" | "reserved" | "tripwire"
        ) {
            return Err(
                "semantic_derivation.authority classification is not recognized".to_owned(),
            );
        }
        validate_confidence(authority.get("confidence"))?;
        validate_evidence(authority.get("evidence"))?;
    }
    Ok(())
}

fn validate_semantic_finding(value: &Value) -> Result<(), String> {
    let finding = value
        .as_object()
        .ok_or_else(|| "semantic finding must be an object".to_owned())?;
    let kind = finding
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !matches!(
        kind,
        "parked" | "already_decided" | "genuinely_stuck" | "actually_a_release"
    ) {
        return Err("semantic finding kind is not recognized".to_owned());
    }
    validate_confidence(finding.get("confidence"))?;
    validate_evidence(finding.get("evidence"))
}

fn validate_confidence(value: Option<&Value>) -> Result<(), String> {
    if value
        .and_then(Value::as_f64)
        .is_none_or(|value| !(0.0..=1.0).contains(&value))
    {
        return Err("semantic confidence must be between zero and one".to_owned());
    }
    Ok(())
}

fn validate_evidence(value: Option<&Value>) -> Result<(), String> {
    let evidence = value
        .and_then(Value::as_object)
        .ok_or_else(|| "semantic evidence must be an object".to_owned())?;
    if !matches!(
        evidence.get("source").and_then(Value::as_str),
        Some("title" | "label" | "body" | "comment")
    ) {
        return Err("semantic evidence source is not recognized".to_owned());
    }
    evidence
        .get("quote")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "semantic evidence quote must be a non-empty string".to_owned())?;
    Ok(())
}

fn valid_blocked_by(value: &str) -> bool {
    if value.chars().any(char::is_whitespace) {
        return false;
    }
    let Some((repository, number)) = value.rsplit_once('#') else {
        return false;
    };
    if repository.contains('#') {
        return false;
    }
    let mut repository_parts = repository.split('/');
    matches!(
        (
            repository_parts.next(),
            repository_parts.next(),
            repository_parts.next()
        ),
        (Some(owner), Some(name), None) if !owner.is_empty() && !name.is_empty()
    ) && number.starts_with(|character: char| character.is_ascii_digit() && character != '0')
        && number.chars().all(|character| character.is_ascii_digit())
}

fn require_string<'a>(
    object: &'a serde_json::Map<String, Value>,
    field: &str,
) -> Result<&'a str, String> {
    object
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{field} must be a string"))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use serde_json::json;

    use super::{QueueDocument, list_queue_json, read_queue, valid_blocked_by};

    #[test]
    fn blocked_by_matches_bash_grammar() {
        assert!(valid_blocked_by("synthetic-org/project#1"));
        for malformed in [
            "synthetic#org/project#1",
            "synthetic-org/pro#ject#1",
            "synthetic-org/project#0",
            "synthetic-org/project#01",
            "synthetic-org/group/project#1",
            "synthetic-org/project name#1",
        ] {
            assert!(!valid_blocked_by(malformed), "accepted {malformed}");
        }
    }

    #[test]
    fn malformed_queue_is_named_by_line() {
        let fixture = tempdir().expect("temp dir");
        let queue = fixture.path().join("queue.jsonl");
        fs::write(&queue, "{not json}\n").expect("write fixture");
        let error = read_queue(&queue).expect_err("bad queue must fail");
        assert!(error.to_string().contains("malformed queue row 1"));
    }

    #[test]
    fn unexplained_branch_write_accepts_its_reserved_reference_shape() {
        let row = json!({
            "id": "placeholder-org/alpha@refs/heads/ostrom/item",
            "repo": "placeholder-org/alpha",
            "ref": "@ostrom/item",
            "title": "Pushed branch ostrom/item",
            "kind": "unexplained-write",
            "mandate": {"reason": "placeholder alarm"},
            "state": "pending",
            "opened": "2026-08-01T00:00:00Z",
        });
        QueueDocument::from_value(row).expect("unexplained branch row is valid");
    }

    #[test]
    fn list_filters_with_bash_order_and_newlines() {
        let fixture = tempdir().expect("temp dir");
        let queue = fixture.path().join("queue.jsonl");
        let pending = r##"{"id":"example-org/repo#1","repo":"example-org/repo","ref":"#1","title":"Synthetic","kind":"decision","mandate":{"reason":"synthetic"},"state":"pending","opened":"2030-01-01T00:00:00Z"}"##;
        let approved = pending.replace("#1", "#2").replace("pending", "approved");
        fs::write(&queue, format!("{pending}\n{approved}\n")).expect("write fixture");
        assert_eq!(
            list_queue_json(&queue).expect("list queue"),
            format!("{pending}\n").as_bytes()
        );
    }
}
