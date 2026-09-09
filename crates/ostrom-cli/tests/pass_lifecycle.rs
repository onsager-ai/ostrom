#![cfg(unix)]

use std::{
    env, fs,
    io::Write,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

mod support;
use tempfile::TempDir;

const CLAUDE_STREAM_JSON: &str = concat!(
    "{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"session-fixture\",\"model\":\"claude-fixture\"}\n",
    "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"tool-fixture\",\"name\":\"Bash\",\"input\":{\"command\":\"true\"}}]}}\n",
    "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"tool-fixture\",\"content\":\"ok\",\"is_error\":false}]}}\n",
    "{\"type\":\"result\",\"subtype\":\"success\",\"session_id\":\"session-fixture\",\"duration_ms\":125,\"num_turns\":1,\"total_cost_usd\":1.25,\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"cache_read_input_tokens\":2,\"cache_creation_input_tokens\":3}}\n",
);

fn stream_script(stream: &str) -> String {
    assert!(!stream.contains('\''));
    format!("printf '%s' '{stream}'")
}

fn stream_script_with_work(stream: &str) -> String {
    let started = json!({
        "ts": "2026-08-01T00:00:00Z",
        "kind": "pass-started",
        "fact": {"owner": "builder-inner-wake1"},
        "narration": {},
    });
    format!(
        "printf '%s\\n' '{started}' >>\"$OSTROM_HOME/sprint.jsonl\"\n{}",
        stream_script(stream)
    )
}

struct Fixture {
    root: TempDir,
    state: PathBuf,
    claude: PathBuf,
}

impl Fixture {
    fn new(script: &str) -> Self {
        let root = tempfile::tempdir().expect("temporary pass fixture");
        let state = root.path().join("ostrom");
        fs::create_dir_all(state.join("roles")).expect("create role settings");
        fs::write(state.join("roles/builder.settings.json"), "{}\n").expect("write settings");
        fs::write(state.join("loop-armed"), "").expect("arm pass");
        fs::write(state.join("builder-pass-id"), "a1b2c3d4\n").expect("write id");
        fs::write(state.join("builder-wake-counter"), "6\n").expect("write wake");
        let claude = root.path().join("claude-stub");
        fs::write(&claude, format!("#!/usr/bin/env bash\n{script}\n")).expect("write stub");
        fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).expect("chmod stub");
        Self {
            root,
            state,
            claude,
        }
    }

    fn command(&self) -> Command {
        self.command_for("builder")
    }

    fn command_for(&self, role: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ostrom"));
        command
            .args(["pass", role])
            .env_clear()
            .env("OSTROM_HOME", &self.state)
            .env("CLAUDE_CONFIG_DIR", self.root.path())
            .env("HOME", self.root.path())
            .env("PATH", env::var_os("PATH").unwrap_or_default())
            .env("CLAUDE_BIN", &self.claude);
        command
    }

    fn events_command(&self, run: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ostrom"));
        command
            .args(["events", run])
            .env_clear()
            .env("OSTROM_HOME", &self.state)
            .env("HOME", self.root.path());
        command
    }

    fn write_blocked_dispatchability_state(&self) {
        fs::write(
            self.state.join("mandates.yaml"),
            r#"provider: file
cadence_hours: 1
stuck_after_days: 7
bounce_all: []
projects:
  - repo: placeholder-org/alpha
    delegated: []
    excluded: []
    reserved: []
    default: delegated
    paused: false
    bounce: []
"#,
        )
        .expect("write mandate fixture");
        fs::write(
            self.state.join("queue.jsonl"),
            concat!(
                "{\"id\":\"placeholder-org/alpha#1\",\"repo\":\"placeholder-org/alpha\",",
                "\"ref\":\"#1\",\"title\":\"Placeholder decision\",\"kind\":\"decision\",",
                "\"mandate\":{\"reason\":\"placeholder\"},\"state\":\"pending\",",
                "\"opened\":\"2026-01-01T00:00:00Z\",\"needs_judgment\":true,\"blocked_by\":[]}\n"
            ),
        )
        .expect("write blocked queue");
        let state = json!({
            "version": 2,
            "work_ranking": [],
            "work_ranking_faults": [],
            "repos": {
                "placeholder-org/alpha": {"ci_drift": {}}
            },
            "dependency_graph": {
                "graph_version": 1,
                "configured_repositories": ["placeholder-org/alpha"],
                "nodes": [{
                    "id": "placeholder-org/alpha#1",
                    "open": true,
                    "dependencies": [],
                    "unsatisfied": [],
                    "children": [],
                    "dispatchable": true,
                    "unblocking_power": 0
                }],
                "edges": [],
                "faults": []
            }
        });
        fs::write(
            self.state.join("state.json"),
            serde_json::to_vec(&state).expect("serialize sweep state"),
        )
        .expect("write sweep state");
    }

    fn approve_blocked_decision(&self) {
        let queue = fs::read_to_string(self.state.join("queue.jsonl"))
            .expect("read blocked queue")
            .replace("\"state\":\"pending\"", "\"state\":\"approved\"");
        fs::write(self.state.join("queue.jsonl"), queue).expect("approve decision");
    }

    fn trace(&self) -> Vec<Value> {
        fs::read_to_string(self.state.join("sprint.jsonl"))
            .expect("read trace")
            .lines()
            .map(|line| serde_json::from_str(line).expect("trace JSON"))
            .collect()
    }

    fn run_events(&self) -> Vec<Value> {
        String::from_utf8(self.run_event_bytes())
            .expect("event stream UTF-8")
            .lines()
            .map(|line| serde_json::from_str(line).expect("event JSON"))
            .collect()
    }

    fn run_event_bytes(&self) -> Vec<u8> {
        let run_directories = fs::read_dir(self.state.join("runs"))
            .expect("read run directories")
            .collect::<Result<Vec<_>, _>>()
            .expect("read run directory entries");
        assert_eq!(run_directories.len(), 1, "expected exactly one pass run");
        fs::read(run_directories[0].path().join("events.jsonl")).expect("read pass events")
    }

    fn transcript_bytes(&self) -> Vec<u8> {
        let transcripts = fs::read_dir(self.state.join("pass-runs/builder"))
            .expect("read transcript directory")
            .collect::<Result<Vec<_>, _>>()
            .expect("read transcript entries");
        assert_eq!(transcripts.len(), 1, "expected exactly one transcript");
        fs::read(transcripts[0].path()).expect("read transcript")
    }

    fn assert_released(&self) {
        assert!(!self.state.join("builder-pass.lease").exists());
        let trace = self.trace();
        assert_eq!(
            trace.last().and_then(|row| row["kind"].as_str()),
            Some("pass-ended")
        );
    }
}

#[test]
fn events_fd_bytes_match_the_durable_stream_and_the_flag_wins() {
    let fixture = Fixture::new("exit 0");
    let output = fixture
        .command()
        .args(["--events-fd", "1"])
        .env("OSTROM_EVENTS_FD", "not-a-descriptor")
        .output()
        .expect("run pass with an event descriptor");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, fixture.run_event_bytes());
}

#[test]
fn events_fd_environment_streams_the_durable_bytes() {
    let fixture = Fixture::new("exit 0");
    let output = fixture
        .command()
        .env("OSTROM_EVENTS_FD", "1")
        .output()
        .expect("run pass with the event descriptor environment variable");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, fixture.run_event_bytes());
}

fn control_request(kind: &str, by: &str) -> Value {
    let mut payload = json!({"controlId": "supervisor-control", "kind": kind, "by": by});
    if kind == "steer" {
        payload["text"] = "Please use the next turn for this instruction".into();
    }
    json!({"type": "control.requested", "payload": payload})
}

fn wait_for_run_event(fixture: &Fixture, event_type: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(directories) = fs::read_dir(fixture.state.join("runs")) {
            for directory in directories.flatten() {
                if let Ok(bytes) = fs::read_to_string(directory.path().join("events.jsonl")) {
                    for event in bytes
                        .lines()
                        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                    {
                        if event["type"] == event_type {
                            return event;
                        }
                    }
                }
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {event_type}");
}

fn finish_control_pass(child: Child) -> std::process::Output {
    // A timeout fails the test and cleans up the supervised process group.
    let mut child = child;
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().expect("probe pass").is_none() {
        if Instant::now() >= deadline {
            signal(child.id(), "TERM");
            let _ = child.wait();
            panic!("control pass did not finish");
        }
        thread::sleep(Duration::from_millis(25));
    }
    child.wait_with_output().expect("collect control pass")
}

fn assert_one_terminal(events: &[Value], outcome: &str) {
    let finished = events
        .iter()
        .filter(|event| event["type"] == "run.finished")
        .collect::<Vec<_>>();
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["payload"]["outcome"], outcome);
    assert_eq!(events.last().expect("last event")["type"], "run.finished");
}

#[test]
fn control_fd_interrupt_lands_in_the_open_call_and_the_flag_wins() {
    let stream = CLAUDE_STREAM_JSON
        .lines()
        .take(2)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let fixture = Fixture::new(&format!("{}\nexec sleep 30", stream_script(&stream)));
    let mut child = fixture
        .command()
        .args(["--control-fd", "0", "--events-fd", "1"])
        .env("OSTROM_CONTROL_FD", "not-a-descriptor")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start controlled pass");
    let mut input = child.stdin.take().expect("control pipe");
    wait_for_run_event(&fixture, "agent.tool_use");
    let by = "unfamiliar supervisor / 任意 identity";
    writeln!(input, "{}", control_request("interrupt", by)).expect("send interrupt");
    let output = finish_control_pass(child);
    assert_eq!(output.status.code(), Some(130));
    assert_eq!(output.stdout, fixture.run_event_bytes());
    let events = fixture.run_events();
    assert_eq!(
        events
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "run.started",
            "agent.started",
            "agent.tool_use",
            "control.requested",
            "control.applied",
            "run.finished"
        ]
    );
    assert_eq!(events[3]["payload"]["by"], by);
    assert_eq!(events[4]["payload"]["by"], by);
    assert_eq!(events[4]["payload"]["controlId"], "supervisor-control");
    assert_eq!(events[4]["payload"]["ok"], true);
    assert_eq!(events[4]["payload"]["landedIn"], "tool-fixture");
    assert_eq!(events[5]["payload"]["by"], by);
    assert_one_terminal(&events, "interrupted");
    fixture.assert_released();
    assert_eq!(
        fixture.trace().last().unwrap()["fact"]["outcome"],
        "interrupted"
    );
}

#[test]
fn a_control_descriptor_above_stderr_is_inherited_through_the_supervisor() {
    let fixture = Fixture::new("exec sleep 30");
    let pass = fixture.command();
    let mut child = Command::new("bash")
        .args(["-c", "exec 7<&0; exec \"$@\" --control-fd 7", "supervisor"])
        .arg(pass.get_program())
        .args(pass.get_args())
        .env_clear()
        .envs(
            pass.get_envs()
                .filter_map(|(key, value)| value.map(|value| (key, value))),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("inherit descriptor seven");
    writeln!(
        child.stdin.as_mut().unwrap(),
        "{}",
        control_request("interrupt", "fd-seven")
    )
    .expect("write inherited descriptor");
    let output = finish_control_pass(child);
    assert_eq!(output.status.code(), Some(130));
    let events = fixture.run_events();
    assert_eq!(events[1]["payload"]["by"], "fd-seven");
    assert_one_terminal(&events, "interrupted");
}

#[test]
fn an_invalid_control_fd_environment_value_is_a_configuration_error() {
    let fixture = Fixture::new("exit 0");
    let output = fixture
        .command()
        .env("OSTROM_CONTROL_FD", "not-a-descriptor")
        .output()
        .expect("refuse invalid descriptor configuration");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("OSTROM_CONTROL_FD must be an unsigned integer")
    );
    assert!(!fixture.state.join("runs").exists());
}

#[test]
fn control_fd_environment_refuses_steer_immediately_and_preserves_any_by() {
    for by in ["unknown:supervisor", "", "a different principal"] {
        let fixture = Fixture::new(&format!(
            "for i in {{1..200}}; do test -f \"$OSTROM_HOME/continue\" && break; sleep 0.05; done\n{}",
            stream_script_with_work(CLAUDE_STREAM_JSON)
        ));
        let mut child = fixture
            .command()
            .env("OSTROM_CONTROL_FD", "0")
            .args(["--events-fd", "1"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start controlled pass from environment");
        let mut input = child.stdin.take().expect("control pipe");
        writeln!(input, "{}", control_request("steer", by)).expect("send steer");
        let applied = wait_for_run_event(&fixture, "control.applied");
        assert_eq!(applied["payload"]["ok"], false);
        assert_eq!(applied["payload"]["reason"], "unsupported");
        assert_eq!(applied["payload"]["by"], by);
        assert!(
            child.try_wait().unwrap().is_none(),
            "refusal must precede normal exit"
        );
        fs::write(fixture.state.join("continue"), "go").expect("release normal exit");
        // Keep the writer open: the pass must not wait for control EOF to finish.
        let output = finish_control_pass(child);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, fixture.run_event_bytes());
        assert_eq!(fixture.transcript_bytes(), CLAUDE_STREAM_JSON.as_bytes());
        let events = fixture.run_events();
        assert_eq!(events[1]["type"], "control.requested");
        assert_eq!(
            events[1]["payload"],
            control_request("steer", by)["payload"]
        );
        assert_one_terminal(&events, "completed");
    }
}

#[test]
fn malformed_control_lines_are_refused_and_the_reader_continues() {
    let fixture = Fixture::new(&format!(
        "for i in {{1..200}}; do test -f \"$OSTROM_HOME/continue\" && break; sleep 0.05; done\n{}",
        stream_script_with_work(CLAUDE_STREAM_JSON)
    ));
    let mut child = fixture
        .command()
        .args(["--control-fd", "0", "--events-fd", "1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start pass for malformed controls");
    let mut input = child.stdin.take().expect("control pipe");
    let malformed = [
        b"not json\n".to_vec(),
        b"\xff\n".to_vec(),
        b"\n".to_vec(),
        b"{\"type\":\"control.requested\",\"payload\":{}}\n".to_vec(),
        b"{\"type\":\"agent.text\",\"payload\":{\"text\":\"wrong type\"}}\n".to_vec(),
        format!("{}\n", control_request("teleport", "arbitrary")).into_bytes(),
        b"{\"type\":\"control.requested\",\"payload\":{\"controlId\":\"empty\",\"kind\":\"steer\",\"by\":\"any\"}}\n".to_vec(),
        format!("{}\n", json!({"type":"control.requested", "payload":{
            "controlId":"oversized", "kind":"steer", "by":"any", "text":"x".repeat(ethogram::MAX_EXCERPT_SCALARS + 1)
        }})).into_bytes(),
        format!("{}\n", json!({"v":1,"runId":"not-an-inbound-envelope","seq":1,"ts":"2026-08-01T00:00:00Z",
            "type":"control.requested","payload":control_request("interrupt", "any")["payload"]})).into_bytes(),
    ];
    for line in &malformed {
        input.write_all(line).expect("send malformed line");
    }
    writeln!(input, "{}", control_request("steer", "still reading")).expect("send valid line");
    wait_for_run_event(&fixture, "control.applied");
    fs::write(fixture.state.join("continue"), "go").unwrap();
    let output = finish_control_pass(child);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, fixture.run_event_bytes());
    let events = fixture.run_events();
    let refusals = events
        .iter()
        .filter(|event| event["type"] == "capture.refused")
        .collect::<Vec<_>>();
    assert_eq!(refusals.len(), malformed.len());
    for refusal in refusals {
        assert_eq!(refusal["payload"]["cause"], "malformed");
        assert_eq!(refusal["payload"]["sourceType"], "control.requested");
        assert!(!refusal["payload"]["detail"].as_str().unwrap().is_empty());
    }
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "control.requested")
            .count(),
        1
    );
    assert_eq!(fixture.transcript_bytes(), CLAUDE_STREAM_JSON.as_bytes());
    assert_one_terminal(&events, "completed");
}

#[test]
fn closed_and_unreadable_control_descriptors_do_not_fail_the_pass() {
    for unreadable in [false, true] {
        let fixture = Fixture::new(&stream_script_with_work(CLAUDE_STREAM_JSON));
        let mut command = fixture.command();
        if unreadable {
            // A directory can be opened through the inherited fd, but reading it fails.
            command
                .args(["--control-fd", "0"])
                .stdin(fs::File::open(fixture.root.path()).expect("directory descriptor"));
        } else {
            command.args(["--control-fd", "4294967295"]);
        }
        let output = command
            .args(["--events-fd", "1"])
            .output()
            .expect("run with broken input");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, fixture.run_event_bytes());
        let events = fixture.run_events();
        let refusals = events
            .iter()
            .filter(|event| event["type"] == "capture.refused")
            .collect::<Vec<_>>();
        assert_eq!(refusals.len(), 1);
        let detail = refusals[0]["payload"]["detail"].as_str().unwrap();
        assert!(detail.contains(if unreadable {
            "could not read control descriptor"
        } else {
            "could not open control fd"
        }));
        assert_one_terminal(&events, "completed");
        fixture.assert_released();
    }
}

#[test]
fn control_eof_and_an_idle_writer_do_not_delay_normal_exit() {
    for close_writer in [false, true] {
        let fixture = Fixture::new(&stream_script_with_work(CLAUDE_STREAM_JSON));
        let mut child = fixture
            .command()
            .args(["--control-fd", "0"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start idle control pass");
        let mut input = child.stdin.take();
        if close_writer {
            drop(input.take());
        }
        let output = finish_control_pass(child);
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
        let events = fixture.run_events();
        assert!(!events.iter().any(
            |event| event["type"].as_str().unwrap().starts_with("control.")
                || event["type"] == "capture.refused"
        ));
        assert_one_terminal(&events, "completed");
    }
}

#[test]
fn controls_after_interrupt_terminal_emit_nothing() {
    let fixture = Fixture::new("exec sleep 30");
    let mut child = fixture
        .command()
        .args(["--control-fd", "0", "--events-fd", "1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start interrupt race");
    let mut input = child.stdin.take().unwrap();
    writeln!(
        input,
        "{}\n{}\n{}\nmalformed",
        control_request("interrupt", "first"),
        control_request("interrupt", "second"),
        control_request("steer", "third")
    )
    .unwrap();
    let output = finish_control_pass(child);
    assert_eq!(output.status.code(), Some(130));
    assert_eq!(output.stdout, fixture.run_event_bytes());
    let events = fixture.run_events();
    assert_eq!(
        events
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "run.started",
            "control.requested",
            "control.applied",
            "run.finished"
        ]
    );
    assert_eq!(events[1]["payload"]["by"], "first");
    assert_one_terminal(&events, "interrupted");
}

#[test]
fn without_a_control_descriptor_stdin_is_ignored_and_existing_bytes_are_preserved() {
    let fixture = Fixture::new(&stream_script(CLAUDE_STREAM_JSON));
    let mut child = fixture
        .command()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start pass without control descriptor");
    writeln!(
        child.stdin.as_mut().unwrap(),
        "{}\nmalformed",
        control_request("interrupt", "ignored")
    )
    .unwrap();
    let output = finish_control_pass(child);
    assert!(output.status.success());
    assert_eq!(output.stdout, b"");
    assert_eq!(output.stderr, b"");
    assert_eq!(fixture.transcript_bytes(), CLAUDE_STREAM_JSON.as_bytes());
    assert_eq!(
        fs::read(fixture.state.join("builder-pass-id")).unwrap(),
        b"a1b2c3d4\n"
    );
    assert_eq!(
        fs::read(fixture.state.join("builder-wake-counter")).unwrap(),
        b"7\n"
    );
    assert_eq!(normalize_pass_trace(&fs::read(fixture.state.join("sprint.jsonl")).unwrap()),
        concat!(
            "{\"ts\":\"2026-08-01T00:00:00Z\",\"kind\":\"pass-started\",\"fact\":{\"owner\":\"builder-a1b2c3d4-wake7\"},\"narration\":{}}\n",
            "{\"ts\":\"2026-08-01T00:00:00Z\",\"kind\":\"pass-ended\",\"fact\":{\"owner\":\"builder-a1b2c3d4-wake7\",\"outcome\":\"no-op\",\"cost_usd\":1.25,\"duration_seconds\":0,\"reason\":\"blocked\"},\"narration\":{}}\n"
        ).as_bytes());
    let events = fixture.run_events();
    assert_eq!(
        events
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "run.started",
            "agent.started",
            "agent.tool_use",
            "agent.tool_result",
            "agent.completed",
            "run.finished"
        ]
    );
    assert_one_terminal(&events, "no-op");
}

#[test]
fn stream_json_is_teed_byte_for_byte_and_emits_the_agent_roster() {
    let fixture = Fixture::new(&stream_script(CLAUDE_STREAM_JSON));
    let output = fixture.command().output().expect("run captured pass");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(fixture.transcript_bytes(), CLAUDE_STREAM_JSON.as_bytes());
    let events = fixture.run_events();
    let event_types = events
        .iter()
        .filter_map(|event| event["type"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        event_types,
        [
            "run.started",
            "agent.started",
            "agent.tool_use",
            "agent.tool_result",
            "agent.completed",
            "run.finished",
        ]
    );
    assert_eq!(
        event_types
            .iter()
            .filter(|event_type| **event_type == "run.finished")
            .count(),
        1
    );

    let agent_cost = events
        .iter()
        .find(|event| event["type"] == "agent.completed")
        .and_then(|event| event["payload"]["costUsd"].as_f64());
    let pass_cost = fixture
        .trace()
        .into_iter()
        .rev()
        .find(|row| row["kind"] == "pass-ended")
        .and_then(|row| row["fact"]["cost_usd"].as_f64());
    assert_eq!(agent_cost, pass_cost);
    assert_eq!(agent_cost, Some(1.25));
}

#[test]
fn malformed_capture_line_is_refused_without_failing_the_pass() {
    let stream = CLAUDE_STREAM_JSON.replacen('\n', "\nthis is not a stream-json frame\n", 1);
    let fixture = Fixture::new(&stream_script(&stream));
    let output = fixture.command().output().expect("run malformed capture");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(fixture.transcript_bytes(), stream.as_bytes());
    let events = fixture.run_events();
    let refusal = events
        .iter()
        .find(|event| event["type"] == "capture.refused")
        .expect("capture refusal event");
    assert_eq!(refusal["payload"]["cause"], "malformed");
    assert!(
        refusal["payload"]["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("malformed raw line 2"))
    );
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "agent.completed")
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "run.finished")
            .count(),
        1
    );
}

#[test]
fn over_bound_agent_text_draft_is_refused_without_failing_the_pass() {
    // The normaliser bounds text but preserves parentToolUseId. This produces
    // an agent.text draft that only the real sink's validation will refuse.
    let count = ethogram::MAX_TEXT_SCALARS + 1;
    let frame = json!({
        "type": "assistant",
        "parent_tool_use_id": "🦀".repeat(count),
        "message": {"content": [{"type": "text", "text": "observed message"}]},
    });
    let stream = CLAUDE_STREAM_JSON.replacen('\n', &format!("\n{frame}\n"), 1);
    let fixture = Fixture::new(&stream_script_with_work(&stream));
    let output = fixture.command().output().expect("run over-bound capture");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.transcript_bytes(), stream.as_bytes());
    let events = fixture.run_events();
    let refusals = events
        .iter()
        .filter(|event| event["type"] == "capture.refused")
        .collect::<Vec<_>>();
    assert_eq!(refusals.len(), 1);
    let refusal = &refusals[0]["payload"];
    assert_eq!(refusal["cause"], "over_bound");
    assert_eq!(refusal["sourceRunId"], events[0]["runId"]);
    assert_eq!(refusal["sourceType"], "agent.text");
    assert_eq!(refusal["field"], "payload.parentToolUseId");
    assert_eq!(refusal["count"], count);
    assert_eq!(refusal["max"], ethogram::MAX_TEXT_SCALARS);
    assert_eq!(refusal["unit"], "scalars");
    assert!(!events.iter().any(|event| event["type"] == "agent.text"));
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "agent.completed")
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "run.finished")
            .count(),
        1
    );
    assert_eq!(
        events.last().expect("terminal event")["payload"]["outcome"],
        "completed"
    );
    fixture.assert_released();
    assert_eq!(
        fixture.trace().last().expect("pass-ended")["fact"]["outcome"],
        "completed"
    );
}

#[test]
fn invalid_normalised_draft_is_refused_without_failing_the_pass() {
    // A u64 is accepted by the normaliser, but this exceeds ethogram's wire
    // safe-integer bound and must be rejected by the sink as Invalid.
    let valid_result = CLAUDE_STREAM_JSON.lines().last().expect("result frame");
    let invalid_result =
        valid_result.replace("\"num_turns\":1", &format!("\"num_turns\":{}", u64::MAX));
    let stream =
        CLAUDE_STREAM_JSON.replace(valid_result, &format!("{invalid_result}\n{valid_result}"));
    let fixture = Fixture::new(&stream_script_with_work(&stream));
    let output = fixture
        .command()
        .output()
        .expect("run invalid draft capture");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events = fixture.run_events();
    let refusals = events
        .iter()
        .filter(|event| event["type"] == "capture.refused")
        .collect::<Vec<_>>();
    assert_eq!(refusals.len(), 1);
    let refusal = &refusals[0]["payload"];
    assert_eq!(refusal["cause"], "malformed");
    assert_eq!(refusal["sourceRunId"], events[0]["runId"]);
    assert_eq!(refusal["sourceType"], "agent.completed");
    assert_eq!(refusal["field"], "payload.turns");
    assert!(
        refusal["detail"]
            .as_str()
            .expect("validation detail")
            .contains("safe integer bound")
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "agent.completed")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "run.finished")
            .count(),
        1
    );
    assert_eq!(
        events.last().expect("terminal event")["payload"]["outcome"],
        "completed"
    );
    fixture.assert_released();
    assert_eq!(
        fixture.trace().last().expect("pass-ended")["fact"]["outcome"],
        "completed"
    );
}

#[test]
fn facts_only_withholds_agent_events_only_from_the_live_descriptor() {
    let fixture = Fixture::new(&stream_script(CLAUDE_STREAM_JSON));
    let output = fixture
        .command()
        .args(["--events-fd", "1", "--facts-only"])
        .output()
        .expect("run facts-only pass");
    assert!(output.status.success());

    let live = String::from_utf8(output.stdout)
        .expect("live events UTF-8")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("live event JSON"))
        .collect::<Vec<_>>();
    assert_eq!(
        live.iter()
            .filter_map(|event| event["type"].as_str())
            .collect::<Vec<_>>(),
        ["run.started", "run.finished"]
    );
    assert!(
        fixture
            .run_events()
            .iter()
            .any(|event| event["type"] == "agent.tool_use")
    );

    let malformed = CLAUDE_STREAM_JSON.replacen('\n', "\nnot a stream-json frame\n", 1);
    let gap_fixture = Fixture::new(&stream_script(&malformed));
    let gap_output = gap_fixture
        .command()
        .args(["--events-fd", "1", "--facts-only"])
        .output()
        .expect("run facts-only pass with capture gap");
    assert!(gap_output.status.success());
    let gap_live = String::from_utf8(gap_output.stdout).expect("live gap events UTF-8");
    assert!(gap_live.contains("\"type\":\"capture.refused\""));
    assert!(!gap_live.contains("\"type\":\"agent."));
}

#[test]
fn agent_events_flow_to_the_live_descriptor_by_default_and_env_can_withhold_them() {
    let default_fixture = Fixture::new(&stream_script(CLAUDE_STREAM_JSON));
    let default = default_fixture
        .command()
        .args(["--events-fd", "1"])
        .output()
        .expect("run default live pass");
    assert!(default.status.success());
    assert_eq!(default.stdout, default_fixture.run_event_bytes());
    assert!(String::from_utf8_lossy(&default.stdout).contains("\"type\":\"agent.tool_result\""));

    let facts_fixture = Fixture::new(&stream_script(CLAUDE_STREAM_JSON));
    let facts = facts_fixture
        .command()
        .args(["--events-fd", "1"])
        .env("OSTROM_FACTS_ONLY", "true")
        .output()
        .expect("run environment facts-only pass");
    assert!(facts.status.success());
    assert!(!String::from_utf8_lossy(&facts.stdout).contains("\"type\":\"agent."));
    assert!(
        facts_fixture
            .run_events()
            .iter()
            .any(|event| event["type"] == "agent.completed")
    );
}

#[test]
fn a_cost_cap_trip_emits_one_terminal_event() {
    let stream = CLAUDE_STREAM_JSON.replace("1.25", "0.5");
    let fixture = Fixture::new(&format!("{}\nsleep 30", stream_script(&stream)));
    let manifest = fixture.state.join("ostrom.yaml");
    fs::write(
        &manifest,
        concat!(
            "manifest_version: 1\n",
            "defaults:\n",
            "  loop:\n",
            "    spend_usd: 0.5\n",
        ),
    )
    .expect("write capped operator manifest");
    let trusted_keys = support::sign_manifest(&manifest);

    let output = fixture
        .command()
        .env("OSTROM_POLICY_TRUSTED_KEYS", &trusted_keys)
        .output()
        .expect("run capped pass");
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("reached its cost cap"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let events = fixture.run_events();
    let finished = events
        .iter()
        .filter(|event| event["type"] == "run.finished")
        .collect::<Vec<_>>();
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0]["payload"]["outcome"], "capped");
    assert!(
        finished[0]["payload"]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("cost cap tripped"))
    );
}

#[test]
fn an_unwritable_events_fd_does_not_kill_the_pass() {
    let fixture = Fixture::new("exit 0");
    let output = fixture
        .command()
        .args(["--events-fd", "4294967295"])
        .output()
        .expect("run pass with an unwritable event descriptor");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("ostrom observability: could not open events fd 4294967295")
    );
    assert_eq!(fixture.run_events().len(), 2);
}

#[test]
fn events_prints_canonical_jsonl_and_after_replays_then_follows() {
    let fixture = Fixture::new("exit 0");
    let status = fixture.command().status().expect("run pass");
    assert!(status.success());
    let durable = fixture.run_event_bytes();
    let run_id = fixture.run_events()[0]["runId"]
        .as_str()
        .expect("event run ID")
        .to_owned();

    let snapshot = fixture
        .events_command(&run_id)
        .output()
        .expect("read the run snapshot");
    assert!(
        snapshot.status.success(),
        "{}",
        String::from_utf8_lossy(&snapshot.stderr)
    );
    assert_eq!(snapshot.stdout, durable);

    let followed = fixture
        .events_command(&run_id)
        .args(["--after", "1"])
        .output()
        .expect("replay and follow the terminal event");
    assert!(
        followed.status.success(),
        "{}",
        String::from_utf8_lossy(&followed.stderr)
    );
    let terminal = durable
        .split_inclusive(|byte| *byte == b'\n')
        .nth(1)
        .expect("terminal event line");
    assert_eq!(followed.stdout, terminal);
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if path.exists() && fs::metadata(path).is_ok_and(|metadata| metadata.len() > 0) {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {}", path.display());
}

fn signal(pid: u32, name: &str) {
    assert!(
        Command::new("kill")
            .args([format!("-{name}"), pid.to_string()])
            .status()
            .expect("send signal")
            .success()
    );
}

fn wait(mut child: Child) -> ExitStatus {
    child.wait().expect("wait for pass")
}

/// ostrom#99, the production path: this test must exercise the realtime clock
/// that the binary constructs, not a deterministic library clock.
///
/// This is not a hypothetical. ostrom#323 was exactly this shape one variable
/// over: every dispatch test set `OSTROM_HOME` or `CLAUDE_CONFIG_DIR`, the bug
/// required neither to be set, and it took the loop down for 48 hours while CI
/// stayed green. The bash suite guarded the clock case deliberately, with its
/// own fixture and an explicit unset clock. That guard must not be lost in the
/// move to Rust. Remove every retired clock name here, including the
/// helper-mediated audit, replay, and excuse clocks, so reintroducing an
/// ambient fixture clock elsewhere cannot silently weaken this test.
///
/// The claim is "the real clock, not the simulated day" — not a specific date —
/// so both sides of a UTC midnight crossing are accepted rather than letting a
/// midnight run flake.
#[test]
fn an_unpinned_clock_stamps_pass_rows_with_the_real_date() {
    let fixture = Fixture::new("exit 0");

    let before = chrono_free_utc_date();
    let mut command = fixture.command();
    for name in [
        "MANDATE_NOW_EPOCH",
        "MANDATE_TRACE_TIME",
        "MANDATE_TODAY",
        "MANDATE_GATE_TIME",
        "MANDATE_SWEEP_TIME",
        "MANDATE_EVENT_TIME",
        "MANDATE_DIGEST_TIME",
        "MANDATE_LEASE_NOW_EPOCH",
        "MANDATE_AUDIT_TIME",
        "MANDATE_REPLAY_TIME",
        "MANDATE_EXCUSE_TIME",
    ] {
        command.env_remove(name);
    }
    let status = command.status().expect("run pass with an unpinned clock");
    assert!(status.success());
    let after = chrono_free_utc_date();

    let trace = fixture.trace();
    let stamps = trace
        .iter()
        .filter(|row| {
            matches!(
                row["kind"].as_str(),
                Some("pass-started") | Some("pass-ended")
            )
        })
        .map(|row| {
            row["ts"]
                .as_str()
                .expect("row carries a timestamp")
                .get(..10)
                .expect("timestamp starts with a date")
                .to_owned()
        })
        .collect::<Vec<_>>();

    assert!(
        !stamps.is_empty(),
        "the pass wrote no pass-started/pass-ended rows"
    );
    for stamp in &stamps {
        assert!(
            stamp == &before || stamp == &after,
            "row stamped {stamp}, which is neither {before} nor {after} — the pass \
             is reading a simulated clock on the path production takes"
        );
        assert_ne!(
            stamp, "2026-08-01",
            "row carries the suite's pinned fixture day even though the clock was \
             unpinned; the pinned value is leaking into the production path"
        );
    }
}

/// `%Y-%m-%d` for now, without taking a chrono dependency in the test crate.
fn chrono_free_utc_date() -> String {
    let output = Command::new("date")
        .args(["-u", "+%Y-%m-%d"])
        .output()
        .expect("read the real UTC date");
    String::from_utf8(output.stdout)
        .expect("date is UTF-8")
        .trim()
        .to_owned()
}

#[test]
fn recorded_shell_output_matches_apart_from_the_injected_clock() {
    let fixture = Fixture::new(concat!(
        "printf '%s\\n' '{\"ts\":\"2026-08-01T00:00:00Z\",\"kind\":\"pass-started\",\"fact\":{\"owner\":\"builder-placeholder-session-wake7\"},\"narration\":{}}' >>\"$OSTROM_HOME/sprint.jsonl\"\n",
        "printf '%s\\n' '{\"type\":\"result\",\"total_cost_usd\":1.25}'"
    ));
    let output = fixture.command().output().expect("run pass");
    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        include_bytes!("fixtures/pass/builder-stdout.expected.txt")
    );
    assert_eq!(
        output.stderr,
        include_bytes!("fixtures/pass/builder-stderr.expected.txt")
    );
    assert_eq!(
        normalize_pass_trace(
            &fs::read(fixture.state.join("sprint.jsonl")).expect("read native trace")
        ),
        include_bytes!("fixtures/pass/builder-trace.expected.jsonl")
    );
    fixture.assert_released();
}

/// The recorded fixture compares the trace byte for byte, so any wall-clock
/// field in it is a latent flake. This is the case that actually failed in CI:
/// the same pass emitted `duration_seconds` 0 on an idle machine and 1 under
/// load. Forcing the pass to take over a second must not change a single byte.
#[test]
fn a_slow_pass_changes_only_realtime_duration_and_timestamps() {
    let fixture = Fixture::new(concat!(
        "sleep 2\n",
        "printf '%s\\n' '{\"ts\":\"2026-08-01T00:00:00Z\",\"kind\":\"pass-started\",\"fact\":{\"owner\":\"builder-placeholder-session-wake7\"},\"narration\":{}}' >>\"$OSTROM_HOME/sprint.jsonl\"\n",
        "printf '%s\\n' '{\"type\":\"result\",\"total_cost_usd\":1.25}'"
    ));
    let output = fixture.command().output().expect("run pass");
    assert!(output.status.success());
    assert_eq!(
        normalize_pass_trace(
            &fs::read(fixture.state.join("sprint.jsonl")).expect("read native trace")
        ),
        include_bytes!("fixtures/pass/builder-trace.expected.jsonl")
    );
    fixture.assert_released();
}

fn normalize_pass_trace(bytes: &[u8]) -> Vec<u8> {
    let mut normalized = Vec::new();
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let mut row: Value = serde_json::from_slice(line).expect("trace JSON");
        row["ts"] = Value::String("2026-08-01T00:00:00Z".to_owned());
        if row["kind"] == "pass-ended" {
            row["fact"]["duration_seconds"] = Value::from(0);
        }
        serde_json::to_writer(&mut normalized, &row).expect("serialize normalized trace");
        normalized.push(b'\n');
    }
    normalized
}

#[test]
fn error_exit_releases_and_finalizes() {
    let fixture = Fixture::new("exit 42");
    let status = fixture.command().status().expect("run failing pass");
    assert_eq!(status.code(), Some(42));
    fixture.assert_released();
    assert_eq!(fixture.trace().last().unwrap()["fact"]["outcome"], "failed");
    let events = fixture.run_events();
    assert_eq!(events[0]["type"], "run.started");
    assert_eq!(events[0]["payload"]["kind"], "loop");
    assert_eq!(events[1]["type"], "run.finished");
    assert_eq!(events[1]["payload"]["outcome"], "failed");
    assert_eq!(events[1]["payload"]["reason"], "pass-failed");
}

#[test]
fn successful_pass_emits_a_completed_lifecycle() {
    let fixture = Fixture::new(concat!(
        "printf '%s\\n' '{\"ts\":\"2026-08-01T00:00:00Z\",\"kind\":\"pass-started\",\"fact\":{\"owner\":\"builder-inner-wake1\"},\"narration\":{}}' >>\"$OSTROM_HOME/sprint.jsonl\"\n",
        "printf '%s\\n' '{\"ts\":\"2026-08-01T00:00:01Z\",\"kind\":\"pass-ended\",\"fact\":{\"owner\":\"builder-inner-wake1\",\"outcome\":\"completed\"},\"narration\":{}}' >>\"$OSTROM_HOME/sprint.jsonl\""
    ));

    assert!(fixture.command().status().expect("run pass").success());

    let events = fixture.run_events();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["type"], "run.started");
    assert_eq!(events[0]["payload"]["actor"], "builder");
    assert_eq!(events[0]["payload"]["harness"], "claude");
    assert_eq!(events[1]["type"], "run.finished");
    assert_eq!(events[1]["payload"]["outcome"], "completed");
    assert!(events[1]["payload"]["durationMs"].is_u64());
}

#[test]
fn sigterm_releases_finalizes_and_kills_the_process_group() {
    let fixture = Fixture::new(concat!(
        "printf '%s\\n' \"$$\" >\"$OSTROM_HOME/child.pid\"\n",
        "(trap '' TERM; while :; do sleep 1; done) &\n",
        "printf '%s\\n' \"$!\" >\"$OSTROM_HOME/grandchild.pid\"\n",
        "trap 'exit 143' TERM\n",
        "while :; do sleep 1; done"
    ));
    let child = fixture.command().spawn().expect("start pass");
    wait_for(&fixture.state.join("grandchild.pid"));
    signal(child.id(), "TERM");
    let status = wait(child);
    assert_eq!(status.code(), Some(143));
    fixture.assert_released();
    assert_eq!(
        fixture.trace().last().unwrap()["fact"]["outcome"],
        "timed-out"
    );
    let events = fixture.run_events();
    let event_types = events
        .iter()
        .filter_map(|event| event["type"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        event_types,
        [
            "run.started",
            "control.requested",
            "control.applied",
            "run.finished",
        ]
    );
    assert_eq!(events[2]["payload"]["ok"], true);
    assert_eq!(events[3]["payload"]["outcome"], "interrupted");
    assert_eq!(
        event_types
            .iter()
            .filter(|event_type| **event_type == "run.finished")
            .count(),
        1
    );
    let grandchild =
        fs::read_to_string(fixture.state.join("grandchild.pid")).expect("read grandchild pid");
    assert!(
        !Command::new("kill")
            .args(["-0", grandchild.trim()])
            .status()
            .expect("probe grandchild")
            .success()
    );
}

#[test]
fn killed_child_releases_and_finalizes() {
    let fixture = Fixture::new("kill -KILL $$");
    let status = fixture.command().status().expect("run pass");
    assert!(!status.success());
    fixture.assert_released();
}

#[test]
fn orphaned_worker_does_not_retain_the_lease() {
    let fixture = Fixture::new(concat!(
        "printf '%s\\n' \"$$\" >\"$OSTROM_HOME/child.pid\"\n",
        "trap 'exit 143' TERM\n",
        "while :; do sleep 1; done"
    ));
    let child = fixture.command().spawn().expect("start pass");
    wait_for(&fixture.state.join("child.pid"));
    signal(child.id(), "KILL");
    let _ = wait(child);
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && fixture.state.join("builder-pass.lease").exists() {
        thread::sleep(Duration::from_millis(25));
    }
    fixture.assert_released();
}

#[test]
fn sigterm_cleanup_does_not_depend_on_sh() {
    let fixture = Fixture::new(concat!(
        "printf '%s\\n' \"$$\" >\"$OSTROM_HOME/child.pid\"\n",
        "trap 'exit 143' TERM\n",
        "while :; do sleep 1; done"
    ));
    let path = fixture.root.path().join("path-without-sh");
    fs::create_dir(&path).expect("create isolated PATH");
    symlink("/bin/bash", path.join("bash")).expect("link bash");
    symlink("/bin/sleep", path.join("sleep")).expect("link sleep");
    let child = fixture
        .command()
        .env("PATH", &path)
        .spawn()
        .expect("start pass without sh on PATH");
    wait_for(&fixture.state.join("child.pid"));
    signal(child.id(), "TERM");
    assert_eq!(wait(child).code(), Some(143));
    fixture.assert_released();

    let binary = fs::read(env!("CARGO_BIN_EXE_ostrom")).expect("read built binary");
    assert!(!binary.windows(5).any(|window| window == b"sh -c"));
    assert!(
        !binary
            .windows(b"signal_file=$1".len())
            .any(|window| window == b"signal_file=$1")
    );
}

#[test]
fn disarmed_and_outer_lease_held_passes_do_not_spawn_or_trace() {
    let disarmed = Fixture::new("touch \"$OSTROM_TEST_MARKER\"");
    let marker = disarmed.root.path().join("spawned");
    fs::remove_file(disarmed.state.join("loop-armed")).expect("disarm fixture");
    let output = disarmed
        .command()
        .env("OSTROM_TEST_MARKER", &marker)
        .output()
        .expect("run disarmed pass");
    assert_eq!(output.status.code(), Some(78));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("loop is disarmed"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!marker.exists());
    assert!(!disarmed.state.join("sprint.jsonl").exists());
    let events = disarmed.run_events();
    assert_eq!(events[1]["type"], "run.finished");
    assert_eq!(events[1]["payload"]["outcome"], "no-op");
    assert_eq!(events[1]["payload"]["reason"], "disarmed");

    let held = Fixture::new("touch \"$OSTROM_TEST_MARKER\"");
    let marker = held.root.path().join("spawned");
    fs::write(
        held.state.join("builder-pass.lease"),
        format!(
            "{{\"owner\":\"fixture-holder\",\"started_at\":1,\"expires_at\":{}}}\n",
            u64::MAX
        ),
    )
    .expect("write held outer lease");
    let output = held
        .command()
        .env("OSTROM_TEST_MARKER", &marker)
        .output()
        .expect("run overlapping pass");
    assert!(output.status.success());
    assert!(!marker.exists());
    assert!(!held.state.join("sprint.jsonl").exists());
    let events = held.run_events();
    assert_eq!(events[1]["type"], "run.finished");
    assert_eq!(events[1]["payload"]["outcome"], "no-op");
    assert_eq!(events[1]["payload"]["reason"], "lease-held");
}

#[test]
fn an_unchanged_fully_blocked_backlog_ends_before_spawning_and_records_zero_cost() {
    let fixture = Fixture::new(concat!(
        "printf '%s\\n' spawned >>\"$OSTROM_TEST_MARKER\"\n",
        "printf '%s\\n' '{\"type\":\"result\",\"total_cost_usd\":1.25}'"
    ));
    fixture.write_blocked_dispatchability_state();
    let marker = fixture.root.path().join("spawned");

    for _ in 0..2 {
        assert!(
            fixture
                .command()
                .env("OSTROM_TEST_MARKER", &marker)
                .status()
                .expect("run blocked pass")
                .success()
        );
    }

    assert_eq!(
        fs::read_to_string(&marker)
            .expect("read spawn marker")
            .lines()
            .count(),
        1,
        "the unchanged second pass spawned the agent"
    );
    let ended = fixture
        .trace()
        .into_iter()
        .filter(|row| row["kind"] == "pass-ended")
        .collect::<Vec<_>>();
    assert_eq!(ended.len(), 2);
    assert_eq!(ended[0]["fact"]["cost_usd"], 1.25);
    assert_eq!(ended[0]["fact"]["dispatchable_count"], 0);
    assert_eq!(ended[1]["fact"]["outcome"], "no-op");
    assert_eq!(ended[1]["fact"]["reason"], "no-dispatchable-work-unchanged");
    assert_eq!(ended[1]["fact"]["cost_usd"], 0.0);
    assert_eq!(ended[1]["fact"]["queue_count"], 1);
    assert_eq!(ended[1]["fact"]["dispatchable_count"], 0);
    let hash = ended[1]["fact"]["dispatchability_hash"]
        .as_str()
        .expect("terminal trace carries the snapshot hash");
    assert_eq!(hash.len(), 64);
    assert_eq!(
        fs::read_to_string(fixture.state.join("builder-dispatchability-hash"))
            .expect("read durable snapshot hash")
            .trim_end(),
        hash
    );
    fixture.assert_released();
}

#[test]
fn a_dispatchability_input_change_defeats_the_short_circuit_on_the_next_pass() {
    let fixture = Fixture::new("printf '%s\\n' spawned >>\"$OSTROM_TEST_MARKER\"");
    fixture.write_blocked_dispatchability_state();
    let marker = fixture.root.path().join("spawned");

    assert!(
        fixture
            .command()
            .env("OSTROM_TEST_MARKER", &marker)
            .status()
            .expect("establish blocked snapshot")
            .success()
    );
    fixture.approve_blocked_decision();
    assert!(
        fixture
            .command()
            .env("OSTROM_TEST_MARKER", &marker)
            .status()
            .expect("run immediately after approval")
            .success()
    );

    assert_eq!(
        fs::read_to_string(&marker)
            .expect("read spawn marker")
            .lines()
            .count(),
        2,
        "the approved decision did not spawn on the very next pass"
    );
    let terminal = fixture.trace().pop().expect("changed terminal row");
    assert_eq!(terminal["fact"]["dispatchable_count"], 1);
    assert_ne!(terminal["fact"]["reason"], "no-dispatchable-work-unchanged");
    fixture.assert_released();
}

#[test]
fn a_failed_agent_pass_does_not_establish_a_blocked_snapshot() {
    let fixture = Fixture::new("exit 42");
    fixture.write_blocked_dispatchability_state();
    assert_eq!(
        fixture
            .command()
            .status()
            .expect("run failed blocked pass")
            .code(),
        Some(42)
    );
    assert!(!fixture.state.join("builder-dispatchability-hash").exists());

    fs::write(
        &fixture.claude,
        "#!/usr/bin/env bash\nprintf '%s\\n' spawned >\"$OSTROM_TEST_MARKER\"\n",
    )
    .expect("replace failed agent stub");
    let marker = fixture.root.path().join("retried");
    assert!(
        fixture
            .command()
            .env("OSTROM_TEST_MARKER", &marker)
            .status()
            .expect("retry blocked pass")
            .success()
    );
    assert!(marker.exists(), "the failed pass suppressed its retry");
    fixture.assert_released();
}

#[test]
fn default_branch_turning_green_defeats_the_short_circuit_without_a_candidate() {
    let fixture = Fixture::new("printf '%s\\n' spawned >>\"$OSTROM_TEST_MARKER\"");
    fixture.write_blocked_dispatchability_state();
    let state_path = fixture.state.join("state.json");
    let mut state: Value =
        serde_json::from_slice(&fs::read(&state_path).expect("read state")).expect("parse state");
    state["repos"]["placeholder-org/alpha"]["ci_drift"] = json!({
        "17": {"run_id": 41, "red_since": "2026-07-31T00:00:00Z"}
    });
    fs::write(
        &state_path,
        serde_json::to_vec(&state).expect("serialize red state"),
    )
    .expect("write red state");
    let marker = fixture.root.path().join("spawned");
    assert!(
        fixture
            .command()
            .env("OSTROM_TEST_MARKER", &marker)
            .status()
            .expect("establish red snapshot")
            .success()
    );

    state["repos"]["placeholder-org/alpha"]["ci_drift"] = json!({});
    fs::write(
        &state_path,
        serde_json::to_vec(&state).expect("serialize green state"),
    )
    .expect("write green state");
    assert!(
        fixture
            .command()
            .env("OSTROM_TEST_MARKER", &marker)
            .status()
            .expect("run first green pass")
            .success()
    );

    assert_eq!(
        fs::read_to_string(&marker)
            .expect("read spawn marker")
            .lines()
            .count(),
        2,
        "the default-branch transition did not wake the agent immediately"
    );
    let terminal = fixture.trace().pop().expect("green terminal row");
    assert_eq!(terminal["fact"]["dispatchable_count"], 0);
    assert_ne!(terminal["fact"]["reason"], "no-dispatchable-work-unchanged");
    fixture.assert_released();
}

#[test]
fn roles_receive_their_permission_modes_and_wakes_retain_one_identity() {
    let fixture = Fixture::new(concat!(
        "printf '%s\\n' \"$@\" >\"$OSTROM_TEST_ARGS\"\n",
        "printf '%s\\n' '{\"ts\":\"2026-08-01T00:00:00Z\",\"kind\":\"pass-started\",\"fact\":{\"owner\":\"builder-inner-wake\"},\"narration\":{}}' >>\"$OSTROM_HOME/sprint.jsonl\"\n",
        "printf '%s\\n' '{\"type\":\"result\",\"total_cost_usd\":1.25}'"
    ));
    let arguments = fixture.root.path().join("arguments");
    for _ in 0..2 {
        let status = fixture
            .command()
            .env("OSTROM_TEST_ARGS", &arguments)
            .status()
            .expect("run builder pass");
        assert!(status.success());
    }
    let args = fs::read_to_string(&arguments).expect("read builder arguments");
    assert!(args.contains("--permission-mode\nauto\n"));
    assert!(args.contains("--max-turns\n200\n"));
    assert!(args.contains("# Mandate Work\n"));
    assert!(!args.lines().any(|line| line == "ostrom pass builder"));
    assert!(!args.lines().any(|line| line == "default" || line == "40"));
    let trace = fixture.trace();
    let wrapper_owners = trace
        .iter()
        .filter(|row| row["kind"] == "pass-started")
        .filter_map(|row| row["fact"]["owner"].as_str())
        .filter(|owner| *owner != "builder-inner-wake")
        .collect::<Vec<_>>();
    assert_eq!(wrapper_owners.len(), 2);
    assert!(wrapper_owners[0].ends_with("-wake7"));
    assert!(wrapper_owners[1].ends_with("-wake8"));
    assert_eq!(
        wrapper_owners[0].rsplit_once("-wake").unwrap().0,
        wrapper_owners[1].rsplit_once("-wake").unwrap().0
    );

    let gatekeeper = Fixture::new("printf '%s\\n' \"$@\" >\"$OSTROM_TEST_ARGS\"");
    fs::write(
        gatekeeper.state.join("roles/gatekeeper.settings.json"),
        "{}\n",
    )
    .expect("write gatekeeper settings");
    let gatekeeper_arguments = gatekeeper.root.path().join("gatekeeper-arguments");
    assert!(
        gatekeeper
            .command_for("gatekeeper")
            .env("OSTROM_TEST_ARGS", &gatekeeper_arguments)
            .status()
            .expect("run gatekeeper pass")
            .success()
    );
    let args = fs::read_to_string(gatekeeper_arguments).expect("read gatekeeper arguments");
    assert!(args.contains("--permission-mode\nmanual\n"));
    assert!(args.contains("# Mandate Gatekeep\n"));
    assert!(!args.lines().any(|line| line == "ostrom pass gatekeeper"));
    assert!(!args.lines().any(|line| line == "default"));
}

#[test]
fn wrapper_outcome_follows_inner_protocol_evidence() {
    let cases = [
        ("", true, "no-op", Some("blocked")),
        (
            "printf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"permission_denials\":[{\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"ostrom sweep\"}}]}'",
            true,
            "permission-denied",
            None,
        ),
        ("exit 42", false, "failed", None),
        (
            concat!(
                "printf '%s\\n' '{\"ts\":\"2026-08-01T00:00:00Z\",\"kind\":\"pass-started\",\"fact\":{\"owner\":\"builder-inner-wake1\"},\"narration\":{}}' >>\"$OSTROM_HOME/sprint.jsonl\"\n",
                "printf '%s\\n' '{\"ts\":\"2026-08-01T00:00:00Z\",\"kind\":\"pass-ended\",\"fact\":{\"outcome\":\"failed\"},\"narration\":{}}' >>\"$OSTROM_HOME/sprint.jsonl\""
            ),
            true,
            "failed",
            None,
        ),
        (
            concat!(
                "printf '%s\\n' '{\"ts\":\"2026-08-01T00:00:00Z\",\"kind\":\"pass-started\",\"fact\":{\"owner\":\"builder-inner-wake1\"},\"narration\":{}}' >>\"$OSTROM_HOME/sprint.jsonl\"\n",
                "printf '%s\\n' '{\"ts\":\"2026-08-01T00:00:00Z\",\"kind\":\"pass-ended\",\"fact\":{\"outcome\":\"completed\"},\"narration\":{}}' >>\"$OSTROM_HOME/sprint.jsonl\""
            ),
            true,
            "completed",
            None,
        ),
    ];
    for (script, success, outcome, reason) in cases {
        let fixture = Fixture::new(script);
        let output = fixture.command().output().expect("run outcome case");
        assert_eq!(output.status.success(), success, "{outcome}");
        let terminal = fixture.trace().pop().expect("terminal trace row");
        assert_eq!(terminal["fact"]["outcome"], outcome);
        assert_eq!(terminal["fact"]["reason"].as_str(), reason);
    }

    let gatekeeper = Fixture::new("");
    fs::write(
        gatekeeper.state.join("roles/gatekeeper.settings.json"),
        "{}\n",
    )
    .expect("write gatekeeper settings");
    assert!(
        gatekeeper
            .command_for("gatekeeper")
            .status()
            .expect("run gatekeeper no-op")
            .success()
    );
    let terminal = gatekeeper.trace().pop().expect("gatekeeper terminal");
    assert_eq!(terminal["fact"]["outcome"], "no-op");
    assert_eq!(terminal["fact"]["reason"], "blocked");
}

#[test]
fn permission_denial_overrides_partial_inner_protocol_evidence() {
    let fixture = Fixture::new(concat!(
        "printf '%s\\n' '{\"ts\":\"2026-08-01T00:00:00Z\",\"kind\":\"pass-started\",\"fact\":{\"owner\":\"builder-inner-wake1\"},\"narration\":{}}' >>\"$OSTROM_HOME/sprint.jsonl\"\n",
        "printf '%s\\n' '{\"type\":\"result\",\"total_cost_usd\":0.5,\"permission_denials\":[{\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"ostrom repair-prs owner\"}}]}'"
    ));
    assert!(
        fixture
            .command()
            .status()
            .expect("run denied pass")
            .success()
    );
    let terminal = fixture.trace().pop().expect("denied terminal row");
    assert_eq!(terminal["fact"]["outcome"], "permission-denied");
    assert!(terminal["fact"].get("reason").is_none());
    assert_eq!(terminal["fact"]["cost_usd"], 0.5);
}

#[test]
fn inner_lease_cleanup_distinguishes_child_and_preexisting_owners() {
    let acquired = Fixture::new(concat!(
        "printf '{\"owner\":\"builder-child\",\"started_at\":%s,\"expires_at\":9999999999}\\n' \"$(date +%s)\" >\"$OSTROM_HOME/builder.lease\"\n",
        "printf '%s\\n' '{\"ts\":\"2026-08-01T00:00:00Z\",\"kind\":\"pass-started\",\"fact\":{\"owner\":\"builder-inner-wake1\"},\"narration\":{}}' >>\"$OSTROM_HOME/sprint.jsonl\""
    ));
    assert!(
        acquired
            .command()
            .status()
            .expect("run acquired lease")
            .success()
    );
    assert!(!acquired.state.join("builder.lease").exists());

    let preexisting = Fixture::new("");
    fs::write(
        preexisting.state.join("builder.lease"),
        "{\"owner\":\"interactive-builder\",\"started_at\":1,\"expires_at\":9999999999}\n",
    )
    .expect("write preexisting inner lease");
    assert!(
        preexisting
            .command()
            .status()
            .expect("run preexisting lease")
            .success()
    );
    assert!(preexisting.state.join("builder.lease").exists());
    let terminal = preexisting.trace().pop().expect("preexisting terminal");
    assert_eq!(terminal["fact"]["outcome"], "no-op");
    assert_eq!(terminal["fact"]["reason"], "lease-held");
}

#[test]
fn a_daily_budget_hold_reaches_the_live_event_descriptor() {
    let fixture = Fixture::new("exit 99");
    let output = fixture
        .command()
        .env("MANDATE_DAILY_CAP_USD", "0")
        .args(["--events-fd", "1"])
        .output()
        .expect("run budget hold with live events");
    assert_eq!(output.status.code(), Some(75));
    assert_eq!(output.stdout, fixture.run_event_bytes());
    assert_eq!(
        fixture
            .run_events()
            .iter()
            .map(|event| event["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["run.started", "decision.requested", "run.finished"]
    );
    assert!(!fixture.state.join("current").exists());
    assert!(!fixture.state.join("versions").exists());
    fixture.assert_released();
}

#[test]
fn daily_cap_uses_only_valid_costs_on_the_current_day() {
    for (cap, code, spawned, outcome, reason) in [
        ("8", 0, true, "completed", None),
        ("7", 75, false, "held", Some("daily-cap")),
        ("not-a-number", 0, true, "completed", None),
    ] {
        let fixture = Fixture::new(concat!(
            "touch \"$OSTROM_TEST_MARKER\"\n",
            "printf '%s\\n' '{\"ts\":\"2026-08-01T00:00:00Z\",\"kind\":\"pass-started\",\"fact\":{\"owner\":\"builder-inner-wake1\"},\"narration\":{}}' >>\"$OSTROM_HOME/sprint.jsonl\""
        ));
        let today = chrono_free_utc_date();
        let trace = [
            serde_json::json!({
                "ts": "1900-01-01T23:59:59Z",
                "kind": "pass-ended",
                "fact": {"cost_usd": 999},
                "narration": {}
            }),
            serde_json::json!({
                "ts": format!("{today}T00:00:00Z"),
                "kind": "pass-ended",
                "fact": {"cost_usd": 7},
                "narration": {}
            }),
            serde_json::json!({
                "ts": format!("{today}T00:00:01Z"),
                "kind": "pass-ended",
                "fact": {"cost_usd": "bad"},
                "narration": {}
            }),
            serde_json::json!({
                "ts": format!("{today}T00:00:02Z"),
                "kind": "pass-ended",
                "fact": {},
                "narration": {}
            }),
        ]
        .into_iter()
        .map(|row| format!("{row}\n"))
        .collect::<String>();
        fs::write(fixture.state.join("sprint.jsonl"), trace).expect("write spend trace");
        let marker = fixture.root.path().join("spawned");
        assert_eq!(
            fixture
                .command()
                .env("MANDATE_DAILY_CAP_USD", cap)
                .env("OSTROM_TEST_MARKER", &marker)
                .status()
                .expect("run spend case")
                .code(),
            Some(code),
            "cap {cap}"
        );
        assert_eq!(marker.exists(), spawned, "cap {cap}");
        let terminal = fixture.trace().pop().expect("spend terminal");
        assert_eq!(terminal["fact"]["outcome"], outcome, "cap {cap}");
        assert_eq!(terminal["fact"]["reason"].as_str(), reason, "cap {cap}");
        let events = fixture.run_events();
        let finished = events
            .iter()
            .filter(|event| event["type"] == "run.finished")
            .collect::<Vec<_>>();
        assert_eq!(finished.len(), 1);
        // FileSink stores without payload validation at this pin, so assert
        // ethogram validation as well as sink acceptance below. The outcome
        // string alone would miss an incompatible pin.
        ethogram::validate("run.finished", &finished[0]["payload"])
            .expect("terminal event must pass ethogram payload validation");
        let sink = umwelt_runtime::FileSink::new(fixture.root.path().join("validated-runs"));
        for event in &events {
            let event: ethogram::Event =
                serde_json::from_value(event.clone()).expect("valid event envelope");
            umwelt_runtime::Sink::forward(&sink, event)
                .expect("emitted event must be accepted by the production sink");
        }
        assert_eq!(
            finished[0]["payload"]["outcome"],
            if spawned { "completed" } else { "blocked" }
        );
        assert_eq!(
            finished[0]["payload"]["reason"].as_str(),
            if spawned { None } else { Some("budget") }
        );
        assert_eq!(
            events
                .iter()
                .map(|event| event["type"].as_str().unwrap())
                .collect::<Vec<_>>(),
            if spawned {
                vec!["run.started", "run.finished"]
            } else {
                vec!["run.started", "decision.requested", "run.finished"]
            }
        );
        assert!(
            events
                .iter()
                .all(|event| event["payload"]["outcome"] != "held")
        );
        assert!(
            fixture
                .trace()
                .iter()
                .all(|row| row["fact"]["outcome"] != "blocked")
        );
        let decisions = events
            .iter()
            .filter(|event| {
                event["type"] == "decision.requested" && event["payload"]["kind"] == "budget"
            })
            .collect::<Vec<_>>();
        let decision_facts = fixture
            .trace()
            .into_iter()
            .filter(|row| row["kind"] == "decision-requested" && row["fact"]["kind"] == "budget")
            .collect::<Vec<_>>();
        assert_eq!(decisions.len(), usize::from(!spawned));
        assert_eq!(decision_facts.len(), usize::from(!spawned));
        if !spawned {
            let payload = &decisions[0]["payload"];
            ethogram::validate("decision.requested", payload).expect("valid budget decision");
            assert!(payload.get("onTimeout").is_none());
            assert!(payload.get("on_timeout").is_none());
            assert_eq!(
                payload["subject"],
                format!("account:{}", fixture.state.display())
            );
            let question = payload["dossier"]["question"]
                .as_str()
                .expect("budget question");
            assert!(question.contains(&fixture.state.display().to_string()));
            assert!(question.contains("daily ceiling of 7 USD"));
            assert!(question.contains("7 USD spent"));
            assert_eq!(
                payload["dossier"]["optionsRuledOut"],
                json!(["Proceeding under the current spend ceiling"])
            );
            assert_eq!(
                payload["options"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|option| option["id"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                ["raise", "wait"]
            );
            assert!(
                payload["options"][0]["label"]
                    .as_str()
                    .unwrap()
                    .contains(&fixture.state.join("current").display().to_string())
            );
            assert_eq!(
                decision_facts[0]["fact"],
                json!({
                    "decision_id": payload["decisionId"],
                    "kind": "budget",
                    "subject": payload["subject"],
                })
            );
            ostrom_core::EventPayload::new(decision_facts[0]["fact"].as_object().unwrap().clone())
                .expect("budget fact has no narration");
            assert_eq!(
                terminal["fact"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                ["owner", "outcome", "cost_usd", "duration_seconds", "reason"]
            );
        }
        assert!(
            terminal["ts"]
                .as_str()
                .is_some_and(|timestamp| timestamp.starts_with(&today))
        );
        assert!(
            terminal["fact"]["duration_seconds"]
                .as_u64()
                .is_some_and(|duration| duration <= 5)
        );
    }
}

const HOSTILE_ENV_CHILD: &str = "OSTROM_TEST_HOSTILE_ENV_CHILD";

fn normalize_libtest_durations(output: &[u8]) -> Vec<u8> {
    const SUMMARY_PREFIX: &[u8] = b"test result: ";
    const DURATION_PREFIX: &[u8] = b"; finished in ";

    let mut normalized = Vec::with_capacity(output.len());
    for line in output.split_inclusive(|byte| *byte == b'\n') {
        if !line.starts_with(SUMMARY_PREFIX) {
            normalized.extend_from_slice(line);
            continue;
        }

        let Some(prefix_offset) = line
            .windows(DURATION_PREFIX.len())
            .position(|window| window == DURATION_PREFIX)
        else {
            normalized.extend_from_slice(line);
            continue;
        };
        let duration_start = prefix_offset + DURATION_PREFIX.len();
        let Some(seconds_offset) = line[duration_start..].iter().position(|byte| *byte == b's')
        else {
            normalized.extend_from_slice(line);
            continue;
        };
        let duration_end = duration_start + seconds_offset;
        let duration = &line[duration_start..duration_end];
        if duration.is_empty()
            || !duration
                .iter()
                .all(|byte| byte.is_ascii_digit() || *byte == b'.')
        {
            normalized.extend_from_slice(line);
            continue;
        }

        normalized.extend_from_slice(&line[..duration_start]);
        normalized.extend_from_slice(b"<duration>");
        normalized.extend_from_slice(&line[duration_end..]);
    }
    normalized
}

#[test]
fn libtest_duration_normalization_is_narrow() {
    let output = b"test result: ok. 1 passed; 0 failed; finished in 0.14s\n\
operator result: finished in 0.14s\n";
    assert_eq!(
        normalize_libtest_durations(output),
        b"test result: ok. 1 passed; 0 failed; finished in <duration>s\n\
operator result: finished in 0.14s\n"
    );

    let changed_result = b"test result: FAILED. 0 passed; 1 failed; finished in 0.20s\n\
operator result: finished in 0.14s\n";
    assert_ne!(
        normalize_libtest_durations(output),
        normalize_libtest_durations(changed_result)
    );
}

#[test]
fn polluted_operator_environment_cannot_change_a_pass_result() {
    if env::var_os(HOSTILE_ENV_CHILD).is_some() {
        let fixture = Fixture::new("");
        let output = fixture.command().output().expect("run hermetic pass");
        assert!(output.status.success());
        let terminal = fixture.trace().pop().expect("hermetic terminal");
        assert_eq!(terminal["fact"]["outcome"], "no-op");
        assert_eq!(terminal["fact"]["reason"], "blocked");
        return;
    }

    let executable = env::current_exe().expect("current integration test executable");
    let run = |polluted: bool| {
        let mut command = Command::new(&executable);
        command
            .env_clear()
            .env("PATH", env::var_os("PATH").unwrap_or_default())
            .env(HOSTILE_ENV_CHILD, "1")
            .args([
                "--exact",
                "polluted_operator_environment_cannot_change_a_pass_result",
                "--nocapture",
            ]);
        if polluted {
            for name in [
                "OSTROM_HOME",
                "CLAUDE_CONFIG_DIR",
                "ANTHROPIC_API_KEY",
                "MANDATE_SEMANTIC_DERIVER",
                "MANDATE_SEMANTIC_MODEL",
                "MANDATE_DAILY_CAP_USD",
                "MANDATE_LEASE_NAME",
                "MANDATE_MAX_IMPLEMENTERS",
                "MANDATE_IMPLEMENTER_SOURCE_REPO",
                "MANDATE_GH_AS_BIN",
                "MANDATE_SYSTEMD_RUN_BIN",
            ] {
                command.env(name, "hostile-operator-value");
            }
        }
        command.output().expect("run hermetic child")
    };
    let clean = run(false);
    let polluted = run(true);
    assert!(clean.status.success());
    assert!(polluted.status.success());
    assert_eq!(
        normalize_libtest_durations(&clean.stdout),
        normalize_libtest_durations(&polluted.stdout)
    );
    assert_eq!(
        normalize_libtest_durations(&clean.stderr),
        normalize_libtest_durations(&polluted.stderr)
    );
}

#[test]
fn a_declared_actor_owns_the_pass_prompt_and_permission_mode() {
    // The builder ships `auto` and the Mandate Work prompt. An operator
    // manifest that declares the actor overrides both, which is the point of
    // resolving them from policy: changing what an agent is told, and what it
    // may do unattended, becomes an authored, signed decision rather than a
    // binary release.
    //
    // The declaration is the operator's, not a visited repository's. A pass
    // travels between repositories, so a repository able to declare the
    // operation could rewrite the instructions the builder arrives with.
    let fixture = Fixture::new(
        r#"printf '%s\n' "$@" >"$OSTROM_TEST_ARGS"
cp "$3" "$OSTROM_HOME/observed-settings.json""#,
    );
    let manifest = fixture.state.join("ostrom.yaml");
    fs::write(
        &manifest,
        concat!(
            "manifest_version: 1\n",
            "actors:\n",
            "  builder:\n",
            "    permission_mode: manual\n",
            "operations:\n",
            "  build-pass:\n",
            "    steps:\n",
            "      - uses: agent/claude\n",
            "        with:\n",
            "          prompt: declared by policy, not compiled in\n",
            "grants:\n",
            "  builder-build:\n",
            "    actors: builder\n",
            "    operations: build-pass\n",
            "    repositories: placeholder-org/repo\n",
        ),
    )
    .expect("write operator manifest");
    let trusted_keys = support::sign_manifest(&manifest);

    let arguments = fixture.root.path().join("declared-arguments");
    assert!(
        fixture
            .command()
            .env("OSTROM_POLICY_TRUSTED_KEYS", &trusted_keys)
            .env("OSTROM_TEST_ARGS", &arguments)
            .status()
            .expect("run governed builder pass")
            .success()
    );

    let args = fs::read_to_string(&arguments).expect("read arguments");
    assert!(
        args.contains("declared by policy, not compiled in"),
        "{args}"
    );
    assert!(!args.contains("# Mandate Work"), "{args}");
    assert!(args.contains("--permission-mode\nmanual\n"), "{args}");

    // The fixture writes a hand-maintained roles/builder.settings.json. The
    // grant outranks it: the profile the harness receives is derived from
    // policy, so it cannot drift from the authorization it expresses.
    assert!(
        args.contains("/permission-") && args.contains(".settings.json"),
        "grants should derive the profile, not the hand-written file: {args}"
    );
    let derived = fs::read_to_string(fixture.state.join("observed-settings.json"))
        .expect("read derived role settings");
    let derived: Value = serde_json::from_str(&derived).expect("parse derived settings");
    assert_eq!(derived["env"]["OSTROM_ACTOR"], "builder");
}

#[test]
fn init_produces_a_manifest_whose_edited_prompt_reaches_the_harness() {
    // The point of `ostrom init` is that the prompt stops being a thing only a
    // release can change. It writes what the binary ships as an editable file,
    // and an edit signed into policy is what the next pass runs.
    //
    // Order matters: prompts are materialized into the manifest at signing
    // time, so an edit made after signing is not covered by policy identity
    // and would not take effect. init, edit, sign, run.
    let fixture = Fixture::new("printf '%s\\n' \"$@\" >\"$OSTROM_TEST_ARGS\"");

    let init = Command::new(env!("CARGO_BIN_EXE_ostrom"))
        .arg("init")
        .env_clear()
        .env("OSTROM_HOME", &fixture.state)
        .env("PATH", env::var_os("PATH").unwrap_or_default())
        .status()
        .expect("run ostrom init");
    assert!(init.success());

    let manifest = fixture.state.join("ostrom.yaml");
    let prompt = fixture.state.join("prompts/work.md");
    assert!(manifest.is_file(), "init wrote no manifest");
    assert!(
        fs::read_to_string(&prompt)
            .expect("read shipped prompt")
            .contains("# Mandate Work"),
        "init should write the prompt the binary ships"
    );

    fs::write(
        &prompt,
        "# Edited By The Operator\n\nDo the edited thing.\n",
    )
    .expect("edit the builder prompt");
    let trusted_keys = support::sign_manifest(&manifest);

    let arguments = fixture.root.path().join("init-arguments");
    assert!(
        fixture
            .command()
            .env("OSTROM_POLICY_TRUSTED_KEYS", &trusted_keys)
            .env("OSTROM_TEST_ARGS", &arguments)
            .status()
            .expect("run builder pass")
            .success()
    );

    let args = fs::read_to_string(&arguments).expect("read arguments");
    assert!(args.contains("# Edited By The Operator"), "{args}");
    assert!(!args.contains("# Mandate Work"), "{args}");
    // The actor declares `auto`, which is also the builder's shipped default;
    // asserting it confirms the actor was read rather than merely defaulted.
    assert!(args.contains("--permission-mode\nauto\n"), "{args}");
}

#[test]
fn operator_owned_settings_keep_answer_unsupported_and_are_never_edited() {
    let fixture = Fixture::new("while ! test -f \"$OSTROM_HOME/continue\"; do sleep 0.02; done");
    let settings = fixture.state.join("roles/builder.settings.json");
    let before = fs::read(&settings).unwrap();
    let mut child = fixture
        .command()
        .args(["--control-fd", "0", "--events-fd", "1"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut pipe = child.stdin.take().unwrap();
    let mut control = control_request("answer", "spawning-supervisor");
    control["payload"]["decisionId"] = "not-a-live-permission".into();
    control["payload"]["optionId"] = "allow".into();
    writeln!(pipe, "{control}").unwrap();
    let applied = wait_for_run_event(&fixture, "control.applied");
    assert_eq!(applied["payload"]["ok"], false);
    assert_eq!(applied["payload"]["reason"], "unsupported");
    assert_eq!(applied["payload"]["by"], "spawning-supervisor");
    assert_eq!(fs::read(&settings).unwrap(), before);
    fs::write(fixture.state.join("continue"), "").unwrap();
    assert!(finish_control_pass(child).status.success());
    assert_eq!(fs::read(&settings).unwrap(), before);
}

fn bridge_policy(fixture: &Fixture) -> PathBuf {
    let manifest = fixture.state.join("ostrom.yaml");
    fs::write(&manifest, "manifest_version: 1\nactors: {builder: {permission_mode: manual}}\noperations:\n  build-pass:\n    steps: [{uses: agent/claude, with: {prompt: 'permission transport fixture'}}]\ngrants:\n  builder-build: {actors: builder, operations: build-pass}\n").unwrap();
    support::sign_manifest(&manifest)
}

// A simulated Claude process invokes the actual ostrom hook command from the actual settings.
// This integration fixture is not the real Claude exchange required for the protocol corpus.
const BRIDGE_HARNESS: &str = r#"
python3 - "$@" <<'PY'
import json, os, pathlib, subprocess, sys, time
args = sys.argv[1:]
settings = pathlib.Path(args[args.index('--settings') + 1])
state = pathlib.Path(os.environ['OSTROM_HOME'])
(state / 'settings-path').write_text(str(settings))
profile = json.loads(settings.read_text())
(state / 'bridge-settings.json').write_text(json.dumps(profile))
handler = profile['hooks']['PermissionRequest'][0]['hooks'][0]
request = {'hook_event_name': 'PermissionRequest', 'tool_name': 'Bash', 'tool_input': {'command': 'ostrom build-pass sample'}}
result = subprocess.run(handler['command'], shell=True, input=json.dumps(request), text=True, capture_output=True, timeout=handler['timeout'])
(state / 'hook-output.json').write_text(result.stdout)
print(json.dumps({'type':'result', 'subtype':'success', 'session_id':'bridge-fixture', 'duration_ms':1, 'num_turns':1, 'total_cost_usd':0}))
PY
"#;

#[test]
fn permission_answer_crosses_fd_four_and_releases_the_real_hook_process() {
    let fixture = Fixture::new(BRIDGE_HARNESS);
    let keys = bridge_policy(&fixture);
    let mut command = Command::new("sh");
    command
        .args([
            "-c",
            "exec 4<&0; exec \"$@\"",
            "spawning-supervisor",
            env!("CARGO_BIN_EXE_ostrom"),
            "pass",
            "builder",
            "--control-fd",
            "4",
            "--events-fd",
            "1",
        ])
        .env_clear()
        .env("PATH", env::var_os("PATH").unwrap_or_default())
        .env("OSTROM_HOME", &fixture.state)
        .env("HOME", fixture.root.path())
        .env("CLAUDE_CONFIG_DIR", fixture.root.path())
        .env("CLAUDE_BIN", &fixture.claude)
        .env("OSTROM_POLICY_TRUSTED_KEYS", keys)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let mut pipe = child.stdin.take().unwrap();
    let requested = wait_for_run_event(&fixture, "decision.requested");
    let steer = control_request("steer", "spawning-supervisor");
    writeln!(pipe, "{steer}").unwrap();
    let refused = wait_for_run_event(&fixture, "control.applied");
    assert_eq!(refused["payload"]["reason"], "unsupported");
    assert_eq!(refused["payload"]["by"], "spawning-supervisor");
    let mut control = control_request("answer", "spawning-supervisor");
    control["payload"]["decisionId"] = requested["payload"]["decisionId"].clone();
    control["payload"]["optionId"] = "allow".into();
    writeln!(pipe, "{control}").unwrap();
    let output = finish_control_pass(child);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.stdout,
        fixture.run_event_bytes(),
        "the event descriptor and durable sink must agree"
    );
    let hook: Value =
        serde_json::from_slice(&fs::read(fixture.state.join("hook-output.json")).unwrap()).unwrap();
    assert_eq!(hook["hookSpecificOutput"]["decision"]["behavior"], "allow");
    let events = fixture.run_events();
    let answered = events
        .iter()
        .find(|e| e["type"] == "decision.answered")
        .unwrap();
    assert_eq!(answered["payload"]["requestedRunId"], requested["runId"]);
    assert_eq!(answered["runId"], requested["runId"]);
    assert_eq!(
        answered["payload"]["decisionId"],
        requested["payload"]["decisionId"]
    );
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "control.applied" && e["payload"]["ok"] == true)
    );
    let path = fs::read_to_string(fixture.state.join("settings-path")).unwrap();
    assert!(
        !Path::new(&path).exists(),
        "per-run settings survived normal exit"
    );
    assert!(
        !Path::new(&path).parent().unwrap().exists(),
        "private channel directory survived normal exit"
    );
    assert!(
        !fixture
            .state
            .join("roles/builder.derived.settings.json")
            .exists()
    );
}

#[test]
fn permission_channel_is_removed_on_interrupt_and_harness_failure() {
    for interrupt in [true, false] {
        let fixture = Fixture::new(if interrupt {
            BRIDGE_HARNESS
        } else {
            "printf '%s' \"$3\" >\"$OSTROM_HOME/settings-path\"; exit 19"
        });
        let keys = bridge_policy(&fixture);
        let mut child = fixture
            .command()
            .env("OSTROM_POLICY_TRUSTED_KEYS", keys)
            .args(["--control-fd", "0"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        if interrupt {
            wait_for_run_event(&fixture, "decision.requested");
            writeln!(
                child.stdin.as_mut().unwrap(),
                "{}",
                control_request("interrupt", "spawning-supervisor")
            )
            .unwrap();
        }
        let output = finish_control_pass(child);
        assert!(!output.status.success());
        let settings = fs::read_to_string(fixture.state.join("settings-path")).unwrap();
        assert!(
            !Path::new(&settings).parent().unwrap().exists(),
            "channel survived abnormal run end"
        );
        let events = fixture.run_events();
        assert_eq!(
            events
                .iter()
                .filter(|e| e["type"] == "run.finished")
                .count(),
            1
        );
    }
}

#[test]
fn permission_channel_is_removed_when_harness_cannot_spawn() {
    let fixture = Fixture::new("true");
    let keys = bridge_policy(&fixture);
    fs::write(
        &fixture.claude,
        "#!/missing-permission-harness-interpreter\n",
    )
    .unwrap();
    let output = fixture
        .command()
        .env("OSTROM_POLICY_TRUSTED_KEYS", keys)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(
        fixture.run_events().last().unwrap()["payload"]["outcome"],
        "failed",
        "spawn failure was recorded as success"
    );
    for run in fs::read_dir(fixture.state.join("runs")).unwrap().flatten() {
        assert!(
            !fs::read_dir(run.path())
                .unwrap()
                .flatten()
                .any(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("permission-")),
            "channel survived spawn failure"
        );
    }
}

#[test]
fn permission_cleanup_error_is_a_failed_pass() {
    let fixture = Fixture::new(
        "printf '%s' \"$3\" >\"$OSTROM_HOME/settings-path\"; chmod 500 \"$(dirname \"$(dirname \"$3\")\")\"",
    );
    let keys = bridge_policy(&fixture);
    let output = fixture
        .command()
        .env("OSTROM_POLICY_TRUSTED_KEYS", keys)
        .output()
        .unwrap();
    let settings = fs::read_to_string(fixture.state.join("settings-path")).unwrap();
    let directory = Path::new(&settings).parent().unwrap().parent().unwrap();
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        !output.status.success(),
        "cleanup failure exited successfully"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("could not remove permission channel")
    );
    assert_eq!(
        fixture.run_events().last().unwrap()["payload"]["outcome"],
        "failed",
        "cleanup failure was recorded as success"
    );
}
