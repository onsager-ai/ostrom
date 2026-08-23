//! Read-mostly operator surfaces kept byte-compatible with their shell predecessors.

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::SystemTime,
};

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{
    OstromPaths, environment, set_private_file_mode, sweep::load_config,
    sweep::load_config_or_defaults,
};

const QUERY_LIMIT: usize = 200;
const EXCUSE_CONDITIONS: [&str; 5] = [
    "required_checks",
    "review_threads",
    "bounce_selectors",
    "reserved_refs",
    "merge_protocol",
];

/// Audit never collapses unknowns into a score. No verdict, a verdict only at
/// another SHA, and null-SHA-only history describe different missing evidence
/// and require different operator responses. Null-SHA records remain quality
/// defects even when another record lets the same merge join successfully.
#[derive(Debug, Clone)]
pub struct AuditOptions {
    pub paths: OstromPaths,
    pub working_directory: PathBuf,
    pub days: u64,
    pub audit_time: DateTime<Utc>,
}

#[derive(Debug, Error)]
pub enum AuditError {
    #[error("mandate audit: gh is required")]
    GhRequired,
    #[error("mandate audit: no mandates.yaml found at {user} or {repo}")]
    NotConfigured { user: String, repo: String },
    #[error("mandate audit: mandates.yaml contains no projects")]
    EmptyRoster,
    #[error("mandate audit: could not load mandates: {0}")]
    Config(String),
    #[error("usage: ostrom audit [--days N]")]
    InvalidDays,
    #[error("mandate audit: gh is not authenticated for {0}; run 'gh auth login'")]
    Authentication(String),
    #[error("mandate audit: gate log exists but is not readable: {0}")]
    GateUnreadable(String),
    #[error("mandate audit: gate log is malformed: {0}")]
    GateMalformed(String),
    #[error("{detail}mandate audit: failed to query merged PRs for {repo}")]
    Query { repo: String, detail: String },
}

impl AuditError {
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::GhRequired => 1,
            Self::NotConfigured { .. }
            | Self::EmptyRoster
            | Self::Config(_)
            | Self::InvalidDays => 2,
            Self::Authentication(_) => 3,
            Self::GateUnreadable(_) => 4,
            Self::GateMalformed(_) | Self::Query { .. } => 5,
        }
    }
}

#[derive(Debug, Error)]
pub enum ExcuseError {
    #[error("mandate excuse: gh is required")]
    GhRequired,
    #[error(
        "usage: ostrom excuse grant <owner/repo#number> <condition> <reason...> | list [<owner/repo#number>]"
    )]
    Usage,
    #[error(
        "mandate excuse: condition must be one of required_checks, review_threads, bounce_selectors, reserved_refs, merge_protocol"
    )]
    Condition,
    #[error("mandate excuse: reason must not be empty")]
    EmptyReason,
    #[error("{detail}mandate excuse: could not resolve {target}")]
    Resolve { target: String, detail: String },
    #[error("{detail}mandate excuse: cannot grant for {target}: head SHA is unavailable")]
    HeadUnavailable { target: String, detail: String },
    #[error("{detail}mandate excuse: {target} did not return a full 40-character head SHA")]
    InvalidHead { target: String, detail: String },
    #[error("mandate excuse: cannot write {0}")]
    Write(String),
    #[error("mandate excuse: cannot read {0}")]
    Read(String),
    #[error("mandate excuse: could not read the clock")]
    Clock,
}

impl ExcuseError {
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::GhRequired => 1,
            Self::Usage | Self::Condition | Self::EmptyReason => 2,
            Self::Resolve { .. }
            | Self::HeadUnavailable { .. }
            | Self::InvalidHead { .. }
            | Self::Write(_)
            | Self::Read(_)
            | Self::Clock => 3,
        }
    }
}

#[derive(Debug, Error)]
pub enum LocalDriftError {
    #[error("mandate local drift: git is required")]
    GitRequired,
    #[error("mandate local drift: could not load mandates: {0}")]
    Config(String),
}

impl LocalDriftError {
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::GitRequired => 1,
            Self::Config(_) => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GateLogState {
    Present,
    Empty,
    Absent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuditOutcome {
    NoVerdict,
    NullShaOnly,
    OtherSha,
    MissingMergedSha,
    Pass,
    Fail,
    Inconclusive,
    UnknownVerdict,
}

#[derive(Debug)]
struct JoinedMerge<'a> {
    id: String,
    merged_sha: String,
    merge_commit: String,
    records: Vec<&'a Value>,
    latest: Option<&'a Value>,
    outcome: AuditOutcome,
}

pub fn audit(options: &AuditOptions) -> Result<String, AuditError> {
    if !command_exists("gh") {
        return Err(AuditError::GhRequired);
    }
    let user_path = options.paths.config.join("mandates.yaml");
    let repo_path = options.working_directory.join(".ostrom/mandates.yaml");
    if !user_path.exists() && !repo_path.exists() {
        return Err(AuditError::NotConfigured {
            user: user_path.display().to_string(),
            repo: repo_path.display().to_string(),
        });
    }
    let config = load_config(&options.paths, &options.working_directory)
        .map_err(|error| AuditError::Config(error.to_string()))?;
    if config.projects.is_empty() {
        return Err(AuditError::EmptyRoster);
    }
    let host = environment::GH_HOST
        .value()
        .unwrap_or_else(|| "github.com".to_owned());
    if !command_output(Command::new("gh").args(["auth", "status", "--hostname", &host]))
        .is_ok_and(|output| output.status.success())
    {
        return Err(AuditError::Authentication(host));
    }

    let seconds = options
        .days
        .checked_mul(86_400)
        .and_then(|seconds| i64::try_from(seconds).ok())
        .ok_or(AuditError::InvalidDays)?;
    let cutoff = options
        .audit_time
        .checked_sub_signed(Duration::seconds(seconds))
        .ok_or(AuditError::InvalidDays)?;
    let audit_time = options
        .audit_time
        .to_rfc3339_opts(SecondsFormat::Secs, true);
    let cutoff = cutoff.to_rfc3339_opts(SecondsFormat::Secs, true);
    let gate_path = options.paths.state.join("gate.jsonl");
    let (gate_state, gate_records) = read_gate_records(&gate_path)?;

    let mut text = String::new();
    push_line(
        &mut text,
        &format!(
            "mandate audit: merged-SHA gate verdicts over the last {} days",
            options.days
        ),
    );
    push_line(
        &mut text,
        &format!("Window: {cutoff} through {audit_time} (by mergedAt)."),
    );
    match gate_state {
        GateLogState::Empty => push_line(
            &mut text,
            &format!(
                "Gate log is empty at {}: every count below reflects no recorded verdicts, not a measurement of zero.",
                gate_path.display()
            ),
        ),
        GateLogState::Absent => push_line(
            &mut text,
            &format!(
                "Gate log is absent at {}: every count below reflects no recorded verdicts, not a measurement of zero.",
                gate_path.display()
            ),
        ),
        GateLogState::Present => {}
    }
    push_line(
        &mut text,
        "Pass-with-excuse is a subset of pass; unknown join states remain separate.",
    );
    push_line(&mut text, "");

    for project in config.projects {
        let repo = project.repo.as_str();
        let output = command_output(Command::new("gh").args([
            "pr",
            "list",
            "--repo",
            repo,
            "--state",
            "merged",
            "--limit",
            "200",
            "--json",
            "number,mergedAt,headRefOid,mergeCommit",
        ]))
        .map_err(|_| AuditError::Query {
            repo: repo.to_owned(),
            detail: String::new(),
        })?;
        if !output.status.success() {
            return Err(AuditError::Query {
                repo: repo.to_owned(),
                detail: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        let merged: Vec<Value> =
            serde_json::from_slice(&output.stdout).map_err(|error| AuditError::Query {
                repo: repo.to_owned(),
                detail: format!("mandate audit: malformed merged-PR response: {error}\n"),
            })?;
        let query_count = merged.len();
        let joined = join_merges(repo, &merged, &gate_records, &cutoff);
        render_audit_repository(&mut text, repo, &joined);
        if query_count == QUERY_LIMIT {
            push_line(
                &mut text,
                &format!(
                    "{repo}: merged-PR query hit the {QUERY_LIMIT}-item cap — the {}-day window may be incomplete",
                    options.days
                ),
            );
            push_line(&mut text, "");
        }
    }
    push_line(
        &mut text,
        "No score is calculated: outcome, exception use, join coverage, and condition failures are separate counts.",
    );
    Ok(text)
}

fn read_gate_records(path: &Path) -> Result<(GateLogState, Vec<Value>), AuditError> {
    if !path.exists() {
        return Ok((GateLogState::Absent, Vec::new()));
    }
    let bytes =
        fs::read(path).map_err(|_| AuditError::GateUnreadable(path.display().to_string()))?;
    if bytes.is_empty() {
        return Ok((GateLogState::Empty, Vec::new()));
    }
    let records = serde_json::Deserializer::from_slice(&bytes)
        .into_iter::<Value>()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| AuditError::GateMalformed(error.to_string()))?;
    Ok((GateLogState::Present, records))
}

fn join_merges<'a>(
    repo: &str,
    merged: &[Value],
    gate_records: &'a [Value],
    cutoff: &str,
) -> Vec<JoinedMerge<'a>> {
    let mut index: BTreeMap<&str, Vec<&Value>> = BTreeMap::new();
    for record in gate_records {
        if let Some(pr) = record.get("pr").and_then(Value::as_str) {
            index.entry(pr).or_default().push(record);
        }
    }
    merged
        .iter()
        .filter(|pull| {
            pull.get("mergedAt")
                .and_then(Value::as_str)
                .is_some_and(|merged_at| !merged_at.is_empty() && merged_at >= cutoff)
        })
        .map(|pull| {
            let number = pull
                .get("number")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            let id = format!("{repo}#{number}");
            let merged_sha = string_value(pull.get("headRefOid")).to_owned();
            let records = index.get(id.as_str()).cloned().unwrap_or_default();
            let sha_records = records
                .iter()
                .copied()
                .filter(|record| crate::gate::is_merge_evidence(record))
                .collect::<Vec<_>>();
            let latest = sha_records
                .iter()
                .copied()
                .rfind(|record| string_value(record.get("head_sha")) == merged_sha);
            let outcome = if merged_sha.is_empty() {
                AuditOutcome::MissingMergedSha
            } else if records.is_empty() {
                AuditOutcome::NoVerdict
            } else if let Some(record) = latest {
                match string_value(record.get("verdict")) {
                    "pass" => AuditOutcome::Pass,
                    "fail" => AuditOutcome::Fail,
                    "inconclusive" => AuditOutcome::Inconclusive,
                    _ => AuditOutcome::UnknownVerdict,
                }
            } else if sha_records.is_empty() {
                AuditOutcome::NullShaOnly
            } else {
                AuditOutcome::OtherSha
            };
            JoinedMerge {
                id,
                merged_sha,
                merge_commit: merge_commit_oid(pull),
                records,
                latest,
                outcome,
            }
        })
        .collect()
}

fn render_audit_repository(text: &mut String, repo: &str, merges: &[JoinedMerge<'_>]) {
    let count = |outcome| {
        merges
            .iter()
            .filter(|merge_| merge_.outcome == outcome)
            .count()
    };
    let passes = count(AuditOutcome::Pass);
    let failures = count(AuditOutcome::Fail);
    let inconclusive = count(AuditOutcome::Inconclusive);
    let pass_excused = merges
        .iter()
        .filter(|merge_| merge_.outcome == AuditOutcome::Pass)
        .filter(|merge_| {
            merge_.latest.is_some_and(|record| {
                record
                    .get("conditions")
                    .and_then(Value::as_array)
                    .is_some_and(|conditions| {
                        conditions
                            .iter()
                            .any(|condition| string_value(condition.get("result")) == "excused")
                    })
            })
        })
        .count();
    let null_sha_records = merges
        .iter()
        .flat_map(|merge_| &merge_.records)
        .filter(|record| record.get("head_sha").is_none_or(Value::is_null))
        .count();
    let prs_with_null_sha_records = merges
        .iter()
        .filter(|merge_| {
            merge_
                .records
                .iter()
                .any(|record| record.get("head_sha").is_none_or(Value::is_null))
        })
        .count();

    push_line(text, &format!("REPOSITORY {repo}"));
    push_line(text, "MERGE OUTCOMES");
    push_line(text, "bucket\tcount");
    push_count(
        text,
        "no verdict at any SHA",
        count(AuditOutcome::NoVerdict),
    );
    push_count(
        text,
        "only null-SHA verdicts (unjoinable)",
        count(AuditOutcome::NullShaOnly),
    );
    push_count(
        text,
        "verdict exists, but none at the merged SHA",
        count(AuditOutcome::OtherSha),
    );
    push_count(
        text,
        "merged PR missing headRefOid",
        count(AuditOutcome::MissingMergedSha),
    );
    push_count(text, "pass at the merged SHA", passes);
    push_count(
        text,
        "of passes, contains an excused condition",
        pass_excused,
    );
    push_count(
        text,
        "fail or inconclusive at the merged SHA",
        failures + inconclusive,
    );
    push_count(text, "  fail", failures);
    push_count(text, "  inconclusive", inconclusive);
    push_count(
        text,
        "unrecognized verdict at the merged SHA",
        count(AuditOutcome::UnknownVerdict),
    );
    push_count(text, "total merged PRs in window", merges.len());
    push_line(text, "");

    push_line(text, "RECORD QUALITY");
    push_line(text, "measure\tcount");
    push_count(
        text,
        "null head_sha records for merged PRs in window",
        null_sha_records,
    );
    push_count(
        text,
        "merged PRs touched by a null head_sha record",
        prs_with_null_sha_records,
    );
    push_line(text, "");

    render_condition_breakdown(
        text,
        "FAILED CONDITION BREAKDOWN",
        merges,
        AuditOutcome::Fail,
        "fail",
    );
    render_condition_breakdown(
        text,
        "INCONCLUSIVE CONDITION BREAKDOWN",
        merges,
        AuditOutcome::Inconclusive,
        "inconclusive",
    );

    push_line(text, "UNKNOWN JOIN DETAILS");
    push_line(
        text,
        "pr\treason\tmerged head\tmerge commit\tnull-SHA records",
    );
    let unknowns = merges
        .iter()
        .filter(|merge_| {
            matches!(
                merge_.outcome,
                AuditOutcome::NoVerdict
                    | AuditOutcome::NullShaOnly
                    | AuditOutcome::OtherSha
                    | AuditOutcome::MissingMergedSha
                    | AuditOutcome::UnknownVerdict
            )
        })
        .collect::<Vec<_>>();
    if unknowns.is_empty() {
        push_line(text, "none\t-\t-\t-\t0");
    } else {
        for merge_ in unknowns {
            let reason = match merge_.outcome {
                AuditOutcome::NoVerdict => "no verdict at any SHA",
                AuditOutcome::NullShaOnly => "only null-SHA verdicts",
                AuditOutcome::OtherSha => "none at merged SHA",
                AuditOutcome::MissingMergedSha => "missing headRefOid",
                _ => "unrecognized verdict",
            };
            let nulls = merge_
                .records
                .iter()
                .filter(|record| record.get("head_sha").is_none_or(Value::is_null))
                .count();
            push_line(
                text,
                &format!(
                    "{}\t{}\t{}\t{}\t{}",
                    merge_.id,
                    reason,
                    dash_if_empty(&merge_.merged_sha),
                    dash_if_empty(&merge_.merge_commit),
                    nulls
                ),
            );
        }
    }
    push_line(text, "");

    push_line(text, "NON-PASS VERDICTS AT THE MERGED SHA");
    push_line(text, "pr\tverdict\tmerged head\tmerge commit");
    let non_passes = merges
        .iter()
        .filter(|merge_| {
            matches!(
                merge_.outcome,
                AuditOutcome::Fail | AuditOutcome::Inconclusive
            )
        })
        .collect::<Vec<_>>();
    if non_passes.is_empty() {
        push_line(text, "none\t-\t-\t-");
    } else {
        for merge_ in non_passes {
            let verdict = if merge_.outcome == AuditOutcome::Fail {
                "fail"
            } else {
                "inconclusive"
            };
            push_line(
                text,
                &format!(
                    "{}\t{}\t{}\t{}",
                    merge_.id, verdict, merge_.merged_sha, merge_.merge_commit
                ),
            );
        }
    }
    push_line(text, "");
}

fn render_condition_breakdown(
    text: &mut String,
    heading: &str,
    merges: &[JoinedMerge<'_>],
    outcome: AuditOutcome,
    result: &str,
) {
    let mut counts = BTreeMap::<String, usize>::new();
    for merge_ in merges.iter().filter(|merge_| merge_.outcome == outcome) {
        let names = merge_
            .latest
            .and_then(|record| record.get("conditions"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|condition| string_value(condition.get("result")) == result)
            .filter_map(|condition| condition.get("name").and_then(Value::as_str))
            .collect::<BTreeSet<_>>();
        for name in names {
            *counts.entry(name.to_owned()).or_default() += 1;
        }
    }
    push_line(text, heading);
    push_line(text, "condition\tmerged PRs");
    if counts.is_empty() {
        push_line(text, "none\t0");
    } else {
        for (name, count) in counts {
            push_line(text, &format!("{name}\t{count}"));
        }
    }
    push_line(text, "");
}

fn merge_commit_oid(pull: &Value) -> String {
    match pull.get("mergeCommit") {
        Some(Value::Object(object)) => object
            .get("oid")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        Some(Value::String(oid)) => oid.clone(),
        _ => String::new(),
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct ExceptionRecord {
    ts: String,
    repo: String,
    pr: u64,
    head_sha: String,
    condition: String,
    reason: String,
}

pub fn grant_excuse(
    paths: &OstromPaths,
    target: &str,
    condition: &str,
    reason_parts: &[String],
    timestamp: Option<DateTime<Utc>>,
) -> Result<String, ExcuseError> {
    require_gh()?;
    let (repo, pr) = parse_target(target)?;
    if !EXCUSE_CONDITIONS.contains(&condition) {
        return Err(ExcuseError::Condition);
    }
    let reason = reason_parts.join(" ").trim().to_owned();
    if reason.is_empty() {
        return Err(ExcuseError::EmptyReason);
    }
    let head_sha = resolve_head(target, repo, pr)?;
    let timestamp = timestamp.map_or_else(current_time, Ok)?;
    let record = ExceptionRecord {
        ts: timestamp.to_rfc3339_opts(SecondsFormat::Secs, true),
        repo: repo.to_owned(),
        pr,
        head_sha,
        condition: condition.to_owned(),
        reason,
    };
    let encoded = serde_json::to_string(&record).map_err(|_| {
        ExcuseError::Write(paths.state.join("exceptions.jsonl").display().to_string())
    })?;
    let path = paths.state.join("exceptions.jsonl");
    fs::create_dir_all(&paths.state).map_err(|_| ExcuseError::Write(path.display().to_string()))?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|_| ExcuseError::Write(path.display().to_string()))?;
    set_private_file_mode(&path).map_err(|_| ExcuseError::Write(path.display().to_string()))?;
    file.write_all(format!("{encoded}\n").as_bytes())
        .map_err(|_| ExcuseError::Write(path.display().to_string()))?;
    Ok(format!("{encoded}\n"))
}

pub fn list_excuses(paths: &OstromPaths, filter: Option<&str>) -> Result<String, ExcuseError> {
    require_gh()?;
    if let Some(target) = filter {
        parse_target(target)?;
    }
    let path = paths.state.join("exceptions.jsonl");
    let bytes = match fs::read(&path) {
        Ok(bytes) if bytes.is_empty() => return Ok(String::new()),
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(_) => return Err(ExcuseError::Read(path.display().to_string())),
    };
    let records = serde_json::Deserializer::from_slice(&bytes)
        .into_iter::<Value>()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ExcuseError::Read(path.display().to_string()))?;
    if !records.iter().all(valid_exception_record) {
        return Err(ExcuseError::Read(path.display().to_string()));
    }
    let mut targets = BTreeSet::new();
    for record in &records {
        let target = exception_target(record);
        if filter.is_none_or(|expected| expected == target) {
            targets.insert(target);
        }
    }
    let mut heads = BTreeMap::new();
    for target in targets {
        let head = parse_target(&target)
            .ok()
            .and_then(|(repo, pr)| resolve_head(&target, repo, pr).ok())
            .unwrap_or_default();
        heads.insert(target, head);
    }
    let mut text = String::new();
    for record in records {
        let target = exception_target(&record);
        if filter.is_some_and(|expected| expected != target) {
            continue;
        }
        let recorded_head = string_value(record.get("head_sha"));
        let current = heads.get(&target).map_or("", String::as_str);
        let state = if current.is_empty() {
            "unknown"
        } else if current == recorded_head {
            "current"
        } else {
            "superseded"
        };
        let reason =
            serde_json::to_string(record.get("reason").and_then(Value::as_str).unwrap_or(""))
                .map_err(|_| ExcuseError::Read(path.display().to_string()))?;
        push_line(
            &mut text,
            &format!(
                "{state} {target} {} head_sha={recorded_head} reason={reason}",
                string_value(record.get("condition"))
            ),
        );
    }
    Ok(text)
}

fn require_gh() -> Result<(), ExcuseError> {
    command_exists("gh")
        .then_some(())
        .ok_or(ExcuseError::GhRequired)
}

fn parse_target(target: &str) -> Result<(&str, u64), ExcuseError> {
    let pattern = Regex::new(r"^[^/\s#]+/[^/\s#]+#[1-9][0-9]*$").expect("target regex is valid");
    if !pattern.is_match(target) {
        return Err(ExcuseError::Usage);
    }
    let (repo, number) = target.rsplit_once('#').ok_or(ExcuseError::Usage)?;
    let pr = number.parse().map_err(|_| ExcuseError::Usage)?;
    Ok((repo, pr))
}

fn resolve_head(target: &str, repo: &str, pr: u64) -> Result<String, ExcuseError> {
    let output = command_output(Command::new("gh").args([
        "pr",
        "view",
        &pr.to_string(),
        "--repo",
        repo,
        "--json",
        "headRefOid",
    ]))
    .map_err(|_| ExcuseError::Resolve {
        target: target.to_owned(),
        detail: String::new(),
    })?;
    if !output.status.success() {
        return Err(ExcuseError::Resolve {
            target: target.to_owned(),
            detail: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    let detail = String::from_utf8_lossy(&output.stderr).into_owned();
    let value: Value =
        serde_json::from_slice(&output.stdout).map_err(|_| ExcuseError::InvalidHead {
            target: target.to_owned(),
            detail: detail.clone(),
        })?;
    let head = string_value(value.get("headRefOid"));
    if head.is_empty() {
        return Err(ExcuseError::HeadUnavailable {
            target: target.to_owned(),
            detail,
        });
    }
    let pattern = Regex::new(r"^[0-9a-fA-F]{40}$").expect("SHA regex is valid");
    if !pattern.is_match(head) {
        return Err(ExcuseError::InvalidHead {
            target: target.to_owned(),
            detail,
        });
    }
    Ok(head.to_owned())
}

fn current_time() -> Result<DateTime<Utc>, ExcuseError> {
    Ok(DateTime::<Utc>::from(SystemTime::now()))
}

fn valid_exception_record(record: &Value) -> bool {
    let Some(object) = record.as_object() else {
        return false;
    };
    object.get("ts").is_some_and(Value::is_string)
        && object.get("repo").is_some_and(Value::is_string)
        && object.get("pr").is_some_and(Value::is_number)
        && object.get("head_sha").is_some_and(Value::is_string)
        && object.get("condition").is_some_and(Value::is_string)
        && object.get("reason").is_some_and(Value::is_string)
}

fn exception_target(record: &Value) -> String {
    format!(
        "{}#{}",
        string_value(record.get("repo")),
        record.get("pr").and_then(Value::as_u64).unwrap_or_default()
    )
}

/// `git cherry` is patch-id based: it recognizes rebases but not squash
/// merges. The report therefore exposes both raw and unmatched-patch counts,
/// and no classification is permission to delete a branch. Every Git and
/// GitHub operation below is a read; unattended callers rely on that boundary.
pub fn local_drift(
    paths: &OstromPaths,
    working_directory: &Path,
    local_only: bool,
) -> Result<String, LocalDriftError> {
    if !command_exists("git") {
        return Err(LocalDriftError::GitRequired);
    }
    let config = load_config_or_defaults(paths, working_directory)
        .map_err(|error| LocalDriftError::Config(error.to_string()))?;
    if config.search_roots.is_empty() {
        return Ok(String::new());
    }
    let mut rows = Vec::new();
    let mut repositories = BTreeSet::new();
    for configured_root in config.search_roots {
        let configured = PathBuf::from(&configured_root);
        if !configured.is_dir() {
            rows.push(format!(
                "unknown\troot={configured_root}\treason=search-root-unreadable"
            ));
            continue;
        }
        let Ok(root) = configured.canonicalize() else {
            rows.push(format!(
                "unknown\troot={configured_root}\treason=search-root-unreadable"
            ));
            continue;
        };
        if git_success(&root, &["rev-parse", "--git-dir"]) {
            add_repository(&root, &mut repositories);
        }
        let mut markers = Vec::new();
        find_git_markers(&root, &mut markers);
        for marker in markers {
            if let Some(candidate) = marker.parent() {
                add_repository(candidate, &mut repositories);
            }
        }
    }
    for repository in repositories {
        scan_repository(&repository, local_only, &mut rows);
    }
    if rows.is_empty() {
        return Ok(String::new());
    }
    let mut text = String::from(
        "LOCAL DRIFT\nLIMIT: git cherry is patch-id based: it catches rebases but not squash merges; squash-merged work can appear unpublished. Counts are raw_commits / patches_not_in_main; review before deleting.\n",
    );
    for row in rows {
        push_line(&mut text, &row);
    }
    Ok(text)
}

fn find_git_markers(directory: &Path, markers: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_name() == ".git" {
            markers.push(path);
            continue;
        }
        if entry
            .file_type()
            .is_ok_and(|kind| kind.is_dir() && !kind.is_symlink())
        {
            find_git_markers(&path, markers);
        }
    }
}

fn add_repository(candidate: &Path, repositories: &mut BTreeSet<PathBuf>) {
    let Some(output) = git_output(candidate, &["worktree", "list", "--porcelain"])
        .filter(|output| output.status.success())
    else {
        return;
    };
    let first = String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from);
    let Some(repository) = first else {
        return;
    };
    let resolved = if repository.is_dir() {
        repository.canonicalize().unwrap_or(repository)
    } else {
        repository
    };
    repositories.insert(resolved);
}

fn scan_repository(repository: &Path, local_only: bool, rows: &mut Vec<String>) {
    let Some(worktree_output) = git_output(repository, &["worktree", "list", "--porcelain"])
        .filter(|output| output.status.success())
    else {
        rows.push(format!(
            "unknown\trepository={}\treason=worktree-list-unavailable",
            repository.display()
        ));
        return;
    };
    let worktree_text = String::from_utf8_lossy(&worktree_output.stdout);
    let mut branch_worktrees = BTreeMap::new();
    for block in worktree_text
        .split("\n\n")
        .filter(|block| !block.is_empty())
    {
        let mut worktree = None;
        let mut branch = "(detached)";
        let mut bare = false;
        for line in block.lines() {
            if let Some(path) = line.strip_prefix("worktree ") {
                worktree = Some(PathBuf::from(path));
            } else if let Some(name) = line.strip_prefix("branch refs/heads/") {
                branch = name;
            } else if line == "bare" {
                bare = true;
            }
        }
        let Some(worktree) = worktree.filter(|_| !bare) else {
            continue;
        };
        if branch != "(detached)" {
            branch_worktrees
                .entry(branch.to_owned())
                .or_insert_with(|| worktree.clone());
        }
        if !worktree.is_dir() {
            rows.push(format!(
                "unknown\trepository={}\tworktree={}\treason=worktree-unreadable",
                repository.display(),
                worktree.display()
            ));
        } else if let Some(output) = git_output(
            &worktree,
            &["status", "--porcelain", "--untracked-files=normal"],
        ) {
            let dirty_count = String::from_utf8_lossy(&output.stdout).lines().count();
            if dirty_count > 0 {
                rows.push(format!(
                    "dirty\trepository={}\tworktree={}\tbranch={branch}\tchanges={dirty_count}",
                    repository.display(),
                    worktree.display()
                ));
            }
        }
    }

    if !git_success(
        repository,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "refs/remotes/origin/main",
        ],
    ) {
        rows.push(format!(
            "unknown\trepository={}\treason=origin-main-unavailable",
            repository.display()
        ));
        return;
    }
    let Some(branch_output) = git_output(
        repository,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
    )
    .filter(|output| output.status.success()) else {
        rows.push(format!(
            "unknown\trepository={}\treason=local-branches-unavailable",
            repository.display()
        ));
        return;
    };
    for branch in String::from_utf8_lossy(&branch_output.stdout)
        .lines()
        .filter(|branch| !branch.is_empty())
    {
        let reference = format!("refs/heads/{branch}");
        let range = format!("origin/main..{reference}");
        let Some(raw_commits) = git_count(repository, &["rev-list", "--count", &range]) else {
            rows.push(format!(
                "unknown\trepository={}\tbranch={branch}\treason=commit-count-unavailable",
                repository.display()
            ));
            continue;
        };
        if raw_commits == 0 {
            continue;
        }
        let Some(cherry) = git_output(repository, &["cherry", "origin/main", &reference])
            .filter(|output| output.status.success())
        else {
            rows.push(format!(
                "unknown\trepository={}\tbranch={branch}\traw_commits={raw_commits}\treason=patch-classification-unavailable",
                repository.display()
            ));
            continue;
        };
        let patches = String::from_utf8_lossy(&cherry.stdout)
            .lines()
            .filter(|line| line.starts_with('+'))
            .count();
        let worktree = branch_worktrees
            .get(branch)
            .map_or_else(|| "-".to_owned(), |path| path.display().to_string());
        let prefix = format!(
            "repository={}\tworktree={worktree}\tbranch={branch}\traw_commits={raw_commits}\tpatches_not_in_main={patches}",
            repository.display()
        );
        if patches == 0 {
            rows.push(format!(
                "landed\t{prefix}\treview=cleanup-candidate-not-delete-proof"
            ));
            continue;
        }
        let upstream_ref = format!("refs/heads/{branch}");
        let upstream = git_output(
            repository,
            &["for-each-ref", "--format=%(upstream:short)", &upstream_ref],
        )
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_default();
        if upstream.is_empty() {
            rows.push(format!(
                "unpublished\t{prefix}\tpublication=unpushed-no-upstream"
            ));
            continue;
        }
        let ahead_range = format!("{upstream}..{reference}");
        let Some(ahead) = git_count(repository, &["rev-list", "--count", &ahead_range]) else {
            rows.push(format!(
                "unpublished\t{prefix}\tpublication=upstream-status-unknown"
            ));
            continue;
        };
        if ahead > 0 {
            rows.push(format!(
                "unpublished\t{prefix}\tpublication=unpushed-ahead-by-{ahead}"
            ));
            continue;
        }
        if local_only {
            continue;
        }
        let gh_directory = branch_worktrees
            .get(branch)
            .map_or(repository, PathBuf::as_path);
        let pr_status = command_output(Command::new("gh").current_dir(gh_directory).args([
            "pr",
            "list",
            "--head",
            branch,
            "--state",
            "all",
            "--limit",
            "100",
            "--json",
            "state,mergedAt",
        ]))
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| serde_json::from_slice::<Value>(&output.stdout).ok())
        .filter(Value::is_array);
        let Some(prs) = pr_status else {
            rows.push(format!(
                "unpublished\t{prefix}\tpublication=pr-status-unknown"
            ));
            continue;
        };
        let published = prs.as_array().is_some_and(|prs| {
            prs.iter().any(|pr| {
                matches!(string_value(pr.get("state")), "OPEN" | "MERGED")
                    || pr.get("mergedAt").is_some_and(|value| !value.is_null())
            })
        });
        if !published {
            rows.push(format!(
                "unpublished\t{prefix}\tpublication=pushed-no-open-pr-or-merge"
            ));
        }
    }
}

fn git_output(directory: &Path, args: &[&str]) -> Option<Output> {
    command_output(Command::new("git").arg("-C").arg(directory).args(args)).ok()
}

fn git_success(directory: &Path, args: &[&str]) -> bool {
    git_output(directory, args).is_some_and(|output| output.status.success())
}

fn git_count(directory: &Path, args: &[&str]) -> Option<u64> {
    let output = git_output(directory, args)?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

fn command_output(command: &mut Command) -> std::io::Result<Output> {
    command.output()
}

fn command_exists(name: &str) -> bool {
    let Some(path) = environment::PATH.value_os() else {
        return false;
    };
    env::split_paths(&path).any(|directory| directory.join(name).is_file())
}

fn string_value(value: Option<&Value>) -> &str {
    value.and_then(Value::as_str).unwrap_or("")
}

fn dash_if_empty(value: &str) -> &str {
    if value.is_empty() { "-" } else { value }
}

fn push_count(text: &mut String, label: &str, count: usize) {
    push_line(text, &format!("{label}\t{count}"));
}

fn push_line(text: &mut String, line: &str) {
    text.push_str(line);
    text.push('\n');
}
