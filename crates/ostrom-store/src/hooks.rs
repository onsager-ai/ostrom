use std::{
    collections::BTreeSet,
    fs,
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use regex::Regex;
use serde_json::{Map, Value, json};

use crate::{Clock, OstromPaths, load_config, local_drift, read_queue};

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
    let config = match load_config(&options.paths, &options.working_directory) {
        Ok(config) => config,
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

    render_section(
        &mut body,
        "DECISIONS WAITING",
        &["tripwire", "decision"],
        &active,
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
                Some(
                    "tripwire"
                        | "decision"
                        | "drift"
                        | "stuck"
                        | "merge-gate-fault"
                        | "unexplained-write"
                )
            )
        })
        .filter_map(|row| row.get("repo").and_then(Value::as_str))
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
