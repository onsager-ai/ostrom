use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Output},
};

use ostrom_core::{GateConfig, GateProject, GateSelector, PolicyManifest, sha256_hex};
use regex::{Regex, RegexBuilder};
use serde_json::{Value, json};
use thiserror::Error;

use crate::{
    OstromPaths, policy_manifest_digest, read_commit_checks, set_private_file_mode,
    sweep::merge_yaml,
};

const SHIPPED_DEFAULTS: &str = include_str!("../../../plugins/ostrom/config/gate.defaults.yaml");
const REVIEW_QUERY: &str = "query($owner:String!, $repo:String!, $number:Int!, $cursor:String) {\n  repository(owner:$owner, name:$repo) {\n    pullRequest(number:$number) {\n      author { login }\n      reviewThreads(first:100, after:$cursor) {\n        nodes {\n          id\n          isResolved\n          resolvedBy { login }\n          comments(last:1) { nodes { author { login } } }\n        }\n        pageInfo { hasNextPage endCursor }\n      }\n    }\n  }\n}";

#[derive(Debug, Clone)]
pub struct GateOptions {
    pub paths: OstromPaths,
    pub working_directory: PathBuf,
    pub target: String,
    pub timestamp: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

#[derive(Debug, Error)]
pub enum GateError {
    #[error("usage: ostrom gate <owner/repo#number>")]
    InvalidTarget,
    #[error("mandate gate: could not serialize verdict")]
    Serialize,
}

impl GateError {
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::InvalidTarget => 64,
            Self::Serialize => 2,
        }
    }
}

#[derive(Debug)]
struct Target<'a> {
    full: &'a str,
    repo: &'a str,
    owner: &'a str,
    repo_name: &'a str,
    number: u64,
}

#[derive(Debug)]
struct Acquisition {
    metadata_ready: bool,
    metadata: Value,
    metadata_error: String,
    head_sha: String,
    checks_ready: bool,
    checks: Vec<Value>,
    checks_error: String,
    checks_partial_error: String,
    diff_ready: bool,
    paths: Vec<String>,
    diff_error: String,
    diff_content_ready: bool,
    diff_content: String,
    diff_content_error: String,
    threads_ready: bool,
    threads: Vec<Value>,
    threads_error: String,
    thread_author: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum JudgmentState {
    Judged,
    NotJudged,
    CannotTell(String),
}

impl JudgmentState {
    const fn as_str(&self) -> &'static str {
        match self {
            Self::Judged => "judged",
            Self::NotJudged => "not-judged",
            Self::CannotTell(_) => "cannot-tell",
        }
    }
}

pub fn run_gate(options: &GateOptions) -> Result<GateOutput, GateError> {
    let target = parse_target(&options.target)?;
    let (config, config_error, config_source) =
        load_gate_config_for_repo(&options.paths, &options.working_directory, target.repo);

    let metadata_output = match gh(&[
        "pr",
        "view",
        &target.number.to_string(),
        "--repo",
        target.repo,
        "--json",
        "number,title,author,headRefOid,labels,closingIssuesReferences,mergeable,isDraft",
    ]) {
        Ok(output) => output,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(requirement_failure(&target, "gh"));
        }
        Err(error) => synthetic_output(error.to_string()),
    };

    let mut acquisition = acquire_metadata(metadata_output, target.number);
    acquire_checks(&mut acquisition, &target);
    acquire_paths(&mut acquisition, &target);
    if config.as_ref().is_some_and(config_needs_diff_content) {
        acquire_diff_content(&mut acquisition, &target);
    }
    acquire_threads(&mut acquisition, &target);

    let mut conditions = if let Some(config) = config.as_ref() {
        evaluate_conditions(config, &acquisition, &target)
    } else {
        unavailable_conditions(&config_error)
    };
    apply_shipped_manifest_bounce(&mut conditions, &acquisition);

    let mut stderr = config_source.map_or_else(String::new, |source| {
        format!("mandate gate: policy source={source}\n")
    });
    apply_exceptions(
        &mut conditions,
        &options.paths.state.join("exceptions.jsonl"),
        &target,
        &acquisition.head_sha,
        &mut stderr,
    );
    let verdict = aggregate(&conditions);
    let gate_path = options.paths.state.join("gate.jsonl");
    let judgment_digest = judgment_digest(target.full, verdict, &conditions)?;
    let already_judged = already_judged(
        &gate_path,
        target.full,
        &acquisition.head_sha,
        &judgment_digest,
    );
    if let JudgmentState::CannotTell(reason) = &already_judged {
        stderr.push_str(&format!("mandate gate: {reason}\n"));
    }
    let record = json!({
        "ts": options.timestamp,
        "pr": target.full,
        "head_sha": if acquisition.head_sha.is_empty() {
            Value::Null
        } else {
            Value::String(acquisition.head_sha.clone())
        },
        "evidence": !acquisition.head_sha.is_empty(),
        "judgment_digest": judgment_digest,
        "verdict": verdict,
        "already_judged": already_judged.as_str(),
        "conditions": conditions,
    });

    // A null-SHA row cannot identify the artifact that was judged, so it is
    // durable delivery memory but never merge evidence. Every evidence reader
    // filters the marker explicitly before joining by SHA.
    if append_record(&gate_path, &record).is_err() {
        let reported_state = match already_judged {
            JudgmentState::NotJudged => JudgmentState::CannotTell(format!(
                "could not append judgment history at {}",
                gate_path.display()
            )),
            state => state,
        };
        return Ok(GateOutput {
            stdout: format!(
                "verdict: inconclusive pr={} head_sha={} already_judged={}\n",
                target.full,
                if acquisition.head_sha.is_empty() {
                    "unknown"
                } else {
                    &acquisition.head_sha
                },
                reported_state.as_str(),
            ),
            stderr: format!(
                "{stderr}mandate gate: could not append {}\n",
                gate_path.display()
            ),
            exit_code: 2,
        });
    }

    Ok(GateOutput {
        stdout: render_record(&record)?,
        stderr,
        exit_code: verdict_exit(verdict),
    })
}

fn parse_target(value: &str) -> Result<Target<'_>, GateError> {
    if value.chars().any(char::is_whitespace) {
        return Err(GateError::InvalidTarget);
    }
    let (repo, number_text) = value.rsplit_once('#').ok_or(GateError::InvalidTarget)?;
    let mut parts = repo.split('/');
    let (Some(owner), Some(repo_name), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(GateError::InvalidTarget);
    };
    if owner.is_empty() || repo_name.is_empty() || owner.contains('#') || repo_name.contains('#') {
        return Err(GateError::InvalidTarget);
    }
    if number_text.starts_with('0') || !number_text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(GateError::InvalidTarget);
    }
    let number = number_text
        .parse::<u64>()
        .map_err(|_| GateError::InvalidTarget)?;
    Ok(Target {
        full: value,
        repo,
        owner,
        repo_name,
        number,
    })
}

fn requirement_failure(target: &Target<'_>, command: &str) -> GateOutput {
    GateOutput {
        stdout: format!(
            "verdict: inconclusive pr={} head_sha=unknown already_judged=cannot-tell\n",
            target.full
        ),
        stderr: format!("mandate gate: {command} is required\n"),
        exit_code: 2,
    }
}

fn synthetic_output(message: String) -> Output {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        Output {
            status: std::process::ExitStatus::from_raw(1 << 8),
            stdout: Vec::new(),
            stderr: message.into_bytes(),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = message;
        unreachable!("the Ostrom CLI currently targets Unix hosts")
    }
}

fn gh(arguments: &[&str]) -> io::Result<Output> {
    Command::new("gh").args(arguments).output()
}

fn acquire_metadata(output: Output, expected_number: u64) -> Acquisition {
    let parsed = serde_json::from_slice::<Value>(&output.stdout).ok();
    let head_sha = parsed
        .as_ref()
        .and_then(|value| value.get("headRefOid"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let metadata_ready = output.status.success()
        && parsed
            .as_ref()
            .is_some_and(|value| valid_metadata(value, expected_number));
    let metadata_error = if metadata_ready {
        error_text(&output.stderr)
    } else if output.status.success() {
        "pull request metadata was incomplete".to_owned()
    } else {
        error_text(&output.stderr)
    };
    Acquisition {
        metadata_ready,
        metadata: if metadata_ready {
            parsed.expect("ready metadata was parsed")
        } else {
            json!({})
        },
        metadata_error,
        head_sha,
        checks_ready: false,
        checks: Vec::new(),
        checks_error: String::new(),
        checks_partial_error: String::new(),
        diff_ready: false,
        paths: Vec::new(),
        diff_error: String::new(),
        diff_content_ready: false,
        diff_content: String::new(),
        diff_content_error: "diff content was not requested".to_owned(),
        threads_ready: false,
        threads: Vec::new(),
        threads_error: String::new(),
        thread_author: String::new(),
    }
}

fn valid_metadata(value: &Value, expected_number: u64) -> bool {
    value.as_object().is_some()
        && value.get("number").and_then(Value::as_u64) == Some(expected_number)
        && value.get("title").and_then(Value::as_str).is_some()
        && value
            .pointer("/author/login")
            .and_then(Value::as_str)
            .is_some_and(|login| !login.is_empty())
        && value
            .get("headRefOid")
            .and_then(Value::as_str)
            .is_some_and(|sha| !sha.is_empty())
        && value.get("labels").and_then(Value::as_array).is_some()
        && value
            .get("closingIssuesReferences")
            .and_then(Value::as_array)
            .is_some()
        && value.get("mergeable").and_then(Value::as_str).is_some()
        && value.get("isDraft").and_then(Value::as_bool).is_some()
}

fn acquire_checks(acquisition: &mut Acquisition, target: &Target<'_>) {
    if acquisition.head_sha.is_empty() {
        acquisition.checks_error = "pull request head SHA was unavailable".to_owned();
        return;
    }
    let result = read_commit_checks(target.repo, &acquisition.head_sha, |endpoint| {
        let output = gh(&["api", endpoint]).map_err(|error| error.to_string())?;
        if output.status.success() {
            Ok(output.stdout)
        } else {
            Err(error_text(&output.stderr))
        }
    });
    match result {
        Ok(result) => {
            acquisition.checks_ready = true;
            acquisition.checks = result.checks;
            acquisition.checks_error.clear();
            acquisition.checks_partial_error = result
                .statuses_error
                .map_or_else(String::new, |error| error.to_string());
        }
        Err(error) => acquisition.checks_error = error.to_string(),
    }
}

fn acquire_paths(acquisition: &mut Acquisition, target: &Target<'_>) {
    let output = gh(&[
        "pr",
        "diff",
        &target.number.to_string(),
        "--repo",
        target.repo,
        "--name-only",
    ]);
    match output {
        Ok(output) if output.status.success() => {
            acquisition.paths = String::from_utf8_lossy(&output.stdout)
                .split('\n')
                .filter(|path| !path.is_empty())
                .map(str::to_owned)
                .collect();
            acquisition.diff_ready = true;
        }
        Ok(output) => acquisition.diff_error = error_text(&output.stderr),
        Err(error) => acquisition.diff_error = error.to_string(),
    }
}

fn acquire_diff_content(acquisition: &mut Acquisition, target: &Target<'_>) {
    let output = gh(&[
        "pr",
        "diff",
        &target.number.to_string(),
        "--repo",
        target.repo,
    ]);
    match output {
        Ok(output) if output.status.success() => {
            acquisition.diff_content = String::from_utf8_lossy(&output.stdout).into_owned();
            acquisition.diff_content_ready = true;
        }
        Ok(output) => acquisition.diff_content_error = error_text(&output.stderr),
        Err(error) => acquisition.diff_content_error = error.to_string(),
    }
}

fn acquire_threads(acquisition: &mut Acquisition, target: &Target<'_>) {
    let mut cursor = String::new();
    for _ in 0..100 {
        let number = target.number.to_string();
        let mut arguments = vec![
            "api".to_owned(),
            "graphql".to_owned(),
            "-f".to_owned(),
            format!("query={REVIEW_QUERY}"),
            "-f".to_owned(),
            format!("owner={}", target.owner),
            "-f".to_owned(),
            format!("repo={}", target.repo_name),
            "-F".to_owned(),
            format!("number={number}"),
        ];
        if !cursor.is_empty() {
            arguments.extend(["-f".to_owned(), format!("cursor={cursor}")]);
        }
        let output = match Command::new("gh").args(&arguments).output() {
            Ok(output) => output,
            Err(error) => {
                acquisition.threads_error = error.to_string();
                return;
            }
        };
        if !output.status.success() {
            acquisition.threads_error = error_text(&output.stderr);
            return;
        }
        let Ok(page) = serde_json::from_slice::<Value>(&output.stdout) else {
            acquisition.threads_error =
                "review-thread query returned missing data or an API error".to_owned();
            return;
        };
        if !valid_thread_page(&page) {
            acquisition.threads_error =
                "review-thread query returned missing data or an API error".to_owned();
            return;
        }
        let page_author = page
            .pointer("/data/repository/pullRequest/author/login")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if acquisition.thread_author.is_empty() {
            acquisition.thread_author = page_author.to_owned();
        } else if page_author != acquisition.thread_author {
            acquisition.threads_error =
                "pull request author changed during review-thread pagination".to_owned();
            return;
        }
        acquisition.threads.extend(
            page.pointer("/data/repository/pullRequest/reviewThreads/nodes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .cloned(),
        );
        let page_info = &page["data"]["repository"]["pullRequest"]["reviewThreads"]["pageInfo"];
        if page_info["hasNextPage"].as_bool() == Some(false) {
            acquisition.threads_ready = true;
            return;
        }
        let next = page_info["endCursor"].as_str().unwrap_or_default();
        if next.is_empty() || next == cursor {
            acquisition.threads_error =
                "review-thread pagination did not return a new cursor".to_owned();
            return;
        }
        cursor = next.to_owned();
    }
    acquisition.threads_error = "review-thread query exceeded 100 pages".to_owned();
}

fn valid_thread_page(page: &Value) -> bool {
    if page
        .get("errors")
        .and_then(Value::as_array)
        .is_some_and(|errors| !errors.is_empty())
    {
        return false;
    }
    let Some(pull) = page.pointer("/data/repository/pullRequest") else {
        return false;
    };
    let Some(author) = pull
        .pointer("/author/login")
        .and_then(Value::as_str)
        .filter(|author| !author.is_empty())
    else {
        return false;
    };
    let _ = author;
    let Some(nodes) = pull
        .pointer("/reviewThreads/nodes")
        .and_then(Value::as_array)
    else {
        return false;
    };
    if pull
        .pointer("/reviewThreads/pageInfo/hasNextPage")
        .and_then(Value::as_bool)
        .is_none()
    {
        return false;
    }
    nodes.iter().all(|node| {
        node.get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| !id.is_empty())
            && node.get("isResolved").and_then(Value::as_bool).is_some()
            && valid_resolver(node.get("resolvedBy"))
            && node
                .pointer("/comments/nodes")
                .and_then(Value::as_array)
                .is_some_and(|comments| comments.iter().all(valid_comment))
    })
}

fn valid_resolver(value: Option<&Value>) -> bool {
    value.is_none_or(|value| {
        value.is_null()
            || value
                .get("login")
                .and_then(Value::as_str)
                .is_some_and(|login| !login.is_empty())
    })
}

fn valid_comment(comment: &Value) -> bool {
    comment.get("author").is_none_or(|author| {
        author.is_null()
            || author
                .get("login")
                .and_then(Value::as_str)
                .is_some_and(|login| !login.is_empty())
    })
}

fn error_text(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "no error detail returned".to_owned();
    }
    let flattened = bytes
        .iter()
        .map(|byte| if *byte == b'\n' { b' ' } else { *byte })
        .take(500)
        .collect::<Vec<_>>();
    String::from_utf8_lossy(&flattened).into_owned()
}

/// Whether a gate journal row may be joined as merge evidence.
///
/// New rows carry `evidence`, written as `!head_sha.is_empty()`. Rows written
/// before that field existed carry no marker, and for those the head SHA *is*
/// the marker — it was recorded exactly when the judgment identified an
/// artifact. Reading a missing field as "not evidence" would silently retire
/// the entire journal: the live host holds 4,519 rows, none of them marked,
/// and 4,422 of them carrying a SHA. The merge-gate-fault check derives its
/// epoch floor from these rows and skips every merge when the floor is absent,
/// so it would go dark rather than loud — the same class of defect this change
/// exists to fix.
pub(crate) fn is_merge_evidence(record: &Value) -> bool {
    match record.get("evidence") {
        Some(marker) => marker.as_bool() == Some(true),
        None => record
            .get("head_sha")
            .and_then(Value::as_str)
            .is_some_and(|sha| !sha.is_empty()),
    }
}

pub(crate) fn load_gate_config(paths: &OstromPaths, cwd: &Path) -> Result<GateConfig, String> {
    load_gate_config_with_source(paths, cwd).map(|(config, _)| config)
}

fn load_gate_config_with_source(
    paths: &OstromPaths,
    cwd: &Path,
) -> Result<(GateConfig, Option<String>), String> {
    if let Some((manifest, digest)) = load_current_manifest(paths)? {
        let config = GateConfig::from_manifest(&manifest)
            .map_err(|error| truncate_error(&error.to_string()))?;
        return Ok((config, Some(format!("manifest digest={digest}"))));
    }

    let user_path = paths.config.join("gate.yaml");
    let repo_path = cwd.join(".ostrom/gate.yaml");
    let mut merged = match serde_yaml::from_str::<serde_yaml::Value>(SHIPPED_DEFAULTS) {
        Ok(value) => value,
        Err(error) => return Err(error.to_string()),
    };
    for path in [&user_path, &repo_path] {
        if path.exists() {
            let overlay = fs::read_to_string(path)
                .map_err(|error| error.to_string())
                .and_then(|text| {
                    serde_yaml::from_str::<serde_yaml::Value>(&text)
                        .map_err(|error| error.to_string())
                });
            match overlay {
                Ok(overlay) => merge_yaml(&mut merged, overlay),
                Err(error) => return Err(truncate_error(&error)),
            }
        }
    }
    let serialized = match serde_yaml::to_string(&merged) {
        Ok(value) => value,
        Err(error) => return Err(error.to_string()),
    };
    let mut config =
        GateConfig::from_yaml(&serialized).map_err(|error| truncate_error(&error.to_string()))?;
    for path in ["path:ostrom.yaml", "path:ostrom.yml"] {
        if !config
            .bounce_all
            .iter()
            .any(|selector| selector.as_str() == path)
        {
            config
                .bounce_all
                .push(GateSelector::new(path).map_err(|error| truncate_error(&error.to_string()))?);
        }
    }
    Ok((config, None))
}

fn load_current_manifest(paths: &OstromPaths) -> Result<Option<(PolicyManifest, String)>, String> {
    const MATERIALIZED_MANIFEST: &str = "ostrom.yaml";

    let current = paths.current_policy_version();
    let target = match fs::read_link(&current) {
        Ok(target) => target,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
            return Err(format!(
                "current policy refused: current_target_invalid path={}",
                current.display()
            ));
        }
        Err(_) => {
            return Err(format!(
                "current policy refused: current_unreadable path={}",
                current.display()
            ));
        }
    };
    let mut components = target.components();
    let valid_root =
        matches!(components.next(), Some(Component::Normal(part)) if part == "versions");
    let digest = match components.next() {
        Some(Component::Normal(digest)) => digest.to_str().map(str::to_owned),
        _ => None,
    };
    let digest = digest.filter(|digest| valid_policy_digest(digest));
    if !valid_root || components.next().is_some() || digest.is_none() || target.is_absolute() {
        return Err(format!(
            "current policy refused: current_target_invalid path={}",
            current.display()
        ));
    }
    let digest = digest.expect("validated current policy digest is present");
    let directory = paths.state.join(&target);
    match fs::metadata(&directory) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(format!(
                "current policy refused: version_not_directory path={}",
                directory.display()
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(format!(
                "current policy refused: version_missing path={}",
                directory.display()
            ));
        }
        Err(_) => {
            return Err(format!(
                "current policy refused: version_unreadable path={}",
                directory.display()
            ));
        }
    }
    let path = directory.join(MATERIALIZED_MANIFEST);
    let bytes = fs::read(&path).map_err(|error| {
        format!(
            "current policy refused: {} path={}",
            if error.kind() == io::ErrorKind::NotFound {
                "manifest_missing"
            } else {
                "manifest_unreadable"
            },
            path.display()
        )
    })?;
    let manifest = std::str::from_utf8(&bytes)
        .ok()
        .and_then(|source| PolicyManifest::parse_yaml(source).ok())
        .ok_or_else(|| "current policy refused: current_drift drift=ostrom.yaml".to_owned())?;
    let observed_digest = policy_manifest_digest(&manifest).map_err(|_| {
        format!(
            "current policy refused: digest_unavailable path={}",
            path.display()
        )
    })?;
    let canonical = manifest.to_yaml().map_err(|_| {
        format!(
            "current policy refused: canonical_form_unavailable path={}",
            path.display()
        )
    })?;
    if observed_digest != digest || bytes != canonical.as_bytes() {
        return Err("current policy refused: current_drift drift=ostrom.yaml".to_owned());
    }
    Ok(Some((manifest, digest)))
}

fn valid_policy_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn load_gate_config_for_repo(
    paths: &OstromPaths,
    cwd: &Path,
    repo: &str,
) -> (Option<GateConfig>, String, Option<String>) {
    let user_path = paths.config.join("gate.yaml");
    let repo_path = cwd.join(".ostrom/gate.yaml");
    let current_present = match fs::symlink_metadata(paths.current_policy_version()) {
        Ok(_) => true,
        Err(error) => error.kind() != io::ErrorKind::NotFound,
    };
    if !current_present && !user_path.exists() && !repo_path.exists() {
        return (
            None,
            format!(
                "no gate.yaml found at {} or {}",
                user_path.display(),
                repo_path.display()
            ),
            None,
        );
    }
    let (config, source) = match load_gate_config_with_source(paths, cwd) {
        Ok(config) => config,
        Err(error) => return (None, error, None),
    };
    if config
        .projects
        .iter()
        .filter(|project| project.repo.as_str() == repo)
        .count()
        != 1
    {
        let source_name = if source.is_some() {
            "composed manifest"
        } else {
            "gate.yaml"
        };
        return (
            None,
            format!("{source_name} has no project entry for {repo}"),
            source,
        );
    }
    (Some(config), String::new(), source)
}

fn truncate_error(error: &str) -> String {
    error.replace('\n', " ").chars().take(500).collect()
}

fn config_needs_diff_content(config: &GateConfig) -> bool {
    config
        .bounce_all
        .iter()
        .chain(config.projects.iter().flat_map(|project| &project.bounce))
        .any(|selector| selector.as_str().starts_with("substance:"))
}

fn unavailable_conditions(reason: &str) -> Vec<Value> {
    let plain = || json!({"reason": reason});
    vec![
        condition(
            "mergeable",
            "inconclusive",
            &[],
            json!({"mergeable": null, "reason": reason}),
        ),
        condition(
            "draft",
            "inconclusive",
            &[],
            json!({"isDraft": null, "reason": reason}),
        ),
        condition("required_checks", "inconclusive", &[], plain()),
        condition("review_threads", "inconclusive", &[], plain()),
        condition("bounce_selectors", "inconclusive", &[], plain()),
        condition("reserved_refs", "inconclusive", &[], plain()),
    ]
}

fn apply_shipped_manifest_bounce(conditions: &mut [Value], acquisition: &Acquisition) {
    if !acquisition.diff_ready {
        return;
    }
    let matches = acquisition
        .paths
        .iter()
        .filter(|path| matches!(path.as_str(), "ostrom.yaml" | "ostrom.yml"))
        .map(|path| {
            json!({
                "selector": format!("path:{path}"),
                "tier": "content-derived",
            })
        })
        .collect::<Vec<_>>();
    if matches.is_empty() {
        return;
    }
    if let Some(slot) = conditions
        .iter_mut()
        .find(|condition| condition["name"] == "bounce_selectors")
    {
        *slot = condition(
            "bounce_selectors",
            "fail",
            &["content-derived"],
            json!({"matches": matches, "unobservable": []}),
        );
    }
}

fn evaluate_conditions(
    config: &GateConfig,
    acquisition: &Acquisition,
    target: &Target<'_>,
) -> Vec<Value> {
    let project = config
        .projects
        .iter()
        .find(|project| project.repo.as_str() == target.repo)
        .expect("configuration was resolved for the target repository");
    vec![
        evaluate_mergeable(acquisition),
        evaluate_draft(acquisition),
        evaluate_checks(project, acquisition),
        evaluate_threads(acquisition),
        evaluate_bounce(config, project, acquisition, target),
        evaluate_reserved(project, acquisition, target),
    ]
}

fn condition(name: &str, result: &str, tier: &[&str], detail: Value) -> Value {
    json!({"name": name, "result": result, "tier": tier, "detail": detail})
}

fn evaluate_mergeable(acquisition: &Acquisition) -> Value {
    if !acquisition.metadata_ready {
        return condition(
            "mergeable",
            "inconclusive",
            &["content-derived"],
            json!({"mergeable": null, "reason": acquisition.metadata_error}),
        );
    }
    let mergeable = acquisition.metadata["mergeable"]
        .as_str()
        .unwrap_or_default();
    let result = match mergeable {
        "MERGEABLE" => "pass",
        "CONFLICTING" => "fail",
        _ => "inconclusive",
    };
    condition(
        "mergeable",
        result,
        &["content-derived"],
        json!({"mergeable": mergeable}),
    )
}

fn evaluate_draft(acquisition: &Acquisition) -> Value {
    if !acquisition.metadata_ready {
        return condition(
            "draft",
            "inconclusive",
            &["content-derived"],
            json!({"isDraft": null, "reason": acquisition.metadata_error}),
        );
    }
    let draft = acquisition.metadata["isDraft"].as_bool().unwrap_or(false);
    condition(
        "draft",
        if draft { "fail" } else { "pass" },
        &["content-derived"],
        json!({"isDraft": draft}),
    )
}

fn evaluate_checks(project: &GateProject, acquisition: &Acquisition) -> Value {
    if !acquisition.checks_ready {
        return condition(
            "required_checks",
            "inconclusive",
            &["content-derived"],
            json!({"reason": acquisition.checks_error}),
        );
    }
    let mut selected = Vec::new();
    for selector in &project.required_checks {
        let matches = acquisition
            .checks
            .iter()
            .filter(|check| glob_match(check_name(check), selector, false))
            .map(|check| json!({"name": check_name(check), "state": check_state(check)}))
            .collect::<Vec<_>>();
        let result = if matches.is_empty()
            || matches
                .iter()
                .any(|check| known_failure(check["state"].as_str().unwrap_or_default()))
        {
            "fail"
        } else if matches
            .iter()
            .any(|check| pending(check["state"].as_str().unwrap_or_default()))
        {
            "pending"
        } else if matches.iter().any(|check| {
            let state = check["state"].as_str().unwrap_or_default();
            !green(state) && !known_failure(state) && !pending(state)
        }) {
            "inconclusive"
        } else {
            "pass"
        };
        selected.push(json!({"selector": selector, "result": result, "matches": matches}));
    }
    let result = if selected.iter().any(|value| value["result"] == "fail") {
        "fail"
    } else if selected
        .iter()
        .any(|value| matches!(value["result"].as_str(), Some("pending" | "inconclusive")))
    {
        "inconclusive"
    } else {
        "pass"
    };
    let tier = if project.required_checks.is_empty() {
        Vec::new()
    } else {
        vec!["content-derived"]
    };
    let mut detail = json!({"selectors": selected});
    if !acquisition.checks_partial_error.is_empty() {
        detail["partial_read"] = json!({"statuses": acquisition.checks_partial_error});
    }
    condition("required_checks", result, &tier, detail)
}

fn check_name(check: &Value) -> &str {
    check
        .get("name")
        .or_else(|| check.get("context"))
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn check_state(check: &Value) -> String {
    check
        .get("conclusion")
        .filter(|value| !value.is_null())
        .or_else(|| check.get("state"))
        .or_else(|| check.get("status"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_uppercase()
}

fn green(state: &str) -> bool {
    matches!(state, "SUCCESS" | "NEUTRAL" | "SKIPPED")
}

fn known_failure(state: &str) -> bool {
    matches!(
        state,
        "FAILURE" | "ERROR" | "CANCELLED" | "TIMED_OUT" | "ACTION_REQUIRED" | "STALE"
    )
}

fn pending(state: &str) -> bool {
    matches!(
        state,
        "PENDING" | "EXPECTED" | "QUEUED" | "IN_PROGRESS" | "WAITING" | "REQUESTED"
    )
}

fn evaluate_threads(acquisition: &Acquisition) -> Value {
    if !acquisition.threads_ready {
        return condition(
            "review_threads",
            "inconclusive",
            &["content-derived"],
            json!({"reason": acquisition.threads_error}),
        );
    }
    let author = acquisition.thread_author.to_ascii_lowercase();
    let open = acquisition
        .threads
        .iter()
        .filter(|thread| thread["isResolved"].as_bool() == Some(false))
        .collect::<Vec<_>>();
    let unresolved = open.len();
    // Answered is diagnostic only. An author's reply is the same unverified
    // self-assertion as an author's resolution, so neither can clear the work.
    let answered = open
        .iter()
        .filter(|thread| {
            thread
                .pointer("/comments/nodes")
                .and_then(Value::as_array)
                .and_then(|comments| comments.last())
                .and_then(|comment| comment.pointer("/author/login"))
                .and_then(Value::as_str)
                .is_some_and(|login| !login.is_empty() && login.to_ascii_lowercase() == author)
        })
        .count();
    let by_author = acquisition
        .threads
        .iter()
        .filter(|thread| {
            thread["isResolved"].as_bool() == Some(true)
                && thread
                    .pointer("/resolvedBy/login")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_ascii_lowercase()
                    == author
        })
        .count();
    let missing_resolver = acquisition
        .threads
        .iter()
        .filter(|thread| {
            thread["isResolved"].as_bool() == Some(true)
                && thread
                    .pointer("/resolvedBy/login")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .is_empty()
        })
        .count();
    let result = if unresolved > 0 || by_author > 0 {
        "fail"
    } else if missing_resolver > 0 || author.is_empty() {
        "inconclusive"
    } else {
        "pass"
    };
    condition(
        "review_threads",
        result,
        &["content-derived"],
        json!({
            "unresolved": unresolved,
            "answered": answered,
            "unanswered": unresolved - answered,
            "resolved_by_pr_author": by_author,
            "resolved_with_missing_resolver": missing_resolver,
        }),
    )
}

fn evaluate_bounce(
    config: &GateConfig,
    project: &GateProject,
    acquisition: &Acquisition,
    target: &Target<'_>,
) -> Value {
    let mut matches = Vec::new();
    let mut unobservable = Vec::new();
    for selector in config.bounce_all.iter().chain(&project.bounce) {
        let (prefix, pattern) = selector
            .as_str()
            .split_once(':')
            .expect("gate selectors are validated");
        let tier = selector_tier(prefix);
        let observable = match prefix {
            "path" => acquisition.diff_ready,
            "substance" => pattern == "fly-spend" && acquisition.diff_content_ready,
            _ => acquisition.metadata_ready,
        };
        if !observable {
            let error = match prefix {
                "path" => acquisition.diff_error.clone(),
                "substance" if pattern != "fly-spend" => {
                    format!("unknown substance predicate: {pattern}")
                }
                "substance" => acquisition.diff_content_error.clone(),
                _ => acquisition.metadata_error.clone(),
            };
            unobservable.push(json!({"selector": selector.as_str(), "tier": tier, "error": error}));
        } else if selector_matches(selector, pattern, acquisition, target) {
            matches.push(json!({"selector": selector.as_str(), "tier": tier}));
        }
    }
    let result = if !matches.is_empty() {
        "fail"
    } else if !unobservable.is_empty() {
        "inconclusive"
    } else {
        "pass"
    };
    let tiers = if !matches.is_empty() {
        unique_tiers(&matches)
    } else if !unobservable.is_empty() {
        unique_tiers(&unobservable)
    } else {
        Vec::new()
    };
    condition(
        "bounce_selectors",
        result,
        &tiers,
        json!({"matches": matches, "unobservable": unobservable}),
    )
}

fn selector_tier(prefix: &str) -> &'static str {
    if matches!(prefix, "path" | "ref" | "substance") {
        "content-derived"
    } else {
        "author-written"
    }
}

fn unique_tiers(values: &[Value]) -> Vec<&str> {
    values
        .iter()
        .filter_map(|value| value["tier"].as_str())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn selector_matches(
    selector: &GateSelector,
    pattern: &str,
    acquisition: &Acquisition,
    target: &Target<'_>,
) -> bool {
    let prefix = selector
        .as_str()
        .split_once(':')
        .map_or("", |(prefix, _)| prefix);
    let title = acquisition.metadata["title"].as_str().unwrap_or_default();
    match prefix {
        "label" => acquisition.metadata["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|label| label["name"].as_str())
            .any(|label| glob_match(label, pattern, false)),
        "scope" => conventional(title)
            .1
            .iter()
            .any(|scope| glob_match(scope, pattern, false)),
        "type" => glob_match(&conventional(title).0, pattern, false),
        "path" => acquisition
            .paths
            .iter()
            .any(|path| glob_match(path, pattern, true)),
        "ref" => refs(acquisition, target)
            .iter()
            .any(|number| format!("#{number}") == pattern),
        "title" => glob_match(title, pattern, false),
        "substance" => pattern == "fly-spend" && fly_spend(&acquisition.diff_content),
        _ => false,
    }
}

fn conventional(title: &str) -> (String, Vec<String>) {
    let expression = Regex::new(r"^([^(:\s]+)(?:\(([^)]*)\))?:").expect("static regex");
    let Some(captures) = expression.captures(title) else {
        return (String::new(), Vec::new());
    };
    let item_type = captures
        .get(1)
        .map_or("", |value| value.as_str())
        .to_owned();
    let scopes = captures
        .get(2)
        .map_or("", |value| value.as_str())
        .split(',')
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .map(str::to_owned)
        .collect();
    (item_type, scopes)
}

fn refs(acquisition: &Acquisition, target: &Target<'_>) -> BTreeSet<u64> {
    let mut refs = BTreeSet::from([target.number]);
    refs.extend(
        acquisition.metadata["closingIssuesReferences"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|reference| reference["number"].as_u64()),
    );
    refs
}

fn evaluate_reserved(
    project: &GateProject,
    acquisition: &Acquisition,
    target: &Target<'_>,
) -> Value {
    if !acquisition.metadata_ready {
        return condition(
            "reserved_refs",
            "inconclusive",
            &["content-derived"],
            json!({"reason": acquisition.metadata_error}),
        );
    }
    let refs = refs(acquisition, target);
    let matches = project
        .reserved
        .iter()
        .filter(|number| refs.contains(number))
        .map(|number| format!("ref:#{number}"))
        .collect::<Vec<_>>();
    condition(
        "reserved_refs",
        if matches.is_empty() { "pass" } else { "fail" },
        if matches.is_empty() {
            &[]
        } else {
            &["content-derived"]
        },
        json!({"matches": matches}),
    )
}

fn glob_match(value: &str, glob: &str, path: bool) -> bool {
    let mut body = String::from("^");
    let characters = glob.chars().collect::<Vec<_>>();
    let mut index = 0;
    while index < characters.len() {
        if characters[index] == '*' && path && characters.get(index + 1) == Some(&'*') {
            if characters.get(index + 2) == Some(&'/') {
                body.push_str("(?:.*/)?");
                index += 3;
            } else {
                body.push_str(".*");
                index += 2;
            }
        } else if characters[index] == '*' {
            body.push_str(if path { "[^/]*" } else { ".*" });
            index += 1;
        } else {
            body.push_str(&regex::escape(&characters[index].to_string()));
            index += 1;
        }
    }
    body.push('$');
    RegexBuilder::new(&body)
        .case_insensitive(true)
        .build()
        .is_ok_and(|regex| regex.is_match(value))
}

fn fly_spend(diff: &str) -> bool {
    let mut old_path = String::new();
    let mut new_path = String::new();
    let mut old_table: Option<String> = None;
    let mut new_table: Option<String> = None;
    let mut in_hunk = false;
    for line in diff.split('\n') {
        if line.starts_with("diff --git ") {
            old_path.clear();
            new_path.clear();
            old_table = None;
            new_table = None;
            in_hunk = false;
        } else if line.starts_with("--- ") && !in_hunk {
            old_path = diff_path(line.trim_start_matches("--- "));
        } else if line.starts_with("+++ ") && !in_hunk {
            new_path = diff_path(line.trim_start_matches("+++ "));
        } else if line.starts_with("@@") {
            old_table = None;
            new_table = None;
            in_hunk = true;
        } else if in_hunk && is_fly_path(&old_path, &new_path) {
            if let Some(content) = line.strip_prefix(' ') {
                if let Some(table) = fly_table(content) {
                    old_table = Some(table.clone());
                    new_table = Some(table);
                }
            } else if let Some(content) = line.strip_prefix('-') {
                let table = fly_table(content);
                if old_table.as_deref() != Some("env")
                    && fly_spend_line(content, old_table.as_deref())
                {
                    return true;
                }
                if table.is_some() {
                    old_table = table;
                }
            } else if let Some(content) = line.strip_prefix('+') {
                let table = fly_table(content);
                if new_table.as_deref() != Some("env")
                    && fly_spend_line(content, new_table.as_deref())
                {
                    return true;
                }
                if table.is_some() {
                    new_table = table;
                }
            }
        }
    }
    false
}

fn diff_path(value: &str) -> String {
    value
        .trim_matches('"')
        .strip_prefix("a/")
        .or_else(|| value.trim_matches('"').strip_prefix("b/"))
        .unwrap_or(value.trim_matches('"'))
        .to_owned()
}

fn is_fly_path(old: &str, new: &str) -> bool {
    old == "fly.toml"
        || old.ends_with("/fly.toml")
        || new == "fly.toml"
        || new.ends_with("/fly.toml")
}

fn fly_table(content: &str) -> Option<String> {
    let expression =
        Regex::new(r"^\[{1,2}\s*([A-Za-z0-9_.-]+)\s*\]{1,2}(?:\s*#.*)?$").expect("static regex");
    expression
        .captures(content)
        .and_then(|captures| captures.get(1))
        .map(|value| value.as_str().to_owned())
}

fn fly_spend_line(content: &str, table: Option<&str>) -> bool {
    let content = content.trim_start();
    let direct = Regex::new(r"^(?:[A-Za-z0-9_-]+\.)*(?:vm|memory|cpu|cpus|count|region)\s*=")
        .expect("static regex");
    let section =
        Regex::new(r"^\[{1,2}\s*(?:[A-Za-z0-9_-]+\.)*(?:vm|scaling)\s*\]{1,2}(?:\s*#.*)?$")
            .expect("static regex");
    let assignment = Regex::new(r"^[A-Za-z0-9_.-]+\s*=").expect("static regex");
    direct.is_match(content)
        || section.is_match(content)
        || (table
            .and_then(|table| table.split('.').next_back())
            .is_some_and(|table| matches!(table, "vm" | "scaling"))
            && fly_table(content).is_none()
            && assignment.is_match(content))
}

fn apply_exceptions(
    conditions: &mut [Value],
    path: &Path,
    target: &Target<'_>,
    head_sha: &str,
    stderr: &mut String,
) {
    if head_sha.is_empty() || !path.metadata().is_ok_and(|metadata| metadata.len() > 0) {
        return;
    }
    let records = fs::read_to_string(path).ok().and_then(|text| {
        serde_json::Deserializer::from_str(&text)
            .into_iter::<Value>()
            .collect::<Result<Vec<_>, _>>()
            .ok()
    });
    let Some(records) = records else {
        stderr.push_str(&format!(
            "mandate gate: could not read {}; ignoring all exceptions\n",
            path.display()
        ));
        return;
    };
    for condition in conditions {
        let result = condition["result"].as_str().unwrap_or_default();
        if !matches!(result, "fail" | "inconclusive") {
            continue;
        }
        let name = condition["name"].as_str().unwrap_or_default();
        let reason = records.iter().rev().find_map(|record| {
            (record["repo"].as_str() == Some(target.repo)
                && record["pr"].as_u64() == Some(target.number)
                && record["head_sha"].as_str() == Some(head_sha)
                && record["condition"].as_str() == Some(name))
            .then(|| record["reason"].as_str())
            .flatten()
            .filter(|reason| !reason.is_empty())
        });
        if let Some(reason) = reason {
            condition["result"] = Value::String("excused".to_owned());
            condition
                .as_object_mut()
                .expect("condition is an object")
                .insert(
                    "exception_reason".to_owned(),
                    Value::String(reason.to_owned()),
                );
        }
    }
}

fn aggregate(conditions: &[Value]) -> &'static str {
    if conditions
        .iter()
        .any(|condition| condition["result"] == "fail")
    {
        "fail"
    } else if conditions
        .iter()
        .any(|condition| condition["result"] == "inconclusive")
    {
        "inconclusive"
    } else {
        "pass"
    }
}

fn judgment_digest(target: &str, verdict: &str, conditions: &[Value]) -> Result<String, GateError> {
    let mut conditions = conditions.to_vec();
    conditions.sort_by(|left, right| {
        left["name"]
            .as_str()
            .expect("conditions have names")
            .cmp(right["name"].as_str().expect("conditions have names"))
    });
    let material = serde_json::to_vec(&json!({
        "pr": target,
        "verdict": verdict,
        "conditions": conditions,
    }))
    .map_err(|_| GateError::Serialize)?;
    Ok(format!("sha256:{}", sha256_hex(&material)))
}

fn already_judged(path: &Path, target: &str, head_sha: &str, digest: &str) -> JudgmentState {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return JudgmentState::NotJudged;
        }
        Err(error) => {
            return JudgmentState::CannotTell(format!(
                "could not read judgment history at {}: {error}",
                path.display()
            ));
        }
    };
    let records = match serde_json::Deserializer::from_str(&text)
        .into_iter::<Value>()
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(records) => records,
        Err(error) => {
            return JudgmentState::CannotTell(format!(
                "judgment history at {} is malformed: {error}",
                path.display()
            ));
        }
    };
    let evidence = !head_sha.is_empty();
    let previous = records.iter().rev().find(|record| {
        record["pr"].as_str() == Some(target)
            && record["evidence"].as_bool() == Some(evidence)
            && if evidence {
                record["head_sha"].as_str() == Some(head_sha)
            } else {
                record["head_sha"].is_null()
            }
    });
    let Some(previous) = previous else {
        return JudgmentState::NotJudged;
    };
    match previous["judgment_digest"].as_str() {
        Some(previous) if previous == digest => JudgmentState::Judged,
        Some(_) => JudgmentState::NotJudged,
        None => JudgmentState::CannotTell(format!(
            "matching judgment in {} has no judgment_digest",
            path.display()
        )),
    }
}

fn append_record(path: &Path, record: &Value) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut encoded = serde_json::to_vec(record).map_err(io::Error::other)?;
    encoded.push(b'\n');
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    set_private_file_mode(path).map_err(io::Error::other)?;
    file.write_all(&encoded)
}

fn render_record(record: &Value) -> Result<String, GateError> {
    let mut output = format!(
        "verdict: {} pr={} head_sha={} already_judged={}\n",
        record["verdict"].as_str().unwrap_or_default(),
        record["pr"].as_str().unwrap_or_default(),
        record["head_sha"].as_str().unwrap_or("unknown"),
        record["already_judged"]
            .as_str()
            .ok_or(GateError::Serialize)?,
    );
    for condition in record["conditions"]
        .as_array()
        .ok_or(GateError::Serialize)?
    {
        let tiers = condition["tier"].as_array().ok_or(GateError::Serialize)?;
        let tier = if tiers.is_empty() {
            "none".to_owned()
        } else {
            tiers
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(",")
        };
        let exception = condition
            .get("exception_reason")
            .map(|reason| {
                serde_json::to_string(reason)
                    .map(|reason| format!(" exception_reason={reason}"))
                    .map_err(|_| GateError::Serialize)
            })
            .transpose()?
            .unwrap_or_default();
        let detail =
            serde_json::to_string(&condition["detail"]).map_err(|_| GateError::Serialize)?;
        output.push_str(&format!(
            "condition {}: {} tier={tier}{exception} detail={detail}\n",
            condition["name"].as_str().unwrap_or_default(),
            condition["result"].as_str().unwrap_or_default(),
        ));
    }
    Ok(output)
}

const fn verdict_exit(verdict: &str) -> i32 {
    match verdict.as_bytes() {
        b"pass" => 0,
        b"fail" => 1,
        _ => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn point_current(paths: &OstromPaths, digest: &str) {
        std::os::unix::fs::symlink(
            Path::new("versions").join(digest),
            paths.current_policy_version(),
        )
        .expect("point current policy");
    }

    #[cfg(windows)]
    fn point_current(paths: &OstromPaths, digest: &str) {
        std::os::windows::fs::symlink_dir(
            Path::new("versions").join(digest),
            paths.current_policy_version(),
        )
        .expect("point current policy");
    }

    fn materialize_current(paths: &OstromPaths, manifest: &PolicyManifest, digest: &str) {
        let version = paths.policy_versions_dir().join(digest);
        fs::create_dir_all(&version).expect("create policy version");
        fs::write(
            version.join("ostrom.yaml"),
            manifest.to_yaml().expect("canonical manifest"),
        )
        .expect("write policy version");
        point_current(paths, digest);
    }

    #[test]
    fn gate_config_preserves_shipped_user_repo_layering() {
        let fixture = tempfile::tempdir().expect("temporary gate config fixture");
        let paths = OstromPaths {
            config: fixture.path().join("config"),
            state: fixture.path().join("state"),
        };
        let repository = fixture.path().join("repository");
        fs::create_dir_all(&paths.config).unwrap();
        fs::create_dir_all(repository.join(".ostrom")).unwrap();
        fs::write(
            paths.config.join("gate.yaml"),
            r#"provider: file
bounce_all: []
projects:
  - repo: placeholder-org/placeholder-repo
    required_checks: [verify-*]
    bounce: [path:protected/**]
    reserved: [41]
"#,
        )
        .unwrap();
        fs::write(
            repository.join(".ostrom/gate.yaml"),
            "bounce_all: [title:*principal review*]\n",
        )
        .unwrap();
        let (config, error, source) =
            load_gate_config_for_repo(&paths, &repository, "placeholder-org/placeholder-repo");
        assert!(error.is_empty());
        assert!(source.is_none());
        let config = config.expect("layered gate config");
        assert_eq!(config.bounce_all[0].as_str(), "title:*principal review*");
        assert_eq!(
            config.projects[0].repo.as_str(),
            "placeholder-org/placeholder-repo"
        );
        assert_eq!(config.projects[0].required_checks, ["verify-*"]);
        assert_eq!(config.projects[0].bounce[0].as_str(), "path:protected/**");
        assert_eq!(config.projects[0].reserved, [41]);
    }

    #[test]
    fn current_manifest_precedes_legacy_gate_config() {
        let fixture = tempfile::tempdir().expect("temporary gate config fixture");
        let paths = OstromPaths {
            config: fixture.path().to_path_buf(),
            state: fixture.path().to_path_buf(),
        };
        fs::write(
            paths.config.join("gate.yaml"),
            "provider: file\nprojects:\n  - repo: placeholder-org/legacy\n",
        )
        .expect("legacy fallback");
        let manifest = PolicyManifest::from_yaml(
            "manifest_version: 1\ngrants:\n  current: {repositories: placeholder-org/current}\n",
        )
        .expect("current manifest");
        let digest = policy_manifest_digest(&manifest).expect("manifest digest");
        materialize_current(&paths, &manifest, &digest);

        let (config, source) = load_gate_config_with_source(&paths, fixture.path())
            .expect("current manifest gate config");
        assert_eq!(config.projects.len(), 1);
        assert_eq!(config.projects[0].repo.as_str(), "placeholder-org/current");
        assert_eq!(source, Some(format!("manifest digest={digest}")));
    }

    #[test]
    fn drifted_current_manifest_is_named_and_never_falls_back() {
        let fixture = tempfile::tempdir().expect("temporary gate config fixture");
        let paths = OstromPaths {
            config: fixture.path().to_path_buf(),
            state: fixture.path().to_path_buf(),
        };
        fs::write(
            paths.config.join("gate.yaml"),
            "provider: file\nprojects:\n  - repo: placeholder-org/legacy\n",
        )
        .expect("legacy fallback");
        let manifest = PolicyManifest::from_yaml(
            "manifest_version: 1\ngrants:\n  current: {repositories: placeholder-org/current}\n",
        )
        .expect("current manifest");
        materialize_current(&paths, &manifest, &"0".repeat(64));

        let error = load_gate_config(&paths, fixture.path())
            .expect_err("digest drift must not read gate.yaml");
        assert!(error.contains("current_drift"), "{error}");
    }

    #[test]
    fn repository_absent_from_current_manifest_is_a_named_gate_failure() {
        let fixture = tempfile::tempdir().expect("temporary gate config fixture");
        let paths = OstromPaths {
            config: fixture.path().to_path_buf(),
            state: fixture.path().to_path_buf(),
        };
        let manifest = PolicyManifest::from_yaml(
            "manifest_version: 1\ngrants:\n  current: {repositories: placeholder-org/current}\n",
        )
        .expect("current manifest");
        let digest = policy_manifest_digest(&manifest).expect("manifest digest");
        materialize_current(&paths, &manifest, &digest);

        let (config, error, source) =
            load_gate_config_for_repo(&paths, fixture.path(), "placeholder-org/absent");
        assert!(config.is_none());
        assert_eq!(
            error,
            "composed manifest has no project entry for placeholder-org/absent"
        );
        assert_eq!(source, Some(format!("manifest digest={digest}")));
    }

    #[test]
    fn target_parser_is_exact() {
        assert!(parse_target("placeholder-org/alpha#1").is_ok());
        for value in [
            "placeholder-org/alpha#0",
            "placeholder-org/alpha#01",
            "placeholder-org/alpha",
            "placeholder-org/alpha#1 extra",
            "placeholder-org//alpha#1",
        ] {
            assert!(parse_target(value).is_err(), "accepted {value}");
        }
    }

    #[test]
    fn malformed_judgment_history_is_cannot_tell() {
        let fixture = tempfile::tempdir().expect("temporary judgment fixture");
        let path = fixture.path().join("gate.jsonl");
        fs::write(&path, "{malformed}\n").expect("write malformed judgment history");
        let state = already_judged(
            &path,
            "placeholder-org/alpha#7",
            "aaaaaaaaaaaaaaaa",
            "sha256:placeholder",
        );
        assert!(matches!(state, JudgmentState::CannotTell(reason) if reason.contains("malformed")));
    }

    #[test]
    fn judgment_digest_treats_conditions_as_a_set() {
        let first = vec![
            condition("draft", "pass", &[], json!({"isDraft": false})),
            condition("mergeable", "pass", &[], json!({"mergeable": "MERGEABLE"})),
        ];
        let second = first.iter().rev().cloned().collect::<Vec<_>>();
        assert_eq!(
            judgment_digest("placeholder-org/alpha#7", "pass", &first).unwrap(),
            judgment_digest("placeholder-org/alpha#7", "pass", &second).unwrap()
        );
    }

    #[test]
    fn exception_requires_every_key_and_a_nonempty_reason() {
        let fixture = tempfile::tempdir().expect("temporary exception fixture");
        let path = fixture.path().join("exceptions.jsonl");
        let records = [
            json!({"repo":"placeholder-org/beta","pr":7,"head_sha":"sha","condition":"mergeable","reason":"wrong repo"}),
            json!({"repo":"placeholder-org/alpha","pr":8,"head_sha":"sha","condition":"mergeable","reason":"wrong pr"}),
            json!({"repo":"placeholder-org/alpha","pr":7,"head_sha":"other","condition":"mergeable","reason":"wrong sha"}),
            json!({"repo":"placeholder-org/alpha","pr":7,"head_sha":"sha","condition":"draft","reason":"wrong condition"}),
            json!({"repo":"placeholder-org/alpha","pr":7,"head_sha":"sha","condition":"mergeable","reason":""}),
        ];
        let text = records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&path, format!("{text}\n")).expect("write synthetic exceptions");
        let target = parse_target("placeholder-org/alpha#7").expect("valid target");
        let mut conditions = vec![condition(
            "mergeable",
            "fail",
            &["content-derived"],
            json!({"mergeable":"CONFLICTING"}),
        )];
        let mut stderr = String::new();
        apply_exceptions(&mut conditions, &path, &target, "sha", &mut stderr);
        assert_eq!(conditions[0]["result"], "fail");
        assert!(stderr.is_empty());

        fs::write(
            &path,
            "{\"repo\":\"placeholder-org/alpha\",\"pr\":7,\"head_sha\":\"sha\",\"condition\":\"mergeable\",\"reason\":\"principal accepted placeholder conflict\"}\n",
        )
        .expect("write matching synthetic exception");
        apply_exceptions(&mut conditions, &path, &target, "sha", &mut stderr);
        assert_eq!(conditions[0]["result"], "excused");
        assert_eq!(
            conditions[0]["exception_reason"],
            "principal accepted placeholder conflict"
        );
    }

    /// The live journal holds 4,519 rows written before the `evidence` marker
    /// existed, 4,422 of them carrying a head SHA. Reading a missing marker as
    /// "not evidence" retires all of them at once, and because the merge-gate
    /// check skips every merge when its epoch floor is absent, it would go
    /// silent rather than loud — the exact failure this module exists to stop.
    #[test]
    fn a_row_written_before_the_evidence_marker_is_evidence_when_it_names_a_sha() {
        let legacy_with_sha = json!({"pr": "placeholder-org/alpha#1", "head_sha": "a".repeat(40)});
        let legacy_without_sha = json!({"pr": "placeholder-org/alpha#2", "head_sha": Value::Null});
        let marked =
            json!({"pr": "placeholder-org/alpha#3", "head_sha": "b".repeat(40), "evidence": true});
        let unmarked =
            json!({"pr": "placeholder-org/alpha#4", "head_sha": Value::Null, "evidence": false});

        assert!(is_merge_evidence(&legacy_with_sha));
        assert!(!is_merge_evidence(&legacy_without_sha));
        assert!(is_merge_evidence(&marked));
        assert!(!is_merge_evidence(&unmarked));
    }

    /// An explicit marker always wins, so a row can never be promoted back into
    /// evidence by the SHA it happens to carry.
    #[test]
    fn an_explicit_evidence_marker_overrides_the_head_sha() {
        let contradictory = json!({"head_sha": "c".repeat(40), "evidence": false});
        assert!(!is_merge_evidence(&contradictory));
    }
}
