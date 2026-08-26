use std::{
    env,
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use chrono::{DateTime, Duration, SecondsFormat, Utc};
use ostrom_core::{MandateConfig, ProjectMandate, Selector};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::{
    OstromPaths, environment,
    selector::{SelectorCandidate, glob_match, selector_match},
    sweep::load_config,
};

const QUERY_LIMIT: usize = 200;

#[derive(Debug, Clone)]
pub struct ReplayOptions {
    pub paths: OstromPaths,
    pub working_directory: PathBuf,
    pub days: u64,
    pub replay_time: DateTime<Utc>,
}

#[derive(Debug, Error)]
pub enum ReplayError {
    #[error("mandate replay: gh is required")]
    GhRequired,
    #[error("mandate replay: no mandates.yaml found at {user} or {repo}")]
    NotConfigured { user: String, repo: String },
    #[error("mandate replay: mandates.yaml contains no projects")]
    EmptyRoster,
    #[error("mandate replay: could not load mandates: {0}")]
    Config(String),
    #[error("usage: ostrom replay [days]")]
    InvalidDays,
    #[error("mandate replay: gh is not authenticated for {0}; run 'gh auth login'")]
    Authentication(String),
    #[error("mandate replay: cannot read sweep state at {path}: {detail}")]
    StateUnreadable { path: String, detail: String },
    #[error("mandate replay: malformed sweep state at {path}: {detail}")]
    StateMalformed { path: String, detail: String },
    #[error("mandate replay: cannot read selector events at {path}: {detail}")]
    EventsUnreadable { path: String, detail: String },
    #[error("mandate replay: malformed selector events at {path}: {detail}")]
    EventsMalformed { path: String, detail: String },
    #[error("mandate replay: failed to query merged PRs for {repo}{detail}")]
    Query { repo: String, detail: String },
    #[error("mandate replay: malformed merged-PR response for {repo}: {detail}")]
    Response { repo: String, detail: String },
    #[error(
        "mandate replay: merged-PR query for {repo} reached query_limit {limit}; refusing a truncated replay"
    )]
    QueryTruncated { repo: String, limit: usize },
}

impl ReplayError {
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::GhRequired => 1,
            Self::NotConfigured { .. }
            | Self::EmptyRoster
            | Self::Config(_)
            | Self::InvalidDays => 2,
            Self::Authentication(_) => 3,
            Self::StateUnreadable { .. }
            | Self::StateMalformed { .. }
            | Self::EventsUnreadable { .. }
            | Self::EventsMalformed { .. } => 4,
            Self::Query { .. } | Self::Response { .. } | Self::QueryTruncated { .. } => 5,
        }
    }
}

#[derive(Debug)]
struct ReplayMiss {
    id: String,
    title: String,
    merged: String,
    url: String,
    irreversible: Vec<String>,
}

#[derive(Debug)]
struct SelectorRecord<'a> {
    repo: Option<&'a str>,
    source: &'static str,
    selector: &'a Selector,
}

#[derive(Debug)]
struct CurrentItem {
    repo: String,
    matched_selector: Option<String>,
}

#[derive(Debug)]
struct Rejection {
    repo: String,
    matched_selector: Option<String>,
}

pub fn replay(options: &ReplayOptions) -> Result<String, ReplayError> {
    if !command_exists("gh") {
        return Err(ReplayError::GhRequired);
    }
    let user_path = options.paths.config.join("mandates.yaml");
    let repo_path = options.working_directory.join(".ostrom/mandates.yaml");
    if !user_path.exists() && !repo_path.exists() {
        return Err(ReplayError::NotConfigured {
            user: user_path.display().to_string(),
            repo: repo_path.display().to_string(),
        });
    }
    let config = load_config(&options.paths, &options.working_directory)
        .map_err(|error| ReplayError::Config(error.to_string()))?;
    if config.projects.is_empty() {
        return Err(ReplayError::EmptyRoster);
    }
    let host = environment::GH_HOST
        .value()
        .unwrap_or_else(|| "github.com".to_owned());
    if !Command::new("gh")
        .args(["auth", "status", "--hostname", &host])
        .output()
        .is_ok_and(|output| output.status.success())
    {
        return Err(ReplayError::Authentication(host));
    }

    let seconds = options
        .days
        .checked_mul(86_400)
        .and_then(|seconds| i64::try_from(seconds).ok())
        .ok_or(ReplayError::InvalidDays)?;
    let cutoff = options
        .replay_time
        .checked_sub_signed(Duration::seconds(seconds))
        .ok_or(ReplayError::InvalidDays)?
        .to_rfc3339_opts(SecondsFormat::Secs, true);
    let mut misses = Vec::new();
    for project in &config.projects {
        let repo = project.repo.as_str();
        let output = Command::new("gh")
            .args([
                "pr",
                "list",
                "--repo",
                repo,
                "--state",
                "merged",
                "--limit",
                // Derived from QUERY_LIMIT rather than repeated: the truncation
                // guard below compares against it, so a second literal here
                // could drift into either false truncation errors or silently
                // truncated replays.
                &QUERY_LIMIT.to_string(),
                "--json",
                "number,title,labels,url,files,baseRefName,mergedAt,closingIssuesReferences",
            ])
            .output()
            .map_err(|error| ReplayError::Query {
                repo: repo.to_owned(),
                detail: format!(": {error}"),
            })?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr)
                .replace('\n', " ")
                .trim()
                .to_owned();
            return Err(ReplayError::Query {
                repo: repo.to_owned(),
                detail: if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                },
            });
        }
        let response: Value =
            serde_json::from_slice(&output.stdout).map_err(|error| ReplayError::Response {
                repo: repo.to_owned(),
                detail: error.to_string(),
            })?;
        let pulls = response.as_array().ok_or_else(|| ReplayError::Response {
            repo: repo.to_owned(),
            detail: "response is not an array".to_owned(),
        })?;
        if pulls.len() >= QUERY_LIMIT {
            return Err(ReplayError::QueryTruncated {
                repo: repo.to_owned(),
                limit: QUERY_LIMIT,
            });
        }
        for pull in pulls {
            if let Some(miss) = replay_miss(&config, project, repo, pull, &cutoff)? {
                misses.push(miss);
            }
        }
    }
    misses.sort_by(|left, right| left.merged.cmp(&right.merged));

    let state_path = options.paths.sweep_state_file();
    let state = read_state(&state_path)?;
    let events_path = options.paths.selector_events_file();
    let events = read_events(&events_path)?;
    render_report(options.days, &config, &misses, &state, &events)
}

fn replay_miss(
    config: &MandateConfig,
    project: &ProjectMandate,
    repo: &str,
    pull: &Value,
    cutoff: &str,
) -> Result<Option<ReplayMiss>, ReplayError> {
    let number =
        pull.get("number")
            .and_then(Value::as_u64)
            .ok_or_else(|| ReplayError::Response {
                repo: repo.to_owned(),
                detail: "pull request has no numeric number".to_owned(),
            })?;
    let merged = string_value(pull.get("mergedAt"));
    if merged.is_empty() || merged < cutoff {
        return Ok(None);
    }
    let title = nonempty_string(pull.get("title")).unwrap_or("(title unavailable)");
    let linked = linked_issues(pull.get("closingIssuesReferences"));
    let mut labels = label_names(pull.get("labels"));
    for issue in &linked {
        labels.extend(label_names(issue.get("labels")));
    }
    labels.sort();
    labels.dedup();
    let mut refs = vec![number];
    refs.extend(
        linked
            .iter()
            .filter_map(|issue| issue.get("number").and_then(Value::as_u64)),
    );
    refs.sort_unstable();
    refs.dedup();
    let mut files = pull
        .get("files")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|file| nonempty_string(file.get("path")).map(str::to_owned))
        .collect::<Vec<_>>();
    files.sort();
    files.dedup();
    let candidate = SelectorCandidate {
        item_type: "pr",
        title,
        labels: &labels,
        refs: &refs,
        files: &files,
    };
    if config
        .bounce_all
        .iter()
        .chain(&project.bounce)
        .any(|selector| selector_match(&candidate, selector))
    {
        return Ok(None);
    }
    let irreversible = irreversible_reasons(&files);
    if irreversible.is_empty() {
        return Ok(None);
    }
    Ok(Some(ReplayMiss {
        id: format!("{repo}#{number}"),
        title: title.to_owned(),
        merged: merged.to_owned(),
        url: string_value(pull.get("url")).to_owned(),
        irreversible,
    }))
}

fn irreversible_reasons(files: &[String]) -> Vec<String> {
    let mut reasons = Vec::new();
    if let Some(path) = files.iter().find(|path| workflow_shaped(path)) {
        reasons.push(format!("workflow file: {path}"));
    }
    if let Some(path) = files.iter().find(|path| credential_shaped(path)) {
        reasons.push(format!("credential-shaped path: {path}"));
    }
    reasons
}

fn workflow_shaped(path: &str) -> bool {
    glob_match(path, ".github/workflows/**", true)
        || glob_match(path, "**/release*.y*ml", true)
        || glob_match(path, "**/.goreleaser*", true)
}

fn credential_shaped(path: &str) -> bool {
    [
        "**/*credential*",
        "**/*secret*",
        "**/*.pem",
        "**/*.key",
        "**/.env",
        "**/.env.*",
        "**/id_rsa*",
        "**/*.p12",
        "**/*.pfx",
    ]
    .iter()
    .any(|glob| glob_match(path, glob, true))
}

fn read_state(path: &Path) -> Result<Value, ReplayError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Value::Object(Map::new()));
        }
        Err(error) => {
            return Err(ReplayError::StateUnreadable {
                path: path.display().to_string(),
                detail: error.to_string(),
            });
        }
    };
    if bytes.is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    let state: Value =
        serde_json::from_slice(&bytes).map_err(|error| ReplayError::StateMalformed {
            path: path.display().to_string(),
            detail: error.to_string(),
        })?;
    if !state.is_object() {
        return Err(ReplayError::StateMalformed {
            path: path.display().to_string(),
            detail: "state is not an object".to_owned(),
        });
    }
    Ok(state)
}

fn read_events(path: &Path) -> Result<Vec<Value>, ReplayError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(ReplayError::EventsUnreadable {
                path: path.display().to_string(),
                detail: error.to_string(),
            });
        }
    };
    serde_json::Deserializer::from_slice(&bytes)
        .into_iter::<Value>()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| ReplayError::EventsMalformed {
            path: path.display().to_string(),
            detail: error.to_string(),
        })
}

fn render_report(
    days: u64,
    config: &MandateConfig,
    misses: &[ReplayMiss],
    state: &Value,
    events: &[Value],
) -> Result<String, ReplayError> {
    let current_items = current_items(state)?;
    let rejections = events
        .iter()
        .filter(|event| string_value(event.get("decision")) == "reject")
        .map(|event| Rejection {
            repo: string_value(event.get("id"))
                .split('#')
                .next()
                .unwrap_or_default()
                .to_owned(),
            matched_selector: event
                .get("matched_selector")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
        .collect::<Vec<_>>();
    let mut records = selector_records(config);
    records.sort_by(|left, right| {
        left.repo
            .unwrap_or("*")
            .cmp(right.repo.unwrap_or("*"))
            .then_with(|| left.source.cmp(right.source))
            .then_with(|| left.selector.as_str().cmp(right.selector.as_str()))
    });

    let mut output = String::new();
    writeln!(output, "mandate replay: lower bound only").expect("write String");
    writeln!(output, "A PR that touched nothing irreversible and matched no selector may still have been a miss; this only counts what it can see.").expect("write String");
    writeln!(output).expect("write String");
    writeln!(output, "MISSES (lower bound) — merged PRs in the last {days} days that touched an irreversible surface and matched no bounce selector:").expect("write String");
    if misses.is_empty() {
        writeln!(output, "  none flagged").expect("write String");
    } else {
        for miss in misses {
            writeln!(
                output,
                "{}  {} — merged {}; {} — {}",
                miss.id,
                miss.title,
                miss.merged,
                miss.irreversible.join("; "),
                miss.url
            )
            .expect("write String");
        }
    }
    writeln!(output).expect("write String");
    writeln!(output, "PER-SELECTOR REPORT").expect("write String");
    writeln!(output, "tier\trepo\tsource\tselector\tfired\tdismissed").expect("write String");
    for record in records {
        let repo = record.repo.unwrap_or("*");
        let selector = record.selector.as_str();
        let fired = current_items
            .iter()
            .filter(|item| record.repo.is_none_or(|expected| item.repo == expected))
            .filter(|item| item.matched_selector.as_deref() == Some(selector))
            .count();
        let dismissed = rejections
            .iter()
            .filter(|item| record.repo.is_none_or(|expected| item.repo == expected))
            .filter(|item| item.matched_selector.as_deref() == Some(selector))
            .count();
        writeln!(
            output,
            "{}\t{repo}\t{}\t{selector}\t{fired}\t{dismissed}",
            selector_tier(selector),
            record.source
        )
        .expect("write String");
    }
    writeln!(output).expect("write String");
    writeln!(output, "fired = open items the last sweep classified via that selector (current snapshot, not lifetime history).").expect("write String");
    writeln!(
        output,
        "dismissed = rejections recorded by `ostrom queue reject` of a row that selector produced, in selector-events.jsonl."
    )
    .expect("write String");
    writeln!(output, "path: only applies to PRs; for issues every prefix above is author-written and there is no content-derived gating at all.").expect("write String");
    writeln!(output).expect("write String");
    let no_selector_dismissals = rejections
        .iter()
        .filter(|rejection| {
            rejection
                .matched_selector
                .as_deref()
                .unwrap_or_default()
                .starts_with("default:")
        })
        .count();
    writeln!(output, "Dismissals attributed to no selector (the project default fired instead): {no_selector_dismissals}").expect("write String");
    writeln!(
        output,
        "These are not a selector's false alarms and are kept out of the table above."
    )
    .expect("write String");
    writeln!(output).expect("write String");
    writeln!(
        output,
        "Unmatched irreversible-surface merges (misses, lower bound): {}",
        misses.len()
    )
    .expect("write String");
    writeln!(output, "This is a separate count from the dismissal figures above — it measures misses, not false alarms, and is not combined with them into any single score.").expect("write String");
    Ok(output)
}

fn current_items(state: &Value) -> Result<Vec<CurrentItem>, ReplayError> {
    let Some(repos) = state.get("repos") else {
        return Ok(Vec::new());
    };
    if repos.is_null() {
        return Ok(Vec::new());
    }
    let repos = repos
        .as_object()
        .ok_or_else(|| ReplayError::StateMalformed {
            path: "state.json".to_owned(),
            detail: "repos is not an object".to_owned(),
        })?;
    let mut current = Vec::new();
    for (repo, repo_state) in repos {
        let Some(items) = repo_state.get("items") else {
            continue;
        };
        if items.is_null() {
            continue;
        }
        let items = items
            .as_object()
            .ok_or_else(|| ReplayError::StateMalformed {
                path: "state.json".to_owned(),
                detail: format!("items for {repo} is not an object"),
            })?;
        current.extend(items.values().map(|item| {
            CurrentItem {
                repo: repo.clone(),
                matched_selector: item
                    .get("matched_selector")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            }
        }));
    }
    Ok(current)
}

fn selector_records(config: &MandateConfig) -> Vec<SelectorRecord<'_>> {
    let mut records = config
        .bounce_all
        .iter()
        .map(|selector| SelectorRecord {
            repo: None,
            source: "bounce_all",
            selector,
        })
        .collect::<Vec<_>>();
    for project in &config.projects {
        for (selectors, source) in [
            (&project.bounce, "project bounce"),
            (&project.excluded, "excluded"),
            (&project.delegated, "delegated"),
        ] {
            records.extend(selectors.iter().map(|selector| SelectorRecord {
                repo: Some(project.repo.as_str()),
                source,
                selector,
            }));
        }
    }
    records
}

fn selector_tier(selector: &str) -> &'static str {
    if selector.starts_with("path:") || selector.starts_with("ref:") {
        "content-derived"
    } else {
        "author-written"
    }
}

fn label_names(labels: Option<&Value>) -> Vec<String> {
    let labels = match labels {
        Some(Value::Array(labels)) => labels.as_slice(),
        Some(Value::Object(labels)) => labels
            .get("nodes")
            .and_then(Value::as_array)
            .map_or(&[] as &[Value], Vec::as_slice),
        _ => &[],
    };
    labels
        .iter()
        .filter_map(|label| nonempty_string(label.get("name")).map(str::to_owned))
        .collect()
}

fn linked_issues(value: Option<&Value>) -> Vec<&Value> {
    match value {
        Some(Value::Array(issues)) => issues.iter().collect(),
        Some(Value::Object(connection)) => connection
            .get("nodes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .collect(),
        _ => Vec::new(),
    }
}

fn string_value(value: Option<&Value>) -> &str {
    value.and_then(Value::as_str).unwrap_or_default()
}

fn nonempty_string(value: Option<&Value>) -> Option<&str> {
    let value = string_value(value);
    (!value.is_empty()).then_some(value)
}

/// Being a regular file is not enough — a non-executable `gh` on PATH would
/// pass this and then fail downstream as a confusing query error rather than
/// the intended "gh is required". This is the third time the same defect has
/// appeared in this codebase (#286 in the pass guard, #289 in dispatch
/// resolution), so it reuses the one helper rather than writing a third test.
fn command_exists(name: &str) -> bool {
    environment::PATH.value_os().is_some_and(|path| {
        env::split_paths(&path)
            .map(|directory| directory.join(name))
            .any(|candidate| crate::pass::is_executable_file(&candidate))
    })
}
