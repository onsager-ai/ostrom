//! Private permission transport. Only the pass loop writes ethogram events.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{self, BufRead, Read, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use ethogram::{
    AGENT_WARNING, AgentWarningPayload, ControlAppliedPayload, ControlAppliedReason,
    ControlRequestedPayload, DecisionAnsweredPayload, DecisionDossier, DecisionKind,
    DecisionOption, DecisionRequestedPayload, EventDraft, MAX_EXCERPT_SCALARS, PayloadExtension,
    excerpt,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::{NamedTempFile, TempDir};

use crate::RunEventGuard;

// Tripwire against Claude Code 2.1.265: the step-1 probe's 90 s stalled MCP
// permission call was honoured, so no bound shorter than that was in force.
// A bound above 90 s was never measured. The design depends only on our wait;
// we explicitly set the per-server tool-call timeout in the rendered MCP config.
const WAIT_SECONDS: u64 = 30;
const HANDLER_MARGIN_SECONDS: u64 = 5;
const HANDLER_TIMEOUT_MS: u64 = (WAIT_SECONDS + HANDLER_MARGIN_SECONDS) * 1000;
const MCP_SERVER: &str = "ostrom_permission";
const MCP_TOOL: &str = "approve";
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
    tool_use_id: String,
    expires_at: DateTime<Utc>,
    input: Value,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    tool_use_id: String,
    option: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    tool_use_id: String,
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
    mcp_config: PathBuf,
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

/// Whether this repository's private permission channel form exists for `os`
/// (Linux and macOS today). The pass checks this *before* attempting
/// [`PermissionBridge::create`], so a platform without it takes the pass's
/// pre-#541 fallback -- no bridge, no MCP flags, a live answer refused as
/// unsupported -- rather than failing outright (ostrom#544). Kept as a
/// one-line wrapper over the same pure decision `channel_open_flags` already
/// makes, so the fallback and the bridge itself can never disagree about
/// which platforms are supported.
pub(crate) fn platform_supports_bridge(os: &str) -> bool {
    channel_open_flags(os).is_some()
}

/// The one `agent.warning{stage:"permission-bridge"}` a fallback pass emits,
/// naming the platform so the absence of live answers is observable rather
/// than silent (ostrom#544; this repository's principle 5).
pub(crate) fn platform_fallback_warning(os: &str) -> EventDraft {
    let message = excerpt(
        &format!(
            "private permission channels are unsupported on {os}; the pass \
             proceeds without a bridge, so a live control answer is refused \
             as unsupported"
        ),
        MAX_EXCERPT_SCALARS,
    );
    EventDraft {
        event_type: AGENT_WARNING.to_owned(),
        payload: serde_json::to_value(AgentWarningPayload {
            stage: Some("permission-bridge".to_owned()),
            message: message.text,
            extra: PayloadExtension::new(),
        })
        .expect("agent.warning payload serialises"),
        captured_at: None,
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

fn member(prefix: &str, tool_use_id: &str) -> String {
    // File-safe encoding only; the harness id itself is the decision/control correlator.
    format!("{prefix}-{:x}.json", Sha256::digest(tool_use_id.as_bytes()))
}

fn request_member(sequence: u64) -> String {
    format!("request-{sequence}.json")
}

fn tool_use_id(input: &Value) -> io::Result<&str> {
    input["tool_use_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(|| invalid("permission tool_use_id is absent"))
}

impl PermissionBridge {
    /// Used by both the pass and the real Claude doctor agreement test.
    pub fn create(
        run_directory: &Path,
        run_id: &str,
        derived: &str,
        executable: &Path,
    ) -> io::Result<Self> {
        let mut profile: Value = serde_json::from_str(derived)?;
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
        // Measured against Claude Code 2.1.265: under `defaultMode: "dontAsk"` (the
        // generated profile's unbridged mode), an ungranted call is refused before the
        // permission-prompt tool is ever consulted -- there is nothing for this bridge
        // to receive. Only `"default"` reaches the tool for an ungranted call, which is
        // exactly the case this bridge exists to turn into `decision.requested`. A
        // granted call still never reaches the tool: the rendered allow rule matches it
        // first. This override is the bridged case only; the unbridged profile keeps
        // `dontAsk`, which is correct there because a prompt with nobody to answer it
        // should be denied.
        profile["permissions"]["defaultMode"] = json!("default");
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
        publish(
            directory.path(),
            settings
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| invalid("settings filename"))?,
            &profile,
        )?;
        let mcp_config = directory.path().join("mcp.json");
        publish(
            directory.path(),
            "mcp.json",
            &json!({"mcpServers": {
                MCP_SERVER: {
                    "type": "stdio",
                    "command": executable,
                    "args": ["permission-server", "--channel", channel],
                    "timeout": HANDLER_TIMEOUT_MS
                }
            }}),
        )?;
        Ok(Self {
            directory,
            channel,
            identity,
            settings,
            mcp_config,
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

    #[must_use]
    pub fn mcp_config_path(&self) -> &Path {
        &self.mcp_config
    }

    pub(crate) fn configure(&self, command: &mut std::process::Command) {
        command.arg("--mcp-config").arg(&self.mcp_config).args([
            "--strict-mcp-config",
            "--permission-prompts",
            "host",
            "--permission-prompt-tool",
            &format!("mcp__{MCP_SERVER}__{MCP_TOOL}"),
        ]);
    }

    pub(crate) fn poll(&mut self, events: &RunEventGuard) -> Result<(), crate::RunEventError> {
        if same_channel(&self.channel, &self.identity).is_ok() {
            // Each request is atomically published by one tool call; each reply by the runner.
            // These are transport messages, not a second event sink or bounding pass.
            if let Ok(entries) = fs::read_dir(self.directory.path()) {
                let mut entries: Vec<_> = entries.flatten().collect();
                entries.sort_by_key(|e| {
                    e.file_name().to_str().and_then(|n| {
                        n.strip_prefix("request-")?
                            .strip_suffix(".json")?
                            .parse::<u64>()
                            .ok()
                    })
                });
                for entry in entries {
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
                    if self.processed.contains(&sequence) {
                        continue;
                    }
                    let Ok(request) = read::<Request>(&entry.path()) else {
                        continue;
                    };
                    if request.sequence != sequence
                        || tool_use_id(&request.input).ok() != Some(request.tool_use_id.as_str())
                        || self.pending.contains_key(&request.tool_use_id)
                    {
                        continue;
                    }
                    self.processed.insert(sequence);
                    let id = request.tool_use_id.clone();
                    // Every call that reaches the tool becomes a decision. An
                    // ungranted one is the ordinary case: `defaultMode: "default"`
                    // forwards exactly those, and turning them into a decision the
                    // principal answers is what this bridge is for.
                    //
                    // A *granted* one is the interesting case. Claude auto-allows a
                    // matching allow rule before the tool is consulted (measured,
                    // 2.1.265), so a granted call arriving here means something
                    // outranked the grant -- a cwd or managed ask/deny rule, or a
                    // matcher disagreement. Allowing it would let ostrom out-permit
                    // the harness, overriding a rule the harness applied. That is
                    // precisely when a human should see it, so it escalates like any
                    // other and the dossier says why.
                    let outranked = granted(&self.allow, &request.input);
                    let draft = requested(&id, &request, outranked);
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
            let receipt = read::<Receipt>(&self.directory.path().join(member("receipt", id))).ok();
            let receipt = receipt.filter(|r| {
                same_channel(&self.channel, &self.identity).is_ok()
                    && r.tool_use_id == *id
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
        // by anything but that run's permission server, and no reader other than the spawning supervisor.
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
            tool_use_id: pending.request.tool_use_id.clone(),
            option: input.option_id.clone().expect("validated option"),
        };
        if publish(
            self.directory.path(),
            &member("reply", &reply.tool_use_id),
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
    if input["tool_name"] != "Bash" {
        return false;
    }
    let Some(command) = input["input"]["command"].as_str() else {
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

fn requested(id: &str, request: &Request, outranked_grant: bool) -> EventDraft {
    let question = excerpt(
        &format!(
            "Allow {} with input {}?",
            request.input["tool_name"], request.input["input"]
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
                // Present only when true, and only then meaningful: the actor's own
                // grants permit this call, yet it still reached the tool, so a rule
                // outside ostrom's profile refused it first. A principal answering
                // this decision should know that before choosing.
                extra: if outranked_grant {
                    PayloadExtension::from_iter([("outrankedGrant".to_owned(), true.into())])
                } else {
                    PayloadExtension::new()
                },
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
    } else if timeout {
        // `decision.answered{byTimeout:true}` alone reads exactly like "the
        // supervisor chose not to answer" -- indistinguishable from "an
        // answer was sent and lost" (ostrom#528). No control ever arrived
        // for this decision, so say that plainly, beside the fact above.
        let message = excerpt(
            &format!("decision {id} expired with no control received on the descriptor"),
            MAX_EXCERPT_SCALARS,
        );
        events.append(draft(
            AGENT_WARNING,
            &AgentWarningPayload {
                stage: Some("permission-bridge".to_owned()),
                message: message.text,
                extra: PayloadExtension::new(),
            },
        ))?;
    }
    pending.answered = true;
    Ok(())
}

fn denial(id: &str) -> Value {
    json!({"behavior": "deny", "interrupt": false,
        "message": format!("{id}: no answer within onTimeout")})
}

/// Execute one permission call. Every failure returns an explicit tool-only denial.
#[must_use]
pub fn permission_request(channel: &Path, input: &Value) -> Value {
    handle_permission(channel, input, Duration::from_secs(WAIT_SECONDS))
}

fn handle_permission(channel: &Path, input: &Value, wait: Duration) -> Value {
    let started = Instant::now();
    let id = tool_use_id(input)
        .unwrap_or("invalid-permission-request")
        .to_owned();
    let mut published_sequence = None;
    let result = (|| -> io::Result<Value> {
        tool_use_id(input)?;
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
        // Claim this harness id once; a repeated call must never consume an earlier allow.
        publish(directory, &member("claim", &id), &input)?;
        let mut sequence = 1_u64;
        loop {
            same_channel(channel, &identity)?;
            match publish(
                directory,
                &request_member(sequence),
                &Request {
                    sequence,
                    tool_use_id: id.clone(),
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
            let path = directory.join(member("reply", &id));
            match read::<Reply>(&path) {
                Ok(reply) => {
                    if reply.tool_use_id != id || !matches!(reply.option.as_str(), "allow" | "deny")
                    {
                        return Err(invalid("invalid permission reply"));
                    }
                    if started.elapsed() >= wait {
                        break;
                    }
                    same_channel(channel, &identity)?;
                    publish(
                        directory,
                        &member("receipt", &id),
                        &Receipt {
                            tool_use_id: id.clone(),
                            option: reply.option.clone(),
                            timeout: false,
                        },
                    )?;
                    return Ok(if reply.option == "allow" {
                        json!({"behavior": "allow", "updatedInput": input["input"]})
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
            &member("receipt", &id),
            &Receipt {
                tool_use_id: id.clone(),
                option: "deny".to_owned(),
                timeout: true,
            },
        );
        Ok(denial(&id))
    })();
    result.unwrap_or_else(|_| {
        if published_sequence.is_some() {
            if let Some(directory) = channel.parent() {
                let _ = publish(
                    directory,
                    &member("receipt", &id),
                    &Receipt {
                        tool_use_id: id.clone(),
                        option: "deny".to_owned(),
                        timeout: true,
                    },
                );
            }
        }
        denial(&id)
    })
}

/// Newline-delimited JSON-RPC over stdio. Only this MCP tool is exposed; the pass
/// loop remains the sole event writer. Each tool call waits on its private channel.
pub fn serve_stdio(
    channel: &Path,
    mut input: impl BufRead,
    mut output: impl Write,
) -> io::Result<()> {
    loop {
        let mut bytes = Vec::new();
        let count = input
            .by_ref()
            .take(MAX_TRANSPORT_BYTES + 1)
            .read_until(b'\n', &mut bytes)?;
        if count == 0 {
            return Ok(());
        }
        if count as u64 > MAX_TRANSPORT_BYTES {
            return Err(invalid("MCP request exceeds byte limit"));
        }
        let response = match serde_json::from_slice::<Value>(&bytes) {
            Err(_) => {
                json!({"jsonrpc":"2.0", "id":null, "error":{"code":-32700,"message":"Parse error"}})
            }
            Ok(message) => {
                if message["jsonrpc"] != "2.0" || !message["method"].is_string() {
                    json!({"jsonrpc":"2.0", "id":null, "error":{"code":-32600,"message":"Invalid Request"}})
                } else if let Some(id) = message.get("id") {
                    let result = match message["method"].as_str().unwrap() {
                        "initialize" => Ok(json!({
                            "protocolVersion":"2025-11-25",
                            "capabilities":{"tools":{}},
                            "serverInfo":{"name":MCP_SERVER,"version":env!("CARGO_PKG_VERSION")}
                        })),
                        "tools/list" => Ok(json!({"tools":[{
                            "name":MCP_TOOL, "description":"Answer a pass permission request",
                            "inputSchema":{"type":"object", "properties":{
                                "tool_name":{"type":"string"}, "input":{"type":"object"},
                                "tool_use_id":{"type":"string", "minLength":1}
                            }, "required":["tool_name","input","tool_use_id"]}
                        }]})),
                        "tools/call" if message["params"]["name"] == MCP_TOOL => {
                            let decision =
                                permission_request(channel, &message["params"]["arguments"]);
                            Ok(json!({"content":[{"type":"text","text":decision.to_string()}]}))
                        }
                        "tools/call" => {
                            Err(json!({"code":-32602,"message":"Unknown permission tool"}))
                        }
                        "ping" => Ok(json!({})),
                        _ => Err(json!({"code":-32601,"message":"Method not found"})),
                    };
                    match result {
                        Ok(result) => json!({"jsonrpc":"2.0", "id":id, "result":result}),
                        Err(error) => json!({"jsonrpc":"2.0", "id":id, "error":error}),
                    }
                } else {
                    // Notifications, including initialized/cancelled, have no response.
                    continue;
                }
            }
        };
        serde_json::to_writer(&mut output, &response)?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::{Clock, OstromPaths, RunEventStart};
    use std::os::unix::fs::{PermissionsExt, symlink};
    use umwelt_runtime::{FileSink, Source};

    // Keep short deadline tests independent of each other's filesystem contention.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
                // The default input() command ("ostrom build item") is deliberately
                // outside this allow list, so it is ungranted and drives the
                // decision.requested flow most tests exercise. A dedicated grant
                // ("deploy") lets a separate test exercise the granted, auto-allowed
                // path without contaminating the ungranted default.
                r#"{"permissions":{"defaultMode":"dontAsk","allow":["Bash(ostrom deploy *)"]}}"#,
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
                assert!(
                    Instant::now() < until,
                    "handler never published its request"
                );
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
        fn start_handler(&self, input: Value, wait: Duration) -> thread::JoinHandle<Value> {
            let path = self.bridge.channel.clone();
            thread::spawn(move || handle_permission(&path, &input, wait))
        }
    }

    fn input() -> Value {
        json!({"tool_use_id": "toolu_test", "tool_name": "Bash", "input": {"command": "ostrom build item"}})
    }
    fn assert_denied(output: &Value) {
        let decision = output;
        assert_eq!(decision["behavior"], "deny", "handler must fail closed");
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
    fn expiry_deny_message_is_a_stable_model_visible_contract() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let output = denial("toolu_01FH91gQucc6nx6tynSQiDbW");
        assert_eq!(
            output,
            json!({"behavior":"deny", "interrupt":false,
            "message":"toolu_01FH91gQucc6nx6tynSQiDbW: no answer within onTimeout"})
        );
    }

    #[test]
    fn harness_id_is_the_decision_and_reply_correlator() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let mut f = Fixture::new();
        let handle = f.start_handler(input(), Duration::from_secs(2));
        let id = f.wait_request();
        assert_eq!(id, "toolu_test");
        f.answer(&id, "allow");
        let reply: Reply = read(&f.bridge.directory.path().join(member("reply", &id))).unwrap();
        assert_eq!(reply.tool_use_id, id);
        let output = handle.join().unwrap();
        assert_eq!(output["behavior"], "allow");
        assert_eq!(output["updatedInput"], input()["input"]);
        f.poll();
        assert_eq!(f.wire()[3].payload["decisionId"], id);
    }

    #[test]
    fn repeated_tool_use_id_cannot_reuse_an_allow() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let mut f = Fixture::new();
        let handle = f.start_handler(input(), Duration::from_secs(2));
        let id = f.wait_request();
        f.answer(&id, "allow");
        assert_eq!(handle.join().unwrap()["behavior"], "allow");
        f.poll();
        assert_denied(&handle_permission(
            &f.bridge.channel,
            &input(),
            Duration::from_millis(100),
        ));
        assert!(
            !f.bridge.directory.path().join(request_member(2)).exists(),
            "repeated tool_use_id published another request"
        );
        f.poll();
        assert_eq!(
            f.wire()
                .iter()
                .filter(|e| e.event_type == ethogram::DECISION_REQUESTED)
                .count(),
            1
        );
    }

    #[test]
    fn absent_or_mismatched_tool_use_id_cannot_register_a_request() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let mut f = Fixture::new();
        for id in [Value::Null, json!(""), json!(17)] {
            let mut value = input();
            value["tool_use_id"] = id;
            assert_denied(&handle_permission(
                &f.bridge.channel,
                &value,
                Duration::from_millis(100),
            ));
            assert!(
                !f.bridge.directory.path().join(request_member(1)).exists(),
                "absent tool_use_id was published"
            );
        }
        publish(
            f.bridge.directory.path(),
            &request_member(1),
            &Request {
                sequence: 1,
                tool_use_id: "toolu_other".into(),
                expires_at: crate::Clock::realtime().now() + chrono::Duration::seconds(30),
                input: input(),
            },
        )
        .unwrap();
        f.poll();
        assert!(
            f.bridge.pending.is_empty(),
            "mismatched tool_use_id was registered"
        );
    }

    #[test]
    fn duplicate_transport_request_cannot_replace_pending_decision() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let mut f = Fixture::new();
        let handle = f.start_handler(input(), Duration::from_secs(2));
        let id = f.wait_request();
        f.answer(&id, "allow");
        let mut request: Request =
            read(&f.bridge.directory.path().join(request_member(1))).unwrap();
        request.sequence = 2;
        publish(f.bridge.directory.path(), &request_member(2), &request).unwrap();
        f.poll();
        assert_eq!(
            f.wire()
                .iter()
                .filter(|e| e.event_type == ethogram::DECISION_REQUESTED)
                .count(),
            1
        );
        assert_eq!(handle.join().unwrap()["behavior"], "allow");
    }

    #[test]
    fn wrong_receipt_id_cannot_complete_a_decision() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let mut f = Fixture::new();
        let handle = f.start_handler(input(), Duration::from_secs(2));
        let id = f.wait_request();
        publish(
            f.bridge.directory.path(),
            &member("receipt", &id),
            &Receipt {
                tool_use_id: "toolu_other".into(),
                option: "deny".into(),
                timeout: true,
            },
        )
        .unwrap();
        f.poll();
        assert!(
            !f.bridge.pending[&id].answered,
            "receipt for another tool_use_id was accepted"
        );
        fs::remove_file(&f.bridge.channel).unwrap();
        assert_denied(&handle.join().unwrap());
    }

    #[test]
    fn mcp_protocol_loads_one_tool_and_preserves_the_content_wire() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let f = Fixture::new();
        let mut bytes = Vec::new();
        for message in [
            json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            json!({"jsonrpc":"2.0","id":"list","method":"tools/list"}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"approve","arguments":input()}}),
        ] {
            writeln!(bytes, "{message}").unwrap();
        }
        // Missing channel exercises a real tool response without a runner or a wait.
        fs::remove_file(&f.bridge.channel).unwrap();
        let mut output = Vec::new();
        serve_stdio(&f.bridge.channel, bytes.as_slice(), &mut output).unwrap();
        let replies: Vec<Value> = output
            .split(|b| *b == b'\n')
            .filter(|b| !b.is_empty())
            .map(|b| serde_json::from_slice(b).unwrap())
            .collect();
        assert_eq!(replies.len(), 3, "MCP must not reply to notifications");
        assert_eq!(replies[0]["result"]["protocolVersion"], "2025-11-25");
        assert_eq!(replies[1]["id"], "list");
        assert_eq!(replies[1]["result"]["tools"].as_array().unwrap().len(), 1);
        assert_eq!(replies[1]["result"]["tools"][0]["name"], MCP_TOOL);
        let content = &replies[2]["result"]["content"];
        assert_eq!(content.as_array().unwrap().len(), 1);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(
            serde_json::from_str::<Value>(content[0]["text"].as_str().unwrap()).unwrap(),
            denial("toolu_test")
        );
    }

    #[test]
    fn mcp_rejects_invalid_protocol_methods_and_tools_without_a_channel_write() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let f = Fixture::new();
        for (message, code) in [
            ("not json\n".to_owned(), -32700),
            (json!({"jsonrpc":"1.0","id":3,"method":"tools/list"}).to_string(), -32600),
            (json!({"jsonrpc":"2.0","id":3,"method":5}).to_string(), -32600),
            (json!({"jsonrpc":"2.0","id":3,"method":"unknown"}).to_string(), -32601),
            (json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"other","arguments":input()}}).to_string(), -32602),
        ] {
            let mut output = Vec::new();
            serve_stdio(&f.bridge.channel, message.as_bytes(), &mut output).unwrap();
            let reply: Value = serde_json::from_slice(&output).unwrap();
            assert_eq!(reply["error"]["code"], code, "{message}");
            assert!(!f.bridge.directory.path().join(request_member(1)).exists());
        }
        let mut output = Vec::new();
        let bytes = vec![b' '; MAX_TRANSPORT_BYTES as usize + 1];
        assert!(
            serve_stdio(&f.bridge.channel, bytes.as_slice(), &mut output)
                .unwrap_err()
                .to_string()
                .contains("MCP request exceeds byte limit")
        );
    }

    #[test]
    fn handler_timeout_strictly_exceeds_decision_wait() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let fixture = Fixture::new();
        let mcp: Value = read(fixture.bridge.mcp_config_path()).unwrap();
        let handler = mcp["mcpServers"][MCP_SERVER]["timeout"].as_u64().unwrap();
        let channel: Channel = read(&fixture.bridge.channel).unwrap();
        assert!(
            handler > channel.wait_seconds * 1000,
            "handler timeout must strictly exceed decision wait: handler={handler}ms wait={}s",
            channel.wait_seconds
        );
        assert_eq!(
            handler,
            (channel.wait_seconds + HANDLER_MARGIN_SECONDS) * 1000
        );
    }

    #[test]
    fn valid_answers_are_receipted_on_the_requesting_run() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        for option in ["allow", "deny"] {
            let mut f = Fixture::new();
            let handle = f.start_handler(input(), Duration::from_secs(2));
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
                assert_eq!(output["behavior"], "allow");
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
    fn invalid_answers_never_reach_the_handler() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let mut f = Fixture::new();
        let handle = f.start_handler(input(), Duration::from_secs(2));
        let id = f.wait_request();
        for (id, option, reason) in [
            ("unknown", "allow", "no-such-decision"),
            (id.as_str(), "other", "option-not-offered"),
        ] {
            f.answer(id, option);
            assert_eq!(f.wire().last().unwrap().payload["reason"], reason);
            assert!(
                !f.bridge
                    .directory
                    .path()
                    .join(member("reply", "toolu_test"))
                    .exists(),
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
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let mut f = Fixture::new();
        let handle = f.start_handler(input(), Duration::from_millis(150));
        let id = f.wait_request();
        let began = Instant::now();
        let output = handle.join().unwrap();
        assert_denied(&output);
        assert!(
            began.elapsed() < Duration::from_secs(1),
            "expiry hung the handler"
        );
        assert!(output.to_string().contains(&id));
        f.poll();
        let wire = f.wire();
        let answered = &wire[wire.len() - 2];
        assert_eq!(answered.event_type, ethogram::DECISION_ANSWERED);
        assert_eq!(answered.payload["byTimeout"], true);
        assert_eq!(answered.payload["requestedRunId"], "permission-test");
        assert_eq!(wire[1].payload["onTimeout"], "deny");
        // No control ever arrived for this decision: distinguishable from an
        // answer that was sent and lost (ostrom#528).
        let warning = wire.last().unwrap();
        assert_eq!(warning.event_type, ethogram::AGENT_WARNING);
        assert_eq!(warning.payload["stage"], "permission-bridge");
        assert!(
            warning.payload["message"].as_str().unwrap().contains(&id),
            "message should name the decision: {warning:?}"
        );
    }

    #[test]
    fn expiry_with_no_control_received_warns_distinctly_from_a_lost_answer() {
        // This is the guard for ostrom#528's incident: `decision.answered{byTimeout:true}`
        // alone cannot be told apart from "an answer was sent and lost". A pending
        // decision that never saw a control at all must say so.
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let mut f = Fixture::new();
        let handle = f.start_handler(input(), Duration::from_millis(150));
        let id = f.wait_request();
        assert_denied(&handle.join().unwrap());
        f.poll();
        let wire = f.wire();
        let warnings: Vec<_> = wire
            .iter()
            .filter(|event| event.event_type == ethogram::AGENT_WARNING)
            .collect();
        assert_eq!(warnings.len(), 1, "expected exactly one warning: {wire:?}");
        assert_eq!(warnings[0].payload["stage"], "permission-bridge");
        let message = warnings[0].payload["message"].as_str().unwrap();
        assert!(message.contains(&id), "message should name {id}: {message}");
        assert!(
            message.contains("no control received"),
            "message should state no control ever arrived: {message}"
        );
        for event in &wire {
            ethogram::validate(&event.event_type, &event.payload).unwrap();
        }
    }

    #[test]
    fn missing_channel_denies_promptly() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let f = Fixture::new();
        fs::remove_file(&f.bridge.channel).unwrap();
        let start = Instant::now();
        assert_denied(&permission_request(&f.bridge.channel, &input()));
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn channel_removed_while_waiting_denies_and_refuses_forwarding() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let mut f = Fixture::new();
        let handle = f.start_handler(input(), Duration::from_secs(2));
        let id = f.wait_request();
        fs::remove_file(&f.bridge.channel).unwrap();
        f.answer(&id, "allow");
        assert_eq!(f.wire().last().unwrap().payload["reason"], "not-live");
        assert_denied(&handle.join().unwrap());
        f.poll();
        let wire = f.wire();
        let answered = &wire[wire.len() - 2];
        assert_eq!(answered.event_type, ethogram::DECISION_ANSWERED);
        assert_eq!(answered.payload["byTimeout"], true);
        // The rejected "allow" above never set `pending.control`, so this
        // expiry, too, never received a control.
        assert_eq!(wire.last().unwrap().event_type, ethogram::AGENT_WARNING);
    }

    #[test]
    fn replacement_and_nonprivate_channels_are_rejected() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
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
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let mut f = Fixture::new();
        let handle = f.start_handler(input(), Duration::from_millis(150));
        f.wait_request();
        publish(
            f.bridge.directory.path(),
            &member("reply", "toolu_test"),
            &Reply {
                tool_use_id: "toolu_unknown".into(),
                option: "allow".into(),
            },
        )
        .unwrap();
        assert_denied(&handle.join().unwrap());
        thread::sleep(Duration::from_millis(160));
        f.poll();
        let wire = f.wire();
        assert_eq!(wire[wire.len() - 2].payload["byTimeout"], true);
        assert_eq!(wire.last().unwrap().event_type, ethogram::AGENT_WARNING);
    }

    #[test]
    fn ungranted_tools_and_shell_syntax_raise_a_decision_instead_of_denying_outright() {
        // The gate inverted (ostrom#528): under the bridged profile's
        // `defaultMode: "default"`, a call the rendered allow rule does not cover
        // is exactly what reaches the permission-prompt tool (measured, Claude
        // Code 2.1.265), so it becomes a decision the principal can answer rather
        // than an unattended, immediate denial.
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        for value in [
            json!({"tool_use_id": "toolu_test", "tool_name": "Write", "input": {}}),
            json!({"tool_use_id": "toolu_test", "tool_name": "Bash", "input": {"command": "ostrom other item"}}),
            json!({"tool_use_id": "toolu_test", "tool_name": "Bash", "input": {"command": "ostrom deploy item; rm something"}}),
        ] {
            let mut f = Fixture::new();
            let handle = f.start_handler(value, Duration::from_secs(2));
            let id = f.wait_request();
            assert_eq!(id, "toolu_test");
            f.answer(&id, "deny");
            assert_denied(&handle.join().unwrap());
            f.poll();
            assert_eq!(
                f.wire()
                    .iter()
                    .filter(|e| e.event_type == ethogram::DECISION_REQUESTED)
                    .count(),
                1,
                "ungranted call did not raise exactly one decision"
            );
        }
    }

    #[test]
    fn a_granted_call_reaching_the_tool_escalates_and_is_flagged() {
        // Claude auto-allows a matching allow rule before the tool is consulted, so
        // a granted call arriving here means something outside ostrom's profile
        // refused it first. Allowing it would let ostrom out-permit the harness, so
        // it escalates like any other call and the dossier records the anomaly.
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let mut f = Fixture::new();
        let granted = json!({"tool_use_id": "toolu_test", "tool_name": "Bash", "input": {"command": "ostrom deploy item"}});
        let _handle = f.start_handler(granted, Duration::from_secs(2));
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            f.poll();
            if !f.bridge.pending.is_empty() {
                break;
            }
            assert!(
                Instant::now() < until,
                "granted call reaching the tool was not escalated"
            );
            thread::sleep(POLL);
        }
        let requested = f
            .wire()
            .into_iter()
            .find(|e| e.event_type == ethogram::DECISION_REQUESTED)
            .expect("a granted call that reaches the tool must raise a decision");
        assert_eq!(
            requested.payload["dossier"]["outrankedGrant"], true,
            "the dossier must say the actor's grants permitted this call, so the \
             principal knows something outranked them"
        );
    }

    #[test]
    fn channels_are_private_distinct_and_removed_with_settings() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
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
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
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
        f.bridge.close().unwrap();
        assert!(second.channel.exists());
    }

    #[test]
    fn concurrent_calls_get_distinct_per_channel_sequences() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let mut f = Fixture::new();
        let a = f.start_handler(input(), Duration::from_secs(2));
        let mut second = input();
        second["tool_use_id"] = "toolu_second".into();
        let b = f.start_handler(second, Duration::from_secs(2));
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
            results.iter().filter(|v| v["behavior"] == "allow").count(),
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
    fn mcp_argv_preserves_channel_and_executable_without_shell_expansion() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let f = Fixture::new();
        let mcp: Value = read(f.bridge.mcp_config_path()).unwrap();
        let server = &mcp["mcpServers"][MCP_SERVER];
        assert_eq!(server["command"], "/tmp/ostrom 'quoted' executable");
        assert_eq!(
            server["args"],
            json!(["permission-server", "--channel", f.bridge.channel])
        );
        let settings: Value = read(f.bridge.settings_path()).unwrap();
        assert_eq!(
            settings,
            // `create()` overrides `defaultMode` to `"default"` for the bridged
            // profile: under the generated profile's `"dontAsk"`, an ungranted call
            // is refused before the permission-prompt tool is ever consulted
            // (measured, Claude Code 2.1.265), so this bridge would never receive a
            // request to turn into `decision.requested`.
            json!({"permissions":{"defaultMode":"default","allow":["Bash(ostrom deploy *)"]}})
        );
        assert!(f.root.path().exists());
    }

    #[test]
    fn unsolicited_receipt_cannot_authorize_and_does_not_prevent_expiry() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let mut f = Fixture::new();
        let handle = f.start_handler(input(), Duration::from_millis(100));
        f.wait_request();
        publish(
            f.bridge.directory.path(),
            &member("receipt", "toolu_test"),
            &Receipt {
                tool_use_id: "toolu_test".into(),
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
        let wire = f.wire();
        assert_eq!(
            wire[wire.len() - 2].payload["byTimeout"],
            true,
            "invalid receipt prevented expiry"
        );
        assert_eq!(wire.last().unwrap().event_type, ethogram::AGENT_WARNING);
    }

    #[test]
    fn expired_answers_are_not_forwarded() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let mut f = Fixture::new();
        let handle = f.start_handler(input(), Duration::from_millis(100));
        let id = f.wait_request();
        f.bridge.pending.get_mut(&id).unwrap().request.expires_at =
            crate::Clock::realtime().now() - chrono::Duration::seconds(1);
        f.answer(&id, "allow");
        assert_eq!(f.wire().last().unwrap().payload["reason"], "not-live");
        assert!(
            !f.bridge
                .directory
                .path()
                .join(member("reply", "toolu_test"))
                .exists()
        );
        assert_denied(&handle.join().unwrap());
    }

    #[test]
    fn mismatched_request_sequence_is_never_registered() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let mut f = Fixture::new();
        publish(
            f.bridge.directory.path(),
            &request_member(1),
            &Request {
                sequence: 2,
                tool_use_id: "toolu_test".into(),
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
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
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
            value["input"]["command"] = command.into();
            assert!(
                !granted(&allow, &value),
                "command outside the rendered grant was accepted: {command}"
            );
        }
        assert!(granted(&allow, &input()));
    }

    #[test]
    fn input_excerpt_is_bounded_once_and_validates() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let request = Request {
            sequence: 1,
            tool_use_id: "toolu_test".into(),
            expires_at: crate::Clock::realtime().now(),
            input: json!({"tool_name": "Bash", "input": {"command": "😀".repeat(MAX_EXCERPT_SCALARS + 100)}}),
        };
        let value = requested("bounded", &request, false);
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
        let value = handle_permission(
            &bridge.channel,
            &json!({"tool_use_id":"toolu_test"}),
            Duration::from_millis(20),
        );
        assert!(
            !bridge.directory.path().join(request_member(1)).exists(),
            "changed wait was accepted"
        );
        assert_eq!(value["behavior"], "deny");
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

// Not unix-gated: the platform decision and the warning it emits are pure
// functions of an injected OS string (ostrom#544), so this must not depend on
// the host this test happens to run on -- and must never require Windows.
#[cfg(test)]
mod fallback_decision_tests {
    use super::{AGENT_WARNING, platform_fallback_warning, platform_supports_bridge};

    #[test]
    fn only_linux_and_macos_report_bridge_support() {
        assert!(platform_supports_bridge("linux"));
        assert!(platform_supports_bridge("macos"));
        for os in ["windows", "freebsd", "unsupported", ""] {
            assert!(
                !platform_supports_bridge(os),
                "{os} was wrongly reported as bridge-capable"
            );
        }
    }

    #[test]
    fn fallback_warning_names_the_platform_once() {
        let draft = platform_fallback_warning("windows");
        assert_eq!(draft.event_type, AGENT_WARNING);
        assert_eq!(draft.payload["stage"], "permission-bridge");
        let message = draft.payload["message"].as_str().unwrap();
        assert!(
            message.contains("windows"),
            "warning must name the platform: {message}"
        );
        ethogram::validate(&draft.event_type, &draft.payload).unwrap();
    }
}
