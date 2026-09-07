use std::{fs, path::PathBuf};

#[test]
fn sink_calls_ethogram_validate_on_append_and_forward() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/sink.rs");
    let source = fs::read_to_string(&path).expect("read sink.rs");
    // Defend CLAUDE.md principle 2: consumers built on this documented guard
    // before either call existed (#37). Check each production method, ignoring
    // comments; the behavioral battery separately proves refusals and ordering.
    let implementation = source
        .split("impl Sink for FileSink {")
        .nth(1)
        .expect("FileSink implementation");
    for (method, next, argument) in [
        ("append", "forward", "draft"),
        ("forward", "last_seq", "event"),
    ] {
        let body = implementation
            .split(&format!("fn {method}("))
            .nth(1)
            .expect("sink method")
            .split(&format!("fn {next}("))
            .next()
            .expect("method body");
        let code: String = body
            .lines()
            .map(|line| line.split("//").next().unwrap_or_default())
            .collect();
        assert!(code.contains(&format!("ethogram::validate(&{argument}.event_type, &{argument}.payload).map_err(SinkFault::from)?;")),
            "{}: {method} must propagate ethogram::validate before stamping/sequence checks", path.display());
    }
}
