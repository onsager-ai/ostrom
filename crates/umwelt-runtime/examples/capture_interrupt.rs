//! One-shot Claude control-path capture driver.
//!
//! This program starts a real paid harness process. Build and test it freely,
//! but execute it only when a deliberate capture has been authorised.

use std::{
    env,
    error::Error,
    ffi::OsString,
    fmt::{self, Display, Formatter},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use ethogram::{
    AGENT_STARTED, AgentStartedPayload, CONTROL_APPLIED, ControlAppliedPayload, ControlKind,
    ControlRequestedPayload, Event, EventDraft, PayloadExtension, RUN_FINISHED, RunFinishedPayload,
    RunOutcome,
};
use umwelt_capture::{ChildStdoutSource, Normaliser, claude::ClaudeNormaliser};
use umwelt_runtime::{
    CapsWatchdog, Clock, ControlError, FileSink, ResumeError, ResumedSession, RunCaps, RunControl,
    SessionResumer, Sink, Source, SystemClock, process_control, run_directory_name,
};

const DEFAULT_PROMPT: &str = "Use the Bash tool exactly once to run `sleep 300`. Wait for it to finish before replying. Do not simulate the command or use another tool.";
const TOOL_CALL_TIMEOUT: Duration = Duration::from_secs(30);
const CHANNEL_POLL: Duration = Duration::from_millis(100);
const READER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

type RawLine = Result<String, umwelt_capture::CaptureFault>;
type StdoutReader = (Receiver<RawLine>, JoinHandle<()>);

fn main() {
    if let Err(error) = run() {
        eprintln!("capture failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), CaptureError> {
    let config = Config::parse(env::args_os().skip(1))?;
    fs::create_dir_all(&config.output).map_err(|error| {
        CaptureError::new(format!(
            "cannot create output directory {}: {error}",
            config.output.display()
        ))
    })?;

    let run_id = new_run_id()?;
    let run_directory = run_directory_name(&run_id);
    let final_run_path = config.output.join(&run_directory);
    if final_run_path.exists() {
        return Err(CaptureError::new(format!(
            "refusing to replace existing capture directory {}",
            final_run_path.display()
        )));
    }

    // Keep an incomplete run out of the requested output. TempDir removes it
    // on every ordinary error path; only the completed directory is renamed
    // into place.
    let staging = tempfile::Builder::new()
        .prefix(".capture-interrupt-")
        .tempdir_in(&config.output)
        .map_err(|error| CaptureError::new(format!("cannot create staging directory: {error}")))?;
    let staged_run_path = staging.path().join(&run_directory);
    fs::create_dir(&staged_run_path)
        .map_err(|error| CaptureError::new(format!("cannot create run directory: {error}")))?;
    let raw_path = staged_run_path.join("raw.ndjson");
    let raw = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&raw_path)
        .map_err(|error| CaptureError::new(format!("cannot create raw.ndjson: {error}")))?;

    let sink = FileSink::new(staging.path());
    let caps = RunCaps::default();
    let mut watchdog = CapsWatchdog::new(caps, SystemClock::default())
        .map_err(|error| CaptureError::new(format!("cannot start watchdog: {error}")))?;
    let mut normaliser = ClaudeNormaliser::new();
    let mut trigger = MidToolCallTrigger::default();
    let mut session_id = None;
    let mut raw_lines = 0_u64;

    let mut command = Command::new("claude");
    command
        .args(claude_arguments(&config.prompt))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    process_control::set_process_group(&mut command);
    let child = command.spawn().map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            CaptureError::new(
                "claude was not found on PATH; expected a runnable Claude Code CLI producing stream-json",
            )
        } else {
            CaptureError::new(format!(
                "could not start claude; expected a runnable Claude Code CLI producing stream-json: {error}"
            ))
        }
    })?;
    let mut child = ChildScope::new(child, Duration::from_millis(caps.kill_grace_ms));
    let stdout = child.take_stdout()?;
    let raw_capture = raw
        .try_clone()
        .map_err(|error| CaptureError::new(format!("cannot clone raw.ndjson: {error}")))?;
    let (receiver, reader) = stdout_reader(stdout, raw_capture)?;
    child.attach_reader(reader);
    let started_at = Instant::now();

    loop {
        match receiver.recv_timeout(CHANNEL_POLL) {
            Ok(line) => {
                let line = line.map_err(|error| {
                    CaptureError::new(format!("could not read claude stdout: {error}"))
                })?;
                raw_lines = raw_lines
                    .checked_add(1)
                    .ok_or_else(|| CaptureError::new("raw line count overflowed u64"))?;

                let drafts = normaliser.line(&line).map_err(|error| {
                    CaptureError::new(format!("Claude normalisation failed: {error}"))
                })?;
                let should_interrupt = append_and_observe(
                    drafts,
                    &run_id,
                    &sink,
                    &mut watchdog,
                    &mut trigger,
                    &mut session_id,
                )?;
                if should_interrupt {
                    break;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Some(status) = child.try_wait()? {
                    return Err(early_exit_error(status.success(), status.code(), raw_lines));
                }
                if started_at.elapsed() >= TOOL_CALL_TIMEOUT {
                    return Err(CaptureError::new(format!(
                        "no open tool call was observed within {} seconds; expected claude to start the prompt's long-running Bash call, so the child was terminated and no capture was published",
                        TOOL_CALL_TIMEOUT.as_secs()
                    )));
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                let status = child.wait()?;
                return Err(early_exit_error(status.success(), status.code(), raw_lines));
            }
        }
    }

    if let Some(status) = child.try_wait()? {
        return Err(CaptureError::new(format!(
            "claude exited with {status} after reporting a tool call but before the interrupt could land; expected the tool call to remain open"
        )));
    }
    let session_id = session_id.ok_or_else(|| {
        CaptureError::new(
            "an open tool call was observed without a preceding Claude session id; refusing an incomplete control capture",
        )
    })?;

    let mut control = RunControl::new(run_id.clone(), Some(session_id.clone()), caps, NoResume);
    let live_child = child
        .child_mut()
        .ok_or_else(|| CaptureError::new("claude child handle is unavailable"))?;
    let post_interrupt_raw_lines = finish_interrupted_capture(
        &mut control,
        live_child,
        &watchdog,
        &sink,
        &receiver,
        &mut raw_lines,
    )?;
    child.mark_reaped();
    raw.sync_all()
        .map_err(|error| CaptureError::new(format!("cannot durably write raw.ndjson: {error}")))?;

    let events = sink
        .read_from(&run_id, 0)
        .map_err(|error| CaptureError::new(format!("cannot replay completed capture: {error}")))?;
    verify_capture(&events)?;
    let event_types = events
        .iter()
        .map(|event| event.event_type.as_str())
        .collect::<Vec<_>>();
    let landed_in = interrupt_landed_in(&events)?;
    write_metadata(
        &staged_run_path.join("meta.toml"),
        events.len(),
        post_interrupt_raw_lines,
    )?;

    drop(raw);
    fs::rename(&staged_run_path, &final_run_path).map_err(|error| {
        CaptureError::new(format!(
            "capture completed but could not move it to {}: {error}",
            final_run_path.display()
        ))
    })?;
    drop(staging);

    println!("run id: {run_id}");
    println!("session id: {session_id}");
    println!("raw lines: {raw_lines}");
    println!("raw lines after interrupt: {post_interrupt_raw_lines}");
    println!("event types: {}", event_types.join(" -> "));
    println!("landedIn populated: {}", landed_in.is_some());
    if let Some(tool_use_id) = landed_in {
        println!("landedIn: {tool_use_id}");
    }
    println!("capture: {}", final_run_path.display());
    println!("queued steer: answered not-live at interrupt time; no resume was started");
    println!("late interrupt: refused synchronously as not-live; no event was emitted");
    Ok(())
}

fn claude_arguments(prompt: &str) -> Vec<OsString> {
    [
        "-p",
        "--allowedTools",
        "Bash",
        "--output-format",
        "stream-json",
        "--verbose",
        "--model",
        "haiku",
        prompt,
    ]
    .into_iter()
    .map(OsString::from)
    .collect()
}

fn stdout_reader(
    stdout: std::process::ChildStdout,
    raw_capture: File,
) -> Result<StdoutReader, CaptureError> {
    let (sender, receiver) = mpsc::channel();
    let reader = thread::Builder::new()
        .name("capture-interrupt-stdout".to_owned())
        .spawn(move || {
            for line in ChildStdoutSource::with_raw_capture(stdout, raw_capture) {
                if sender.send(line).is_err() {
                    break;
                }
            }
        })
        .map_err(|error| CaptureError::new(format!("cannot start stdout reader: {error}")))?;
    Ok((receiver, reader))
}

#[allow(clippy::too_many_arguments)]
fn append_and_observe<C: Clock>(
    drafts: Vec<EventDraft>,
    run_id: &str,
    sink: &impl Sink,
    watchdog: &mut CapsWatchdog<C>,
    trigger: &mut MidToolCallTrigger,
    session_id: &mut Option<String>,
) -> Result<bool, CaptureError> {
    let mut should_interrupt = false;
    for draft in drafts {
        let event = sink.append(run_id, draft).map_err(|error| {
            CaptureError::new(format!("cannot append normalised event: {error}"))
        })?;
        remember_session_id(&event, session_id)?;
        if watchdog
            .observe(&event)
            .map_err(|error| CaptureError::new(format!("watchdog rejected event: {error}")))?
            .is_some()
        {
            return Err(CaptureError::new(
                "an unexpected cap tripped in the uncapped capture driver",
            ));
        }
        should_interrupt |= trigger.poll(watchdog);
    }
    Ok(should_interrupt)
}

fn remember_session_id(event: &Event, session_id: &mut Option<String>) -> Result<(), CaptureError> {
    if event.event_type != AGENT_STARTED {
        return Ok(());
    }
    let started: AgentStartedPayload = serde_json::from_value(event.payload.clone())
        .map_err(|error| CaptureError::new(format!("invalid agent.started payload: {error}")))?;
    if let Some(observed) = started.session_id {
        match session_id {
            Some(existing) if existing != &observed => {
                return Err(CaptureError::new(format!(
                    "Claude session id changed from {existing:?} to {observed:?}"
                )));
            }
            Some(_) => {}
            None => *session_id = Some(observed),
        }
    }
    Ok(())
}

fn finish_interrupted_capture<C, R, S>(
    control: &mut RunControl<R>,
    child: &mut Child,
    watchdog: &CapsWatchdog<C>,
    sink: &S,
    receiver: &Receiver<RawLine>,
    raw_lines: &mut u64,
) -> Result<u64, CaptureError>
where
    C: Clock,
    R: SessionResumer,
    S: Sink,
{
    control
        .steer(
            control_request(
                "capture-steer-1",
                ControlKind::Steer,
                Some("Continue with a short summary on the next turn."),
            ),
            sink,
        )
        .map_err(|error| CaptureError::new(format!("could not queue steer: {error}")))?;
    control
        .interrupt(
            control_request("capture-interrupt-1", ControlKind::Interrupt, None),
            Some(child),
            watchdog,
            sink,
        )
        .map_err(|error| CaptureError::new(format!("could not interrupt live run: {error}")))?;

    // run.finished is now durable and every consumer stops there. Continue
    // draining for process hygiene and the verbatim raw capture, but never
    // normalise or append this tail into the closed event stream.
    let drained = drain_stdout(receiver, raw_lines)?;

    // The requesting system records this synchronous refusal in its own
    // durable place. The closed run receives no late control event.
    let late_error = match control.interrupt(
        control_request("capture-interrupt-2", ControlKind::Interrupt, None),
        None,
        watchdog,
        sink,
    ) {
        Ok(()) => {
            return Err(CaptureError::new(
                "late interrupt was accepted after run.finished; expected a synchronous not-live refusal",
            ));
        }
        Err(error) => error,
    };
    if !matches!(late_error, ControlError::NotLive) {
        return Err(CaptureError::new(format!(
            "late interrupt returned {late_error}; expected a synchronous not-live refusal"
        )));
    }

    Ok(drained)
}

fn drain_stdout(receiver: &Receiver<RawLine>, raw_lines: &mut u64) -> Result<u64, CaptureError> {
    let deadline = Instant::now() + READER_SHUTDOWN_TIMEOUT;
    let mut drained = 0_u64;
    loop {
        match receiver.recv_timeout(CHANNEL_POLL) {
            Ok(line) => {
                line.map_err(|error| {
                    CaptureError::new(format!("could not finish reading claude stdout: {error}"))
                })?;
                *raw_lines = raw_lines
                    .checked_add(1)
                    .ok_or_else(|| CaptureError::new("raw line count overflowed u64"))?;
                drained = drained
                    .checked_add(1)
                    .ok_or_else(|| CaptureError::new("post-interrupt line count overflowed u64"))?;
            }
            Err(RecvTimeoutError::Disconnected) => return Ok(drained),
            Err(RecvTimeoutError::Timeout) if Instant::now() >= deadline => {
                return Err(CaptureError::new(
                    "claude stdout did not close after process-group termination",
                ));
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

fn write_metadata(
    path: &Path,
    events: usize,
    post_interrupt_raw_lines: u64,
) -> Result<(), CaptureError> {
    let metadata = capture_metadata(events, post_interrupt_raw_lines);
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(|error| CaptureError::new(format!("cannot create meta.toml: {error}")))?;
    file.write_all(metadata.as_bytes())
        .and_then(|()| file.flush())
        .and_then(|()| file.sync_all())
        .map_err(|error| CaptureError::new(format!("cannot durably write meta.toml: {error}")))
}

fn capture_metadata(events: usize, post_interrupt_raw_lines: u64) -> String {
    let captured_at = chrono::Utc::now().format("%Y-%m-%d");
    format!(
        r#"harness = "claude-code"
cli_version = "reported by system/init in raw.ndjson"
captured_at = "{captured_at}"
model = "haiku"
command = "claude -p --allowedTools Bash --output-format stream-json --verbose --model haiku <prompt>"
prompt_source = "command-line argument, or the driver's default"
events = {events}
raw_lines_after_interrupt = {post_interrupt_raw_lines}

exercises = [
  "interrupt while a tool call is open, with control.applied.landedIn",
  "a queued steer answered not-live before run.finished",
  "a late interrupt refused synchronously after run.finished",
]

[not_exercised]
post_interrupt_events = "Raw lines after the interrupt deliberately produce no events because the run's terminal has already been emitted. raw.ndjson remains the complete harness record."
resumed_run = "No resume is started; the queued steer is answered not-live by the interrupt."
"#
    )
}

fn control_request(
    control_id: &str,
    kind: ControlKind,
    text: Option<&str>,
) -> ControlRequestedPayload {
    ControlRequestedPayload {
        control_id: control_id.to_owned(),
        kind,
        text: text.map(str::to_owned),
        truncated: text.map(|_| false),
        by: "capture-driver".to_owned(),
        extra: PayloadExtension::new(),
    }
}

fn verify_capture(events: &[Event]) -> Result<(), CaptureError> {
    let terminal_indices = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| (event.event_type == RUN_FINISHED).then_some(index))
        .collect::<Vec<_>>();
    if terminal_indices.len() != 1 {
        return Err(CaptureError::new(format!(
            "capture contains {} terminal events; expected exactly one",
            terminal_indices.len()
        )));
    }
    let terminal_index = terminal_indices[0];
    let terminal: RunFinishedPayload =
        serde_json::from_value(events[terminal_index].payload.clone())
            .map_err(|error| CaptureError::new(format!("invalid run.finished payload: {error}")))?;
    if terminal.outcome != RunOutcome::Interrupted {
        return Err(CaptureError::new(format!(
            "terminal outcome was {:?}; expected interrupted",
            terminal.outcome
        )));
    }

    let live = control_applied(events, "capture-interrupt-1")?;
    if !live.ok || live.landed_in.is_none() {
        return Err(CaptureError::new(
            "the live interrupt did not populate landedIn; refusing a capture without a proven mid-tool-call landing",
        ));
    }
    let steer = control_applied(events, "capture-steer-1")?;
    if steer.ok || steer.reason.as_deref() != Some("not-live") {
        return Err(CaptureError::new(
            "the queued steer did not receive not-live at interrupt time",
        ));
    }

    let order = [
        control_requested_index(events, "capture-steer-1")?,
        control_requested_index(events, "capture-interrupt-1")?,
        control_applied_index(events, "capture-interrupt-1")?,
        control_applied_index(events, "capture-steer-1")?,
        terminal_index,
    ];
    if !order.windows(2).all(|pair| pair[0] < pair[1]) {
        return Err(CaptureError::new(format!(
            "control events were not emitted in the required order: {order:?}"
        )));
    }
    if terminal_index + 1 != events.len() {
        return Err(CaptureError::new(
            "run.finished was not the final event in the capture",
        ));
    }
    Ok(())
}

fn interrupt_landed_in(events: &[Event]) -> Result<Option<String>, CaptureError> {
    Ok(control_applied(events, "capture-interrupt-1")?.landed_in)
}

fn control_applied(
    events: &[Event],
    control_id: &str,
) -> Result<ControlAppliedPayload, CaptureError> {
    events
        .iter()
        .filter(|event| event.event_type == CONTROL_APPLIED)
        .find_map(|event| {
            serde_json::from_value::<ControlAppliedPayload>(event.payload.clone())
                .ok()
                .filter(|payload| payload.control_id == control_id)
        })
        .ok_or_else(|| {
            CaptureError::new(format!(
                "capture is missing control.applied for {control_id}"
            ))
        })
}

fn control_requested_index(events: &[Event], control_id: &str) -> Result<usize, CaptureError> {
    events
        .iter()
        .position(|event| {
            event.event_type == ethogram::CONTROL_REQUESTED
                && serde_json::from_value::<ControlRequestedPayload>(event.payload.clone())
                    .is_ok_and(|payload| payload.control_id == control_id)
        })
        .ok_or_else(|| {
            CaptureError::new(format!(
                "capture is missing control.requested for {control_id}"
            ))
        })
}

fn control_applied_index(events: &[Event], control_id: &str) -> Result<usize, CaptureError> {
    events
        .iter()
        .position(|event| {
            event.event_type == CONTROL_APPLIED
                && serde_json::from_value::<ControlAppliedPayload>(event.payload.clone())
                    .is_ok_and(|payload| payload.control_id == control_id)
        })
        .ok_or_else(|| {
            CaptureError::new(format!(
                "capture is missing control.applied for {control_id}"
            ))
        })
}

fn early_exit_error(success: bool, code: Option<i32>, raw_lines: u64) -> CaptureError {
    let status = code.map_or_else(
        || "without an exit code".to_owned(),
        |code| code.to_string(),
    );
    let disposition = if success { "successfully" } else { "non-zero" };
    CaptureError::new(format!(
        "claude exited {disposition} ({status}) after {raw_lines} raw lines but before any open tool call; expected the prompt to start a long-running Bash call, so no capture was published"
    ))
}

fn new_run_id() -> Result<String, CaptureError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| CaptureError::new(format!("system clock is before Unix epoch: {error}")))?
        .as_millis();
    Ok(format!(
        "capture-control-interrupt-{}-{millis}",
        std::process::id()
    ))
}

#[derive(Default)]
struct MidToolCallTrigger {
    fired: bool,
}

impl MidToolCallTrigger {
    fn poll<C: Clock>(&mut self, watchdog: &CapsWatchdog<C>) -> bool {
        let fires = !self.fired && watchdog.most_recent_open_tool_call().is_some();
        self.fired |= fires;
        fires
    }
}

struct ChildScope {
    child: Option<Child>,
    reader: Option<JoinHandle<()>>,
    grace: Duration,
}

impl ChildScope {
    fn new(child: Child, grace: Duration) -> Self {
        Self {
            child: Some(child),
            reader: None,
            grace,
        }
    }

    fn take_stdout(&mut self) -> Result<std::process::ChildStdout, CaptureError> {
        self.child
            .as_mut()
            .ok_or_else(|| CaptureError::new("claude child handle is unavailable"))?
            .stdout
            .take()
            .ok_or_else(|| CaptureError::new("claude stdout was not piped"))
    }

    fn attach_reader(&mut self, reader: JoinHandle<()>) {
        self.reader = Some(reader);
    }

    fn child_mut(&mut self) -> Option<&mut Child> {
        self.child.as_mut()
    }

    fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>, CaptureError> {
        self.child_mut()
            .ok_or_else(|| CaptureError::new("claude child handle is unavailable"))?
            .try_wait()
            .map_err(|error| CaptureError::new(format!("cannot inspect claude process: {error}")))
    }

    fn wait(&mut self) -> Result<std::process::ExitStatus, CaptureError> {
        self.child_mut()
            .ok_or_else(|| CaptureError::new("claude child handle is unavailable"))?
            .wait()
            .map_err(|error| CaptureError::new(format!("cannot wait for claude process: {error}")))
    }

    fn mark_reaped(&mut self) {
        self.child.take();
    }
}

impl Drop for ChildScope {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            process_control::terminate_child_process_group(&mut child, self.grace);
            let _ = child.wait();
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[derive(Debug)]
struct Config {
    output: PathBuf,
    prompt: String,
}

impl Config {
    fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Self, CaptureError> {
        let mut arguments = arguments.into_iter();
        let output = arguments.next().ok_or_else(|| {
            CaptureError::new("usage: capture_interrupt <output-directory> [prompt]")
        })?;
        let prompt = arguments
            .next()
            .map(|prompt| {
                prompt
                    .into_string()
                    .map_err(|_| CaptureError::new("prompt must be valid UTF-8"))
            })
            .transpose()?
            .unwrap_or_else(|| DEFAULT_PROMPT.to_owned());
        if arguments.next().is_some() {
            return Err(CaptureError::new(
                "usage: capture_interrupt <output-directory> [prompt]",
            ));
        }
        Ok(Self {
            output: PathBuf::from(output),
            prompt,
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct NoResume;

impl SessionResumer for NoResume {
    fn resume(&self, _session_id: &str, _text: &str) -> Result<ResumedSession, ResumeError> {
        Err(ResumeError::Unsupported)
    }
}

#[derive(Debug)]
struct CaptureError(String);

impl CaptureError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl Display for CaptureError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for CaptureError {}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        sync::{Arc, Mutex},
    };

    use ethogram::{
        AGENT_TEXT, AGENT_TOOL_RESULT, AGENT_TOOL_USE, AgentTextPayload, AgentToolResultPayload,
        AgentToolUsePayload, StampFields, stamp,
    };

    use super::*;

    #[derive(Clone, Default)]
    struct ManualClock(Arc<Cell<Duration>>);

    impl Clock for ManualClock {
        fn now(&self) -> Duration {
            self.0.get()
        }
    }

    fn draft(event_type: &str, payload: impl serde::Serialize) -> EventDraft {
        EventDraft {
            event_type: event_type.to_owned(),
            payload: serde_json::to_value(payload).expect("serialise fixture payload"),
            captured_at: None,
        }
    }

    fn event(event_type: &str, payload: impl serde::Serialize) -> Event {
        stamp(
            draft(event_type, payload),
            StampFields {
                run_id: "run-1".to_owned(),
                seq: 1,
                ts: "2030-01-02T03:04:05.000Z".to_owned(),
            },
        )
    }

    fn text_event() -> Event {
        event(
            AGENT_TEXT,
            AgentTextPayload {
                stage: None,
                text: "working".to_owned(),
                truncated: Some(false),
                parent_tool_use_id: None,
                extra: PayloadExtension::new(),
            },
        )
    }

    fn tool_use(tool_use_id: &str) -> Event {
        event(
            AGENT_TOOL_USE,
            AgentToolUsePayload {
                stage: None,
                tool: "Bash".to_owned(),
                input_excerpt: Some("sleep 300".to_owned()),
                truncated: Some(false),
                tool_use_id: Some(tool_use_id.to_owned()),
                parent_tool_use_id: None,
                extra: PayloadExtension::new(),
            },
        )
    }

    fn tool_result(tool_use_id: &str) -> Event {
        event(
            AGENT_TOOL_RESULT,
            AgentToolResultPayload {
                stage: None,
                tool: "Bash".to_owned(),
                is_error: Some(false),
                result_excerpt: Some(String::new()),
                truncated: Some(false),
                tool_use_id: Some(tool_use_id.to_owned()),
                parent_tool_use_id: None,
                extra: PayloadExtension::new(),
            },
        )
    }

    #[test]
    fn argument_builder_uses_one_fresh_haiku_stream_session() {
        let arguments = claude_arguments("fixture prompt");
        let arguments = arguments
            .iter()
            .map(|argument| argument.to_str().expect("UTF-8 argument"))
            .collect::<Vec<_>>();

        assert_eq!(
            arguments,
            [
                "-p",
                "--allowedTools",
                "Bash",
                "--output-format",
                "stream-json",
                "--verbose",
                "--model",
                "haiku",
                "fixture prompt",
            ]
        );
        assert!(!arguments.contains(&"--resume"));
        assert!(!arguments.contains(&"--fork-session"));
    }

    #[test]
    fn trigger_fires_only_while_the_watchdog_has_an_open_call() {
        let mut watchdog =
            CapsWatchdog::new(RunCaps::default(), ManualClock::default()).expect("watchdog");
        let mut trigger = MidToolCallTrigger::default();

        watchdog.observe(&text_event()).expect("observe text");
        assert!(!trigger.poll(&watchdog));
        watchdog
            .observe(&tool_result("unopened"))
            .expect("observe unmatched result");
        assert!(!trigger.poll(&watchdog));
        watchdog
            .observe(&tool_use("tool-1"))
            .expect("observe tool use");
        assert!(trigger.poll(&watchdog));
        assert!(!trigger.poll(&watchdog));
    }

    #[test]
    #[cfg(unix)]
    fn full_interrupt_sequence_discards_event_producing_raw_tail_after_terminal() {
        let sink = TrackingSink::default();
        let mut watchdog =
            CapsWatchdog::new(RunCaps::default(), ManualClock::default()).expect("watchdog");
        let mut trigger = MidToolCallTrigger::default();
        let mut session_id = None;
        let should_interrupt = append_and_observe(
            vec![
                draft(
                    AGENT_STARTED,
                    AgentStartedPayload {
                        stage: None,
                        model: Some("claude-haiku-fixture".to_owned()),
                        session_id: Some("session-1".to_owned()),
                        pid: None,
                        extra: PayloadExtension::new(),
                    },
                ),
                draft(
                    AGENT_TOOL_USE,
                    AgentToolUsePayload {
                        stage: None,
                        tool: "Bash".to_owned(),
                        input_excerpt: Some("sleep 30".to_owned()),
                        truncated: Some(false),
                        tool_use_id: Some("tool-1".to_owned()),
                        parent_tool_use_id: None,
                        extra: PayloadExtension::new(),
                    },
                ),
            ],
            "run-1",
            &sink,
            &mut watchdog,
            &mut trigger,
            &mut session_id,
        )
        .expect("observe synthetic pre-interrupt events");
        assert!(should_interrupt);

        let raw_tail =
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"late output"}]}}"#;
        assert!(
            !ClaudeNormaliser::new()
                .line(raw_tail)
                .expect("tail is an event-producing Claude frame")
                .is_empty()
        );
        let (sender, receiver) = mpsc::channel();
        sender
            .send(Ok(raw_tail.to_owned()))
            .expect("queue synthetic raw tail");
        drop(sender);

        let mut command = Command::new("sh");
        command
            .args(["-c", "sleep 30"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        process_control::set_process_group(&mut command);
        let mut child = command.spawn().expect("spawn synthetic live process");
        let mut control = RunControl::new("run-1", session_id, RunCaps::default(), NoResume);
        let mut raw_lines = 2;

        let drained = finish_interrupted_capture(
            &mut control,
            &mut child,
            &watchdog,
            &sink,
            &receiver,
            &mut raw_lines,
        )
        .expect("complete synthetic capture");

        assert_eq!(drained, 1);
        assert_eq!(raw_lines, 3);
        assert_eq!(sink.post_terminal_attempts(), 0);
        let events = sink.events();
        assert_eq!(
            events
                .iter()
                .map(|event| event.event_type.as_str())
                .collect::<Vec<_>>(),
            [
                AGENT_STARTED,
                AGENT_TOOL_USE,
                ethogram::CONTROL_REQUESTED,
                ethogram::CONTROL_REQUESTED,
                CONTROL_APPLIED,
                CONTROL_APPLIED,
                RUN_FINISHED,
            ]
        );
        let steer_applied: ControlAppliedPayload =
            serde_json::from_value(events[5].payload.clone()).expect("steer applied payload");
        assert!(!steer_applied.ok);
        assert_eq!(steer_applied.reason.as_deref(), Some("not-live"));

        let metadata: toml::Value =
            toml::from_str(&capture_metadata(events.len(), drained)).expect("valid meta.toml");
        assert_eq!(metadata["raw_lines_after_interrupt"].as_integer(), Some(1));
        assert!(
            metadata["not_exercised"]["post_interrupt_events"]
                .as_str()
                .is_some_and(|note| note.contains("deliberately produce no events"))
        );
    }

    #[derive(Default)]
    struct TrackingSink(Mutex<TrackingState>);

    #[derive(Default)]
    struct TrackingState {
        events: Vec<Event>,
        finished: bool,
        post_terminal_attempts: usize,
    }

    impl TrackingSink {
        fn events(&self) -> Vec<Event> {
            self.0.lock().expect("tracking sink lock").events.clone()
        }

        fn post_terminal_attempts(&self) -> usize {
            self.0
                .lock()
                .expect("tracking sink lock")
                .post_terminal_attempts
        }
    }

    impl Sink for TrackingSink {
        fn append(&self, run: &str, draft: EventDraft) -> Result<Event, umwelt_runtime::SinkFault> {
            let mut state = self.0.lock().expect("tracking sink lock");
            if state.finished {
                state.post_terminal_attempts += 1;
                return Err(umwelt_runtime::SinkFault::Finished);
            }
            let event = stamp(
                draft,
                StampFields {
                    run_id: run.to_owned(),
                    seq: u64::try_from(state.events.len()).expect("fixture sequence") + 1,
                    ts: "2030-01-02T03:04:05.000Z".to_owned(),
                },
            );
            state.finished = event.event_type == RUN_FINISHED;
            state.events.push(event.clone());
            Ok(event)
        }

        fn forward(&self, event: Event) -> Result<(), umwelt_runtime::SinkFault> {
            let mut state = self.0.lock().expect("tracking sink lock");
            if state.finished {
                state.post_terminal_attempts += 1;
                return Err(umwelt_runtime::SinkFault::Finished);
            }
            state.finished = event.event_type == RUN_FINISHED;
            state.events.push(event);
            Ok(())
        }

        fn last_seq(&self, _run: &str) -> Result<u64, umwelt_runtime::SinkFault> {
            Ok(
                u64::try_from(self.0.lock().expect("tracking sink lock").events.len())
                    .expect("fixture sequence"),
            )
        }
    }
}
