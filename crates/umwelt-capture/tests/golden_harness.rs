use std::cell::Cell;
use std::fs;
use std::path::Path;

use ethogram::EventDraft;
use serde_json::json;
use tempfile::TempDir;
use umwelt_capture::golden::{FIXED_RUN_ID, FIXED_TS, GoldenError, run_case, walk_corpus};
use umwelt_capture::{CaptureFault, Normaliser};

#[derive(Clone)]
struct StubNormaliser {
    event_type: &'static str,
    reject: bool,
}

impl StubNormaliser {
    fn accepting(event_type: &'static str) -> Self {
        Self {
            event_type,
            reject: false,
        }
    }

    fn rejecting() -> Self {
        Self {
            event_type: "stub.event",
            reject: true,
        }
    }
}

impl Normaliser for StubNormaliser {
    fn line(&mut self, raw: &str) -> Result<Vec<EventDraft>, CaptureFault> {
        if self.reject {
            return Err(CaptureFault::NormaliserRejected {
                reason: "stub refusal".to_owned(),
            });
        }
        Ok(vec![EventDraft {
            event_type: self.event_type.to_owned(),
            payload: json!({ "a": 1, "b": raw }),
            captured_at: Some("2026-09-07T12:34:56.000Z".to_owned()),
        }])
    }

    fn finish(&mut self) -> Result<Vec<EventDraft>, CaptureFault> {
        Ok(Vec::new())
    }
}

#[test]
fn differing_payload_key_order_fails_the_comparison() {
    let fixture = Fixture::new();
    let case = fixture.case("key-order");
    write_case(
        &case,
        "value\n",
        &format!(
            "{{\"v\":1,\"type\":\"stub.event\",\"runId\":\"{FIXED_RUN_ID}\",\"seq\":1,\"ts\":\"{FIXED_TS}\",\"payload\":{{\"b\":\"value\",\"a\":1}},\"capturedAt\":\"2026-09-07T12:34:56.000Z\"}}\n"
        ),
        true,
    );

    assert!(matches!(
        run_case(|| StubNormaliser::accepting("stub.event"), &case),
        Err(GoldenError::ExpectedMismatch { line: 1, .. })
    ));
}

#[test]
fn matching_case_preserves_captured_at_and_passes_both_sources() {
    let fixture = Fixture::new();
    let case = fixture.case("matching");
    write_case(
        &case,
        "value\n",
        &format!(
            "{{\"v\":1,\"type\":\"stub.event\",\"runId\":\"{FIXED_RUN_ID}\",\"seq\":1,\"ts\":\"{FIXED_TS}\",\"payload\":{{\"a\":1,\"b\":\"value\"}},\"capturedAt\":\"2026-09-07T12:34:56.000Z\"}}\n"
        ),
        true,
    );

    let report = run_case(|| StubNormaliser::accepting("stub.event"), &case)
        .expect("matching canonical bytes should pass");
    assert_eq!(report.events, 1);
    assert_eq!(report.metadata.harness, "stub");
}

#[test]
fn differing_file_and_memory_output_fails() {
    let fixture = Fixture::new();
    let case = fixture.case("source-parity");
    write_case(&case, "value\n", "", true);
    let calls = Cell::new(0);

    let result = run_case(
        || {
            let call = calls.get();
            calls.set(call + 1);
            if call == 0 {
                StubNormaliser::accepting("stub.memory")
            } else {
                StubNormaliser::accepting("stub.file")
            }
        },
        &case,
    );

    assert!(matches!(
        result,
        Err(GoldenError::SourceMismatch { line: 1, .. })
    ));
}

#[test]
fn missing_metadata_is_an_error() {
    let fixture = Fixture::new();
    let case = fixture.case("missing-meta");
    write_case(&case, "value\n", "", false);

    assert!(matches!(
        run_case(|| StubNormaliser::accepting("stub.event"), &case),
        Err(GoldenError::MetadataMissing { .. })
    ));
}

#[test]
fn unparsable_metadata_is_an_error() {
    let fixture = Fixture::new();
    let case = fixture.case("invalid-meta");
    write_case(&case, "value\n", "", true);
    fs::write(case.join("meta.toml"), "harness = [not valid\n")
        .expect("overwrite metadata fixture");

    assert!(matches!(
        run_case(|| StubNormaliser::accepting("stub.event"), &case),
        Err(GoldenError::MetadataInvalid { .. })
    ));
}

#[test]
fn accepted_refusal_line_fails_the_battery() {
    let fixture = Fixture::new();
    let refuses = fixture.root().join("refuses");
    fs::create_dir_all(&refuses).expect("create refuses directory");
    fs::write(refuses.join("unknown.ndjson"), "accepted\n").expect("write refusal fixture");

    assert!(matches!(
        walk_corpus(|| StubNormaliser::accepting("stub.event"), fixture.root()),
        Err(GoldenError::RefusalAccepted { line: 1, .. })
    ));
}

#[test]
fn rejected_refusal_line_is_counted() {
    let fixture = Fixture::new();
    let refuses = fixture.root().join("refuses");
    fs::create_dir_all(&refuses).expect("create refuses directory");
    fs::write(refuses.join("unknown.ndjson"), "unknown\n").expect("write refusal fixture");

    let report = walk_corpus(StubNormaliser::rejecting, fixture.root())
        .expect("rejection should pass the battery");
    assert_eq!(report.cases, 0);
    assert_eq!(report.refusals, 1);
}

#[test]
fn empty_corpus_succeeds_and_reports_zero_cases() {
    let fixture = Fixture::new();

    let report = walk_corpus(|| StubNormaliser::accepting("stub.event"), fixture.root())
        .expect("an existing empty corpus is valid");
    assert_eq!(report.cases, 0);
    assert_eq!(report.refusals, 0);
}

#[test]
fn end_to_end_capture_is_not_treated_as_a_normaliser_golden() {
    let fixture = Fixture::new();
    let case = fixture.case("runtime-capture");
    fs::create_dir_all(&case).expect("create capture fixture directory");
    fs::write(case.join("raw.ndjson"), "value\n").expect("write raw capture");
    fs::write(case.join("events.jsonl"), "runtime-owned events\n").expect("write event capture");
    fs::write(
        case.join("meta.toml"),
        concat!(
            "harness = \"stub\"\n",
            "cli_version = \"1.0.0\"\n",
            "captured_at = \"2026-09-07\"\n",
            "exercises = [\"the runtime path\"]\n",
        ),
    )
    .expect("write capture metadata");

    let report = walk_corpus(|| StubNormaliser::accepting("stub.event"), fixture.root())
        .expect("capture-only fixture should be left to its dedicated test");
    assert_eq!(report.cases, 0);
    assert_eq!(report.refusals, 0);
}

struct Fixture {
    directory: TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            directory: tempfile::tempdir().expect("create fixture directory"),
        }
    }

    fn root(&self) -> &Path {
        self.directory.path()
    }

    fn case(&self, name: &str) -> std::path::PathBuf {
        self.root().join(name)
    }
}

fn write_case(case: &Path, raw: &str, expected: &str, with_metadata: bool) {
    fs::create_dir_all(case).expect("create case directory");
    fs::write(case.join("raw.ndjson"), raw).expect("write raw fixture");
    fs::write(case.join("expected.jsonl"), expected).expect("write expected fixture");
    if with_metadata {
        fs::write(
            case.join("meta.toml"),
            concat!(
                "harness = \"stub\"\n",
                "cli_version = \"1.0.0\"\n",
                "captured_at = \"2026-09-07\"\n",
                "exercises = [\"the golden harness\"]\n",
            ),
        )
        .expect("write metadata fixture");
    }
}
