use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use ethogram::{DecisionKind, DecisionRequestedPayload};
use regex::Regex;
use serde_json::{Map, Value, json};
use umwelt_runtime::{FileSink, Source};

use crate::{
    Clock, OstromPaths, SweepError, load_config, load_config_or_defaults, local_drift, read_queue,
    read_trace,
};

#[derive(Debug, Clone)]
pub struct DigestOptions {
    pub paths: OstromPaths,
    pub working_directory: PathBuf,
    pub clock: Clock,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookOutput {
    pub stdout: String,
    pub stderr: String,
}

/// The frozen rules this build ships, compiled in so the base constitution
/// layer does not depend on an installed plugin tree.
const SHIPPED_RULES: &str = include_str!("../assets/rules/frozen-rules.md");

#[must_use]
pub fn render_constitution(
    plugin_root: &Path,
    user_rules_root: &Path,
    working_directory: &Path,
    home: &Path,
) -> String {
    let mut layers = Vec::new();
    collect_layer(&mut layers, "user", user_rules_root);
    collect_layer(&mut layers, "repo", &working_directory.join(".ostrom"));

    // The shipped layer is compiled in. It used to be read out of an installed
    // plugin tree, which meant the constitution silently lost its base layer on
    // any machine that had not installed the plugin — including every non-Claude
    // harness. An explicit override still wins, for a fixture or a fork.
    let mut output = fs::read_to_string(plugin_root.join("rules/frozen-rules.md"))
        .unwrap_or_else(|_| SHIPPED_RULES.to_owned());
    if !layers.is_empty() {
        output.push('\n');
        output.push_str(
            "<!-- constitution: layers below override the shipped rules above on conflict -->\n",
        );
        let home = home.to_string_lossy();
        for (label, file) in layers {
            let display = if label == "repo" {
                file.strip_prefix(working_directory).map_or_else(
                    |_| file.to_string_lossy().into_owned(),
                    |relative| format!("./{}", relative.display()),
                )
            } else {
                let display = file.to_string_lossy();
                display
                    .strip_prefix(home.as_ref())
                    .map_or_else(|| display.to_string(), |suffix| format!("~{suffix}"))
            };
            output.push('\n');
            output.push_str(&format!(
                "<!-- constitution layer: {label} ({display}) -->\n\n"
            ));
            output.push_str(&fs::read_to_string(file).unwrap_or_default());
        }
    }
    output
}

pub fn render_digest(options: &DigestOptions) -> HookOutput {
    let waiting = read_waiting_decisions(&options.paths);
    let config = match load_config(&options.paths, &options.working_directory) {
        Ok(config) => config,
        // Pass and dispatch can raise decisions without a mandate roster.
        Err(SweepError::NotConfigured(_))
            if waiting.as_ref().is_ok_and(|waiting| !waiting.is_empty()) =>
        {
            match load_config_or_defaults(&options.paths, &options.working_directory) {
                Ok(config) => config,
                Err(_) => return HookOutput::default(),
            }
        }
        Err(_) => return HookOutput::default(),
    };
    let now = options.clock.epoch_seconds();
    let state_path = options.paths.sweep_state_file();
    let state_modified = fs::metadata(&state_path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_secs());
    let stale = now.saturating_sub(state_modified) >= config.cadence_hours.saturating_mul(3_600);
    let queue = match read_queue(&options.paths.queue_file()) {
        Ok(queue) => queue,
        Err(_) => {
            return HookOutput {
                stdout: String::new(),
                stderr: format!(
                    "mandate digest: queue is malformed; run `ostrom queue list` after repairing {}\n",
                    options.paths.queue_file().display()
                ),
            };
        }
    };
    let active = queue
        .iter()
        .map(|row| row.value())
        .filter(|row| {
            matches!(
                row.get("state").and_then(Value::as_str),
                Some("pending" | "deferred")
            )
        })
        .collect::<Vec<_>>();
    let state_bytes = fs::read(&state_path).unwrap_or_default();
    let state = serde_json::from_slice::<Value>(&state_bytes).ok();
    let cursor = render_cursor(state.as_ref());
    let mut body = String::new();

    let watermark_path = options.paths.state.join(".digest-decisions-read");
    let since = read_watermark(&watermark_path);
    let digest_time = options.clock.timestamp();
    let decisions = read_decisions(&options.paths.trace_file(), &since);
    if decisions.is_empty() {
        push_line(&mut body, "DECISIONS TAKEN: nothing since your last read");
    } else {
        push_line(&mut body, "DECISIONS TAKEN");
        for decision in decisions {
            push_line(&mut body, &decision);
        }
    }
    let failure_escalations = read_failure_escalations(&options.paths.trace_file(), &since);
    if !failure_escalations.is_empty() {
        push_line(&mut body, "DISPATCH FAILURES ESCALATED");
        for escalation in failure_escalations {
            push_line(&mut body, &escalation);
        }
    }

    let waiting_repositories = render_waiting_decisions(
        &mut body,
        &options.paths,
        waiting,
        config.decision_inbox_url.as_deref(),
    );
    render_stalled_holds(&mut body, state.as_ref());
    render_section(
        &mut body,
        &format!("MOVED SINCE {cursor}"),
        &["moved"],
        &active,
    );
    render_section(&mut body, "STUCK", &["stuck"], &active);
    render_section(&mut body, "DRIFT", &["drift"], &active);
    render_section(
        &mut body,
        "UNEXPLAINED WRITES — INVESTIGATE NOW",
        &["unexplained-write"],
        &active,
    );
    render_section(
        &mut body,
        "MERGE GATE FAULTS",
        &["merge-gate-fault"],
        &active,
    );
    let parked = active
        .iter()
        .filter(|row| row.get("kind").and_then(Value::as_str) == Some("parked"))
        .count();
    if parked > 0 {
        push_line(&mut body, &format!("{parked} parked"));
    }

    let unresolvable = unresolvable_repositories(state.as_ref());
    if !unresolvable.is_empty() {
        push_line(&mut body, "UNDISPATCHABLE REPOSITORIES");
        for repository in &unresolvable {
            push_line(
                &mut body,
                &format!("{repository} — source repository not found under search_roots"),
            );
        }
    }
    render_state_rollups(&mut body, state.as_ref());

    let troubled = active
        .iter()
        .filter(|row| {
            matches!(
                row.get("kind").and_then(Value::as_str),
                Some("drift" | "stuck" | "merge-gate-fault" | "unexplained-write")
            )
        })
        .filter_map(|row| row.get("repo").and_then(Value::as_str))
        .chain(waiting_repositories.iter().map(String::as_str))
        .chain(stalled_hold_repositories(state.as_ref()))
        .chain(unresolvable.iter().map(String::as_str))
        .collect::<BTreeSet<_>>()
        .len();
    let nominal = config.projects.len().saturating_sub(troubled);
    if local_drift(&options.paths, &options.working_directory, true)
        .is_ok_and(|text| !text.is_empty())
    {
        push_line(
            &mut body,
            "LOCAL DRIFT — run ostrom local-drift for details",
        );
    }
    if stale {
        push_line(&mut body, "STALE — mandate sweep overdue");
    }
    push_line(&mut body, &format!("{nominal} projects nominal"));

    let today = options.clock.date();
    let date_pattern = Regex::new(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}$").expect("date regex is valid");
    if date_pattern.is_match(&today) {
        let tap = options.paths.state.join(format!(".tap-{today}"));
        if OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(tap)
            .is_ok()
        {
            body.push('\n');
            push_line(&mut body, "BRIEF");
            push_line(
                &mut body,
                "Produce today's brief now. Separate blocked on you from blocked on no one; propose only. `ostrom queue` remains the sole decision surface.",
            );
        }
    }

    mark_notices_reported(&state_path, state, &state_bytes);
    let _ = fs::create_dir_all(&options.paths.state);
    let _ = fs::write(watermark_path, format!("{digest_time}\n"));
    let message = body.trim_end_matches('\n');
    if message.is_empty() {
        return HookOutput::default();
    }
    let envelope = json!({
        "systemMessage": message,
        "hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": message,
        }
    });
    let mut stdout = serde_json::to_string_pretty(&envelope).unwrap_or_default();
    stdout.push('\n');
    HookOutput {
        stdout,
        stderr: String::new(),
    }
}

fn collect_layer(layers: &mut Vec<(&'static str, PathBuf)>, label: &'static str, root: &Path) {
    let single = root.join("rules.md");
    if has_content(&single) {
        layers.push((label, single));
    }
    let mut fragments = fs::read_dir(root.join("rules.d"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "md"))
        .collect::<Vec<_>>();
    fragments.sort();
    for fragment in fragments {
        if has_content(&fragment) {
            layers.push((label, fragment));
        }
    }
}

fn has_content(path: &Path) -> bool {
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    let mut remaining = text.as_str();
    loop {
        let Some(start) = remaining.find("<!--") else {
            return remaining
                .chars()
                .any(|character| !character.is_whitespace());
        };
        if remaining[..start]
            .chars()
            .any(|character| !character.is_whitespace())
        {
            return true;
        }
        let Some(end) = remaining[start + 4..].find("-->") else {
            return false;
        };
        remaining = &remaining[start + 4 + end + 3..];
    }
}

fn render_cursor(state: Option<&Value>) -> String {
    state
        .and_then(|state| state.get("repos"))
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|repos| repos.values())
        .filter_map(|repo| {
            repo.get("previous_cursor")
                .filter(|value| !value.is_null() && value != &&Value::Bool(false))
                .or_else(|| repo.get("cursor"))
                .map(jq_render)
        })
        .min()
        .unwrap_or_else(|| "initial".to_owned())
}

fn render_section(body: &mut String, heading: &str, kinds: &[&str], rows: &[&Value]) {
    let mut rendered = Vec::new();
    let stuck_suffix =
        Regex::new(r"; no movement for [0-9]+ days$").expect("stuck reason regex is valid");
    for row in rows {
        let kind = row.get("kind").and_then(Value::as_str).unwrap_or_default();
        if !kinds.contains(&kind) {
            continue;
        }
        let title = row
            .get("title")
            .and_then(Value::as_str)
            .filter(|title| !title.is_empty())
            .unwrap_or("(title unavailable)");
        let stored_reason = row
            .pointer("/mandate/reason")
            .filter(|value| !value.is_null() && value != &&Value::Bool(false))
            .or_else(|| row.get("mandate"))
            .map_or_else(String::new, jq_render);
        let reason = if kind == "moved" {
            stored_reason
                .strip_suffix("; updated since the read cursor")
                .unwrap_or(&stored_reason)
                .to_owned()
        } else {
            stored_reason
        };
        let suffix = if row.get("state").and_then(Value::as_str) == Some("deferred") {
            " [deferred]"
        } else {
            ""
        };
        let reference = format!(
            "{}{}",
            row.get("repo").map_or_else(String::new, jq_render),
            row.get("ref").map_or_else(String::new, jq_render)
        );
        let content_width = 100_i64 - (char_len(&reference) + 2 + 3 + char_len(suffix));
        let title_width = char_len(title).min(45_i64.max(content_width - char_len(&reason)));
        let essential = reason
            .strip_suffix("; open PR passed CI")
            .or_else(|| {
                stuck_suffix
                    .find(&reason)
                    .map(|matched| &reason[..matched.start()])
            })
            .unwrap_or(&reason);
        let reason_width = (content_width - title_width)
            .max(char_len(essential))
            .max(1);
        rendered.push(format!(
            "{reference}  {} — {}{suffix}",
            truncate(title, title_width),
            truncate(&reason, reason_width)
        ));
    }
    if !rendered.is_empty() {
        push_line(body, heading);
        for row in rendered {
            push_line(body, &row);
        }
    }
}

struct WaitingDecision {
    id: String,
    kind: String,
    subject: String,
}

fn read_waiting_decisions(paths: &OstromPaths) -> Result<Vec<WaitingDecision>, String> {
    let mut requested = BTreeMap::new();
    let mut answered = BTreeSet::new();
    // Membership comes only from facts, independently of the digest read watermark.
    for row in read_trace(&paths.trace_file())
        .map_err(|error| error.to_string())?
        .rows
    {
        let row = row.map_err(|error| error.to_string())?;
        if !matches!(
            row.kind.as_str(),
            "decision-requested" | "decision-answered"
        ) {
            continue;
        }
        let field = |key: &str| {
            row.fact
                .get(key)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| format!("{} fact is missing {key}", row.kind))
        };
        let id = field("decision_id")?.to_owned();
        if row.kind == "decision-answered" {
            answered.insert(id);
        } else {
            let kind = field("kind")?.to_owned();
            let subject = field("subject")?.to_owned();
            // These facts describe adaptation, not a change in authority.
            if !matches!(kind.as_str(), "stuck" | "drift") {
                requested.insert(id.clone(), WaitingDecision { id, kind, subject });
            }
        }
    }
    let mut waiting = requested
        .into_values()
        .filter(|decision| !answered.contains(&decision.id))
        .collect::<Vec<_>>();
    waiting.sort_by(|left, right| {
        (&left.kind, &left.subject, &left.id).cmp(&(&right.kind, &right.subject, &right.id))
    });
    Ok(waiting)
}

fn read_decision_dossiers(
    paths: &OstromPaths,
    waiting: &[WaitingDecision],
) -> Result<BTreeMap<String, DecisionRequestedPayload>, String> {
    let mut requests = BTreeMap::new();
    let entries = match fs::read_dir(paths.runs_dir()) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(requests),
        Err(error) => return Err(error.to_string()),
    };
    let source = FileSink::new(paths.runs_dir());
    for entry in entries {
        let path = entry
            .map_err(|error| error.to_string())?
            .path()
            .join("events.jsonl");
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("{}: {error}", path.display())),
        };
        let mut first = String::new();
        BufReader::new(file)
            .read_line(&mut first)
            .map_err(|error| error.to_string())?;
        if !first.ends_with('\n') {
            continue;
        }
        let first = ethogram::parse_event(&first)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        for event in source
            .read_from(&first.run_id, 0)
            .map_err(|error| format!("{}: {error}", path.display()))?
        {
            if event.event_type != ethogram::DECISION_REQUESTED {
                continue;
            }
            let Some(decision) = waiting
                .iter()
                .find(|decision| event.payload["decisionId"] == decision.id)
            else {
                continue;
            };
            let request: DecisionRequestedPayload =
                serde_json::from_value(event.payload).map_err(|error| error.to_string())?;
            if request.kind.as_str() != decision.kind
                || request.subject.as_deref() != Some(&decision.subject)
            {
                return Err(format!(
                    "request does not match the fact for {}",
                    decision.id
                ));
            }
            if requests
                .get(&decision.id)
                .is_some_and(|prior| prior != &request)
            {
                return Err(format!("conflicting requests for {}", decision.id));
            }
            requests.insert(decision.id.clone(), request);
        }
    }
    Ok(requests)
}

fn render_waiting_decisions(
    body: &mut String,
    paths: &OstromPaths,
    waiting: Result<Vec<WaitingDecision>, String>,
    inbox_url: Option<&str>,
) -> BTreeSet<String> {
    let waiting = match waiting {
        Ok(waiting) if waiting.is_empty() => return BTreeSet::new(),
        Ok(waiting) => waiting,
        Err(error) => {
            push_line(body, "DECISIONS WAITING");
            push_line(body, &format!("Unable to read decision facts: {error}"));
            return BTreeSet::new();
        }
    };
    push_line(body, "DECISIONS WAITING");
    if let Some(url) = inbox_url.filter(|url| !url.trim().is_empty()) {
        push_line(body, &format!("Answer decisions: {url}"));
    }
    let requests = read_decision_dossiers(paths, &waiting).unwrap_or_else(|error| {
        push_line(body, &format!("Unable to read decision dossiers: {error}"));
        BTreeMap::new()
    });
    let mut kind = "";
    for decision in &waiting {
        if kind != decision.kind {
            kind = &decision.kind;
            push_line(body, &format!("  {kind}"));
        }
        push_line(
            body,
            &format!("    {} [decision: {}]", decision.subject, decision.id),
        );
        let Some(request) = requests.get(&decision.id) else {
            push_line(
                body,
                "      Dossier unavailable — restore the local decision.requested event before answering.",
            );
            continue;
        };
        let dossier = &request.dossier;
        push_line(body, &format!("      Question: {}", dossier.question));
        push_line(body, "      Options ruled out:");
        if dossier.options_ruled_out.is_empty() {
            push_line(body, "        (none)");
        }
        for option in &dossier.options_ruled_out {
            push_line(body, &format!("        - {option}"));
        }
        push_line(
            body,
            &format!("      Recommended action: {}", dossier.recommended_action),
        );
        push_line(
            body,
            &format!("      Blast radius: {}", dossier.blast_radius),
        );
        if dossier.truncated == Some(true) {
            push_line(
                body,
                "      Dossier was truncated by the requesting process.",
            );
        }
        push_line(body, "      Options:");
        for option in &request.options {
            push_line(body, &format!("        {}: {}", option.id, option.label));
            let verb = match request.kind {
                DecisionKind::Tripwire => option.id.as_str(),
                DecisionKind::GateInconclusive
                | DecisionKind::HumanDecides
                | DecisionKind::Budget => "approve",
                DecisionKind::Permission | DecisionKind::Unknown(_) => {
                    push_line(body, "          Answer through the requesting process.");
                    continue;
                }
            };
            push_line(
                body,
                &format!(
                    "          ostrom queue {} {} --decision {} --option {}",
                    quote_argument(verb),
                    quote_argument(&decision.subject),
                    quote_argument(&decision.id),
                    quote_argument(&option.id)
                ),
            );
        }
    }
    waiting
        .iter()
        .filter_map(|decision| {
            decision
                .subject
                .split_once('#')
                .map(|(repo, _)| repo.to_owned())
        })
        .collect()
}

fn quote_argument(value: &str) -> String {
    if !value.is_empty()
        && !value.starts_with('#')
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "_./:#-".contains(character))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn read_watermark(path: &Path) -> String {
    let candidate = fs::read_to_string(path)
        .ok()
        .and_then(|text| text.lines().next().map(str::to_owned))
        .unwrap_or_default();
    let pattern = Regex::new(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$")
        .expect("watermark regex is valid");
    if pattern.is_match(&candidate) {
        candidate
    } else {
        "1970-01-01T00:00:00Z".to_owned()
    }
}

fn read_decisions(path: &Path, since: &str) -> Vec<String> {
    let mut decisions = fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|row| row.get("kind").and_then(Value::as_str) == Some("decision-taken"))
        .map(|row| {
            let ts = row
                .get("ts")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let repo = row
                .pointer("/fact/repo")
                .and_then(Value::as_str)
                .unwrap_or("(repo unknown)");
            let reference = row
                .pointer("/fact/ref")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let decision = row
                .pointer("/fact/decision")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .unwrap_or("(decision unavailable)");
            let reversal = row
                .pointer("/fact/reversal")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .unwrap_or("reversal not recorded");
            let reason = row
                .pointer("/narration/reason")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let reason = if reason.is_empty() {
                String::new()
            } else {
                format!(" — {reason}")
            };
            (
                ts,
                format!("{repo}{reference}  {decision}{reason}  [reversal: {reversal}]"),
            )
        })
        .filter(|(timestamp, _)| timestamp.as_str() > since)
        .collect::<Vec<_>>();
    decisions.sort_by(|left, right| right.0.cmp(&left.0));
    decisions.into_iter().map(|(_, row)| row).collect()
}

fn read_failure_escalations(path: &Path, since: &str) -> Vec<String> {
    let mut escalations = fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|row| row.get("kind").and_then(Value::as_str) == Some("dispatch-failure-escalated"))
        .filter_map(|row| {
            let ts = row.get("ts")?.as_str()?.to_owned();
            let item = row.pointer("/fact/item_id")?.as_str()?;
            let reason = row.pointer("/fact/failure_reason")?.as_str()?;
            let count = row
                .pointer("/fact/failure_count")
                .and_then(Value::as_u64)
                .unwrap_or(2);
            (ts.as_str() > since).then(|| {
                (
                    ts,
                    format!("{item} — {reason} ({count} identical failures; dispatch suppressed)"),
                )
            })
        })
        .collect::<Vec<_>>();
    escalations.sort_by(|left, right| right.0.cmp(&left.0));
    escalations.into_iter().map(|(_, row)| row).collect()
}

fn unresolvable_repositories(state: Option<&Value>) -> BTreeSet<String> {
    state
        .and_then(|state| state.get("unresolvable_repositories"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|repository| !repository.is_empty())
        .map(str::to_owned)
        .collect()
}

fn render_state_rollups(body: &mut String, state: Option<&Value>) {
    let Some(repositories) = state
        .and_then(|state| state.get("repos"))
        .and_then(Value::as_object)
    else {
        return;
    };
    for (repository, value) in repositories {
        if let Some(text) = value
            .get("notice")
            .filter(|notice| !notice.is_null())
            .filter(|notice| notice.get("reported").and_then(Value::as_bool) != Some(true))
            .and_then(|notice| notice.get("text"))
        {
            push_line(body, &jq_render(text));
        }
        if let Some(cap) = value.get("item_cap").filter(|cap| !cap.is_null()) {
            push_line(
                body,
                &format!(
                    "{repository}: item cap reached ({}) — sweep may be incomplete",
                    jq_render(cap)
                ),
            );
        }
        let unclassified = value
            .get("unclassified")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if unclassified > 0 {
            push_line(
                body,
                &format!("{repository}: {unclassified} unclassified — ostrom queue triage"),
            );
        }
        let unexplained = value
            .get("unexplained_write_count")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if unexplained > 0 {
            let noun = if unexplained == 1 {
                "unexplained write"
            } else {
                "unexplained writes"
            };
            push_line(
                body,
                &format!("{repository}: {unexplained} {noun} — investigate immediately"),
            );
        }
        let gate_faults = value
            .get("merge_gate_fault_count")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if gate_faults > 0 {
            let noun = if gate_faults == 1 {
                "merge gate fault"
            } else {
                "merge gate faults"
            };
            push_line(
                body,
                &format!("{repository}: {gate_faults} {noun} — ostrom queue triage"),
            );
        }
    }
}

fn render_stalled_holds(body: &mut String, state: Option<&Value>) {
    let findings = state
        .and_then(|state| state.get("stalled_holds"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    if findings.is_empty() {
        return;
    }
    push_line(body, "STALLED HOLDS — DECIDE OR CHANGE THE RULE");
    for finding in findings {
        let id = finding
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("(pull request unavailable)");
        let title = finding
            .get("title")
            .and_then(Value::as_str)
            .filter(|title| !title.is_empty())
            .unwrap_or("(title unavailable)");
        let held_days = finding
            .get("held_days")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        let rule = finding
            .get("rule")
            .and_then(Value::as_str)
            .unwrap_or("floor");
        push_line(
            body,
            &format!("{id}  {title} — held {held_days} days; decide, or change rule {rule}"),
        );
    }
}

fn stalled_hold_repositories(state: Option<&Value>) -> impl Iterator<Item = &str> {
    state
        .and_then(|state| state.get("stalled_holds"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|finding| finding.get("repo").and_then(Value::as_str))
}

fn mark_notices_reported(path: &Path, mut state: Option<Value>, original: &[u8]) {
    if original.is_empty() {
        return;
    }
    let Some(state) = state.as_mut() else {
        return;
    };
    let Some(repositories) = state.get_mut("repos").and_then(Value::as_object_mut) else {
        return;
    };
    let mut changed = false;
    for repository in repositories.values_mut() {
        let Some(notice) = repository.get_mut("notice").and_then(Value::as_object_mut) else {
            continue;
        };
        if notice.get("reported").and_then(Value::as_bool) != Some(true) {
            notice.insert("reported".to_owned(), Value::Bool(true));
            changed = true;
        }
    }
    if !changed {
        return;
    }
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    let modified = metadata.modified().ok();
    sort_json(state);
    let Ok(mut encoded) = serde_json::to_vec_pretty(state) else {
        return;
    };
    encoded.push(b'\n');
    let temporary = path.with_extension("notices.tmp");
    let result = File::create(&temporary)
        .and_then(|mut file| file.write_all(&encoded).map(|()| file))
        .and_then(|file| {
            if let Some(modified) = modified {
                file.set_times(std::fs::FileTimes::new().set_modified(modified))?;
            }
            drop(file);
            fs::rename(&temporary, path)
        });
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
}

fn sort_json(value: &mut Value) {
    match value {
        Value::Array(values) => values.iter_mut().for_each(sort_json),
        Value::Object(object) => {
            for value in object.values_mut() {
                sort_json(value);
            }
            let mut entries = std::mem::take(object).into_iter().collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            *object = entries.into_iter().collect::<Map<_, _>>();
        }
        _ => {}
    }
}

fn truncate(text: &str, width: i64) -> String {
    let width = width.max(0) as usize;
    let length = text.chars().count();
    if length <= width {
        text.to_owned()
    } else if width <= 1 {
        "…".to_owned()
    } else {
        format!("{}…", text.chars().take(width - 1).collect::<String>())
    }
}

fn char_len(text: &str) -> i64 {
    i64::try_from(text.chars().count()).unwrap_or(i64::MAX)
}

fn jq_render(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Null => "null".to_owned(),
        value => value.to_string(),
    }
}

fn push_line(output: &mut String, line: &str) {
    output.push_str(line);
    output.push('\n');
}

#[cfg(test)]
mod tests {
    use ostrom_core::{DecisionOption, Dossier};
    use tempfile::TempDir;
    use umwelt_runtime::Sink;

    use super::*;
    use crate::run_events::{DecisionRequest, emit_decision_requests};
    use crate::{QueueDocument, TraceAppend, append_trace, write_queue};

    struct Fixture {
        root: TempDir,
        options: DigestOptions,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let paths = OstromPaths {
                config: root.path().to_owned(),
                state: root.path().to_owned(),
            };
            fs::write(
                paths.config.join("mandates.yaml"),
                "projects:\n  - repo: example-org/project\n",
            )
            .unwrap();
            fs::write(paths.sweep_state_file(), "{\"repos\":{}}").unwrap();
            fs::write(paths.state.join(".tap-2026-09-08"), "").unwrap();
            let options = DigestOptions {
                paths,
                working_directory: root.path().to_owned(),
                clock: Clock::fixed("2026-09-08T00:00:00Z".parse().unwrap()),
            };
            Self { root, options }
        }

        fn request(&self, run: &str, id: &str, kind: DecisionKind, subject: &str) {
            let options = match kind {
                DecisionKind::Tripwire => vec!["approve", "reject", "defer"],
                DecisionKind::GateInconclusive => vec!["excuse:required_checks", "wait", "fail"],
                DecisionKind::Budget => vec!["raise", "wait"],
                _ => vec!["yes", "no"],
            };
            emit_decision_requests(
                &self.options.paths,
                &self.options.clock.timestamp(),
                run,
                &[DecisionRequest {
                    decision_id: id.to_owned(),
                    kind,
                    subject: subject.to_owned(),
                    dossier: Dossier {
                        question: format!("May {id} proceed?"),
                        options_ruled_out: vec![
                            "Proceed without permission".to_owned(),
                            "Silently bypass the hold".to_owned(),
                        ],
                        recommended_action: "Review the evidence first".to_owned(),
                        blast_radius: "Only the named subject".to_owned(),
                    },
                    options: options
                        .into_iter()
                        .map(|id| DecisionOption {
                            id: id.to_owned(),
                            label: format!("Choose {id}"),
                        })
                        .collect(),
                }],
            )
            .unwrap();
        }

        fn fact(&self, kind: &str, fact: Value) {
            append_trace(
                &self.options.paths.trace_file(),
                &TraceAppend {
                    ts: self.options.clock.timestamp(),
                    kind: kind.to_owned(),
                    fact: fact.as_object().unwrap().clone(),
                    narration: Map::new(),
                },
            )
            .unwrap();
        }

        fn message(&self) -> String {
            let output = render_digest(&self.options);
            assert!(output.stderr.is_empty(), "{}", output.stderr);
            let envelope: Value = serde_json::from_str(&output.stdout).unwrap();
            assert_eq!(
                envelope["systemMessage"],
                envelope["hookSpecificOutput"]["additionalContext"]
            );
            envelope["systemMessage"].as_str().unwrap().to_owned()
        }

        fn queue(&self, kinds: &[&str]) {
            let rows = kinds
                .iter()
                .enumerate()
                .map(|(index, kind)| {
                    QueueDocument::from_value(json!({
                        "id":format!("example-org/project#{}", index + 20),
                        "repo":"example-org/project", "ref":format!("#{}", index + 20),
                        "kind":kind, "title":format!("{kind} queue title"),
                        "state":"pending", "opened":"2026-09-08T00:00:00Z",
                        "mandate":{"reason":format!("{kind} queue reason")}, "needs_judgment":true
                    }))
                    .unwrap()
                })
                .collect::<Vec<_>>();
            write_queue(&self.options.paths.queue_file(), &rows).unwrap();
        }
    }

    fn assert_dossier(message: &str, id: &str) {
        for expected in [
            format!("Question: May {id} proceed?"),
            "Options ruled out:".to_owned(),
            "- Proceed without permission".to_owned(),
            "- Silently bypass the hold".to_owned(),
            "Recommended action: Review the evidence first".to_owned(),
            "Blast radius: Only the named subject".to_owned(),
        ] {
            assert!(message.contains(&expected), "missing {expected}: {message}");
        }
    }

    #[test]
    fn open_decision_is_local_and_persists_across_reads_while_answered_is_absent() {
        let fixture = Fixture::new();
        fixture.request(
            "sweep",
            "open",
            DecisionKind::Tripwire,
            "example-org/project#19",
        );
        fixture.request(
            "sweep",
            "answered",
            DecisionKind::Tripwire,
            "example-org/project#20",
        );
        fixture.fact("decision-answered", json!({"decision_id":"answered", "option":"approve", "by":"fixture-principal", "reversal":"reject"}));
        fixture.fact(
            "decision-requested",
            json!({"decision_id":"answered", "kind":"tripwire", "subject":"example-org/project#20"}),
        );
        fixture.queue(&["tripwire"]);
        fs::write(
            fixture.options.paths.state.join(".digest-decisions-read"),
            "2026-09-09T00:00:00Z\n",
        )
        .unwrap();
        for _ in 0..2 {
            let message = fixture.message();
            assert!(
                message.contains(
                    "DECISIONS WAITING\n  tripwire\n    example-org/project#19 [decision: open]"
                ),
                "{message}"
            );
            assert_dossier(&message, "open");
            for option in ["approve", "reject", "defer"] {
                assert!(
                    message.contains(&format!("{option}: Choose {option}")),
                    "{message}"
                );
                assert!(message.contains(&format!("ostrom queue {option} example-org/project#19 --decision open --option {option}")), "{message}");
            }
            assert!(!message.contains("answered"), "{message}");
            assert!(!message.contains("example-org/project#20"), "{message}");
            assert!(!message.contains("://"), "{message}");
            assert!(!message.contains("Answer decisions:"), "{message}");
            assert!(message.ends_with("0 projects nominal"), "{message}");
        }
    }

    #[test]
    fn inbox_setting_layers_and_clears_without_removing_the_local_dossier() {
        let fixture = Fixture::new();
        fixture.request(
            "sweep",
            "open",
            DecisionKind::Tripwire,
            "example-org/project#19",
        );
        fs::write(
            fixture.options.paths.config.join("mandates.yaml"),
            "decision_inbox_url: http://127.0.0.1:8080/decisions?view=waiting\n",
        )
        .unwrap();
        let repository = fixture.root.path().join(".ostrom");
        fs::create_dir(&repository).unwrap();
        for (overlay, expected) in [
            ("", Some("http://127.0.0.1:8080/decisions?view=waiting")),
            (
                "decision_inbox_url: http://127.0.0.1:9090/answer\n",
                Some("http://127.0.0.1:9090/answer"),
            ),
            ("decision_inbox_url: null\n", None),
            ("decision_inbox_url: '  '\n", None),
        ] {
            fs::write(
                repository.join("mandates.yaml"),
                if overlay.is_empty() { "{}" } else { overlay },
            )
            .unwrap();
            let message = fixture.message();
            assert_dossier(&message, "open");
            assert!(message.contains(
                "ostrom queue approve example-org/project#19 --decision open --option approve"
            ));
            if let Some(url) = expected {
                assert!(
                    message.contains(&format!("Answer decisions: {url}")),
                    "{message}"
                );
            } else {
                assert!(!message.contains("://"), "{message}");
            }
        }
    }

    #[test]
    fn decisions_are_grouped_across_runs_with_commands_for_each_offered_option() {
        let fixture = Fixture::new();
        fixture.request(
            "sweep",
            "tripwire-a",
            DecisionKind::Tripwire,
            "example-org/project#19",
        );
        fixture.request(
            "gate",
            "gate",
            DecisionKind::GateInconclusive,
            "example-org/project#19",
        );
        fixture.request(
            "pass",
            "budget",
            DecisionKind::Budget,
            "account:/tmp/operator's state",
        );
        fixture.request(
            "sweep",
            "human",
            DecisionKind::HumanDecides,
            "example-org/project#19",
        );
        fixture.request(
            "dispatch",
            "tripwire-b",
            DecisionKind::Tripwire,
            "example-org/project#21",
        );
        let message = fixture.message();
        let mut previous = 0;
        for kind in ["budget", "gate_inconclusive", "human_decides", "tripwire"] {
            let heading = format!("\n  {kind}\n");
            assert_eq!(message.matches(&heading).count(), 1, "{message}");
            let position = message.find(&heading).unwrap();
            assert!(position > previous);
            previous = position;
        }
        assert_eq!(message.matches("[decision:").count(), 5, "{message}");
        for command in [
            "ostrom queue approve example-org/project#19 --decision gate --option excuse:required_checks",
            "ostrom queue approve example-org/project#19 --decision human --option yes",
            "ostrom queue approve 'account:/tmp/operator'\\''s state' --decision budget --option raise",
        ] {
            assert!(message.contains(command), "{message}");
        }
    }

    #[test]
    fn queue_rows_do_not_admit_decisions_and_stuck_and_drift_keep_their_sections() {
        let fixture = Fixture::new();
        fixture.queue(&["stuck", "drift", "tripwire", "decision"]);
        for kind in ["stuck", "drift"] {
            fixture.fact(
                "decision-requested",
                json!({"decision_id":kind,"kind":kind,"subject":"example-org/project#99"}),
            );
        }
        assert!(!fixture.message().contains("DECISIONS WAITING"));
        fixture.request(
            "sweep",
            "open",
            DecisionKind::Tripwire,
            "example-org/project#19",
        );
        let message = fixture.message();
        let waiting = message
            .split("DECISIONS WAITING\n")
            .nth(1)
            .unwrap()
            .split("STUCK\n")
            .next()
            .unwrap();
        assert!(
            !waiting.contains("stuck") && !waiting.contains("drift"),
            "{message}"
        );
        assert!(
            message.contains("STUCK\nexample-org/project#20  stuck queue title"),
            "{message}"
        );
        assert!(
            message.contains("DRIFT\nexample-org/project#21  drift queue title"),
            "{message}"
        );
        assert!(
            !message.contains("tripwire queue title") && !message.contains("decision queue title"),
            "{message}"
        );
    }

    #[test]
    fn event_without_fact_is_not_open_and_missing_dossier_does_not_hide_a_fact() {
        let fixture = Fixture::new();
        fixture.request(
            "sweep",
            "event-only",
            DecisionKind::Tripwire,
            "example-org/project#19",
        );
        fs::remove_file(fixture.options.paths.trace_file()).unwrap();
        assert!(!fixture.message().contains("DECISIONS WAITING"));
        fixture.fact(
            "decision-requested",
            json!({"decision_id":"fact-only","kind":"tripwire","subject":"example-org/project#20"}),
        );
        let message = fixture.message();
        assert!(
            message.contains("example-org/project#20 [decision: fact-only]"),
            "{message}"
        );
        assert!(message.contains("Dossier unavailable"), "{message}");
        assert!(!message.contains("event-only"), "{message}");
    }

    #[test]
    fn unreadable_decision_facts_are_visible_in_the_digest() {
        let fixture = Fixture::new();
        for text in [
            "broken\n",
            "{\"ts\":\"2026-09-08T00:00:00Z\",\"kind\":\"decision-requested\",\"fact\":{},\"narration\":{}}\n",
        ] {
            fs::write(fixture.options.paths.trace_file(), text).unwrap();
            assert!(
                fixture
                    .message()
                    .contains("DECISIONS WAITING\nUnable to read decision facts:")
            );
        }
    }

    #[test]
    fn budget_decision_is_visible_without_a_mandate_roster() {
        let fixture = Fixture::new();
        fs::remove_file(fixture.options.paths.config.join("mandates.yaml")).unwrap();
        assert_eq!(render_digest(&fixture.options), HookOutput::default());
        fixture.request(
            "pass",
            "budget",
            DecisionKind::Budget,
            "account:/tmp/operator",
        );
        let message = fixture.message();
        assert!(
            message.contains("DECISIONS WAITING\n  budget\n    account:/tmp/operator"),
            "{message}"
        );
        assert_dossier(&message, "budget");
        assert!(
            message.contains(
                "ostrom queue approve account:/tmp/operator --decision budget --option wait"
            ),
            "{message}"
        );
    }

    #[test]
    fn permission_dossier_is_local_without_advertising_an_unsupported_queue_answer() {
        let fixture = Fixture::new();
        fixture.request(
            "companion",
            "permission",
            DecisionKind::Permission,
            "tool:write",
        );
        let message = fixture.message();
        assert_dossier(&message, "permission");
        assert!(message.contains("yes: Choose yes"), "{message}");
        assert!(
            message.contains("Answer through the requesting process."),
            "{message}"
        );
        assert!(!message.contains("ostrom queue"), "{message}");
    }

    #[test]
    fn local_answer_arguments_round_trip_through_the_shell() {
        for argument in [
            "",
            "#19",
            "account:/tmp/operator's state",
            "$(printf substituted); echo unexpected",
            "two\nlines",
        ] {
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!(
                    "set -- {}; printf '%s\\n' \"$#\"; printf '%s' \"$1\"",
                    quote_argument(argument)
                ))
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(
                String::from_utf8(output.stdout).unwrap(),
                format!("1\n{argument}")
            );
        }
    }

    #[test]
    fn conflicting_or_mismatched_dossiers_are_visible_without_hiding_the_open_subject() {
        for mismatch in [false, true] {
            let fixture = Fixture::new();
            fixture.request(
                "sweep",
                "open",
                DecisionKind::Tripwire,
                "example-org/project#19",
            );
            let source = FileSink::new(fixture.options.paths.runs_dir());
            let mut payload = source.read_from("sweep", 0).unwrap().pop().unwrap().payload;
            if mismatch {
                payload["subject"] = json!("example-org/project#20");
            } else {
                payload["dossier"]["question"] = json!("A conflicting question");
            }
            source
                .append(
                    "sweep",
                    ethogram::EventDraft {
                        event_type: ethogram::DECISION_REQUESTED.to_owned(),
                        payload,
                        captured_at: None,
                    },
                )
                .unwrap();
            let message = fixture.message();
            assert!(
                message.contains("Unable to read decision dossiers:"),
                "{message}"
            );
            assert!(
                message.contains(if mismatch {
                    "request does not match the fact"
                } else {
                    "conflicting requests"
                }),
                "{message}"
            );
            assert!(
                message.contains("example-org/project#19 [decision: open]"),
                "{message}"
            );
        }
    }
}
