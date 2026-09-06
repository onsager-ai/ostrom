//! Append-only event storage.

use std::collections::HashMap;
use std::fmt::{self, Display, Formatter};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
#[cfg(any(test, feature = "conformance"))]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use chrono::{SecondsFormat, Utc};
use ethogram::{Event, EventDraft, StampFields, parse_event, serialise_event, stamp};

use crate::event_types::RUN_FINISHED;

const EVENTS_FILE: &str = "events.jsonl";

/// A failure to store or forward an event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SinkFault {
    /// The run has already emitted `run.finished`.
    Finished,
    /// A forwarded event skips a sequence number.
    Gap { expected: u64, got: u64 },
    /// A forwarded event repeats a sequence number already held.
    Duplicate(u64),
    /// The durable log contains a torn or unparsable line.
    Malformed { line: u64, message: String },
    /// An I/O operation failed.
    Io(String),
}

impl Display for SinkFault {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Finished => write!(formatter, "run has already emitted {RUN_FINISHED}"),
            Self::Gap { expected, got } => {
                write!(
                    formatter,
                    "event sequence must be {expected}; received {got}"
                )
            }
            Self::Duplicate(seq) => write!(formatter, "event sequence {seq} is already stored"),
            Self::Malformed { line, message } => {
                write!(
                    formatter,
                    "malformed durable event at line {line}: {message}"
                )
            }
            Self::Io(message) => write!(formatter, "sink I/O failed: {message}"),
        }
    }
}

impl std::error::Error for SinkFault {}

/// Append-only storage for stamped ethogram events.
pub trait Sink: Send + Sync {
    /// Append one draft to a run, assigning its next `seq` and the sink's `ts`.
    fn append(&self, run: &str, draft: EventDraft) -> Result<Event, SinkFault>;

    /// Forward an already-stamped event, preserving `seq` and `ts` exactly.
    fn forward(&self, event: Event) -> Result<(), SinkFault>;

    /// Return the last `seq` held for `run`, or zero when nothing is held.
    fn last_seq(&self, run: &str) -> Result<u64, SinkFault>;
}

/// A file-backed sink with one `events.jsonl` log per run.
///
/// Operations are serialised only within one `FileSink` instance. Concurrent
/// writers to the same run directory from separate instances or processes are
/// not supported; a cross-process sink must provide its own locking.
#[derive(Debug)]
pub struct FileSink {
    root: PathBuf,
    operation: Mutex<HashMap<String, RunState>>,
    #[cfg(any(test, feature = "conformance"))]
    durable_reads: AtomicU64,
}

impl FileSink {
    /// Create a sink rooted at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            operation: Mutex::new(HashMap::new()),
            #[cfg(any(test, feature = "conformance"))]
            durable_reads: AtomicU64::new(0),
        }
    }

    /// Return the number of cache misses that read the durable event log.
    #[cfg(any(test, feature = "conformance"))]
    pub fn durable_read_count(&self) -> u64 {
        self.durable_reads.load(Ordering::Relaxed)
    }

    fn lock(&self) -> Result<MutexGuard<'_, HashMap<String, RunState>>, SinkFault> {
        self.operation
            .lock()
            .map_err(|_| SinkFault::Io("sink operation lock was poisoned".to_owned()))
    }

    fn event_path(&self, run: &str) -> PathBuf {
        self.root.join(run_directory_name(run)).join(EVENTS_FILE)
    }

    fn state(
        &self,
        states: &mut HashMap<String, RunState>,
        run: &str,
    ) -> Result<RunState, SinkFault> {
        let path = self.event_path(run);
        if let Some(state) = states.get(run).copied() {
            if durable_len(&path)? == state.durable_len {
                return Ok(state);
            }
            states.remove(run);
        }

        #[cfg(any(test, feature = "conformance"))]
        self.durable_reads.fetch_add(1, Ordering::Relaxed);
        let state = read_state(&path, run)?;
        states.insert(run.to_owned(), state);
        Ok(state)
    }

    fn store(&self, run: &str, event: &Event) -> Result<u64, SinkFault> {
        let serialised = serialise_event(event)
            .map_err(|error| SinkFault::Io(format!("cannot serialise event: {error}")))?;
        let run_directory = self.root.join(run_directory_name(run));
        fs::create_dir_all(&run_directory).map_err(io_fault)?;
        append_durably(&run_directory.join(EVENTS_FILE), serialised.as_bytes())
    }
}

impl Sink for FileSink {
    fn append(&self, run: &str, draft: EventDraft) -> Result<Event, SinkFault> {
        let mut states = self.lock()?;
        let state = self.state(&mut states, run)?;
        if state.finished {
            return Err(SinkFault::Finished);
        }

        let seq = state
            .last_seq
            .checked_add(1)
            .ok_or_else(|| SinkFault::Io("event sequence is exhausted".to_owned()))?;
        let event = stamp(
            draft,
            StampFields {
                run_id: run.to_owned(),
                seq,
                ts: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            },
        );
        match self.store(run, &event) {
            Ok(durable_len) => {
                // The cache follows durable state; it never predicts it.
                states.insert(run.to_owned(), RunState::after(&event, durable_len));
                Ok(event)
            }
            Err(fault) => {
                // Even after rollback, reconstruct from disk before reusing `seq`.
                states.remove(run);
                Err(fault)
            }
        }
    }

    fn forward(&self, event: Event) -> Result<(), SinkFault> {
        let mut states = self.lock()?;
        let state = self.state(&mut states, &event.run_id)?;
        if state.finished {
            return Err(SinkFault::Finished);
        }

        let expected = state
            .last_seq
            .checked_add(1)
            .ok_or_else(|| SinkFault::Io("event sequence is exhausted".to_owned()))?;
        if event.seq < expected {
            return Err(SinkFault::Duplicate(event.seq));
        }
        if event.seq > expected {
            return Err(SinkFault::Gap {
                expected,
                got: event.seq,
            });
        }

        let run = event.run_id.clone();
        match self.store(&run, &event) {
            Ok(durable_len) => {
                // Preserve the same durable-before-cached invariant as `append`.
                states.insert(run, RunState::after(&event, durable_len));
                Ok(())
            }
            Err(fault) => {
                // A failed write makes the durable log the only source of truth.
                states.remove(&run);
                Err(fault)
            }
        }
    }

    fn last_seq(&self, run: &str) -> Result<u64, SinkFault> {
        let mut states = self.lock()?;
        Ok(self.state(&mut states, run)?.last_seq)
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct RunState {
    last_seq: u64,
    finished: bool,
    durable_len: u64,
}

impl RunState {
    fn after(event: &Event, durable_len: u64) -> Self {
        Self {
            last_seq: event.seq,
            finished: event.event_type == RUN_FINISHED,
            durable_len,
        }
    }
}

fn read_state(path: &Path, run: &str) -> Result<RunState, SinkFault> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(RunState::default()),
        Err(error) => return Err(io_fault(error)),
    };
    let file_len = file.metadata().map_err(io_fault)?.len();
    let mut reader = BufReader::new(file);
    let mut bytes = Vec::new();
    let mut state = RunState::default();
    let mut line = 0_u64;

    loop {
        bytes.clear();
        let read = reader.read_until(b'\n', &mut bytes).map_err(io_fault)?;
        if read == 0 {
            break;
        }
        line = line
            .checked_add(1)
            .ok_or_else(|| SinkFault::Io("event line count is exhausted".to_owned()))?;
        if bytes.last() != Some(&b'\n') {
            return Err(SinkFault::Malformed {
                line,
                message: "final line is not newline-terminated".to_owned(),
            });
        }
        bytes.pop();
        let source = std::str::from_utf8(&bytes).map_err(|error| SinkFault::Malformed {
            line,
            message: format!("invalid UTF-8: {error}"),
        })?;
        let event = parse_event(source).map_err(|error| SinkFault::Malformed {
            line,
            message: error.to_string(),
        })?;

        let expected = state
            .last_seq
            .checked_add(1)
            .ok_or_else(|| SinkFault::Malformed {
                line,
                message: "event sequence is exhausted".to_owned(),
            })?;
        if event.run_id != run {
            return Err(SinkFault::Malformed {
                line,
                message: format!(
                    "event belongs to run {:?}, not directory run {run:?}",
                    event.run_id
                ),
            });
        }
        if event.seq != expected {
            return Err(SinkFault::Malformed {
                line,
                message: format!("event sequence must be {expected}; received {}", event.seq),
            });
        }
        if state.finished {
            return Err(SinkFault::Malformed {
                line,
                message: format!("event follows {RUN_FINISHED}"),
            });
        }

        state.last_seq = event.seq;
        state.finished = event.event_type == RUN_FINISHED;
    }

    state.durable_len = file_len;
    Ok(state)
}

fn durable_len(path: &Path) -> Result<u64, SinkFault> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(io_fault(error)),
    }
}

fn append_durably(path: &Path, event: &[u8]) -> Result<u64, SinkFault> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(io_fault)?;
    let original_len = file.metadata().map_err(io_fault)?.len();
    let event_len = u64::try_from(event.len())
        .map_err(|_| SinkFault::Io("serialised event length exceeds u64".to_owned()))?;
    let durable_len = original_len
        .checked_add(event_len)
        .and_then(|length| length.checked_add(1))
        .ok_or_else(|| SinkFault::Io("event log length is exhausted".to_owned()))?;

    let write_result = file
        .write_all(event)
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.flush())
        .and_then(|()| file.sync_data());
    if let Err(write_error) = write_result {
        return match file.set_len(original_len).and_then(|()| file.sync_data()) {
            Ok(()) => Err(io_fault(write_error)),
            Err(rollback_error) => Err(SinkFault::Io(format!(
                "{write_error}; additionally failed to roll back partial append: {rollback_error}"
            ))),
        };
    }

    Ok(durable_len)
}

fn io_fault(error: io::Error) -> SinkFault {
    SinkFault::Io(error.to_string())
}

/// Return the directory name for a run under a [`FileSink`] root.
///
/// This function is the on-disk contract for locating a run's directory. Any
/// consumer that needs to find its `events.jsonl`—including `ostrom logs`, a
/// shipper, or `--events-fd`—must call this function rather than reproduce the
/// encoding.
///
/// Every UTF-8 byte outside `[A-Za-z0-9_-]` becomes `%XX`, using uppercase
/// hexadecimal digits. The empty run ID becomes `%`. Run IDs are arbitrary wire
/// strings, so no run ID can produce `.`, `..`, or a name containing a path
/// separator. Every run ID is therefore exactly one directory beneath the sink
/// root. The encoding is injective: two distinct run IDs never share a directory
/// (`%` within an ID is itself encoded as `%25`, so it cannot collide with the
/// empty-ID sentinel).
pub fn run_directory_name(run: &str) -> String {
    if run.is_empty() {
        return "%".to_owned();
    }

    let mut encoded = String::with_capacity(run.len());
    for byte in run.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(encoded, "%{byte:02X}").expect("writing to a String cannot fail");
        }
    }
    encoded
}

/// Reusable behavioral checks for third-party sink implementations.
#[cfg(any(test, feature = "conformance"))]
pub mod conformance {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    use ethogram::{EVENT_SCHEMA_VERSION, Event, EventDraft, parse_event, serialise_event};
    use serde_json::json;

    use super::{RUN_FINISHED, Sink, SinkFault};

    static BATTERY_NUMBER: AtomicU64 = AtomicU64::new(0);

    /// Run the sink conformance battery.
    ///
    /// `new_sink` must reopen the same initially empty durable store each time it
    /// is called. `stored_event_bytes` is a test-only observer that returns each
    /// stored event's canonical bytes, without its JSONL newline, in sequence
    /// order. It exists because the production [`Sink`] deliberately has no
    /// replay API.
    pub fn run_battery<S, F, O>(new_sink: F, stored_event_bytes: O)
    where
        S: Sink + 'static,
        F: Fn() -> S,
        O: Fn(&str) -> Vec<Vec<u8>>,
    {
        let namespace = namespace();
        gapless_across_reopen(&new_sink, &stored_event_bytes, &run(&namespace, "reopen"));
        forward_is_byte_identical(&new_sink, &stored_event_bytes, &run(&namespace, "bytes"));
        forward_rejects_gap_and_duplicate(&new_sink, &run(&namespace, "sequence-errors"));
        append_rejects_finished(&new_sink, &run(&namespace, "append-finished"));
        forward_rejects_finished(&new_sink, &run(&namespace, "forward-finished"));
        append_and_forward_do_not_race(
            &new_sink,
            &stored_event_bytes,
            &run(&namespace, "interleave"),
        );
        unknown_run_has_sequence_zero(&new_sink, &run(&namespace, "unknown"));
        runs_are_independent(&new_sink, &namespace);
    }

    fn namespace() -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let number = BATTERY_NUMBER.fetch_add(1, Ordering::Relaxed);
        format!("umwelt-conformance-{}-{nanos}-{number}", std::process::id())
    }

    fn run(namespace: &str, case: &str) -> String {
        format!("{namespace}-{case}")
    }

    fn draft(event_type: &str) -> EventDraft {
        EventDraft {
            event_type: event_type.to_owned(),
            payload: json!({ "z": 1, "a": { "two": 2, "one": 1 } }),
            captured_at: None,
        }
    }

    fn event(run_id: &str, seq: u64, event_type: &str, ts: &str) -> Event {
        Event {
            v: EVENT_SCHEMA_VERSION,
            event_type: event_type.to_owned(),
            run_id: run_id.to_owned(),
            seq,
            ts: ts.to_owned(),
            payload: json!({ "z": 1, "a": { "two": 2, "one": 1 } }),
            captured_at: Some("2026-09-06T00:00:00.000Z".to_owned()),
        }
    }

    fn gapless_across_reopen<S, F, O>(new_sink: &F, stored: &O, run: &str)
    where
        S: Sink,
        F: Fn() -> S,
        O: Fn(&str) -> Vec<Vec<u8>>,
    {
        let sink = new_sink();
        assert_eq!(
            sink.append(run, draft("test.first"))
                .expect("first append")
                .seq,
            1
        );
        drop(sink);

        let reopened = new_sink();
        assert_eq!(
            reopened
                .append(run, draft("test.second"))
                .expect("append after reopen")
                .seq,
            2
        );
        assert_eq!(stored_sequences(stored, run), vec![1, 2]);
    }

    fn forward_is_byte_identical<S, F, O>(new_sink: &F, stored: &O, run: &str)
    where
        S: Sink,
        F: Fn() -> S,
        O: Fn(&str) -> Vec<Vec<u8>>,
    {
        let forwarded = event(run, 1, "test.forwarded", "1985-10-26T01:21:00.000Z");
        let expected = serialise_event(&forwarded)
            .expect("serialise forwarded fixture")
            .into_bytes();
        new_sink().forward(forwarded).expect("forward event");
        assert_eq!(stored(run), vec![expected]);
    }

    fn forward_rejects_gap_and_duplicate<S, F>(new_sink: &F, run: &str)
    where
        S: Sink,
        F: Fn() -> S,
    {
        let sink = new_sink();
        assert_eq!(
            sink.forward(event(run, 2, "test.gap", "2026-09-06T00:00:02.000Z")),
            Err(SinkFault::Gap {
                expected: 1,
                got: 2
            })
        );
        assert_eq!(sink.last_seq(run).expect("last seq after gap"), 0);
        sink.forward(event(run, 1, "test.first", "2026-09-06T00:00:01.000Z"))
            .expect("forward first event");
        assert_eq!(
            sink.forward(event(run, 1, "test.duplicate", "2026-09-06T00:00:03.000Z")),
            Err(SinkFault::Duplicate(1))
        );
        assert_eq!(sink.last_seq(run).expect("last seq after duplicate"), 1);
    }

    fn append_rejects_finished<S, F>(new_sink: &F, run: &str)
    where
        S: Sink,
        F: Fn() -> S,
    {
        let sink = new_sink();
        sink.append(run, draft(RUN_FINISHED)).expect("finish run");
        assert_eq!(
            sink.append(run, draft("test.late")),
            Err(SinkFault::Finished)
        );
        assert_eq!(sink.last_seq(run).expect("finished sequence"), 1);
    }

    fn forward_rejects_finished<S, F>(new_sink: &F, run: &str)
    where
        S: Sink,
        F: Fn() -> S,
    {
        let sink = new_sink();
        sink.forward(event(run, 1, RUN_FINISHED, "2026-09-06T00:00:01.000Z"))
            .expect("forward finish");
        assert_eq!(
            sink.forward(event(run, 2, "test.late", "2026-09-06T00:00:02.000Z")),
            Err(SinkFault::Finished)
        );
        assert_eq!(sink.last_seq(run).expect("finished sequence"), 1);
    }

    fn append_and_forward_do_not_race<S, F, O>(new_sink: &F, stored: &O, run: &str)
    where
        S: Sink + 'static,
        F: Fn() -> S,
        O: Fn(&str) -> Vec<Vec<u8>>,
    {
        let sink = Arc::new(new_sink());
        sink.append(run, draft("test.first")).expect("seed run");
        let barrier = Arc::new(Barrier::new(3));

        let append_sink = Arc::clone(&sink);
        let append_barrier = Arc::clone(&barrier);
        let append_run = run.to_owned();
        let append = thread::spawn(move || {
            append_barrier.wait();
            append_sink.append(&append_run, draft("test.appended"))
        });

        let forward_sink = Arc::clone(&sink);
        let forward_barrier = Arc::clone(&barrier);
        let forward_run = run.to_owned();
        let forward = thread::spawn(move || {
            forward_barrier.wait();
            forward_sink.forward(event(
                &forward_run,
                2,
                "test.forwarded",
                "2026-09-06T00:00:02.000Z",
            ))
        });

        barrier.wait();
        let appended = append
            .join()
            .expect("append thread")
            .expect("racing append");
        let forwarded = forward.join().expect("forward thread");
        match appended.seq {
            2 => assert_eq!(forwarded, Err(SinkFault::Duplicate(2))),
            3 => assert_eq!(forwarded, Ok(())),
            seq => panic!("racing append received unexpected sequence {seq}"),
        }

        let last = sink.last_seq(run).expect("last interleaved sequence");
        assert_eq!(
            stored_sequences(stored, run),
            (1..=last).collect::<Vec<_>>()
        );
    }

    fn unknown_run_has_sequence_zero<S, F>(new_sink: &F, run: &str)
    where
        S: Sink,
        F: Fn() -> S,
    {
        assert_eq!(new_sink().last_seq(run).expect("unknown last seq"), 0);
    }

    fn runs_are_independent<S, F>(new_sink: &F, namespace: &str)
    where
        S: Sink,
        F: Fn() -> S,
    {
        let first = run(namespace, "independent-a");
        let second = run(namespace, "independent-b");
        let sink = new_sink();
        assert_eq!(
            sink.append(&first, draft("test.a1")).expect("first a").seq,
            1
        );
        assert_eq!(
            sink.append(&first, draft("test.a2")).expect("second a").seq,
            2
        );
        assert_eq!(
            sink.append(&second, draft("test.b1")).expect("first b").seq,
            1
        );
        assert_eq!(sink.last_seq(&first).expect("last a"), 2);
        assert_eq!(sink.last_seq(&second).expect("last b"), 1);
    }

    fn stored_sequences<O>(stored: &O, run: &str) -> Vec<u64>
    where
        O: Fn(&str) -> Vec<Vec<u8>>,
    {
        stored(run)
            .into_iter()
            .map(|bytes| {
                let source = std::str::from_utf8(&bytes).expect("stored event UTF-8");
                parse_event(source).expect("stored event parses").seq
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    use std::process::Command;

    use ethogram::EventDraft;
    use serde_json::json;
    use tempfile::tempdir;

    use super::conformance::run_battery;
    use super::*;

    fn draft(event_type: &str) -> EventDraft {
        EventDraft {
            event_type: event_type.to_owned(),
            payload: json!({ "opaque": true }),
            captured_at: None,
        }
    }

    fn stored_event_bytes(root: &Path, run: &str) -> Vec<Vec<u8>> {
        let path = root.join(run_directory_name(run)).join(EVENTS_FILE);
        match fs::read(path) {
            Ok(bytes) => bytes
                .split(|byte| *byte == b'\n')
                .filter(|line| !line.is_empty())
                .map(<[u8]>::to_vec)
                .collect(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("read stored events: {error}"),
        }
    }

    #[test]
    fn file_sink_passes_conformance_battery() {
        let root = tempdir().expect("battery directory");
        run_battery(
            || FileSink::new(root.path()),
            |run| stored_event_bytes(root.path(), run),
        );
    }

    #[test]
    fn cached_state_reads_the_durable_log_once_per_run() {
        let root = tempdir().expect("sink directory");
        let sink = FileSink::new(root.path());

        for _ in 0..2_000 {
            sink.append("run", draft("test.cached"))
                .expect("cached append");
        }

        assert_eq!(sink.durable_read_count(), 1);
    }

    #[test]
    #[cfg(unix)]
    fn refused_append_does_not_consume_sequence() {
        if running_as_root() {
            return;
        }
        let root = tempdir().expect("sink directory");
        let sink = FileSink::new(root.path());
        let first = sink
            .append("run", draft("test.first"))
            .expect("first append");
        let path = root.path().join("run").join(EVENTS_FILE);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).expect("make read-only");

        assert!(matches!(
            sink.append("run", draft("test.refused")),
            Err(SinkFault::Io(_))
        ));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("make writable");
        let second = sink
            .append("run", draft("test.second"))
            .expect("second append");

        assert_eq!(first.seq, 1);
        assert_eq!(second.seq, 2);
    }

    #[test]
    #[cfg(unix)]
    fn failed_write_evicts_cache_without_advancing_sequence() {
        if running_as_root() {
            return;
        }
        let root = tempdir().expect("sink directory");
        let sink = FileSink::new(root.path());
        sink.append("run", draft("test.first"))
            .expect("prime cache");
        let path = root.path().join("run").join(EVENTS_FILE);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).expect("make read-only");

        assert_eq!(sink.durable_read_count(), 1);
        assert!(matches!(
            sink.append("run", draft("test.failed")),
            Err(SinkFault::Io(_))
        ));
        assert_eq!(sink.durable_read_count(), 1);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("make writable");
        let recovered = sink
            .append("run", draft("test.recovered"))
            .expect("append after cache eviction");
        assert_eq!(sink.durable_read_count(), 2);
        let sequences = stored_event_bytes(root.path(), "run")
            .iter()
            .map(|bytes| {
                parse_event(std::str::from_utf8(bytes).expect("stored UTF-8"))
                    .expect("stored event")
                    .seq
            })
            .collect::<Vec<_>>();

        assert_eq!(recovered.seq, 2);
        assert_eq!(sequences, vec![1, 2]);
        assert_eq!(
            sequences.iter().filter(|sequence| **sequence == 2).count(),
            1
        );
    }

    #[test]
    #[cfg(unix)]
    fn append_is_durable_before_returning_and_failure_changes_nothing() {
        if running_as_root() {
            return;
        }
        let root = tempdir().expect("sink directory");
        let sink = FileSink::new(root.path());
        sink.append("run", draft("test.first"))
            .expect("first append");
        let path = root.path().join("run").join(EVENTS_FILE);
        let before = fs::read(&path).expect("read before failure");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).expect("make read-only");

        assert!(matches!(
            sink.append("run", draft("test.refused")),
            Err(SinkFault::Io(_))
        ));
        assert_eq!(fs::read(&path).expect("read after failure"), before);
        assert_eq!(sink.last_seq("run").expect("last sequence"), 1);
    }

    #[test]
    #[cfg(unix)]
    fn writer_failure_is_returned_every_time_and_run_directory_stays_empty() {
        if running_as_root() {
            return;
        }
        let root = tempdir().expect("sink directory");
        let run_directory = root.path().join("run");
        fs::create_dir(&run_directory).expect("create run directory");
        fs::set_permissions(&run_directory, fs::Permissions::from_mode(0o555))
            .expect("make run directory read-only");
        let sink = FileSink::new(root.path());

        assert!(matches!(
            sink.append("run", draft("test.first")),
            Err(SinkFault::Io(_))
        ));
        assert!(matches!(
            sink.append("run", draft("test.second")),
            Err(SinkFault::Io(_))
        ));
        assert_eq!(
            fs::read_dir(run_directory)
                .expect("read run directory")
                .count(),
            0
        );
    }

    #[test]
    fn torn_final_line_is_malformed_and_cannot_advance_sequence() {
        let root = tempdir().expect("sink directory");
        let sink = FileSink::new(root.path());
        sink.append("run", draft("test.first"))
            .expect("first append");
        let path = root.path().join("run").join(EVENTS_FILE);
        OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open event log")
            .write_all(br#"{\"v\":1,\"type\":\"test.torn\""#)
            .expect("write torn line");

        assert!(matches!(
            FileSink::new(root.path()).last_seq("run"),
            Err(SinkFault::Malformed { line: 2, .. })
        ));
        assert!(matches!(
            sink.append("run", draft("test.second")),
            Err(SinkFault::Malformed { line: 2, .. })
        ));
    }

    #[test]
    fn invalid_utf8_is_rejected_without_lossy_replacement() {
        let root = tempdir().expect("sink directory");
        let run_directory = root.path().join("run");
        fs::create_dir(&run_directory).expect("create run directory");
        fs::write(run_directory.join(EVENTS_FILE), [0xff, b'\n']).expect("write invalid UTF-8");

        assert!(matches!(
            FileSink::new(root.path()).last_seq("run"),
            Err(SinkFault::Malformed { line: 1, message }) if message.contains("invalid UTF-8")
        ));
    }

    #[test]
    fn run_ids_cannot_escape_the_sink_root_via_a_separator() {
        let root = tempdir().expect("sink directory");
        let sink = FileSink::new(root.path());
        sink.append("../outside/run", draft("test.safe"))
            .expect("append escaped run ID safely");

        assert!(
            root.path()
                .join("%2E%2E%2Foutside%2Frun")
                .join(EVENTS_FILE)
                .is_file()
        );
        assert!(
            !root
                .path()
                .parent()
                .expect("root parent")
                .join("outside")
                .exists()
        );
    }

    #[test]
    fn run_ids_cannot_escape_the_sink_root_via_dot_segments() {
        let parent = tempdir().expect("parent directory");
        let root = parent.path().join("sinkroot");
        let sink = FileSink::new(&root);
        sink.append("..", draft("test.dot-segment"))
            .expect("append dot-segment run ID safely");

        let directory = run_directory_name("..");
        assert!(root.join(directory).join(EVENTS_FILE).is_file());
        assert!(!root.join(EVENTS_FILE).exists());
        assert!(!parent.path().join(EVENTS_FILE).exists());
    }

    #[test]
    fn dot_bearing_run_ids_are_distinct_safe_components() {
        let runs = [".", "..", "...", "..%2F", "a.b"];
        let mut directories = runs.map(run_directory_name).to_vec();

        for directory in &directories {
            assert_ne!(directory, ".");
            assert_ne!(directory, "..");
            assert!(!directory.contains('/'));
            assert!(!directory.contains('\\'));
        }
        directories.sort();
        directories.dedup();
        assert_eq!(directories.len(), runs.len());
    }

    #[test]
    fn empty_nul_percent_and_dot_run_ids_use_distinct_directories() {
        let root = tempdir().expect("sink directory");
        let sink = FileSink::new(root.path());
        sink.append("", draft("test.empty"))
            .expect("append empty run ID");
        sink.append("\0", draft("test.nul"))
            .expect("append NUL run ID");
        sink.append("%", draft("test.percent"))
            .expect("append percent run ID");
        sink.append(".", draft("test.dot"))
            .expect("append dot run ID");
        sink.append("..", draft("test.dot-dot"))
            .expect("append dot-dot run ID");

        assert_eq!(run_directory_name("%"), "%25");
        assert!(root.path().join("%").join(EVENTS_FILE).is_file());
        assert!(root.path().join("%00").join(EVENTS_FILE).is_file());
        assert!(root.path().join("%25").join(EVENTS_FILE).is_file());
        assert!(root.path().join("%2E").join(EVENTS_FILE).is_file());
        assert!(root.path().join("%2E%2E").join(EVENTS_FILE).is_file());
        assert_eq!(sink.last_seq("").expect("empty run sequence"), 1);
        assert_eq!(sink.last_seq("\0").expect("NUL run sequence"), 1);
        assert_eq!(sink.last_seq("%").expect("percent run sequence"), 1);
        assert_eq!(sink.last_seq(".").expect("dot run sequence"), 1);
        assert_eq!(sink.last_seq("..").expect("dot-dot run sequence"), 1);
    }

    #[cfg(unix)]
    fn running_as_root() -> bool {
        Command::new("id")
            .arg("-u")
            .output()
            .is_ok_and(|output| output.status.success() && output.stdout == b"0\n")
    }
}
