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
/// top-level key order. Top-level order is the operator's and is preserved as
/// given; nested object keys are sorted by this writer explicitly, on the way
/// out, rather than left to `serde_json::Map`'s backing type. That means the
/// bytes do not depend on whether a consumer enables
/// `serde_json/preserve_order` anywhere in the dependency graph — Cargo
/// unifies features across the whole graph, so that choice would otherwise
/// reach this crate without touching it.
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
    fact: IndexMap<String, Value>,
    narration: IndexMap<String, Value>,
}

/// Copy an operator map, sorting every nested object's keys while keeping the
/// map's own top-level order, which is the operator's and is guaranteed.
fn sorted_nested_map(map: &IndexMap<String, Value>) -> IndexMap<String, Value> {
    map.iter()
        .map(|(key, value)| (key.clone(), sorted_nested(value.clone())))
        .collect()
}

/// Sort object keys recursively by UTF-8 byte order, including objects nested
/// inside arrays. Array element order is left alone.
///
/// Sorted explicitly rather than by leaning on `serde_json::Map` being a
/// `BTreeMap`: `preserve_order` backs it with an insertion-ordered map, and
/// Cargo unifies features across a whole dependency graph, so a consumer
/// enabling it anywhere would otherwise change these bytes without touching
/// this crate and without failing this crate's own CI.
fn sorted_nested(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(sorted_nested).collect()),
        Value::Object(entries) => {
            let mut sorted: Vec<(String, Value)> = entries
                .into_iter()
                .map(|(key, child)| (key, sorted_nested(child)))
                .collect();
            sorted.sort_by(|(left, _), (right, _)| left.as_bytes().cmp(right.as_bytes()));
            Value::Object(sorted.into_iter().collect())
        }
        primitive => primitive,
    }
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
        fact: sorted_nested_map(&record.fact),
        narration: sorted_nested_map(&record.narration),
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

    #[cfg(feature = "preserve-order-probe")]
    #[test]
    fn probe_feature_really_does_reorder_a_raw_value() {
        // Proves the probe is actually active. Without this, the feature could
        // silently fail to enable and the test below would pass for the wrong
        // reason -- a guard that cannot fail is not a guard.
        let raw: Value =
            serde_json::from_str(r#"{"zebra":1,"alpha":2}"#).expect("parse probe input");
        assert_eq!(
            serde_json::to_string(&raw).expect("serialise probe input"),
            r#"{"zebra":1,"alpha":2}"#,
            "preserve-order-probe must make a bare Value keep insertion order"
        );
    }

    #[test]
    fn append_sorts_nested_keys_regardless_of_serde_json_preserve_order() {
        let record = TraceAppend {
            ts: "2030-01-02T03:04:05Z".to_owned(),
            kind: "nested-order-independent".to_owned(),
            fact: Map::from_iter([
                (
                    "zebra".to_owned(),
                    json!({"mango": 1, "apple": 2, "kiwi": {"yak": 1, "bee": 2}}),
                ),
                ("alpha".to_owned(), json!(3)),
            ]),
            narration: Map::from_iter([(
                "zulu".to_owned(),
                json!([{"delta": 1, "charlie": 2}, {"bravo": 3, "alpha": 4}]),
            )]),
        };
        let expected = concat!(
            r#"{"ts":"2030-01-02T03:04:05Z","kind":"nested-order-independent","fact":{"zebra":{"apple":2,"kiwi":{"bee":2,"yak":1},"mango":1},"alpha":3},"narration":{"zulu":[{"charlie":2,"delta":1},{"alpha":4,"bravo":3}]}}"#,
            "\n"
        );

        assert_eq!(
            append_trace(&mut Vec::new(), &record).expect("append nested trace"),
            expected.as_bytes()
        );
    }
}
