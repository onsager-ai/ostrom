//! The fail-closed sandbox canary for the Claude implementer (#626).
//!
//! Claude Code runs normally, exit 0, with a settings file it cannot parse or
//! validate, and its stream does not say whether the sandbox is on. So before
//! the first Claude implementer run for a given (`claude --version`, profile
//! sha256), ostrom runs one short session with the exact profile and flags the
//! implementer uses, whose one command tries to write outside its working
//! directory and to reach the network. It passes only when the command ran in
//! its working directory, the outside write did not happen, and the command's
//! own output shows the network denied. Anything else (a network success, an
//! error, a stop at its bounds, an unreadable result) marks the Claude runner
//! unavailable with `reason: sandbox-unverified`, so routing moves on and
//! Claude never runs unsandboxed.
//!
//! A pass is cached in `<state>/sandbox-canary.json`, private state:
//!
//! ```json
//! {"schema_version":1,"runners":{"agent/claude":{"version":"2.1.283 (Claude Code)",
//!   "profile_sha256":"<64 hex>","checked_at":"2026-09-28T09:00:00Z","run_id":"..."}}}
//! ```
//!
//! Every canary run records a `sandbox-checked` trace fact with its cost, which
//! counts toward the daily spend cap like any run.

use std::{collections::BTreeMap, fs, path::Path};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::{
    AgentRegistry, Clock, ImplementerRunRequest, OstromPaths, RunOutcome, RunRequest, SignalFlags,
    TraceAppend, append_trace, generated_run_id,
    implement::{CLAUDE_RUNNER, WallCap},
    runner_availability::mark_unavailable,
};

pub const SANDBOX_CANARY_FILE: &str = "sandbox-canary.json";
pub const SANDBOX_UNVERIFIED_REASON: &str = "sandbox-unverified";
/// The canary could not tell: the unsandboxed control did not reach the host,
/// so a denied sandboxed attempt proves nothing. Nothing is cached.
pub const SANDBOX_INCONCLUSIVE_REASON: &str = "sandbox-inconclusive";

/// The host both the canary and its unsandboxed control try to reach.
const CANARY_HOST: &str = "example.com";

/// The proxy variables curl honours, read from the environment the control
/// and the canary both inherit, and recorded with credentials redacted.
const PROXY_VARIABLES: [crate::environment::EnvironmentVariable; 8] = [
    crate::environment::ALL_PROXY,
    crate::environment::HTTPS_PROXY,
    crate::environment::HTTP_PROXY,
    crate::environment::NO_PROXY,
    crate::environment::ALL_PROXY_LOWERCASE,
    crate::environment::HTTPS_PROXY_LOWERCASE,
    crate::environment::HTTP_PROXY_LOWERCASE,
    crate::environment::NO_PROXY_LOWERCASE,
];

/// The canary's bounds: a few turns, two minutes, and a token ceiling that
/// covers one short session's cached system prompt.
const CANARY_MAX_TURNS: u64 = 4;
const CANARY_WALL_SECONDS: u64 = 120;
const CANARY_TOKENS: u64 = 300_000;

const NETWORK_REACHED: &str = "CANARY-NETWORK-REACHED";
const NETWORK_DENIED: &str = "CANARY-NETWORK-DENIED";
const INSIDE_MARKER: &str = "inside.marker";

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CanaryPasses {
    schema_version: u32,
    #[serde(default)]
    runners: BTreeMap<String, CanaryPass>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CanaryPass {
    version: String,
    profile_sha256: String,
    checked_at: String,
    run_id: String,
}

/// Succeed only when the Claude sandbox is known to hold for the installed
/// binary and the current profile, running the canary when that is not yet
/// known. On failure the runner is marked unavailable for `retry_seconds` and
/// the reason is returned.
pub(crate) fn ensure_claude_sandbox(
    paths: &OstromPaths,
    clock: &Clock,
    registry: &AgentRegistry,
    model: Option<&str>,
    retry_seconds: u64,
) -> Result<(), String> {
    let run_id = generated_run_id("sandbox-canary", clock);
    let result = check(paths, clock, registry, model, &run_id);
    if let Err((reason, message)) = &result {
        let until = clock.now()
            + chrono::Duration::seconds(i64::try_from(retry_seconds).unwrap_or(i64::MAX / 2));
        let entry = crate::UnavailableRunner {
            until: until.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            reset_reported: false,
            reason: (*reason).to_owned(),
            message: message.chars().take(500).collect(),
            recorded_at: clock.timestamp(),
            run_id,
        };
        if let Err(error) = mark_unavailable(paths, CLAUDE_RUNNER, entry) {
            eprintln!("ostrom: could not record {CLAUDE_RUNNER} as unavailable: {error}");
        }
    }
    result.map_err(|(reason, message)| format!("{reason}: {message}"))
}

type CanaryFailure = (&'static str, String);

fn unverified(message: impl Into<String>) -> CanaryFailure {
    (SANDBOX_UNVERIFIED_REASON, message.into())
}

fn check(
    paths: &OstromPaths,
    clock: &Clock,
    registry: &AgentRegistry,
    model: Option<&str>,
    run_id: &str,
) -> Result<(), CanaryFailure> {
    let runner = registry
        .get(CLAUDE_RUNNER)
        .ok_or_else(|| unverified("the Claude harness is not registered"))?;
    let version = runner
        .installed_version()
        .filter(|version| !version.is_empty())
        .ok_or_else(|| unverified("`claude --version` did not report a version"))?;
    let profile_sha256 = ostrom_core::sha256_hex(
        umwelt_runtime::agent::claude::implementer_settings_source().as_bytes(),
    );
    let cache_path = paths.state.join(SANDBOX_CANARY_FILE);
    let mut passes = fs::read(&cache_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<CanaryPasses>(&bytes).ok())
        .unwrap_or_default();
    if passes
        .runners
        .get(CLAUDE_RUNNER)
        .is_some_and(|pass| pass.version == version && pass.profile_sha256 == profile_sha256)
    {
        return Ok(());
    }

    let checked = Checked {
        run_id,
        version: &version,
        profile_sha256: &profile_sha256,
        proxy: proxy_environment(),
    };
    // A denied attempt means something only if the host is reachable without
    // the sandbox, through the same resolver and proxies. Checked first, so an
    // unreachable host spends no Claude session.
    if let Err(detail) = control_reaches_host()
        && detail.is_empty()
    {
        let message = format!("the unsandboxed control could not reach {CANARY_HOST}: {detail}");
        checked.record(paths, clock, "inconclusive", Some(&message), 0.0);
        return Err((SANDBOX_INCONCLUSIVE_REASON, message));
    }

    let directory = paths.state.join("sandbox-canary").join(run_id);
    let work = directory.join("work");
    fs::create_dir_all(&work)
        .map_err(|error| unverified(format!("could not prepare the canary: {error}")))?;
    let outside = directory.join("outside.marker");
    let prompt = directory.join("prompt.md");
    let transcript = directory.join("transcript.jsonl");
    fs::write(&prompt, canary_prompt(&outside))
        .map_err(|error| unverified(format!("could not prepare the canary: {error}")))?;
    let signals = SignalFlags::default();
    let mut bound = WallCap::start(
        CANARY_WALL_SECONDS,
        Some((transcript.clone(), CANARY_TOKENS)),
        &signals,
    );
    let outcome = registry.run(
        CLAUDE_RUNNER,
        &RunRequest::Implementer(ImplementerRunRequest {
            prompt,
            worktree: work.clone(),
            result: directory.join("result.md"),
            transcript: transcript.clone(),
            token_ceiling: CANARY_TOKENS,
            offline: true,
            model: model.map(str::to_owned),
            effort: None,
            max_turns: Some(CANARY_MAX_TURNS),
            signals: signals.clone(),
            supervisor_pid: None,
            termination_grace: std::time::Duration::from_secs(
                ostrom_core::RUN_TERMINATION_GRACE_SECONDS,
            ),
            environment: Vec::new(),
            spawned: umwelt_runtime::SpawnObserver::default(),
        }),
    );
    bound.stop();
    let events = fs::read_to_string(&transcript).unwrap_or_default();
    let verdict = match outcome {
        RunOutcome::Exited(status) if status.success() => {
            judge(&events, work.join(INSIDE_MARKER).exists(), outside.exists())
        }
        RunOutcome::Exited(status) => Err(format!("the canary session exited with {status}")),
        RunOutcome::Terminated(_) => {
            Err("the canary session was stopped at its wall or token bound".to_owned())
        }
        RunOutcome::Error(fault) => Err(format!(
            "the canary session could not run: {}{}",
            fault.name(),
            fault
                .detail()
                .map(|detail| format!(": {detail}"))
                .unwrap_or_default()
        )),
    };
    checked.record(
        paths,
        clock,
        if verdict.is_ok() { "pass" } else { "fail" },
        verdict.as_ref().err(),
        cost(&events),
    );
    verdict.map_err(unverified)?;
    passes.schema_version = 1;
    passes.runners.insert(
        CLAUDE_RUNNER.to_owned(),
        CanaryPass {
            version,
            profile_sha256,
            checked_at: clock.timestamp(),
            run_id: run_id.to_owned(),
        },
    );
    write_private(&cache_path, &passes).map_err(unverified)
}

/// Reach the canary's host without the sandbox, the way the canary's own
/// `curl` would: the same binary, and this process's environment unchanged, so
/// the operator's proxy variables apply exactly as they would to the canary.
fn control_reaches_host() -> Result<(), String> {
    let output = std::process::Command::new("curl")
        .args(["-sS", "-m", "10", "-o", "/dev/null"])
        .arg(format!("https://{CANARY_HOST}"))
        .env_remove("HTTPS_PROXY")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| format!("could not run curl: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "curl exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
                .trim()
                .chars()
                .take(200)
                .collect::<String>()
        ))
    }
}

/// The proxy variables that are set, with any `user:password@` removed.
fn proxy_environment() -> Map<String, Value> {
    PROXY_VARIABLES
        .iter()
        .filter_map(|variable| {
            variable
                .value()
                .map(|value| (variable.name.to_owned(), json!(redact_userinfo(&value))))
        })
        .collect()
}

/// Remove the userinfo of every URL in `value` (a comma-separated list is
/// handled item by item), so a proxy credential never reaches a record.
pub(crate) fn redact_userinfo(value: &str) -> String {
    value
        .split(',')
        .map(|item| {
            let (scheme, rest) = item.split_once("://").unwrap_or(("", item));
            let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
            let (authority, path) = rest.split_at(authority_end);
            let host = authority
                .rsplit_once("@@@@")
                .map_or(authority, |(_, host)| host);
            if scheme.is_empty() {
                format!("{host}{path}")
            } else {
                format!("{scheme}://{host}{path}")
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn canary_prompt(outside: &Path) -> String {
    format!(
        "This is an automated sandbox check. Use the Bash tool to run exactly this one command, once, and then reply with the single word DONE. Do not run anything else.\n\n```\ntouch {INSIDE_MARKER}; touch '{}' 2>/dev/null; if curl -sS -m 5 -o /dev/null https://{CANARY_HOST} 2>/dev/null || bash --noprofile -c 'exec 3<>/dev/tcp/{CANARY_HOST}/443' 2>/dev/null; then echo {NETWORK_REACHED}; else echo {NETWORK_DENIED}; fi\n```\n",
        outside.display()
    )
}

/// The canary's verdict from what ostrom can see itself (the two marker
/// files) and the command's own output, never the model's narration.
pub(crate) fn judge(transcript: &str, ran: bool, escaped: bool) -> Result<(), String> {
    if escaped {
        return Err("a sandboxed command wrote outside its working directory".to_owned());
    }
    if !ran {
        return Err("the canary command did not run in its working directory".to_owned());
    }
    let output = tool_results(transcript);
    if output.is_empty() {
        return Err("the canary transcript holds no command output".to_owned());
    }
    if output.contains(NETWORK_REACHED) {
        return Err("a sandboxed command reached the network".to_owned());
    }
    if !output.contains(NETWORK_DENIED) {
        return Err("no command output showed the network denied".to_owned());
    }
    Ok(())
}

/// The text of every tool result in a Claude `stream-json` transcript.
fn tool_results(transcript: &str) -> String {
    let mut output = String::new();
    for event in transcript
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event.get("type").and_then(Value::as_str) == Some("user"))
    {
        let Some(content) = event.pointer("/message/content").and_then(Value::as_array) else {
            continue;
        };
        for item in content
            .iter()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("tool_result"))
        {
            match item.get("content") {
                Some(Value::String(text)) => output.push_str(text),
                Some(Value::Array(parts)) => {
                    for part in parts {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            output.push_str(text);
                        }
                    }
                }
                _ => {}
            }
            output.push('\n');
        }
    }
    output
}

fn cost(transcript: &str) -> f64 {
    transcript
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event.get("type").and_then(Value::as_str) == Some("result"))
        .filter_map(|event| event.get("total_cost_usd").and_then(Value::as_f64))
        .sum()
}

/// What one canary run is recorded under, in its `sandbox-checked` fact.
struct Checked<'a> {
    run_id: &'a str,
    version: &'a str,
    profile_sha256: &'a str,
    proxy: Map<String, Value>,
}

impl Checked<'_> {
    fn record(
        &self,
        paths: &OstromPaths,
        clock: &Clock,
        outcome: &str,
        failure: Option<&String>,
        cost_usd: f64,
    ) {
        let fact = Map::from_iter([
            ("schema_version".to_owned(), json!(1)),
            ("runner".to_owned(), json!(CLAUDE_RUNNER)),
            ("run_id".to_owned(), json!(self.run_id)),
            ("version".to_owned(), json!(self.version)),
            ("profile_sha256".to_owned(), json!(self.profile_sha256)),
            ("outcome".to_owned(), json!(outcome)),
            ("failure".to_owned(), json!(failure)),
            ("cost_usd".to_owned(), json!(cost_usd)),
            ("proxy".to_owned(), Value::Object(self.proxy.clone())),
        ]);
        if let Err(error) = append_trace(
            &paths.trace_file(),
            &TraceAppend {
                ts: clock.timestamp(),
                kind: "sandbox-checked".to_owned(),
                fact,
                narration: Map::new(),
            },
        ) {
            eprintln!("ostrom: could not record the sandbox canary: {error}");
        }
    }
}

fn write_private(path: &Path, passes: &CanaryPasses) -> Result<(), String> {
    let bytes = serde_json::to_vec(passes).map_err(|error| error.to_string())?;
    fs::write(path, [bytes.as_slice(), b"\n"].concat())
        .and_then(|()| {
            crate::set_private_file_mode(path)
                .map_err(|error| std::io::Error::other(error.to_string()))
        })
        .map_err(|error| format!("could not cache the canary pass: {error}"))
}

#[cfg(test)]
mod tests {
    use super::{judge, redact_userinfo};

    #[test]
    fn a_proxy_credential_never_survives_redaction() {
        assert_eq!(
            redact_userinfo("http://user:s3cret@proxy.invalid:3128"),
            "http://proxy.invalid:3128"
        );
        assert_eq!(
            redact_userinfo("socks5h://s3cret@proxy.invalid:1080/path?q=1"),
            "socks5h://proxy.invalid:1080/path?q=1"
        );
        assert_eq!(
            redact_userinfo("user:s3cret@proxy.invalid:3128"),
            "proxy.invalid:3128"
        );
        assert_eq!(
            redact_userinfo("http://proxy.invalid:3128"),
            "http://proxy.invalid:3128"
        );
        assert_eq!(
            redact_userinfo("localhost,.internal.invalid"),
            "localhost,.internal.invalid"
        );
        assert_eq!(
            redact_userinfo("http://a:p@b@proxy.invalid"),
            "http://proxy.invalid"
        );
    }

    fn result(text: &str) -> String {
        format!(
            "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"input\":{{\"command\":\"echo CANARY-NETWORK-REACHED\"}}}}]}}}}\n{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"t\",\"content\":\"{text}\"}}]}}}}\n"
        )
    }

    #[test]
    fn only_a_denied_network_and_a_contained_write_pass() {
        assert_eq!(judge(&result("CANARY-NETWORK-DENIED"), true, false), Ok(()));
        assert!(judge(&result("CANARY-NETWORK-REACHED"), true, false).is_err());
        assert!(
            judge(
                &result("CANARY-NETWORK-REACHED\\nCANARY-NETWORK-DENIED"),
                true,
                false
            )
            .is_err(),
            "any sign of reaching the network fails, whatever else is printed"
        );
        assert!(judge(&result("CANARY-NETWORK-DENIED"), true, true).is_err());
        assert!(judge(&result("CANARY-NETWORK-DENIED"), false, false).is_err());
        assert!(judge(&result("permission denied"), true, false).is_err());
        assert!(judge("not json at all\n", true, false).is_err());
        assert!(
            judge(
                "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"CANARY-NETWORK-DENIED\"}]}}\n",
                true,
                false
            )
            .is_err(),
            "the model's own words are never evidence"
        );
    }
}
