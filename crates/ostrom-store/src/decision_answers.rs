//! Apply human answers in a new judgment run. Only identifiers enter the fact ledger.

use std::{
    fs,
    io::Write,
    path::PathBuf,
    process::{Command, Stdio},
};

use ethogram::{
    DecisionAnsweredPayload, DecisionKind, DecisionRequestedPayload, EventDraft, PayloadExtension,
    RunKind, RunOutcome, validate_decision_answer_against_request,
};
use ostrom_core::{EventPayload, sha256_hex};
use serde_json::{Map, Value, json};
use thiserror::Error;
use umwelt_runtime::{FileSink, Source};

use crate::{
    Clock, OstromPaths, QueueDecision, RunEventError, RunEventGuard, RunEventStart, StoreError,
    TraceAppend, append_trace, generated_run_id, grant_excuse_at_head, read_queue, read_trace,
    revoke_excuse, set_private_file_mode, write_queue,
};

#[derive(Debug, Error)]
pub enum DecisionAnswerError {
    #[error("mandate queue: {0}")]
    Refused(String),
    #[error("mandate queue: decision answer: {0}")]
    Validation(#[from] serde_json::Error),
    #[error(transparent)]
    Run(#[from] RunEventError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Queue(#[from] crate::QueueActionError),
    #[error(transparent)]
    Excuse(#[from] crate::ExcuseError),
    #[error("mandate queue: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "mandate queue: answer recorded; could not tick {subject}: {detail}; tick the row manually, do not resubmit the answer"
    )]
    Tick { subject: String, detail: String },
}

impl DecisionAnswerError {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Queue(error) => error.exit_code(),
            Self::Excuse(error) => error.exit_code(),
            _ => 2,
        }
    }
}

/// Flags are an additive path: the unadorned CLI still calls decide_queue_item directly.
pub fn answer_queue_decision(
    paths: &OstromPaths,
    item: &str,
    verb: QueueDecision,
    decision_id: &str,
    option: &str,
    clock: &Clock,
) -> Result<Vec<u8>, DecisionAnswerError> {
    let _lock = AnswerLock::acquire(paths)?;
    let (request, requested_run_id) = find_request(paths, decision_id)?;
    if request.subject.as_deref() != Some(item) {
        return refuse("decision subject does not match the supplied item");
    }
    let history = answer_history(paths, decision_id)?;
    let previous = history.last();
    if previous
        .and_then(|fact| fact.get("option"))
        .and_then(Value::as_str)
        == Some(option)
    {
        return refuse("this answer is already recorded; no delivery was retried");
    }
    // Validate before identity lookup, delivery, or any answer/run append.
    let mut answer = prepare_answer(&request, option, &history, requested_run_id)?;
    if request.kind == DecisionKind::Tripwire && queue_verb(option) != Some(verb) {
        return refuse("queue verb must agree with the decision option");
    }
    let head = if option.starts_with("excuse:") || option.starts_with("revoke:") {
        Some(recorded_gate_head(paths, &request)?)
    } else {
        None
    };
    answer.by = forge_identity()?;
    let payload = serde_json::to_value(&answer)?;
    ethogram::validate(ethogram::DECISION_ANSWERED, &payload)
        .map_err(|error| DecisionAnswerError::Refused(error.to_string()))?;
    let fact = Map::from_iter([
        ("decision_id".to_owned(), json!(decision_id)),
        ("option".to_owned(), json!(option)),
        ("by".to_owned(), json!(&answer.by)),
        ("reversal".to_owned(), json!(&answer.reversal)),
    ]);
    EventPayload::new(fact.clone())
        .map_err(|error| DecisionAnswerError::Refused(error.to_string()))?;
    let mut run = RunEventGuard::start(
        paths,
        None,
        false,
        clock.clone(),
        RunEventStart {
            run_id: generated_run_id("judgment", clock),
            kind: RunKind::Judgment,
            actor: "queue".to_owned(),
            harness: "ostrom".to_owned(),
            model: None,
            schedule: None,
            repository: None,
            work_order: None,
            ceilings: None,
        },
    )?;
    let output = match request.kind {
        DecisionKind::Tripwire => deliver_queue(paths, item, verb, decision_id, clock)?,
        DecisionKind::GateInconclusive => {
            let reason = [format!("Decision {decision_id} answered by {}", answer.by)];
            if let Some(condition) = option.strip_prefix("excuse:") {
                grant_excuse_at_head(
                    paths,
                    item,
                    condition,
                    &reason,
                    Some(clock.now()),
                    head.as_deref(),
                )?
                .into_bytes()
            } else if let Some(condition) = option.strip_prefix("revoke:") {
                revoke_excuse(
                    paths,
                    item,
                    condition,
                    &reason,
                    Some(clock.now()),
                    head.as_deref(),
                )?
                .into_bytes()
            } else {
                Vec::new()
            }
        }
        // A raise is a policy version the principal authors. Recording this answer
        // grants no budget and never edits or signs a manifest. Wait/fail likewise
        // grant no permission; the next gate/pass still evaluates its evidence.
        DecisionKind::Budget | DecisionKind::HumanDecides => Vec::new(),
        _ => return refuse("this decision kind is not answered by ostrom queue"),
    };
    append_trace(
        &paths.trace_file(),
        &TraceAppend {
            ts: clock.timestamp(),
            kind: "decision-answered".to_owned(),
            fact,
            narration: Map::new(),
        },
    )?;
    run.append(EventDraft {
        event_type: ethogram::DECISION_ANSWERED.to_owned(),
        payload,
        captured_at: None,
    })?;
    // The answer is durable before this new outward-facing delivery. Never retry
    // a failed body edit automatically: that must not produce another answer.
    if request.kind == DecisionKind::HumanDecides
        && let Err(detail) = tick_issue(item, decision_id)
    {
        // Drop finishes the run as failed. A terminal-sink fault must not hide
        // the issue reference and the fact that the answer was recorded.
        return Err(DecisionAnswerError::Tick {
            subject: item.to_owned(),
            detail,
        });
    }
    run.finish(RunOutcome::Completed, None, None, None)?;
    Ok(output)
}

fn refuse<T>(message: &str) -> Result<T, DecisionAnswerError> {
    Err(DecisionAnswerError::Refused(message.to_owned()))
}

fn prepare_answer(
    request: &DecisionRequestedPayload,
    option: &str,
    history: &[Map<String, Value>],
    requested_run_id: String,
) -> Result<DecisionAnsweredPayload, DecisionAnswerError> {
    let mut answer = DecisionAnsweredPayload {
        decision_id: request.decision_id.clone(),
        option_id: option.to_owned(),
        by: "pending-forge-identity".to_owned(),
        by_timeout: None,
        reversal: None,
        // find_request only ever returns a request paired with the run that
        // carried its decision.requested event, so this is always known here.
        requested_run_id: Some(requested_run_id),
        extra: PayloadExtension::new(),
    };
    if let Some(condition) = option.strip_prefix("revoke:") {
        let grant = format!("excuse:{condition}");
        // The 2026-09-08 ruling on ethogram#7/ostrom#488 authorizes the
        // producer's recorded reversal on the same decision, although it was
        // not an offered choice. Do not invent an option on the original request.
        if request.kind != DecisionKind::GateInconclusive
            || !request.options.iter().any(|offered| offered.id == grant)
            || !history
                .iter()
                .any(|fact| fact["option"] == grant && fact["reversal"] == option)
        {
            return refuse("revoke must name a recorded excuse reversal on this decision");
        }
        answer.reversal = Some(grant);
        return Ok(answer);
    }
    validate_decision_answer_against_request(request, &answer)?;
    let reversal = match (&request.kind, option) {
        (DecisionKind::Tripwire, "approve") => "reject".to_owned(),
        (DecisionKind::Tripwire, "reject" | "defer") => "approve".to_owned(),
        (DecisionKind::GateInconclusive, option) if option.starts_with("excuse:") => {
            format!("revoke:{}", &option[7..])
        }
        (DecisionKind::GateInconclusive, "wait") => "fail".to_owned(),
        (DecisionKind::GateInconclusive, "fail") => "wait".to_owned(),
        (DecisionKind::Budget, "raise") => "wait".to_owned(),
        (DecisionKind::Budget, "wait") => "raise".to_owned(),
        (DecisionKind::HumanDecides, _) => history
            .last()
            .and_then(|fact| fact["option"].as_str())
            .filter(|prior| *prior != option)
            .or_else(|| {
                request
                    .options
                    .iter()
                    .find(|offered| offered.id != option)
                    .map(|offered| offered.id.as_str())
            })
            .ok_or_else(|| {
                DecisionAnswerError::Refused("decision has no corrective alternative".to_owned())
            })?
            .to_owned(),
        _ => return refuse("offered option has no ostrom delivery or reversal"),
    };
    answer.reversal = Some(reversal);
    // Temporary exception at ba892e84: its helper requires reversals to be
    // offered choices. https://github.com/onsager-ai/ethogram/pull/35 relaxes
    // that check; the ruled grant reversal is revoke:<condition>.
    // Its optionId was checked above; validate the complete pair here after the
    // next independently motivated repin carries ethogram's relaxed helper.
    if !option.starts_with("excuse:") {
        validate_decision_answer_against_request(request, &answer)?;
    }
    Ok(answer)
}

fn queue_verb(option: &str) -> Option<QueueDecision> {
    match option {
        "approve" => Some(QueueDecision::Approve),
        "reject" => Some(QueueDecision::Reject),
        "defer" => Some(QueueDecision::Defer),
        _ => None,
    }
}

/// Returns the matching `decision.requested` payload together with the id of
/// the run that emitted it — `DecisionAnsweredPayload.requested_run_id` needs
/// that run, since ostrom answers on a fresh judgment run rather than the one
/// that asked.
fn find_request(
    paths: &OstromPaths,
    decision_id: &str,
) -> Result<(DecisionRequestedPayload, String), DecisionAnswerError> {
    let mut found: Option<(DecisionRequestedPayload, String)> = None;
    let entries = match fs::read_dir(paths.runs_dir()) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return refuse("unknown decision");
        }
        Err(error) => return Err(error.into()),
    };
    let source = FileSink::new(paths.runs_dir());
    for entry in entries {
        let event_path = entry?.path().join("events.jsonl");
        if !event_path.is_file() {
            continue;
        }
        let text = fs::read_to_string(event_path)?;
        let Some(first) = text.lines().next() else {
            continue;
        };
        let first = ethogram::parse_event(first)?;
        for event in source
            .read_from(&first.run_id, 0)
            .map_err(RunEventError::from)?
        {
            if event.event_type != ethogram::DECISION_REQUESTED
                || event.payload["decisionId"] != decision_id
            {
                continue;
            }
            let run_id = event.run_id.clone();
            let request: DecisionRequestedPayload = serde_json::from_value(event.payload)?;
            match &found {
                Some((prior, _)) if prior != &request => {
                    return refuse("conflicting requests for this decisionId");
                }
                // A repeated gate run re-emits an identical request under the
                // same decisionId, so several runs may carry it. Directory
                // iteration order decides which one is kept, and that is
                // sound: each is a run where this decision was genuinely
                // asked, which is all requestedRunId claims.
                Some(_) => {}
                None => found = Some((request, run_id)),
            }
        }
    }
    found.ok_or_else(|| DecisionAnswerError::Refused(format!("unknown decision {decision_id}")))
}

fn answer_history(
    paths: &OstromPaths,
    decision_id: &str,
) -> Result<Vec<Map<String, Value>>, DecisionAnswerError> {
    let mut facts = Vec::new();
    for row in read_trace(&paths.trace_file())?.rows {
        let row = row.map_err(|error| DecisionAnswerError::Refused(error.to_string()))?;
        if row.kind == "decision-answered" && row.fact["decision_id"] == decision_id {
            facts.push(row.fact);
        }
    }
    Ok(facts)
}

fn recorded_gate_head(
    paths: &OstromPaths,
    request: &DecisionRequestedPayload,
) -> Result<String, DecisionAnswerError> {
    // Join against the gate's facts using part 2's identity, never its dossier prose
    // and never the current remote head, which may have advanced since the request.
    let subject = request.subject.as_deref().unwrap_or_default();
    let text = fs::read_to_string(paths.state.join("gate.jsonl"))?;
    for line in text.lines() {
        let record: Value = serde_json::from_str(line)?;
        if record["pr"] != subject {
            continue;
        }
        let head = record["head_sha"].as_str().unwrap_or_default();
        let digest = record["judgment_digest"].as_str().unwrap_or_default();
        let identity = format!("gate_inconclusive\0{subject}\0{head}\0{digest}");
        if request.decision_id == format!("gate_inconclusive-{}", sha256_hex(identity.as_bytes())) {
            if head.len() != 40 || !head.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return refuse("decision has no recorded full head SHA; cannot apply an excuse");
            }
            return Ok(head.to_owned());
        }
    }
    refuse("no gate record matches this decision; cannot apply an excuse")
}

fn deliver_queue(
    paths: &OstromPaths,
    item: &str,
    verb: QueueDecision,
    decision_id: &str,
    clock: &Clock,
) -> Result<Vec<u8>, DecisionAnswerError> {
    // A reject removes the queue row. Keep its private compatibility document
    // outside the fact ledger so a later opposite verb can restore it.
    let archive = paths
        .state
        .join("decision-queue")
        .join(format!("{}.jsonl", sha256_hex(decision_id.as_bytes())));
    let mut prior = read_queue(&archive)?.into_iter().next();
    if prior.is_none() {
        prior = read_queue(&paths.queue_file())?
            .into_iter()
            .find(|row| row.value()["id"] == item);
        if let Some(row) = &prior {
            write_queue(&archive, std::slice::from_ref(row))?;
        }
    }
    Ok(crate::queue::decide_queue_item_with_prior(
        &paths.queue_file(),
        &paths.sweep_state_file(),
        &paths.selector_events_file(),
        item,
        verb,
        Some(&clock.timestamp()),
        prior.as_ref(),
    )?)
}

fn forge_identity() -> Result<String, DecisionAnswerError> {
    let user = gh_json(&["api", "user"], None).map_err(DecisionAnswerError::Refused)?;
    let id = user["id"].as_u64().filter(|id| *id > 0).ok_or_else(|| {
        DecisionAnswerError::Refused("forge returned no principal identity".to_owned())
    })?;
    Ok(format!("github:user:{id}"))
}

fn tick_issue(subject: &str, decision_id: &str) -> Result<(), String> {
    let (repo, number) = crate::leaves::parse_target(subject).map_err(|error| error.to_string())?;
    let endpoint = format!("repos/{repo}/issues/{number}");
    let read = || -> Result<String, String> {
        gh_json(&["api", &endpoint], None)?["body"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| "forge returned no issue body".to_owned())
    };
    tick_with_delivery(subject, decision_id, read, |body| {
        gh_json(
            &["api", &endpoint, "--method", "PATCH", "--input", "-"],
            Some(&json!({"body": body})),
        )
        .map(|_| ())
    })
}

fn tick_with_delivery(
    subject: &str,
    decision_id: &str,
    mut read: impl FnMut() -> Result<String, String>,
    mut write: impl FnMut(&str) -> Result<(), String>,
) -> Result<(), String> {
    let original = read()?;
    let updated = crate::sweep::tick_human_decision(&original, subject, decision_id)?;
    // GitHub does not support conditional PATCH for issue bodies:
    // https://docs.github.com/en/rest/using-the-rest-api/best-practices-for-using-the-rest-api#use-conditional-requests
    // The ruled fallback is a fresh read immediately before writing, with no
    // intervening delivery. This detects changes between reads, not an atomic CAS.
    let current = read()?;
    if current != original {
        return Err("issue body changed before delivery; refusing to overwrite it".to_owned());
    }
    crate::sweep::tick_human_decision(&current, subject, decision_id)?;
    write(&updated)
}

fn gh_json(args: &[&str], input: Option<&Value>) -> Result<Value, String> {
    let mut child = Command::new("gh")
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    if let Some(input) = input {
        let mut stdin = child.stdin.take().expect("piped gh stdin");
        serde_json::to_writer(&mut stdin, input).map_err(|error| error.to_string())?;
        stdin.flush().map_err(|error| error.to_string())?;
    }
    let output = child
        .wait_with_output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "gh failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|error| error.to_string())
}

struct AnswerLock(PathBuf);

impl AnswerLock {
    fn acquire(paths: &OstromPaths) -> Result<Self, DecisionAnswerError> {
        fs::create_dir_all(&paths.state)?;
        let path = paths.state.join("decision-answer.lock");
        fs::OpenOptions::new().write(true).create_new(true).open(&path)
            .map_err(|error| DecisionAnswerError::Refused(format!("cannot lock answers at {}: {error}; if a command crashed, remove its stale lock before answering", path.display())))?;
        let guard = Self(path);
        set_private_file_mode(&guard.0)?;
        Ok(guard)
    }
}

impl Drop for AnswerLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
