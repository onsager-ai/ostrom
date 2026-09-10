use std::fs;
use std::process::{Command, Stdio};

use tempfile::tempdir;
use umwelt_capture::{ChildStdoutSource, FileLineSource, SliceLineSource};

#[test]
fn slice_and_file_sources_yield_the_same_lines() {
    let directory = tempdir().expect("create source directory");
    let path = directory.path().join("raw.ndjson");
    fs::write(&path, "first\r\nsecond\n").expect("write line source");
    let memory = ["first", "second"];

    let from_memory = SliceLineSource::new(&memory)
        .collect::<Result<Vec<_>, _>>()
        .expect("read memory lines");
    let from_file = FileLineSource::open(path)
        .expect("open file lines")
        .collect::<Result<Vec<_>, _>>()
        .expect("read file lines");

    assert_eq!(from_memory, from_file);
}

#[test]
fn child_stdout_source_yields_process_lines() {
    let mut child = Command::new("sh")
        .args(["-c", "printf 'first\\nsecond\\n'"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn source process");
    let stdout = child.stdout.take().expect("piped child stdout");

    let lines = ChildStdoutSource::new(stdout)
        .collect::<Result<Vec<_>, _>>()
        .expect("read child stdout lines");
    let status = child.wait().expect("wait for source process");

    assert!(status.success());
    assert_eq!(lines, ["first", "second"]);
}

#[test]
fn child_stdout_source_keeps_raw_bytes_and_line_endings() {
    let directory = tempdir().expect("create raw capture directory");
    let raw_path = directory.path().join("raw.ndjson");
    let raw = fs::File::create(&raw_path).expect("create raw capture");
    let mut child = Command::new("sh")
        .args(["-c", "printf 'first\r\nsecond'"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn source process");
    let stdout = child.stdout.take().expect("piped child stdout");

    let lines = ChildStdoutSource::with_raw_capture(stdout, raw)
        .collect::<Result<Vec<_>, _>>()
        .expect("read and capture child stdout lines");
    let status = child.wait().expect("wait for source process");

    assert!(status.success());
    assert_eq!(lines, ["first", "second"]);
    assert_eq!(
        fs::read(raw_path).expect("read raw capture"),
        b"first\r\nsecond"
    );
}
