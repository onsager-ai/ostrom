use std::{fs, io::Write, path::Path};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{StoreError, io_error, set_private_file_mode};

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

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::Map;
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
}
