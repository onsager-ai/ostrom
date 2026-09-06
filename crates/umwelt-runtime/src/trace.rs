use std::io::{self, Write};

use indexmap::IndexMap;
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TraceFactRecord {
    pub ts: String,
    pub kind: String,
    pub fact: IndexMap<String, Value>,
}

/// One trace record to append.
///
/// Producers must deserialize `fact` and `narration` directly into `IndexMap`.
/// Deserializing into [`serde_json::Value`] first silently loses the operator's
/// top-level key order because JSON objects are sorted without
/// `serde_json/preserve_order`. This guarantee covers only the top level:
/// nested objects remain `Value::Object` maps and therefore serialize with
/// sorted keys.
#[derive(Debug, Clone, PartialEq)]
pub struct TraceAppend {
    pub ts: String,
    pub kind: String,
    pub fact: IndexMap<String, Value>,
    pub narration: IndexMap<String, Value>,
}

#[derive(Serialize)]
struct SerializedTraceAppend<'a> {
    ts: &'a str,
    kind: &'a str,
    fact: &'a IndexMap<String, Value>,
    narration: &'a IndexMap<String, Value>,
}

#[derive(Debug, Error)]
pub enum TraceAppendError {
    #[error("malformed trace record: trace ts and kind must not be empty")]
    Malformed,
    #[error("trace record is {bytes} bytes; maximum is 4096")]
    TooLarge { bytes: usize },
    #[error("could not append trace record: {0}")]
    Write(#[source] io::Error),
}

/// Serialize and append one bounded trace record to the supplied writer.
pub fn append_trace(
    writer: &mut (impl Write + ?Sized),
    record: &TraceAppend,
) -> Result<Vec<u8>, TraceAppendError> {
    if record.ts.is_empty() || record.kind.is_empty() {
        return Err(TraceAppendError::Malformed);
    }
    let serialized = SerializedTraceAppend {
        ts: &record.ts,
        kind: &record.kind,
        fact: &record.fact,
        narration: &record.narration,
    };
    let mut bytes = serde_json::to_vec(&serialized).expect("trace record serializes");
    bytes.push(b'\n');
    if bytes.len() > 4096 {
        return Err(TraceAppendError::TooLarge { bytes: bytes.len() });
    }
    writer.write_all(&bytes).map_err(TraceAppendError::Write)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use indexmap::IndexMap as Map;
    use serde_json::{Value, json};

    use super::{TraceAppend, TraceAppendError, append_trace};

    #[test]
    fn malformed_append_is_named_as_a_trace_error() {
        let error = append_trace(
            &mut Vec::new(),
            &TraceAppend {
                ts: String::new(),
                kind: "pass-started".to_owned(),
                fact: Map::new(),
                narration: Map::new(),
            },
        )
        .expect_err("empty trace timestamp must fail");
        assert!(matches!(error, TraceAppendError::Malformed));
        assert_eq!(
            error.to_string(),
            "malformed trace record: trace ts and kind must not be empty"
        );
    }

    #[test]
    fn append_keeps_trace_jsonl_bytes_unchanged() {
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
        let mut output = Vec::new();

        assert_eq!(
            append_trace(&mut output, &record).expect("append trace"),
            expected.as_bytes()
        );
        assert_eq!(output, expected.as_bytes());
    }

    #[test]
    fn append_accepts_local_trace_kinds_without_classifying_them() {
        let record = TraceAppend {
            ts: "2030-01-02T03:04:05Z".to_owned(),
            kind: "decision-taken".to_owned(),
            fact: Map::from_iter([("owner".to_owned(), json!("synthetic-run"))]),
            narration: Map::new(),
        };
        let mut output = Vec::new();

        append_trace(&mut output, &record).expect("append local trace kind");
        assert!(
            String::from_utf8(output)
                .expect("trace UTF-8")
                .contains("decision-taken")
        );
    }

    #[test]
    fn append_preserves_top_level_operator_order_but_sorts_nested_object_keys() {
        let record = TraceAppend {
            ts: "2030-01-02T03:04:05Z".to_owned(),
            kind: "nested-order-limit".to_owned(),
            fact: serde_json::from_str::<Map<String, Value>>(
                r#"{"zebra":{"zebra":1,"alpha":2},"alpha":3}"#,
            )
            .expect("deserialize fact directly into an ordered map"),
            narration: Map::new(),
        };
        let expected = concat!(
            r#"{"ts":"2030-01-02T03:04:05Z","kind":"nested-order-limit","fact":{"zebra":{"alpha":2,"zebra":1},"alpha":3},"narration":{}}"#,
            "\n"
        );

        assert_eq!(
            append_trace(&mut Vec::new(), &record).expect("append nested trace"),
            expected.as_bytes()
        );
    }
}
