use std::{
    collections::HashSet,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use serde_json::Value;

use crate::{StoreError, io_error, set_private_file_mode};

const REQUIRED: &[&str] = &["id", "kind", "mandate", "opened", "ref", "repo", "state"];
const ALLOWED: &[&str] = &[
    "age_days",
    "aged_out",
    "blocked_by",
    "classification",
    "id",
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

pub fn write_queue(path: &Path, rows: &[QueueDocument]) -> Result<(), StoreError> {
    for row in rows {
        validate_queue(row.value())
            .map_err(|message| StoreError::MalformedQueue { line: 0, message })?;
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| io_error("create directory", parent, error))?;
    let temporary = temporary_path(path);
    let mut file = fs::File::create(&temporary)
        .map_err(|error| io_error("create temporary queue", &temporary, error))?;
    set_private_file_mode(&temporary)?;
    for row in rows {
        serde_json::to_writer(&mut file, row.value())
            .map_err(|error| io_error("write temporary queue", &temporary, error.into()))?;
        file.write_all(b"\n")
            .map_err(|error| io_error("write temporary queue", &temporary, error))?;
    }
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
    let reference = require_string(object, "ref")?;
    if reference.strip_prefix('#').is_none_or(|digits| {
        digits.is_empty() || !digits.chars().all(|character| character.is_ascii_digit())
    }) {
        return Err("ref must have the shape #N".to_owned());
    }
    let kind = require_string(object, "kind")?;
    if !matches!(
        kind,
        "tripwire" | "decision" | "moved" | "stuck" | "drift" | "parked" | "merge-gate-fault"
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

    use super::{list_queue_json, read_queue, valid_blocked_by};

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
