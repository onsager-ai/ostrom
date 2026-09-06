//! Conversions between Ostrom-owned types and the harness runtime boundary.

use crate::TraceAppend;

/// Convert an Ostrom trace record into the harness-shaped append record.
#[must_use]
pub fn trace_append(record: &TraceAppend) -> umwelt_runtime::TraceAppend {
    umwelt_runtime::TraceAppend {
        ts: record.ts.clone(),
        kind: record.kind.clone(),
        fact: record.fact.clone(),
        narration: record.narration.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::{Map, json};
    use tempfile::tempdir;

    use super::trace_append;
    use crate::{TraceAppend, append_trace};

    #[test]
    fn trace_append_bytes_agree_with_umwelt() {
        let directory = tempdir().expect("temp dir");
        let path = directory.path().join("sprint.jsonl");
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

        let ostrom_bytes = append_trace(&path, &record).expect("append through ostrom");
        let mut umwelt_output = Vec::new();
        let umwelt_bytes = umwelt_runtime::append_trace(&mut umwelt_output, &trace_append(&record))
            .expect("append through umwelt");

        assert_eq!(ostrom_bytes, umwelt_bytes);
        assert_eq!(fs::read(path).expect("read ostrom trace"), umwelt_output);
    }
}
