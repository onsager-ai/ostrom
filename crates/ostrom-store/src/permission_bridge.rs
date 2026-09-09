//! Private permission transport. Only the pass loop writes ethogram events.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use ethogram::{
    ControlAppliedPayload, ControlAppliedReason, ControlRequestedPayload, DecisionAnsweredPayload,
    DecisionDossier, DecisionKind, DecisionOption, DecisionRequestedPayload, EventDraft,
    MAX_EXCERPT_SCALARS, PayloadExtension, excerpt,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::{NamedTempFile, TempDir};

use crate::RunEventGuard;

// Claude Code 2.1.265, inspected as embedded JavaScript in the installed binary:
// PermissionRequest awaits the synchronous hook (184985466, 187934000); the
// command timeout is seconds (178974691) and starts before spawn (187965069).
// Cancellation returns before parsing output (187972070). The hook itself must
// emit the decision object before then; decision-level onTimeout is ethogram policy.
const WAIT_SECONDS: u64 = 30;
// Leave time for the hook's own denial to be read before Claude cancels the handler.
const HANDLER_MARGIN_SECONDS: u64 = 5;
const HANDLER_SECONDS: u64 = WAIT_SECONDS + HANDLER_MARGIN_SECONDS;
const POLL: Duration = Duration::from_millis(10);
const MAX_TRANSPORT_BYTES: u64 = 1_048_576;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Channel {
    wait_seconds: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    sequence: u64,
    expires_at: DateTime<Utc>,
    input: Value,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    sequence: u64,
    option: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    sequence: u64,
    option: String,
    timeout: bool,
}

struct Pending {
    request: Request,
    control: Option<ControlRequestedPayload>,
    answered: bool,
}

/// Owns both the private channel and the per-run settings until the pass ends.
pub struct PermissionBridge {
    directory: TempDir,
    channel: PathBuf,
    identity: File,
    settings: PathBuf,
    allow: Vec<String>,
    pending: BTreeMap<String, Pending>,
    processed: BTreeSet<u64>,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn publish(directory: &Path, name: &str, value: &impl Serialize) -> io::Result<()> {
    let mut file = NamedTempFile::new_in(directory)?;
    serde_json::to_writer(&mut file, value)?;
    file.flush()?;
    file.persist_noclobber(directory.join(name))
        .map_err(|e| e.error)?;
    Ok(())
}

fn channel_open_flags(os: &str) -> Option<i32> {
    // Native O_NOFOLLOW | O_NONBLOCK, used through safe std OpenOptionsExt.
    // Source: Linux UAPI asm-generic/fcntl.h and Darwin bsd/sys/fcntl.h.
    // The syscall agreement tests exercise both properties on the running OS.
    match os {
        "linux" => Some(0o404000),
        "macos" => Some(0x104),
        _ => None,
    }
}

fn open_channel_file(path: &Path) -> io::Result<File> {
    let flags = channel_open_flags(std::env::consts::OS).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "private permission channels require Linux or macOS",
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .read(true)
            .custom_flags(flags)
            .open(path)
    }
    #[cfg(not(unix))]
    {
        let _ = (flags, path);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "private permission channels require Unix file permissions",
        ))
    }
}

fn private_file(path: &Path) -> io::Result<File> {
    let before = fs::symlink_metadata(path)?;
    if !before.is_file() {
        return Err(invalid("permission channel member is not a regular file"));
    }
    let file = open_channel_file(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let after = file.metadata()?;
        if before.mode() & 0o777 != 0o600
            || before.nlink() != 1
            || before.dev() != after.dev()
            || before.ino() != after.ino()
        {
            return Err(invalid(
                "permission channel member identity or mode changed",
            ));
        }
    }
    Ok(file)
}

fn read<T: for<'a> Deserialize<'a>>(path: &Path) -> io::Result<T> {
    let mut bytes = Vec::new();
    private_file(path)?
        .take(MAX_TRANSPORT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_TRANSPORT_BYTES {
        return Err(invalid("permission transport exceeds byte limit"));
    }
    serde_json::from_slice(&bytes).map_err(Into::into)
}

fn same_channel(path: &Path, identity: &File) -> io::Result<()> {
    let current = private_file(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let a = identity.metadata()?;
        let b = current.metadata()?;
        if a.dev() != b.dev() || a.ino() != b.ino() {
            return Err(invalid("permission channel was replaced"));
        }
        let parent = fs::symlink_metadata(path.parent().ok_or_else(|| invalid("channel parent"))?)?;
        if !parent.is_dir() || parent.mode() & 0o777 != 0o700 || parent.uid() != a.uid() {
            return Err(invalid(
                "permission channel directory identity or mode changed",
            ));
        }
    }
    Ok(())
}

fn decision_id(channel: &Path, sequence: u64) -> String {
    // Correlate by channel path and sequence without disclosing the channel capability.
    format!(
        "permission-{:x}-{sequence}",
        Sha256::digest(channel.as_os_str().as_encoded_bytes())
    )
}

fn member(prefix: &str, sequence: u64) -> String {
    format!("{prefix}-{sequence}.json")
}

fn shell_quote(value: &Path) -> String {
    format!("'{}'", value.to_string_lossy().replace('\'', "'\\''"))
}

impl PermissionBridge {
    /// Used by both the pass and the real Claude doctor agreement test.
    pub fn create(
        run_directory: &Path,
        run_id: &str,
        derived: &str,
        executable: &Path,
    ) -> io::Result<Self> {
        let profile: Value = serde_json::from_str(derived)?;
        let allow = profile["permissions"]["allow"]
            .as_array()
            .ok_or_else(|| invalid("derived permissions.allow is absent"))?
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| invalid("invalid grant rule"))
            })
            .collect::<io::Result<Vec<_>>>()?;
        // This accepts only the format the existing policy renderer owns. Unknown rules fail closed.
        if allow.iter().any(|rule| operation(rule).is_none()) {
            return Err(invalid("unsupported derived permission rule"));
        }
        let directory = tempfile::Builder::new()
            .prefix("permission-")
            .rand_bytes(24)
            .tempdir_in(run_directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))?;
        }
        let channel = directory.path().join("channel");
        publish(
            directory.path(),
            "channel",
            &Channel {
                wait_seconds: WAIT_SECONDS,
            },
        )?;
        let identity = private_file(&channel)?;
        let settings = directory.path().join(format!(
            "{}.settings.json",
            umwelt_runtime::run_directory_name(run_id)
        ));
        let mut profile = profile;
        profile["hooks"] = json!({"PermissionRequest": [{"hooks": [{
            "type": "command",
            "command": format!("{} hook permission-request --channel {}", shell_quote(executable), shell_quote(&channel)),
            "timeout": HANDLER_SECONDS
        }]}]});
        publish(
            directory.path(),
            settings
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| invalid("settings filename"))?,
            &profile,
        )?;
        Ok(Self {
            directory,
            channel,
            identity,
            settings,
            allow,
            pending: BTreeMap::new(),
            processed: BTreeSet::new(),
        })
    }

    pub(crate) fn close(self) -> io::Result<()> {
        self.directory.close()
    }

    #[must_use]
    pub fn settings_path(&self) -> &Path {
        &self.settings
    }

    pub(crate) fn poll(&mut self, events: &RunEventGuard) -> Result<(), crate::RunEventError> {
        if same_channel(&self.channel, &self.identity).is_ok() {
            // Each request is atomically published by one hook; each reply by the runner.
            // These are transport messages, not a second event sink or bounding pass.
            if let Ok(entries) = fs::read_dir(self.directory.path()) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let Some(sequence) = name
                        .to_str()
                        .and_then(|n| n.strip_prefix("request-"))
                        .and_then(|n| n.strip_suffix(".json"))
                        .and_then(|n| n.parse::<u64>().ok())
                        .filter(|s| *s > 0)
                    else {
                        continue;
                    };
                    let id = decision_id(&self.channel, sequence);
                    if self.processed.contains(&sequence) {
                        continue;
                    }
                    let Ok(request) = read::<Request>(&entry.path()) else {
                        continue;
                    };
                    if request.sequence != sequence {
                        continue;
                    }
                    self.processed.insert(sequence);
                    if !granted(&self.allow, &request.input) {
                        let _ = publish(
                            self.directory.path(),
                            &member("reply", sequence),
                            &Reply {
                                sequence,
                                option: "deny".to_owned(),
                            },
                        );
                        continue;
                    }
                    let draft = requested(&id, &request);
                    events.append(draft)?;
                    self.pending.insert(
                        id,
                        Pending {
                            request,
                            control: None,
                            answered: false,
                        },
                    );
                }
            }
        }
        for (id, pending) in &mut self.pending {
            if pending.answered {
                continue;
            }
            let receipt = read::<Receipt>(
                &self
                    .directory
                    .path()
                    .join(member("receipt", pending.request.sequence)),
            )
            .ok();
            let receipt = receipt.filter(|r| {
                same_channel(&self.channel, &self.identity).is_ok()
                    && r.sequence == pending.request.sequence
                    && (r.option == "allow" || r.option == "deny")
                    && (!r.timeout || r.option == "deny")
            });
            let receipt = receipt.filter(|r| {
                r.timeout
                    || pending
                        .control
                        .as_ref()
                        .is_some_and(|c| c.option_id.as_deref() == Some(&r.option))
            });
            if let Some(receipt) = receipt {
                complete(events, id, pending, &receipt.option, receipt.timeout)?;
            } else if crate::Clock::realtime().now() >= pending.request.expires_at
                || same_channel(&self.channel, &self.identity).is_err()
            {
                complete(events, id, pending, "deny", true)?;
            }
        }
        Ok(())
    }

    pub(crate) fn answer(
        &mut self,
        events: &RunEventGuard,
        input: ControlRequestedPayload,
    ) -> Result<(), crate::RunEventError> {
        // Source: principal, 2026-09-08/09, approving ostrom #528's private-channel form.
        // Preconditions: holds only while the channel is created by the pass runner under the run
        // directory at mode 0600, is removed at run end, has no socket, path or network form reachable
        // by anything but that run's hook, and no reader other than the spawning supervisor.
        // Invalid the moment any of those changes.
        events.append(draft(ethogram::CONTROL_REQUESTED, &input))?;
        let reason = match input
            .decision_id
            .as_ref()
            .and_then(|id| self.pending.get(id))
        {
            None => Some(ControlAppliedReason::NoSuchDecision),
            Some(p) if p.answered || p.control.is_some() => {
                Some(ControlAppliedReason::AlreadyAnswered)
            }
            Some(_) if !matches!(input.option_id.as_deref(), Some("allow" | "deny")) => {
                Some(ControlAppliedReason::OptionNotOffered)
            }
            Some(p)
                if crate::Clock::realtime().now() >= p.request.expires_at
                    || same_channel(&self.channel, &self.identity).is_err() =>
            {
                Some(ControlAppliedReason::NotLive)
            }
            Some(_) => None,
        };
        if let Some(reason) = reason {
            events.append(applied(&input, Some(reason)))?;
            return Ok(());
        }
        let pending = self
            .pending
            .get_mut(input.decision_id.as_ref().expect("validated decision"))
            .expect("validated request");
        let reply = Reply {
            sequence: pending.request.sequence,
            option: input.option_id.clone().expect("validated option"),
        };
        if publish(
            self.directory.path(),
            &member("reply", reply.sequence),
            &reply,
        )
        .is_err()
        {
            events.append(applied(&input, Some(ControlAppliedReason::NotLive)))?;
        } else {
            pending.control = Some(input);
        }
        Ok(())
    }
}

fn operation(rule: &str) -> Option<&str> {
    rule.strip_prefix("Bash(ostrom ")?
        .strip_suffix(" *)")
        .filter(|s| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
}

fn granted(allow: &[String], input: &Value) -> bool {
    if input["hook_event_name"] != "PermissionRequest" || input["tool_name"] != "Bash" {
        return false;
    }
    let Some(command) = input["tool_input"]["command"].as_str() else {
        return false;
    };
    // A conservative subset of Bash's grant pattern. Shell syntax is never interpreted here.
    if !command
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b" -_./:=,@%+".contains(&b))
    {
        return false;
    }
    allow.iter().filter_map(|rule| operation(rule)).any(|op| {
        command
            .strip_prefix(&format!("ostrom {op} "))
            .is_some_and(|args| !args.trim().is_empty())
    })
}

fn requested(id: &str, request: &Request) -> EventDraft {
    let question = excerpt(
        &format!(
            "Allow {} with input {}?",
            request.input["tool_name"], request.input["tool_input"]
        ),
        MAX_EXCERPT_SCALARS,
    );
    draft(
        ethogram::DECISION_REQUESTED,
        &DecisionRequestedPayload {
            decision_id: id.to_owned(),
            kind: DecisionKind::Permission,
            dossier: DecisionDossier {
                question: question.text,
                options_ruled_out: vec![],
                recommended_action: "deny".to_owned(),
                blast_radius: "This tool call only".to_owned(),
                truncated: Some(question.truncated),
                extra: PayloadExtension::new(),
            },
            options: vec![
                DecisionOption {
                    id: "allow".to_owned(),
                    label: "Allow this call".to_owned(),
                    extra: PayloadExtension::new(),
                },
                DecisionOption {
                    id: "deny".to_owned(),
                    label: "Deny this call".to_owned(),
                    extra: PayloadExtension::new(),
                },
            ],
            subject: Some("Bash".to_owned()),
            expires_at: Some(request.expires_at.to_rfc3339()),
            on_timeout: Some("deny".to_owned()),
            extra: PayloadExtension::new(),
        },
    )
}

fn draft(kind: &str, value: &impl Serialize) -> EventDraft {
    EventDraft {
        event_type: kind.to_owned(),
        payload: serde_json::to_value(value).expect("permission payload serializes"),
        captured_at: None,
    }
}

fn applied(input: &ControlRequestedPayload, reason: Option<ControlAppliedReason>) -> EventDraft {
    draft(
        ethogram::CONTROL_APPLIED,
        &ControlAppliedPayload {
            control_id: input.control_id.clone(),
            ok: reason.is_none(),
            reason,
            truncated: None,
            landed_in: None,
            extra: PayloadExtension::from_iter([("by".to_owned(), input.by.clone().into())]),
        },
    )
}

fn complete(
    events: &RunEventGuard,
    id: &str,
    pending: &mut Pending,
    option: &str,
    timeout: bool,
) -> Result<(), crate::RunEventError> {
    events.append(draft(
        ethogram::DECISION_ANSWERED,
        &DecisionAnsweredPayload {
            decision_id: id.to_owned(),
            option_id: option.to_owned(),
            by: pending.control.as_ref().filter(|_| !timeout).map_or_else(
                || "principal:runtime:permission-timeout".to_owned(),
                |c| c.by.clone(),
            ),
            by_timeout: Some(timeout),
            reversal: None,
            requested_run_id: Some(events.run_id().to_owned()),
            extra: PayloadExtension::new(),
        },
    ))?;
    if let Some(control) = &pending.control {
        events.append(applied(
            control,
            timeout.then_some(ControlAppliedReason::NotLive),
        ))?;
    }
    pending.answered = true;
    Ok(())
}

fn denial(id: &str) -> Value {
    json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": {
        "behavior": "deny", "interrupt": false, "message": format!("{id}: no answer within onTimeout")
    }}})
}

/// Claude closes hook stdin after writing the request. Bound transport allocation;
/// event narration is excerpted only when the runner constructs its event draft.
pub fn permission_request_from_reader(channel: &Path, reader: impl Read) -> Value {
    let mut bytes = Vec::new();
    if reader
        .take(MAX_TRANSPORT_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() as u64 > MAX_TRANSPORT_BYTES
    {
        return denial(&decision_id(channel, 0));
    }
    let input = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    permission_request(channel, &input)
}

/// Execute the synchronous hook. Every failure returns an explicit tool-only denial.
#[must_use]
pub fn permission_request(channel: &Path, input: &Value) -> Value {
    hook(channel, input, Duration::from_secs(WAIT_SECONDS))
}

fn hook(channel: &Path, input: &Value, wait: Duration) -> Value {
    let started = Instant::now();
    let mut id = decision_id(channel, 0);
    let mut published_sequence = None;
    let result = (|| -> io::Result<Value> {
        let identity = private_file(channel)?;
        same_channel(channel, &identity)?;
        let config: Channel = read(channel)?;
        if config.wait_seconds != WAIT_SECONDS {
            return Err(invalid("permission wait changed"));
        }
        let directory = channel
            .parent()
            .ok_or_else(|| invalid("channel directory absent"))?;
        let expires_at = crate::Clock::realtime().now()
            + chrono::Duration::from_std(wait).map_err(io::Error::other)?;
        let mut sequence = 1_u64;
        loop {
            same_channel(channel, &identity)?;
            id = decision_id(channel, sequence);
            match publish(
                directory,
                &member("request", sequence),
                &Request {
                    sequence,
                    expires_at,
                    input: input.clone(),
                },
            ) {
                Ok(()) => {
                    published_sequence = Some(sequence);
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && started.elapsed() < wait => {
                    sequence = sequence
                        .checked_add(1)
                        .ok_or_else(|| invalid("permission sequence exhausted"))?
                }
                Err(e) => return Err(e),
            }
        }
        loop {
            if started.elapsed() >= wait {
                break;
            }
            same_channel(channel, &identity)?;
            let path = directory.join(member("reply", sequence));
            match read::<Reply>(&path) {
                Ok(reply) => {
                    if reply.sequence != sequence
                        || !matches!(reply.option.as_str(), "allow" | "deny")
                    {
                        return Err(invalid("invalid permission reply"));
                    }
                    if started.elapsed() >= wait {
                        break;
                    }
                    same_channel(channel, &identity)?;
                    publish(
                        directory,
                        &member("receipt", sequence),
                        &Receipt {
                            sequence,
                            option: reply.option.clone(),
                            timeout: false,
                        },
                    )?;
                    return Ok(if reply.option == "allow" {
                        json!({"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": {"behavior": "allow"}}})
                    } else {
                        denial(&id)
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            thread::sleep(POLL.min(wait.saturating_sub(started.elapsed())));
        }
        let _ = publish(
            directory,
            &member("receipt", sequence),
            &Receipt {
                sequence,
                option: "deny".to_owned(),
                timeout: true,
            },
        );
        Ok(denial(&id))
    })();
    result.unwrap_or_else(|_| {
        if let Some(sequence) = published_sequence {
            if let Some(directory) = channel.parent() {
                let _ = publish(
                    directory,
                    &member("receipt", sequence),
                    &Receipt {
                        sequence,
                        option: "deny".to_owned(),
                        timeout: true,
                    },
                );
            }
        }
        denial(&id)
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::{Clock, OstromPaths, RunEventStart};
    use std::os::unix::fs::{PermissionsExt, symlink};
    use umwelt_runtime::{FileSink, Source};

    struct Fixture {
        root: TempDir,
        paths: OstromPaths,
        events: RunEventGuard,
        bridge: PermissionBridge,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let paths = OstromPaths {
                config: root.path().into(),
                state: root.path().into(),
            };
            let events = RunEventGuard::start(
                &paths,
                None,
                false,
                Clock::realtime(),
                RunEventStart {
                    run_id: "permission-test".into(),
                    kind: ethogram::RunKind::Loop,
                    actor: "builder".into(),
                    harness: "claude".into(),
                    model: None,
                    schedule: None,
                    repository: None,
                    work_order: None,
                    ceilings: None,
                },
            )
            .unwrap();
            let bridge = PermissionBridge::create(
                &paths.runs_dir().join("permission-test"),
                "permission-test",
                r#"{"permissions":{"defaultMode":"dontAsk","allow":["Bash(ostrom build *)"]}}"#,
                Path::new("/tmp/ostrom 'quoted' executable"),
            )
            .unwrap();
            Self {
                root,
                paths,
                events,
                bridge,
            }
        }
        fn poll(&mut self) {
            self.bridge.poll(&self.events).unwrap();
        }
        fn wait_request(&mut self) -> String {
            let until = Instant::now() + Duration::from_secs(2);
            loop {
                self.poll();
                if let Some(id) = self.bridge.pending.keys().next() {
                    return id.clone();
                }
                assert!(Instant::now() < until, "hook never published its request");
                thread::sleep(POLL);
            }
        }
        fn wire(&self) -> Vec<ethogram::Event> {
            FileSink::new(self.paths.runs_dir())
                .read_from("permission-test", 0)
                .unwrap()
        }
        fn answer(&mut self, id: &str, option: &str) {
            let input: ControlRequestedPayload = serde_json::from_value(json!({"controlId": format!("control-{option}"), "kind": "answer", "decisionId": id, "optionId": option, "by": "spawning-supervisor"})).unwrap();
            self.bridge.answer(&self.events, input).unwrap();
        }
        fn start_hook(&self, input: Value, wait: Duration) -> thread::JoinHandle<Value> {
            let path = self.bridge.channel.clone();
            thread::spawn(move || hook(&path, &input, wait))
        }
    }

    fn input() -> Value {
        json!({"hook_event_name": "PermissionRequest", "tool_name": "Bash", "tool_input": {"command": "ostrom build item"}})
    }
    fn assert_denied(output: &Value) {
        let decision = &output["hookSpecificOutput"]["decision"];
        assert_eq!(
            output["hookSpecificOutput"]["hookEventName"],
            "PermissionRequest"
        );
        assert_eq!(decision["behavior"], "deny", "hook must fail closed");
        assert_eq!(
            decision["interrupt"], false,
            "denial must not interrupt the run"
        );
        assert!(
            decision["message"]
                .as_str()
                .unwrap()
                .contains(": no answer within onTimeout")
        );
    }

    #[test]
    fn handler_timeout_strictly_exceeds_decision_wait() {
        let fixture = Fixture::new();
        let settings: Value = read(fixture.bridge.settings_path()).unwrap();
        let handler = settings["hooks"]["PermissionRequest"][0]["hooks"][0]["timeout"]
            .as_u64()
            .unwrap();
        let channel: Channel = read(&fixture.bridge.channel).unwrap();
        assert!(
            handler > channel.wait_seconds,
            "handler timeout must strictly exceed decision wait: handler={handler}s wait={}s",
            channel.wait_seconds
        );
        assert_eq!(handler, channel.wait_seconds + HANDLER_MARGIN_SECONDS);
    }

    #[test]
    fn valid_answers_are_receipted_on_the_requesting_run() {
        for option in ["allow", "deny"] {
            let mut f = Fixture::new();
            let handle = f.start_hook(input(), Duration::from_secs(2));
            let id = f.wait_request();
            f.answer(&id, option);
            assert!(
                !f.wire()
                    .iter()
                    .any(|e| e.event_type == ethogram::CONTROL_APPLIED),
                "forward alone must not claim delivery"
            );
            let output = handle.join().unwrap();
            if option == "deny" {
                assert_denied(&output);
            } else {
                assert_eq!(
                    output["hookSpecificOutput"]["decision"]["behavior"],
                    "allow"
                );
            }
            f.poll();
            let wire = f.wire();
            assert_eq!(
                wire.iter()
                    .map(|e| e.event_type.as_str())
                    .collect::<Vec<_>>(),
                [
                    "run.started",
                    "decision.requested",
                    "control.requested",
                    "decision.answered",
                    "control.applied"
                ]
            );
            assert_eq!(wire[3].payload["requestedRunId"], wire[1].run_id);
            assert_eq!(wire[3].payload["byTimeout"], false);
            assert_eq!(wire[4].payload["ok"], true);
            assert_eq!(wire[4].payload["by"], "spawning-supervisor");
            for event in wire {
                ethogram::validate(&event.event_type, &event.payload).unwrap();
            }
        }
    }

    #[test]
    fn invalid_answers_never_reach_the_hook() {
        let mut f = Fixture::new();
        let handle = f.start_hook(input(), Duration::from_secs(2));
        let id = f.wait_request();
        for (id, option, reason) in [
            ("unknown", "allow", "no-such-decision"),
            (id.as_str(), "other", "option-not-offered"),
        ] {
            f.answer(id, option);
            assert_eq!(f.wire().last().unwrap().payload["reason"], reason);
            assert!(
                !f.bridge.directory.path().join(member("reply", 1)).exists(),
                "invalid answer was forwarded"
            );
            assert!(!handle.is_finished());
        }
        f.answer(&id, "allow");
        f.answer(&id, "deny");
        assert_eq!(
            f.wire().last().unwrap().payload["reason"],
            "already-answered"
        );
        handle.join().unwrap();
        f.poll();
        f.answer(&id, "deny");
        assert_eq!(
            f.wire().last().unwrap().payload["reason"],
            "already-answered"
        );
    }

    #[test]
    fn expiry_denies_and_records_timeout_without_a_supervisor() {
        let mut f = Fixture::new();
        let handle = f.start_hook(input(), Duration::from_millis(150));
        let id = f.wait_request();
        let began = Instant::now();
        let output = handle.join().unwrap();
        assert_denied(&output);
        assert!(
            began.elapsed() < Duration::from_secs(1),
            "expiry hung the hook"
        );
        assert!(output.to_string().contains(&id));
        f.poll();
        let wire = f.wire();
        assert_eq!(wire.last().unwrap().event_type, ethogram::DECISION_ANSWERED);
        assert_eq!(wire.last().unwrap().payload["byTimeout"], true);
        assert_eq!(
            wire.last().unwrap().payload["requestedRunId"],
            "permission-test"
        );
        assert_eq!(wire[1].payload["onTimeout"], "deny");
    }

    #[test]
    fn missing_channel_denies_promptly() {
        let f = Fixture::new();
        fs::remove_file(&f.bridge.channel).unwrap();
        let start = Instant::now();
        assert_denied(&permission_request(&f.bridge.channel, &input()));
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn channel_removed_while_waiting_denies_and_refuses_forwarding() {
        let mut f = Fixture::new();
        let handle = f.start_hook(input(), Duration::from_secs(2));
        let id = f.wait_request();
        fs::remove_file(&f.bridge.channel).unwrap();
        f.answer(&id, "allow");
        assert_eq!(f.wire().last().unwrap().payload["reason"], "not-live");
        assert_denied(&handle.join().unwrap());
        f.poll();
        assert_eq!(f.wire().last().unwrap().payload["byTimeout"], true);
    }

    #[test]
    fn replacement_and_nonprivate_channels_are_rejected() {
        let f = Fixture::new();
        let original = f.bridge.channel.with_extension("original");
        fs::rename(&f.bridge.channel, &original).unwrap();
        publish(
            f.bridge.directory.path(),
            "channel",
            &Channel {
                wait_seconds: WAIT_SECONDS,
            },
        )
        .unwrap();
        assert!(
            same_channel(&f.bridge.channel, &f.bridge.identity).is_err(),
            "replacement channel accepted"
        );
        fs::set_permissions(&f.bridge.channel, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            private_file(&f.bridge.channel).is_err(),
            "public channel file accepted"
        );
        assert_denied(&permission_request(&f.bridge.channel, &input()));
        fs::remove_file(&f.bridge.channel).unwrap();
        symlink(&original, &f.bridge.channel).unwrap();
        assert_denied(&permission_request(&f.bridge.channel, &input()));
        assert!(
            private_file(&f.bridge.channel).is_err(),
            "symlink channel accepted"
        );
    }

    #[test]
    fn malformed_reply_is_never_an_allow() {
        let mut f = Fixture::new();
        let handle = f.start_hook(input(), Duration::from_millis(150));
        f.wait_request();
        publish(
            f.bridge.directory.path(),
            &member("reply", 1),
            &Reply {
                sequence: 99,
                option: "allow".into(),
            },
        )
        .unwrap();
        assert_denied(&handle.join().unwrap());
        thread::sleep(Duration::from_millis(160));
        f.poll();
        assert_eq!(f.wire().last().unwrap().payload["byTimeout"], true);
    }

    #[test]
    fn ungranted_tools_and_shell_syntax_are_denied_without_interrupting() {
        for value in [
            json!({"hook_event_name": "PermissionRequest", "tool_name": "Write", "tool_input": {}}),
            json!({"hook_event_name": "PermissionRequest", "tool_name": "Bash", "tool_input": {"command": "ostrom other item"}}),
            json!({"hook_event_name": "PermissionRequest", "tool_name": "Bash", "tool_input": {"command": "ostrom build item; rm something"}}),
        ] {
            let mut f = Fixture::new();
            let handle = f.start_hook(value, Duration::from_secs(2));
            let until = Instant::now() + Duration::from_secs(1);
            while !handle.is_finished() {
                f.poll();
                assert!(
                    Instant::now() < until,
                    "ungranted call was not promptly denied"
                );
                thread::sleep(POLL);
            }
            assert_denied(&handle.join().unwrap());
            assert!(
                !f.wire()
                    .iter()
                    .any(|e| e.event_type == ethogram::DECISION_REQUESTED)
            );
        }
    }

    #[test]
    fn channels_are_private_distinct_and_removed_with_settings() {
        let f = Fixture::new();
        let other = Fixture::new();
        assert_ne!(f.bridge.channel, other.bridge.channel);
        assert_eq!(
            fs::metadata(&f.bridge.channel)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let channel = f.bridge.channel.clone();
        let settings = f.bridge.settings.clone();
        f.bridge.close().unwrap();
        assert!(!channel.exists(), "channel leaked after run");
        assert!(!settings.exists(), "per-run settings leaked after run");
        assert!(other.bridge.channel.exists());
    }

    #[test]
    fn same_run_directory_never_shares_settings_or_channels() {
        let f = Fixture::new();
        let first_bytes = fs::read(f.bridge.settings_path()).unwrap();
        let second = PermissionBridge::create(
            &f.paths.runs_dir().join("permission-test"),
            "permission-test",
            r#"{"permissions":{"allow":["Bash(ostrom build *)"]}}"#,
            Path::new("ostrom"),
        )
        .unwrap();
        assert_ne!(f.bridge.channel, second.channel);
        assert_ne!(f.bridge.settings_path(), second.settings_path());
        assert_eq!(fs::read(f.bridge.settings_path()).unwrap(), first_bytes);
        assert_ne!(
            decision_id(&f.bridge.channel, 1),
            decision_id(&second.channel, 1)
        );
        f.bridge.close().unwrap();
        assert!(second.channel.exists());
    }

    #[test]
    fn concurrent_hooks_get_distinct_per_channel_sequences() {
        let mut f = Fixture::new();
        let a = f.start_hook(input(), Duration::from_secs(2));
        let b = f.start_hook(input(), Duration::from_secs(2));
        let until = Instant::now() + Duration::from_secs(1);
        while f.bridge.pending.len() != 2 {
            f.poll();
            assert!(Instant::now() < until);
            thread::sleep(POLL);
        }
        let ids: Vec<_> = f.bridge.pending.keys().cloned().collect();
        assert_ne!(ids[0], ids[1]);
        f.answer(&ids[0], "allow");
        f.answer(&ids[1], "deny");
        let results = [a.join().unwrap(), b.join().unwrap()];
        assert_eq!(
            results
                .iter()
                .filter(|v| v["hookSpecificOutput"]["decision"]["behavior"] == "allow")
                .count(),
            1
        );
        f.poll();
        assert_eq!(
            f.wire()
                .iter()
                .filter(|e| e.event_type == ethogram::DECISION_ANSWERED)
                .count(),
            2
        );
    }

    #[test]
    fn command_quotes_channel_and_executable_without_shell_expansion() {
        let f = Fixture::new();
        let settings: Value = read(f.bridge.settings_path()).unwrap();
        let command = settings["hooks"]["PermissionRequest"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(
            command.contains("'\\''quoted'\\''"),
            "executable quotes were lost"
        );
        assert!(command.ends_with(&shell_quote(&f.bridge.channel)));
        assert!(f.root.path().exists());
    }

    #[test]
    fn unsolicited_receipt_cannot_authorize_and_does_not_prevent_expiry() {
        let mut f = Fixture::new();
        let handle = f.start_hook(input(), Duration::from_millis(100));
        f.wait_request();
        publish(
            f.bridge.directory.path(),
            &member("receipt", 1),
            &Receipt {
                sequence: 1,
                option: "allow".into(),
                timeout: false,
            },
        )
        .unwrap();
        f.poll();
        assert!(
            !f.wire()
                .iter()
                .any(|e| e.event_type == ethogram::DECISION_ANSWERED),
            "unsolicited receipt authorized a tool"
        );
        assert_denied(&handle.join().unwrap());
        f.poll();
        assert_eq!(
            f.wire().last().unwrap().payload["byTimeout"],
            true,
            "invalid receipt prevented expiry"
        );
    }

    #[test]
    fn expired_answers_are_not_forwarded() {
        let mut f = Fixture::new();
        let handle = f.start_hook(input(), Duration::from_millis(100));
        let id = f.wait_request();
        f.bridge.pending.get_mut(&id).unwrap().request.expires_at =
            crate::Clock::realtime().now() - chrono::Duration::seconds(1);
        f.answer(&id, "allow");
        assert_eq!(f.wire().last().unwrap().payload["reason"], "not-live");
        assert!(!f.bridge.directory.path().join(member("reply", 1)).exists());
        assert_denied(&handle.join().unwrap());
    }

    #[test]
    fn mismatched_request_sequence_is_never_registered() {
        let mut f = Fixture::new();
        publish(
            f.bridge.directory.path(),
            &member("request", 1),
            &Request {
                sequence: 2,
                expires_at: crate::Clock::realtime().now() + chrono::Duration::seconds(30),
                input: input(),
            },
        )
        .unwrap();
        f.poll();
        assert!(
            f.bridge.pending.is_empty(),
            "mismatched request sequence accepted"
        );
        assert!(
            !f.wire()
                .iter()
                .any(|e| e.event_type == ethogram::DECISION_REQUESTED)
        );
    }

    #[test]
    fn grant_matching_never_broadens_the_rendered_literal_prefix() {
        let allow = vec!["Bash(ostrom build *)".to_owned()];
        for command in [
            " ostrom build item",
            "ostrom  build item",
            "ostrom build",
            "ostrom builder item",
            "ostrom build item\nother",
            "ostrom build $(other)",
            "ostrom build 'item'",
        ] {
            let mut value = input();
            value["tool_input"]["command"] = command.into();
            assert!(
                !granted(&allow, &value),
                "command outside the rendered grant was accepted: {command}"
            );
        }
        assert!(granted(&allow, &input()));
    }

    #[test]
    fn input_excerpt_is_bounded_once_and_validates() {
        let request = Request {
            sequence: 1,
            expires_at: crate::Clock::realtime().now(),
            input: json!({"tool_name": "Bash", "tool_input": {"command": "😀".repeat(MAX_EXCERPT_SCALARS + 100)}}),
        };
        let value = requested("bounded", &request);
        ethogram::validate(&value.event_type, &value.payload).unwrap();
        assert_eq!(value.payload["dossier"]["truncated"], true);
        assert_eq!(
            value.payload["dossier"]["question"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            MAX_EXCERPT_SCALARS
        );
    }
}

#[cfg(all(test, unix))]
mod boundary_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn unknown_grant_format_refuses_bridge_creation() {
        let root = tempfile::tempdir().unwrap();
        for profile in [r#"{"permissions":{"allow":["Bash(*)"]}}"#, r#"{}"#] {
            assert!(
                PermissionBridge::create(root.path(), "run", profile, Path::new("ostrom")).is_err(),
                "unrecognized grant was accepted"
            );
        }
    }

    #[test]
    fn changed_wait_and_public_directory_deny() {
        let root = tempfile::tempdir().unwrap();
        let bridge = PermissionBridge::create(
            root.path(),
            "run",
            r#"{"permissions":{"allow":[]}}"#,
            Path::new("ostrom"),
        )
        .unwrap();
        fs::write(&bridge.channel, r#"{"wait_seconds":35}"#).unwrap();
        let value = hook(&bridge.channel, &json!({}), Duration::from_millis(20));
        assert!(
            !bridge.directory.path().join(member("request", 1)).exists(),
            "changed wait was accepted"
        );
        assert_eq!(value["hookSpecificOutput"]["decision"]["behavior"], "deny");
        fs::write(&bridge.channel, r#"{"wait_seconds":30}"#).unwrap();
        fs::set_permissions(bridge.directory.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            same_channel(&bridge.channel, &bridge.identity).is_err(),
            "public channel directory accepted"
        );
    }

    #[test]
    fn oversized_transport_and_linked_files_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        let bridge = PermissionBridge::create(
            root.path(),
            "run",
            r#"{"permissions":{"allow":[]}}"#,
            Path::new("ostrom"),
        )
        .unwrap();
        let bytes = vec![b' '; MAX_TRANSPORT_BYTES as usize + 1];
        let mut file = NamedTempFile::new_in(bridge.directory.path()).unwrap();
        file.write_all(&bytes).unwrap();
        assert!(
            read::<Value>(file.path())
                .unwrap_err()
                .to_string()
                .contains("byte limit")
        );
        let output = permission_request_from_reader(&bridge.channel, bytes.as_slice());
        assert_eq!(output["hookSpecificOutput"]["decision"]["behavior"], "deny");
        fs::hard_link(&bridge.channel, bridge.directory.path().join("linked")).unwrap();
        assert!(
            private_file(&bridge.channel).is_err(),
            "multiply linked channel accepted"
        );
    }
}

#[cfg(all(test, unix))]
mod native_open_tests {
    use super::*;
    use std::{os::unix::fs::symlink, process::Command, sync::mpsc};

    #[test]
    fn native_open_refuses_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let file = NamedTempFile::new_in(root.path()).unwrap();
        let link = root.path().join("link");
        symlink(file.path(), &link).unwrap();
        assert!(
            open_channel_file(&link).is_err(),
            "native channel open followed a symlink"
        );
    }

    #[test]
    fn native_open_does_not_block_on_a_fifo() {
        let root = tempfile::tempdir().unwrap();
        let fifo = root.path().join("fifo");
        assert!(
            Command::new("mkfifo")
                .arg("-m")
                .arg("600")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let path = fifo.clone();
        let (send, receive) = mpsc::channel();
        let worker = thread::spawn(move || {
            let _ = send.send(open_channel_file(&path));
        });
        let result = receive.recv_timeout(Duration::from_secs(1));
        let blocked = matches!(result, Err(mpsc::RecvTimeoutError::Timeout));
        if blocked {
            // Release the deliberately broken blocking-open variant before asserting.
            let _writer = fs::OpenOptions::new().write(true).open(&fifo).unwrap();
            let _ = receive.recv_timeout(Duration::from_secs(1));
        }
        worker.join().unwrap();
        assert!(!blocked, "native channel open blocked on a FIFO");
        assert!(result.is_ok());
        assert!(
            private_file(&fifo).is_err(),
            "FIFO was accepted as a regular channel"
        );
    }

    #[test]
    fn unsupported_platform_has_no_fallback_open_flags() {
        assert!(
            channel_open_flags("unsupported").is_none(),
            "unsupported platform silently weakened channel opening"
        );
    }
}
