use std::{fs, io::Write, path::Path};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{StoreError, event_store::append_trace_event, io_error, set_private_file_mode};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TraceFactRecord {
    pub ts: String,
    pub kind: String,
    pub fact: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MalformedTraceRow {
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for MalformedTraceRow {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "malformed sprint trace record at line {}: {}",
            self.line, self.message
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TraceRead {
    pub rows: Vec<Result<TraceFactRecord, MalformedTraceRow>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TraceAppend {
    pub ts: String,
    pub kind: String,
    pub fact: serde_json::Map<String, Value>,
    pub narration: serde_json::Map<String, Value>,
}

/// Facts and narration remain separate read paths because narration is
/// principal-facing context, not evidence one delivery role may consume from
/// another role's trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceView {
    Facts,
    Narration,
}

#[derive(Debug, Error)]
pub enum TraceActionError {
    #[error("mandate trace: kind must not be empty")]
    EmptyKind,
    #[error("mandate trace: fact-json must be a JSON object")]
    FactNotObject,
    #[error("mandate trace: narration-json must be a JSON object")]
    NarrationNotObject,
    #[error(
        "mandate trace: item-worked order_id must be a non-empty string from a work order's order_id field"
    )]
    InvalidOrderId,
    #[error("mandate trace: item-worked order_id '{0}' matches no work order's order_id field")]
    UnknownOrderId(String),
    #[error("mandate trace: record is {bytes} bytes; maximum is 4096")]
    TooLarge { bytes: usize },
    #[error("mandate trace: malformed sprint trace record")]
    MalformedRead,
    #[error("mandate trace: {0}")]
    Store(#[from] StoreError),
}

impl TraceActionError {
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::Store(_) => 1,
            Self::EmptyKind
            | Self::FactNotObject
            | Self::NarrationNotObject
            | Self::InvalidOrderId
            | Self::UnknownOrderId(_)
            | Self::TooLarge { .. }
            | Self::MalformedRead => 2,
        }
    }
}

#[derive(Deserialize)]
struct RawTrace {
    ts: Value,
    kind: Value,
    fact: Value,
    narration: Value,
}

pub fn read_trace(path: &Path) -> Result<TraceRead, StoreError> {
    if !path.exists() {
        return Ok(TraceRead { rows: Vec::new() });
    }
    let contents = fs::read_to_string(path).map_err(|error| io_error("read", path, error))?;
    let rows = contents
        .lines()
        .enumerate()
        .map(|(index, line)| parse_trace_line(index + 1, line))
        .collect();
    Ok(TraceRead { rows })
}

fn parse_trace_line(line_number: usize, line: &str) -> Result<TraceFactRecord, MalformedTraceRow> {
    let value: Value = serde_json::from_str(line).map_err(|error| MalformedTraceRow {
        line: line_number,
        message: error.to_string(),
    })?;
    let object = value.as_object().ok_or_else(|| MalformedTraceRow {
        line: line_number,
        message: "row must be an object".to_owned(),
    })?;
    let expected = ["fact", "kind", "narration", "ts"];
    if object.len() != expected.len() || expected.iter().any(|key| !object.contains_key(*key)) {
        return Err(MalformedTraceRow {
            line: line_number,
            message: "fields must be exactly ts, kind, fact, narration".to_owned(),
        });
    }
    let raw: RawTrace = serde_json::from_value(value).map_err(|error| MalformedTraceRow {
        line: line_number,
        message: error.to_string(),
    })?;
    let ts = raw.ts.as_str().filter(|value| !value.is_empty());
    let kind = raw.kind.as_str().filter(|value| !value.is_empty());
    let fact = raw.fact.as_object();
    let narration = raw.narration.as_object();
    match (ts, kind, fact, narration) {
        (Some(ts), Some(kind), Some(fact), Some(_)) => Ok(TraceFactRecord {
            ts: ts.to_owned(),
            kind: kind.to_owned(),
            fact: fact.clone(),
        }),
        _ => Err(MalformedTraceRow {
            line: line_number,
            message: "ts/kind must be non-empty strings and fact/narration must be objects"
                .to_owned(),
        }),
    }
}

pub fn append_trace(path: &Path, record: &TraceAppend) -> Result<Vec<u8>, StoreError> {
    if record.ts.is_empty() || record.kind.is_empty() {
        return Err(StoreError::MalformedTrace {
            message: "trace ts and kind must not be empty".to_owned(),
        });
    }
    let mut value = serde_json::Map::new();
    value.insert("ts".to_owned(), Value::String(record.ts.clone()));
    value.insert("kind".to_owned(), Value::String(record.kind.clone()));
    value.insert("fact".to_owned(), Value::Object(record.fact.clone()));
    value.insert(
        "narration".to_owned(),
        Value::Object(record.narration.clone()),
    );
    let mut bytes = serde_json::to_vec(&Value::Object(value)).expect("JSON value serializes");
    bytes.push(b'\n');
    if bytes.len() > 4096 {
        return Err(StoreError::TraceTooLarge { bytes: bytes.len() });
    }
    append_trace_event(path, &record.ts, &record.kind, &record.fact)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| io_error("create trace directory", parent, error))?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| io_error("open trace", path, error))?;
    set_private_file_mode(path)?;
    file.write_all(&bytes)
        .map_err(|error| io_error("append trace", path, error))?;
    Ok(bytes)
}

pub fn append_trace_checked(
    path: &Path,
    work_orders: &Path,
    record: &TraceAppend,
) -> Result<Vec<u8>, TraceActionError> {
    if record.kind.is_empty() {
        return Err(TraceActionError::EmptyKind);
    }
    if record.kind == "item-worked" {
        if let Some(order_id) = record.fact.get("order_id") {
            let order_id = order_id
                .as_str()
                .filter(|value| !value.is_empty())
                .ok_or(TraceActionError::InvalidOrderId)?;
            if !order_id_exists(work_orders, order_id) {
                return Err(TraceActionError::UnknownOrderId(order_id.to_owned()));
            }
        }
    }
    append_trace(path, record).map_err(|error| match error {
        StoreError::TraceTooLarge { bytes } => TraceActionError::TooLarge { bytes },
        other => TraceActionError::Store(other),
    })
}

pub fn read_trace_json(path: &Path, view: TraceView) -> Result<Vec<u8>, TraceActionError> {
    if !path.metadata().is_ok_and(|metadata| metadata.len() > 0) {
        return Ok(Vec::new());
    }
    let contents = fs::read_to_string(path)
        .map_err(|error| TraceActionError::Store(io_error("read trace", path, error)))?;
    let mut output = Vec::new();
    for line in contents.lines() {
        let value: Value =
            serde_json::from_str(line).map_err(|_| TraceActionError::MalformedRead)?;
        validate_trace_value(&value)?;
        let mut row = serde_json::Map::new();
        row.insert("ts".to_owned(), value["ts"].clone());
        row.insert("kind".to_owned(), value["kind"].clone());
        match view {
            TraceView::Facts => row.insert("fact".to_owned(), value["fact"].clone()),
            TraceView::Narration => row.insert("narration".to_owned(), value["narration"].clone()),
        };
        serde_json::to_writer(&mut output, &row).expect("writing JSON to memory cannot fail");
        output.push(b'\n');
    }
    Ok(output)
}

fn validate_trace_value(value: &Value) -> Result<(), TraceActionError> {
    let Some(object) = value.as_object() else {
        return Err(TraceActionError::MalformedRead);
    };
    let expected = ["fact", "kind", "narration", "ts"];
    if object.len() != expected.len() || expected.iter().any(|key| !object.contains_key(*key)) {
        return Err(TraceActionError::MalformedRead);
    }
    if object
        .get("ts")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
        || object
            .get("kind")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        || !object.get("fact").is_some_and(Value::is_object)
        || !object.get("narration").is_some_and(Value::is_object)
    {
        return Err(TraceActionError::MalformedRead);
    }
    Ok(())
}

fn order_id_exists(directory: &Path, order_id: &str) -> bool {
    let Ok(entries) = fs::read_dir(directory) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        let path = entry.path();
        path.extension().and_then(|extension| extension.to_str()) == Some("json")
            && fs::read_to_string(path)
                .ok()
                .and_then(|contents| serde_json::from_str::<Value>(&contents).ok())
                .and_then(|value| value.get("order_id").cloned())
                .and_then(|value| value.as_str().map(str::to_owned))
                .as_deref()
                == Some(order_id)
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::{Map, json};
    use tempfile::tempdir;

    use super::{TraceAppend, append_trace, read_trace};
    use crate::StoreError;

    #[test]
    fn malformed_append_is_named_as_a_trace_error_without_a_fake_line() {
        let fixture = tempdir().expect("temp dir");
        let error = append_trace(
            &fixture.path().join("sprint.jsonl"),
            &TraceAppend {
                ts: String::new(),
                kind: "pass-started".to_owned(),
                fact: Map::new(),
                narration: Map::new(),
            },
        )
        .expect_err("empty trace timestamp must fail");
        assert!(matches!(error, StoreError::MalformedTrace { .. }));
        assert_eq!(
            error.to_string(),
            "malformed trace record: trace ts and kind must not be empty"
        );
    }

    #[test]
    fn malformed_row_is_named_without_discarding_later_rows() {
        let fixture = tempdir().expect("temp dir");
        let path = fixture.path().join("sprint.jsonl");
        let valid = r#"{"ts":"2030-01-01T00:00:00Z","kind":"pass-started","fact":{"owner":"synthetic"},"narration":{}}"#;
        fs::write(&path, format!("{valid}\n{{broken\n{valid}\n")).expect("write trace");
        let read = read_trace(&path).expect("file itself is readable");
        assert_eq!(read.rows.len(), 3);
        assert!(read.rows[0].is_ok());
        assert_eq!(
            read.rows[1].as_ref().expect_err("middle row is bad").line,
            2
        );
        assert!(read.rows[2].is_ok());
        assert!(
            read.rows[1]
                .as_ref()
                .expect_err("middle row is bad")
                .to_string()
                .contains("malformed sprint trace record")
        );
    }

    #[test]
    fn event_routing_keeps_sprint_jsonl_bytes_unchanged() {
        let fixture = tempdir().expect("temp dir");
        let path = fixture.path().join("sprint.jsonl");
        let record = TraceAppend {
            ts: "2030-01-02T03:04:05Z".to_owned(),
            kind: "work-failed".to_owned(),
            fact: Map::from_iter([
                ("order_id".to_owned(), json!("synthetic-order")),
                ("reason".to_owned(), json!("operator-facing explanation")),
            ]),
            narration: Map::from_iter([(
                "detail".to_owned(),
                json!("local narration remains local"),
            )]),
        };
        let expected = concat!(
            r#"{"ts":"2030-01-02T03:04:05Z","kind":"work-failed","fact":{"order_id":"synthetic-order","reason":"operator-facing explanation"},"narration":{"detail":"local narration remains local"}}"#,
            "\n"
        );
        assert_eq!(
            append_trace(&path, &record).expect("append routed trace"),
            expected.as_bytes()
        );
        assert_eq!(
            fs::read(&path).expect("read compatibility trace"),
            expected.as_bytes()
        );
        assert_eq!(
            fs::read(fixture.path().join("events.jsonl")).expect("read event journal"),
            concat!(
                r#"{"v":1,"type":"work.failed","run_id":"synthetic-order","seq":1,"ts":"2030-01-02T03:04:05Z","payload":{"order_id":"synthetic-order"}}"#,
                "\n"
            )
            .as_bytes()
        );
    }

    #[test]
    fn local_only_trace_kinds_remain_compatible_without_expanding_the_vocabulary() {
        let fixture = tempdir().expect("temp dir");
        let path = fixture.path().join("sprint.jsonl");
        let record = TraceAppend {
            ts: "2030-01-02T03:04:05Z".to_owned(),
            kind: "decision-taken".to_owned(),
            fact: Map::from_iter([("owner".to_owned(), json!("synthetic-run"))]),
            narration: Map::new(),
        };
        let expected = concat!(
            r#"{"ts":"2030-01-02T03:04:05Z","kind":"decision-taken","fact":{"owner":"synthetic-run"},"narration":{}}"#,
            "\n"
        );
        append_trace(&path, &record).expect("append local trace kind");
        assert_eq!(fs::read(&path).expect("read trace"), expected.as_bytes());
        assert!(!fixture.path().join("events.jsonl").exists());
    }
}
