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
    if let Err(message) = &result {
        let until = clock.now()
            + chrono::Duration::seconds(i64::try_from(retry_seconds).unwrap_or(i64::MAX / 2));
        let entry = crate::UnavailableRunner {
            until: until.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            reset_reported: false,
            reason: SANDBOX_UNVERIFIED_REASON.to_owned(),
            message: message.chars().take(500).collect(),
            recorded_at: clock.timestamp(),
            run_id,
        };
        if let Err(error) = mark_unavailable(paths, CLAUDE_RUNNER, entry) {
            eprintln!("ostrom: could not record {CLAUDE_RUNNER} as unavailable: {error}");
        }
    }
    result.map_err(|message| format!("{SANDBOX_UNVERIFIED_REASON}: {message}"))
}

fn check(
    paths: &OstromPaths,
    clock: &Clock,
    registry: &AgentRegistry,
    model: Option<&str>,
    run_id: &str,
) -> Result<(), String> {
    let runner = registry
        .get(CLAUDE_RUNNER)
        .ok_or_else(|| "the Claude harness is not registered".to_owned())?;
    let version = runner
        .installed_version()
        .filter(|version| !version.is_empty())
        .ok_or_else(|| "`claude --version` did not report a version".to_owned())?;
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

    let directory = paths.state.join("sandbox-canary").join(run_id);
    let work = directory.join("work");
    fs::create_dir_all(&work).map_err(|error| format!("could not prepare the canary: {error}"))?;
    let outside = directory.join("outside.marker");
    let prompt = directory.join("prompt.md");
    let transcript = directory.join("transcript.jsonl");
    fs::write(&prompt, canary_prompt(&outside))
        .map_err(|error| format!("could not prepare the canary: {error}"))?;
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
    record(
        paths,
        clock,
        run_id,
        &version,
        &profile_sha256,
        verdict.as_ref().err(),
        cost(&events),
    );
    verdict?;
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
    write_private(&cache_path, &passes)
}

fn canary_prompt(outside: &Path) -> String {
    format!(
        "This is an automated sandbox check. Use the Bash tool to run exactly this one command, once, and then reply with the single word DONE. Do not run anything else.\n\n```\ntouch {INSIDE_MARKER}; touch '{}' 2>/dev/null; if curl -sS -m 5 -o /dev/null https://example.com 2>/dev/null || bash -c 'exec 3<>/dev/tcp/example.com/443' 2>/dev/null; then echo {NETWORK_REACHED}; else echo {NETWORK_DENIED}; fi\n```\n",
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

fn record(
    paths: &OstromPaths,
    clock: &Clock,
    run_id: &str,
    version: &str,
    profile_sha256: &str,
    failure: Option<&String>,
    cost_usd: f64,
) {
    let fact = Map::from_iter([
        ("schema_version".to_owned(), json!(1)),
        ("runner".to_owned(), json!(CLAUDE_RUNNER)),
        ("run_id".to_owned(), json!(run_id)),
        ("version".to_owned(), json!(version)),
        ("profile_sha256".to_owned(), json!(profile_sha256)),
        (
            "outcome".to_owned(),
            json!(if failure.is_none() { "pass" } else { "fail" }),
        ),
        ("failure".to_owned(), json!(failure)),
        ("cost_usd".to_owned(), json!(cost_usd)),
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
    use super::judge;

    fn result(text: &str) -> String {
        format!(
            "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"input\":{{\"command\":\"echo CANARY-NETWORK-REACHED\"}}}}]}}}}\n{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"t\",\"content\":\"{text}\"}}]}}}}\n"
        )
    }

    #[test]
    fn only_a_denied_network_and_a_contained_write_pass() {
        assert_eq!(judge(&result("CANARY-NETWORK-DENIED"), true, false), Ok(()));
        assert!(judge(&result("CANARY-NETWORK-REACHED"), true, false).is_err());
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
