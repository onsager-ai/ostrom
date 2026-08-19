use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs, io,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use chrono::{DateTime, Duration, Utc};
use ostrom_core::RepositoryName;
use serde_json::{Map, Value, json};
use thiserror::Error;

use crate::{
    OstromPaths,
    app_token::{InstallationTokenMinter, ScopedAppTokenRequest, authenticated_output},
    set_private_file_mode,
};

const READ_PERMISSIONS: &str = "metadata:read,contents:read";
const WRITE_PERMISSIONS: &str = "metadata:read,contents:write";
const RETAINED_GATE_DAYS: i64 = 90;

#[derive(Debug, Error)]
pub enum PublishError {
    #[error("publication source record is missing: {0}")]
    SourceMissing(String),
    #[error("invalid publication allowlist at {path}: {message}")]
    InvalidAllowlist { path: String, message: String },
    #[error("invalid publication record at {path}: {message}")]
    InvalidRecord { path: String, message: String },
    #[error("gate record has an invalid timestamp day: {0}")]
    InvalidGateDay(String),
    #[error("could not prepare publication path {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("publication command `{command}` failed: {detail}")]
    Command { command: String, detail: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishDestination {
    repository: RepositoryName,
}

impl PublishDestination {
    /// Publication has no implicit constructor: a caller must first supply a
    /// validated repository value from an explicit command-line option.
    #[must_use]
    pub fn explicit(repository: RepositoryName) -> Self {
        Self { repository }
    }

    #[must_use]
    pub fn repository(&self) -> &RepositoryName {
        &self.repository
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PublishOutcome {
    Published,
    Unchanged,
}

pub(crate) struct PublishOptions<'a> {
    pub paths: &'a OstromPaths,
    pub plugin_root: &'a Path,
    pub destination: &'a PublishDestination,
    pub published_at: DateTime<Utc>,
    pub cadence_hours: u64,
}

struct Allowlist {
    fields: BTreeMap<String, Vec<String>>,
    schema_value: Value,
}

#[derive(Clone)]
struct DerivedTree {
    files: BTreeMap<PathBuf, Vec<u8>>,
}

pub(crate) fn publish(
    options: &PublishOptions<'_>,
    minter: &mut dyn InstallationTokenMinter,
) -> Result<PublishOutcome, PublishError> {
    // The destination deliberately has no environment fallback. In
    // particular, MANDATE_PUBLISH_REMOTE inherited by a scratch OSTROM_HOME
    // cannot turn an opted-out sweep into a publishing one.
    let allowlist_path = env::var_os("MANDATE_PUBLISH_ALLOWLIST")
        .filter(|value| !value.is_empty())
        .map_or_else(
            || options.plugin_root.join("config/publish-allowlist.json"),
            PathBuf::from,
        );
    let allowlist = load_allowlist(&allowlist_path)?;
    let mut tree = derive_tree(options, &allowlist)?;
    let publish_dir = options.paths.state.join("publish");
    prepare_checkout(options, &publish_dir, minter)?;
    reuse_previous_time_if_unchanged(&publish_dir, &mut tree)?;
    install_tree(&publish_dir, &tree)?;

    git_required(
        Some(&publish_dir),
        &["diff", "--cached", "--quiet"],
        &[0, 1],
    )
    .and_then(|output| {
        if output.status.code() == Some(0) {
            Ok(PublishOutcome::Unchanged)
        } else {
            let message = format!(
                "chore(state): publish governance snapshot {}",
                options.published_at.format("%Y-%m-%dT%H:%M:%SZ")
            );
            git_required(
                Some(&publish_dir),
                &["commit", "--quiet", "-m", &message],
                &[0],
            )?;
            scoped_required(
                options,
                WRITE_PERMISSIONS,
                "git",
                &[
                    "-C".into(),
                    publish_dir.as_os_str().into(),
                    "push".into(),
                    "--quiet".into(),
                    "origin".into(),
                    "HEAD:state".into(),
                ],
                &[0],
                minter,
            )?;
            Ok(PublishOutcome::Published)
        }
    })
}

fn derive_tree(
    options: &PublishOptions<'_>,
    allowlist: &Allowlist,
) -> Result<DerivedTree, PublishError> {
    let queue_path = options.paths.queue_file();
    let state_path = options.paths.sweep_state_file();
    for source in [&queue_path, &state_path] {
        if !source.is_file() {
            return Err(PublishError::SourceMissing(source.display().to_string()));
        }
    }
    let gate_path = options.paths.state.join("gate.jsonl");
    let merge_path = options.paths.merge_file();
    let queue_source = read_jsonl(&queue_path)?;
    let gate_source = if gate_path.is_file() {
        read_jsonl(&gate_path)?
    } else {
        Vec::new()
    };
    let merge_source = if merge_path.is_file() {
        read_jsonl(&merge_path)?
    } else {
        Vec::new()
    };
    let state_source = read_json(&state_path)?;

    let mut queue_drops = BTreeMap::new();
    let queue = queue_source
        .iter()
        .map(|record| filter_queue(record, allowlist, &mut queue_drops))
        .collect::<Result<Vec<_>, _>>()?;
    let mut gate_drops = BTreeMap::new();
    let gate = gate_source
        .iter()
        .map(|record| filter_gate(record, allowlist, &mut gate_drops))
        .collect::<Result<Vec<_>, _>>()?;
    let mut merge_drops = BTreeMap::new();
    let merge = merge_source
        .iter()
        .map(|record| filter_merge(record, allowlist, &mut merge_drops))
        .collect::<Result<Vec<_>, _>>()?;
    let merge = deduplicate_merges(merge)?;
    let mut state_drops = BTreeMap::new();
    let state = filter_state(&state_source, allowlist, &mut state_drops)?;

    let cutoff = (options.published_at.date_naive() - Duration::days(RETAINED_GATE_DAYS - 1))
        .format("%Y-%m-%d")
        .to_string();
    let mut partitions = BTreeMap::<String, Vec<Value>>::new();
    for record in &gate {
        let timestamp = record.get("ts").and_then(Value::as_str).unwrap_or("");
        let day = timestamp.get(..10).unwrap_or(timestamp);
        if !valid_day_shape(day) {
            return Err(PublishError::InvalidGateDay(day.to_owned()));
        }
        if day >= cutoff.as_str() {
            partitions
                .entry(day.to_owned())
                .or_default()
                .push(record.clone());
        }
    }

    let rollup = build_rollup(&queue, &gate, &state)?;
    let schema_id = schema_id(&allowlist.schema_value)?;
    let manifest = json!({
        "schema_id": format!("git:{schema_id}"),
        "published_at": options.published_at.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "expected_sweep_interval_hours": options.cadence_hours,
        "retention": {"gate_days": RETAINED_GATE_DAYS, "merge": "forever", "rollup": "forever"},
        "record_counts": {
            "queue": queue.len(),
            "gate": gate.len(),
            "merge": merge.len(),
            "state_repos": state.get("repos").and_then(Value::as_object).map_or(0, Map::len),
            "gate_partitions": partitions.len(),
        },
        "dropped_fields": {
            "queue": queue_drops,
            "gate": gate_drops,
            "merge": merge_drops,
            "state": state_drops,
        },
    });

    let mut files = BTreeMap::new();
    files.insert("manifest.json".into(), pretty_json(&manifest));
    files.insert("queue.jsonl".into(), jsonl(&queue));
    files.insert("merge.jsonl".into(), jsonl(&merge));
    files.insert("state.json".into(), pretty_json(&state));
    files.insert("rollup.json".into(), pretty_json(&rollup));
    for (day, records) in partitions {
        files.insert(PathBuf::from(format!("gate/{day}.jsonl")), jsonl(&records));
    }
    Ok(DerivedTree { files })
}

fn load_allowlist(path: &Path) -> Result<Allowlist, PublishError> {
    let value = read_json(path).map_err(|error| match error {
        PublishError::InvalidRecord { message, .. } => PublishError::InvalidAllowlist {
            path: path.display().to_string(),
            message,
        },
        other => other,
    })?;
    let object = value
        .as_object()
        .ok_or_else(|| invalid_allowlist(path, "top level must be an object"))?;
    if object
        .get("_comment")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Err(invalid_allowlist(
            path,
            "_comment must be a non-empty string",
        ));
    }
    let mut fields = BTreeMap::new();
    for (shape, value) in object.iter().filter(|(key, _)| key.as_str() != "_comment") {
        let values = value.as_array().ok_or_else(|| {
            invalid_allowlist(path, format!("{shape} must be a non-empty string array"))
        })?;
        let names = values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned)
            })
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| {
                invalid_allowlist(path, format!("{shape} must be a non-empty string array"))
            })?;
        let unique = names.iter().collect::<BTreeSet<_>>();
        if names.is_empty() || unique.len() != names.len() {
            return Err(invalid_allowlist(
                path,
                format!("{shape} must contain unique non-empty fields"),
            ));
        }
        fields.insert(shape.clone(), names);
    }
    for required in [
        "queue",
        "queue.mandate",
        "queue.mandate.dossier",
        "gate",
        "gate.condition",
        "merge",
        "state",
        "state.dead_selector",
        "state.repo",
        "state.item",
        "state.policy",
        "state.scope_changes",
        "state.notice",
    ] {
        if !fields.contains_key(required) {
            return Err(invalid_allowlist(path, format!("missing shape {required}")));
        }
    }
    let mut schema_value = value;
    schema_value
        .as_object_mut()
        .expect("allowlist object was checked")
        .remove("_comment");
    Ok(Allowlist {
        fields,
        schema_value,
    })
}

fn filter_queue(
    source: &Value,
    allowlist: &Allowlist,
    drops: &mut BTreeMap<String, u64>,
) -> Result<Value, PublishError> {
    // Copying and omission accounting use the same named shapes. Rebuilding
    // each object from those shapes leaves an unknown source key no route
    // into the public value.
    require_object(source, "queue row")?;
    record_unknown(allowlist, "queue", source, "", drops)?;
    let mandate_source = source.get("mandate").unwrap_or(&Value::Null);
    record_unknown(
        allowlist,
        "queue.mandate",
        mandate_source,
        "mandate.",
        drops,
    )?;
    let dossier_source = mandate_source.get("dossier").unwrap_or(&Value::Null);
    record_unknown(
        allowlist,
        "queue.mandate.dossier",
        dossier_source,
        "mandate.dossier.",
        drops,
    )?;

    let mut output = filtered_object(allowlist, "queue", source)?;
    let mandate = if mandate_source.is_object() {
        let mut mandate = filtered_object(allowlist, "queue.mandate", mandate_source)?;
        if dossier_source.is_object() {
            mandate.insert(
                "dossier".to_owned(),
                Value::Object(filtered_object(
                    allowlist,
                    "queue.mandate.dossier",
                    dossier_source,
                )?),
            );
        } else {
            mandate.remove("dossier");
        }
        mandate
    } else {
        Map::new()
    };
    output.insert("mandate".to_owned(), Value::Object(mandate));
    Ok(Value::Object(output))
}

fn filter_gate(
    source: &Value,
    allowlist: &Allowlist,
    drops: &mut BTreeMap<String, u64>,
) -> Result<Value, PublishError> {
    require_object(source, "gate row")?;
    record_unknown(allowlist, "gate", source, "", drops)?;
    let mut conditions = Vec::new();
    if let Some(source_conditions) = source.get("conditions").and_then(Value::as_array) {
        for condition in source_conditions.iter().filter(|value| value.is_object()) {
            record_unknown(
                allowlist,
                "gate.condition",
                condition,
                "conditions[].",
                drops,
            )?;
            let name = condition.get("name").and_then(Value::as_str).unwrap_or("");
            record_detail_unknown(allowlist, name, condition, drops)?;
            conditions.push(Value::Object(filter_condition(allowlist, name, condition)?));
        }
    }
    let mut output = filtered_object(allowlist, "gate", source)?;
    output.insert("conditions".to_owned(), Value::Array(conditions));
    Ok(Value::Object(output))
}

fn filter_merge(
    source: &Value,
    allowlist: &Allowlist,
    drops: &mut BTreeMap<String, u64>,
) -> Result<Value, PublishError> {
    require_object(source, "merge row")?;
    record_unknown(allowlist, "merge", source, "", drops)?;
    let output = Value::Object(filtered_object(allowlist, "merge", source)?);
    for field in ["pr", "opened_at", "merged_at"] {
        required_merge_string(&output, field)?;
    }
    for field in ["opened_by_class", "merged_by_class"] {
        let class = required_merge_string(&output, field)?;
        if !matches!(class, "loop" | "principal") {
            return Err(invalid_record(
                "merge.jsonl",
                format!("{field} must be loop or principal"),
            ));
        }
    }
    Ok(output)
}

fn deduplicate_merges(records: Vec<Value>) -> Result<Vec<Value>, PublishError> {
    let mut keys = BTreeSet::new();
    let mut unique = Vec::new();
    for record in records {
        let pr = required_merge_string(&record, "pr")?;
        let merged_at = required_merge_string(&record, "merged_at")?;
        if keys.insert((pr.to_owned(), merged_at.to_owned())) {
            unique.push(record);
        }
    }
    Ok(unique)
}

fn required_merge_string<'a>(record: &'a Value, field: &str) -> Result<&'a str, PublishError> {
    required_string(record, field, "merge.jsonl").and_then(|value| {
        if value.is_empty() {
            Err(invalid_record(
                "merge.jsonl",
                format!("{field} must not be empty"),
            ))
        } else {
            Ok(value)
        }
    })
}

fn filter_condition(
    allowlist: &Allowlist,
    name: &str,
    source: &Value,
) -> Result<Map<String, Value>, PublishError> {
    let mut output = filtered_object(allowlist, "gate.condition", source)?;
    let Some(detail) = source.get("detail").filter(|detail| detail.is_object()) else {
        output.remove("detail");
        return Ok(output);
    };
    let shape = format!("gate.detail.{name}");
    if !allowlist.fields.contains_key(&shape) {
        output.remove("detail");
        return Ok(output);
    }
    let mut filtered = filtered_object(allowlist, &shape, detail)?;
    match name {
        "bounce_selectors" => {
            for field in ["matches", "unobservable"] {
                let nested_shape = format!(
                    "gate.detail.bounce_selectors.{field_singular}",
                    field_singular = if field == "matches" {
                        "match"
                    } else {
                        "unobservable"
                    }
                );
                let values = detail
                    .get(field)
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|value| value.is_object())
                    .map(|value| {
                        filtered_object(allowlist, &nested_shape, value).map(Value::Object)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                filtered.insert(field.to_owned(), Value::Array(values));
            }
        }
        "reserved_refs" => {
            let matches = detail
                .get("matches")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|value| value.is_string())
                .cloned()
                .collect();
            filtered.insert("matches".to_owned(), Value::Array(matches));
        }
        "required_checks" => {
            let selectors = detail
                .get("selectors")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|value| value.is_object())
                .map(|selector| {
                    let mut filtered = filtered_object(
                        allowlist,
                        "gate.detail.required_checks.selector",
                        selector,
                    )?;
                    let matches = selector
                        .get("matches")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter(|value| value.is_object())
                        .map(|value| {
                            filtered_object(allowlist, "gate.detail.required_checks.match", value)
                                .map(Value::Object)
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    filtered.insert("matches".to_owned(), Value::Array(matches));
                    Ok(Value::Object(filtered))
                })
                .collect::<Result<Vec<_>, PublishError>>()?;
            filtered.insert("selectors".to_owned(), Value::Array(selectors));
        }
        _ => {}
    }
    output.insert("detail".to_owned(), Value::Object(filtered));
    Ok(output)
}

fn record_detail_unknown(
    allowlist: &Allowlist,
    name: &str,
    condition: &Value,
    drops: &mut BTreeMap<String, u64>,
) -> Result<(), PublishError> {
    let Some(detail) = condition.get("detail").filter(|detail| detail.is_object()) else {
        return Ok(());
    };
    let shape = format!("gate.detail.{name}");
    if !allowlist.fields.contains_key(&shape) {
        increment(drops, "conditions[].detail");
        return Ok(());
    }
    record_unknown(allowlist, &shape, detail, "conditions[].detail.", drops)?;
    if name == "bounce_selectors" {
        for (field, nested_shape) in [
            ("matches", "gate.detail.bounce_selectors.match"),
            ("unobservable", "gate.detail.bounce_selectors.unobservable"),
        ] {
            for value in object_array(detail.get(field)) {
                record_unknown(
                    allowlist,
                    nested_shape,
                    value,
                    &format!("conditions[].detail.{field}[]."),
                    drops,
                )?;
            }
        }
    } else if name == "required_checks" {
        for selector in object_array(detail.get("selectors")) {
            record_unknown(
                allowlist,
                "gate.detail.required_checks.selector",
                selector,
                "conditions[].detail.selectors[].",
                drops,
            )?;
            for matched in object_array(selector.get("matches")) {
                record_unknown(
                    allowlist,
                    "gate.detail.required_checks.match",
                    matched,
                    "conditions[].detail.selectors[].matches[].",
                    drops,
                )?;
            }
        }
    }
    Ok(())
}

fn filter_state(
    source: &Value,
    allowlist: &Allowlist,
    drops: &mut BTreeMap<String, u64>,
) -> Result<Value, PublishError> {
    require_object(source, "state")?;
    record_unknown(allowlist, "state", source, "", drops)?;
    for dead in object_array(source.get("dead_selectors")) {
        record_unknown(
            allowlist,
            "state.dead_selector",
            dead,
            "dead_selectors[].",
            drops,
        )?;
    }
    if let Some(repos) = source.get("repos").and_then(Value::as_object) {
        for repo in repos.values().filter(|value| value.is_object()) {
            record_unknown(allowlist, "state.repo", repo, "repos.*.", drops)?;
            if let Some(items) = repo.get("items").and_then(Value::as_object) {
                for item in items.values().filter(|value| value.is_object()) {
                    record_unknown(allowlist, "state.item", item, "repos.*.items.*.", drops)?;
                }
            }
            for (field, shape, prefix) in [
                ("policy", "state.policy", "repos.*.policy."),
                (
                    "scope_changes",
                    "state.scope_changes",
                    "repos.*.scope_changes.",
                ),
                ("notice", "state.notice", "repos.*.notice."),
            ] {
                record_unknown(
                    allowlist,
                    shape,
                    repo.get(field).unwrap_or(&Value::Null),
                    prefix,
                    drops,
                )?;
            }
        }
    }

    let mut output = filtered_object(allowlist, "state", source)?;
    let unresolvable = source
        .get("unresolvable_repositories")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|value| value.is_string())
        .cloned()
        .collect();
    output.insert(
        "unresolvable_repositories".to_owned(),
        Value::Array(unresolvable),
    );
    let dead = object_array(source.get("dead_selectors"))
        .into_iter()
        .map(|value| filtered_object(allowlist, "state.dead_selector", value).map(Value::Object))
        .collect::<Result<Vec<_>, _>>()?;
    output.insert("dead_selectors".to_owned(), Value::Array(dead));
    let mut repos_output = Map::new();
    if let Some(repos) = source.get("repos").and_then(Value::as_object) {
        for (name, repo) in repos {
            let value = if repo.is_object() {
                let mut filtered = filtered_object(allowlist, "state.repo", repo)?;
                let mut items_output = Map::new();
                if let Some(items) = repo.get("items").and_then(Value::as_object) {
                    for (id, item) in items {
                        let filtered_item = if item.is_object() {
                            Value::Object(filtered_object(allowlist, "state.item", item)?)
                        } else {
                            json!({})
                        };
                        items_output.insert(id.clone(), filtered_item);
                    }
                }
                filtered.insert("items".to_owned(), Value::Object(items_output));
                for (field, shape) in [
                    ("policy", "state.policy"),
                    ("scope_changes", "state.scope_changes"),
                ] {
                    let value = repo
                        .get(field)
                        .filter(|value| value.is_object())
                        .map_or_else(
                            || json!({}),
                            |value| {
                                filtered_object(allowlist, shape, value)
                                    .map(Value::Object)
                                    .expect("known allowlist shape")
                            },
                        );
                    filtered.insert(field.to_owned(), value);
                }
                let notice = repo
                    .get("notice")
                    .filter(|value| value.is_object())
                    .map_or_else(
                        || Value::Null,
                        |value| {
                            filtered_object(allowlist, "state.notice", value)
                                .map(Value::Object)
                                .expect("known allowlist shape")
                        },
                    );
                filtered.insert("notice".to_owned(), notice);
                Value::Object(filtered)
            } else {
                json!({})
            };
            repos_output.insert(name.clone(), value);
        }
    }
    output.insert("repos".to_owned(), Value::Object(repos_output));
    Ok(Value::Object(output))
}

fn build_rollup(queue: &[Value], gate: &[Value], state: &Value) -> Result<Value, PublishError> {
    let mut verdicts = Map::new();
    for record in gate {
        let timestamp = required_string(record, "ts", "gate rollup")?;
        let day = timestamp
            .get(..10)
            .ok_or_else(|| PublishError::InvalidGateDay(timestamp.to_owned()))?
            .to_owned();
        let verdict = required_string(record, "verdict", "gate rollup")?;
        let counts = verdicts
            .entry(day)
            .or_insert_with(|| json!({"pass": 0, "fail": 0, "inconclusive": 0}));
        increment_object_number(counts, verdict)?;
    }
    let mut ages = json!({"0-1": 0, "2-7": 0, "8-30": 0, "31+": 0});
    for record in queue {
        let age = record
            .get("age_days")
            .and_then(Value::as_i64)
            .ok_or_else(|| {
                invalid_record("queue.jsonl", "age_days must be an integer for rollup")
            })?;
        let bucket = if age <= 1 {
            "0-1"
        } else if age <= 7 {
            "2-7"
        } else if age <= 30 {
            "8-30"
        } else {
            "31+"
        };
        increment_object_number(&mut ages, bucket)?;
    }
    let mut classifications = Map::new();
    if let Some(repos) = state.get("repos").and_then(Value::as_object) {
        for (name, repo) in repos {
            let mut counts = json!({
                "delegated": 0,
                "reserved": 0,
                "excluded": 0,
                "tripwire": 0,
                "unclassified": 0,
            });
            if let Some(items) = repo.get("items").and_then(Value::as_object) {
                for item in items.values() {
                    let classification = required_string(item, "classification", "state rollup")?;
                    increment_object_number(&mut counts, classification)?;
                }
            }
            classifications.insert(name.clone(), counts);
        }
    }
    Ok(json!({
        "verdicts_by_day": verdicts,
        "queue_age_buckets": ages,
        "repo_classifications": classifications,
    }))
}

fn schema_id(value: &Value) -> Result<String, PublishError> {
    let mut input =
        serde_json::to_vec(&sorted(value)).expect("publication allowlist JSON always serializes");
    input.push(b'\n');
    let mut child = Command::new("git")
        .args(["hash-object", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| command_spawn_error("git hash-object --stdin", error))?;
    use std::io::Write;
    child
        .stdin
        .take()
        .expect("piped hash-object stdin")
        .write_all(&input)
        .map_err(|error| PublishError::Io {
            path: "git hash-object stdin".to_owned(),
            source: error,
        })?;
    let output = child
        .wait_with_output()
        .map_err(|error| command_spawn_error("git hash-object --stdin", error))?;
    require_status("git hash-object --stdin", output, &[0])
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn prepare_checkout(
    options: &PublishOptions<'_>,
    directory: &Path,
    minter: &mut dyn InstallationTokenMinter,
) -> Result<(), PublishError> {
    let parent = directory.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| path_error(parent, source))?;
    if !directory.join(".git").is_dir() {
        scoped_required(
            options,
            READ_PERMISSIONS,
            "gh",
            &[
                "repo".into(),
                "clone".into(),
                options.destination.repository().as_str().into(),
                directory.as_os_str().into(),
                "--".into(),
                "--no-checkout".into(),
            ],
            &[0],
            minter,
        )?;
    }
    set_private_directory_mode(directory)?;

    // Only destination-facing commands mint credentials. Reads and writes
    // mint independently so the clone/fetch grant cannot mutate, while the
    // final write grant is limited to this typed destination.
    let remote = scoped_required(
        options,
        READ_PERMISSIONS,
        "git",
        &[
            "-C".into(),
            directory.as_os_str().into(),
            "ls-remote".into(),
            "--exit-code".into(),
            "--heads".into(),
            "origin".into(),
            "state".into(),
        ],
        &[0, 2],
        minter,
    )?;
    if remote.status.code() == Some(0) {
        scoped_required(
            options,
            READ_PERMISSIONS,
            "git",
            &[
                "-C".into(),
                directory.as_os_str().into(),
                "fetch".into(),
                "--quiet".into(),
                "origin".into(),
                "state".into(),
            ],
            &[0],
            minter,
        )?;
        git_required(
            Some(directory),
            &["checkout", "-B", "state", "FETCH_HEAD"],
            &[0],
        )?;
    } else {
        let orphan = git_required(
            Some(directory),
            &["checkout", "--orphan", "state"],
            &[0, 128],
        )?;
        if orphan.status.success() {
            // A no-checkout clone still seeds the index from the default
            // branch. Clearing it prevents unrelated remote content from
            // entering the first public snapshot.
            git_required(Some(directory), &["read-tree", "--empty"], &[0])?;
        } else {
            git_required(Some(directory), &["checkout", "state"], &[0])?;
        }
    }
    Ok(())
}

fn reuse_previous_time_if_unchanged(
    checkout: &Path,
    tree: &mut DerivedTree,
) -> Result<(), PublishError> {
    // Keeping the prior timestamp when every stable byte matches makes a true
    // no-op leave an empty index rather than manufacturing periodic commits.
    let manifest_path = checkout.join("manifest.json");
    if !manifest_path.is_file() {
        return Ok(());
    }
    let previous = read_json(&manifest_path)?;
    let Some(previous_time) = previous.get("published_at").and_then(Value::as_str) else {
        return Ok(());
    };
    let Some(bytes) = tree.files.get(Path::new("manifest.json")) else {
        return Ok(());
    };
    let mut candidate: Value = serde_json::from_slice(bytes)
        .map_err(|error| invalid_record("derived manifest", error.to_string()))?;
    candidate["published_at"] = json!(previous_time);
    let candidate_bytes = pretty_json(&candidate);
    let current_gate = tree
        .files
        .keys()
        .filter(|path| path.starts_with("gate"))
        .cloned()
        .collect::<BTreeSet<_>>();
    let previous_gate = if checkout.join("gate").is_dir() {
        fs::read_dir(checkout.join("gate"))
            .map_err(|source| path_error(&checkout.join("gate"), source))?
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "jsonl")
            })
            .map(|entry| PathBuf::from("gate").join(entry.file_name()))
            .collect::<BTreeSet<_>>()
    } else {
        BTreeSet::new()
    };
    if current_gate != previous_gate {
        return Ok(());
    }
    for (path, expected) in &tree.files {
        let expected = if path == Path::new("manifest.json") {
            &candidate_bytes
        } else {
            expected
        };
        if fs::read(checkout.join(path)).ok().as_deref() != Some(expected.as_slice()) {
            return Ok(());
        }
    }
    tree.files.insert("manifest.json".into(), candidate_bytes);
    Ok(())
}

fn install_tree(checkout: &Path, tree: &DerivedTree) -> Result<(), PublishError> {
    git_required(
        Some(checkout),
        &[
            "rm",
            "-r",
            "--quiet",
            "--ignore-unmatch",
            "manifest.json",
            "queue.jsonl",
            "state.json",
            "rollup.json",
            "merge.jsonl",
            "gate",
        ],
        &[0],
    )?;
    fs::create_dir_all(checkout.join("gate"))
        .map_err(|source| path_error(&checkout.join("gate"), source))?;
    set_private_directory_mode(&checkout.join("gate"))?;
    for (relative, bytes) in &tree.files {
        let path = checkout.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| path_error(parent, source))?;
        }
        fs::write(&path, bytes).map_err(|source| path_error(&path, source))?;
        set_private_file_mode(&path).map_err(|error| PublishError::InvalidRecord {
            path: path.display().to_string(),
            message: error.to_string(),
        })?;
    }
    git_required(
        Some(checkout),
        &[
            "add",
            "manifest.json",
            "queue.jsonl",
            "state.json",
            "rollup.json",
            "merge.jsonl",
            "gate",
        ],
        &[0],
    )?;
    Ok(())
}

fn scoped_required(
    options: &PublishOptions<'_>,
    permissions: &str,
    program: &str,
    arguments: &[std::ffi::OsString],
    accepted: &[i32],
    minter: &mut dyn InstallationTokenMinter,
) -> Result<Output, PublishError> {
    let display = format!(
        "{program} {}",
        arguments
            .iter()
            .map(|value| value.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ")
    );
    let command = std::iter::once(program.into())
        .chain(arguments.iter().cloned())
        .collect::<Vec<std::ffi::OsString>>();
    let repository = options.destination.repository().as_str();
    let output = authenticated_output(
        options.paths,
        ScopedAppTokenRequest::new("publisher", repository, repository, permissions),
        &command,
        minter,
    )
    .map_err(|error| PublishError::Command {
        command: display.clone(),
        detail: error.to_string(),
    })?;
    require_status(&display, output, accepted)
}

fn git_required(
    directory: Option<&Path>,
    arguments: &[&str],
    accepted: &[i32],
) -> Result<Output, PublishError> {
    let mut command = Command::new("git");
    if let Some(directory) = directory {
        command.arg("-C").arg(directory);
    }
    command.args(arguments);
    let display = match directory {
        Some(directory) => format!("git -C {} {}", directory.display(), arguments.join(" ")),
        None => format!("git {}", arguments.join(" ")),
    };
    let output = command
        .output()
        .map_err(|error| command_spawn_error(&display, error))?;
    require_status(&display, output, accepted)
}

fn require_status(command: &str, output: Output, accepted: &[i32]) -> Result<Output, PublishError> {
    if output
        .status
        .code()
        .is_some_and(|code| accepted.contains(&code))
    {
        Ok(output)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let detail = if stderr.is_empty() {
            format!("process exited with status {}", output.status)
        } else {
            stderr
        };
        Err(PublishError::Command {
            command: command.to_owned(),
            detail,
        })
    }
}

fn read_json(path: &Path) -> Result<Value, PublishError> {
    let bytes = fs::read(path).map_err(|source| path_error(path, source))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| invalid_record(path.display().to_string(), error.to_string()))
}

fn read_jsonl(path: &Path) -> Result<Vec<Value>, PublishError> {
    let text = fs::read_to_string(path).map_err(|source| path_error(path, source))?;
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            serde_json::from_str(line).map_err(|error| {
                invalid_record(
                    path.display().to_string(),
                    format!("row {}: {error}", index + 1),
                )
            })
        })
        .collect()
}

fn filtered_object(
    allowlist: &Allowlist,
    shape: &str,
    source: &Value,
) -> Result<Map<String, Value>, PublishError> {
    let fields = allowlist
        .fields
        .get(shape)
        .ok_or_else(|| PublishError::InvalidAllowlist {
            path: "publication allowlist".to_owned(),
            message: format!("missing shape {shape}"),
        })?;
    let Some(source) = source.as_object() else {
        return Ok(Map::new());
    };
    let mut output = Map::new();
    for field in fields {
        if let Some(value) = source.get(field) {
            output.insert(field.clone(), value.clone());
        }
    }
    Ok(output)
}

fn record_unknown(
    allowlist: &Allowlist,
    shape: &str,
    source: &Value,
    prefix: &str,
    drops: &mut BTreeMap<String, u64>,
) -> Result<(), PublishError> {
    let fields = allowlist
        .fields
        .get(shape)
        .ok_or_else(|| PublishError::InvalidAllowlist {
            path: "publication allowlist".to_owned(),
            message: format!("missing shape {shape}"),
        })?;
    let allowed = fields.iter().map(String::as_str).collect::<BTreeSet<_>>();
    if let Some(object) = source.as_object() {
        for field in object
            .keys()
            .filter(|field| !allowed.contains(field.as_str()))
        {
            increment(drops, &format!("{prefix}{field}"));
        }
    }
    Ok(())
}

fn object_array(value: Option<&Value>) -> Vec<&Value> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|value| value.is_object())
        .collect()
}

fn valid_day_shape(value: &str) -> bool {
    value.len() == 10
        && value.as_bytes()[4] == b'-'
        && value.as_bytes()[7] == b'-'
        && value
            .bytes()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
}

fn require_object(value: &Value, description: &str) -> Result<(), PublishError> {
    if value.is_object() {
        Ok(())
    } else {
        Err(invalid_record(description, "expected a JSON object"))
    }
}

fn required_string<'a>(
    value: &'a Value,
    field: &str,
    description: &str,
) -> Result<&'a str, PublishError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_record(description, format!("{field} must be a string")))
}

fn increment(counts: &mut BTreeMap<String, u64>, field: &str) {
    *counts.entry(field.to_owned()).or_default() += 1;
}

fn increment_object_number(value: &mut Value, field: &str) -> Result<(), PublishError> {
    let object = value
        .as_object_mut()
        .ok_or_else(|| invalid_record("rollup", "counter is not an object"))?;
    let current = object.get(field).and_then(Value::as_u64).unwrap_or(0);
    object.insert(field.to_owned(), json!(current + 1));
    Ok(())
}

fn sorted(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(sorted).collect()),
        Value::Object(object) => {
            let mut names = object.keys().collect::<Vec<_>>();
            names.sort();
            let mut sorted_object = Map::new();
            for name in names {
                sorted_object.insert(name.clone(), sorted(&object[name]));
            }
            Value::Object(sorted_object)
        }
        value => value.clone(),
    }
}

fn pretty_json(value: &Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(&sorted(value)).expect("publication JSON serializes");
    bytes.push(b'\n');
    bytes
}

fn jsonl(values: &[Value]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for value in values {
        bytes.extend(serde_json::to_vec(value).expect("publication row serializes"));
        bytes.push(b'\n');
    }
    bytes
}

fn invalid_allowlist(path: &Path, message: impl Into<String>) -> PublishError {
    PublishError::InvalidAllowlist {
        path: path.display().to_string(),
        message: message.into(),
    }
}

fn invalid_record(path: impl Into<String>, message: impl Into<String>) -> PublishError {
    PublishError::InvalidRecord {
        path: path.into(),
        message: message.into(),
    }
}

fn path_error(path: &Path, source: io::Error) -> PublishError {
    PublishError::Io {
        path: path.display().to_string(),
        source,
    }
}

#[cfg(unix)]
fn set_private_directory_mode(path: &Path) -> Result<(), PublishError> {
    use std::os::unix::fs::PermissionsExt;

    // The checkout contains only public records, but its Git metadata includes
    // remote configuration that a permissive process umask must not expose.
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|source| path_error(path, source))
}

#[cfg(not(unix))]
fn set_private_directory_mode(_path: &Path) -> Result<(), PublishError> {
    Ok(())
}

fn command_spawn_error(command: &str, error: io::Error) -> PublishError {
    PublishError::Command {
        command: command.to_owned(),
        detail: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    fn fixture_options<'a>(
        paths: &'a OstromPaths,
        plugin_root: &'a Path,
        destination: &'a PublishDestination,
    ) -> PublishOptions<'a> {
        PublishOptions {
            paths,
            plugin_root,
            destination,
            published_at: "2026-08-01T00:05:00Z".parse().expect("fixture time"),
            cadence_hours: 24,
        }
    }

    #[test]
    fn derivation_filters_narration_counts_drops_and_retains_all_time_rollups() {
        let root = tempdir().expect("publication fixture");
        let plugin = root.path().join("plugin");
        fs::create_dir_all(plugin.join("config")).expect("create plugin config");
        fs::write(
            plugin.join("config/publish-allowlist.json"),
            include_str!("../../../plugins/ostrom/config/publish-allowlist.json"),
        )
        .expect("write allowlist");
        let paths = OstromPaths {
            config: root.path().to_path_buf(),
            state: root.path().to_path_buf(),
        };
        fs::write(
            paths.queue_file(),
            concat!(
                r##"{"id":"placeholder-org/alpha#1","repo":"placeholder-org/alpha","ref":"#1","title":"Placeholder","kind":"decision","mandate":{"reason":"placeholder reason"},"state":"pending","opened":"2026-07-31T00:00:00Z","age_days":1,"aged_out":false,"needs_judgment":true,"blocked_by":[],"private_note":"drop"}"##,
                "\n",
            ),
        )
        .expect("write queue");
        fs::write(
            root.path().join("gate.jsonl"),
            concat!(
                r#"{"ts":"2026-04-01T00:00:00Z","pr":"placeholder-org/alpha#2","head_sha":"placeholder-sha","verdict":"fail","already_judged":false,"conditions":[]}"#,
                "\n",
                r#"{"ts":"2026-08-01T00:00:00Z","pr":"placeholder-org/alpha#3","head_sha":"placeholder-sha","verdict":"pass","already_judged":false,"conditions":[{"name":"future_condition","result":"pass","tier":[],"detail":{"narration":"drop"}}]}"#,
                "\n",
            ),
        )
        .expect("write gate");
        fs::write(
            paths.merge_file(),
            concat!(
                r#"{"pr":"placeholder-org/alpha#4","order_id":"placeholder-order","opened_at":"2026-03-01T00:00:00Z","merged_at":"2026-04-01T00:00:00Z","opened_by_class":"loop","merged_by_class":"principal","head_sha":"placeholder-merge-sha","actor_login":"placeholder-operator"}"#,
                "\n",
                r#"{"pr":"placeholder-org/alpha#4","order_id":"placeholder-order","opened_at":"2026-03-01T00:00:00Z","merged_at":"2026-04-01T00:00:00Z","opened_by_class":"loop","merged_by_class":"principal","head_sha":"placeholder-merge-sha","actor_login":"placeholder-operator"}"#,
                "\n",
            ),
        )
        .expect("write merge facts");
        fs::write(
            paths.sweep_state_file(),
            r#"{"version":2,"sweep_mode":"full","repos":{"placeholder-org/alpha":{"items":{"placeholder-org/alpha#1":{"classification":"unclassified","fingerprint":"placeholder","first_seen":"2026-07-31T00:00:00Z","updated":"2026-08-01T00:00:00Z","matched_selector":"default:unclassified","stuck":false}},"policy":{},"scope_changes":{},"notice":{"kind":"baseline","reported":false,"text":"drop"}}}}"#,
        )
        .expect("write state");
        let destination = PublishDestination::explicit(
            RepositoryName::new("placeholder-org/alpha").expect("placeholder repository"),
        );
        let options = fixture_options(&paths, &plugin, &destination);
        let allowlist = load_allowlist(&plugin.join("config/publish-allowlist.json"))
            .expect("load fixture allowlist");
        let tree = derive_tree(&options, &allowlist).expect("derive public tree");

        assert!(!tree.files.contains_key(Path::new("gate/2026-04-01.jsonl")));
        assert!(tree.files.contains_key(Path::new("gate/2026-08-01.jsonl")));
        let manifest: Value = serde_json::from_slice(&tree.files[Path::new("manifest.json")])
            .expect("parse manifest");
        assert_eq!(
            manifest["schema_id"],
            "git:b51c9bd1bc47dfa28bd6e168a8573b764bff0d58"
        );
        assert_eq!(manifest["record_counts"]["merge"], 1);
        assert_eq!(manifest["retention"]["merge"], "forever");
        assert_eq!(manifest["dropped_fields"]["queue"]["private_note"], 1);
        assert_eq!(manifest["dropped_fields"]["gate"]["conditions[].detail"], 1);
        assert_eq!(manifest["dropped_fields"]["merge"]["actor_login"], 2);
        assert_eq!(
            manifest["dropped_fields"]["state"]["repos.*.notice.text"],
            1
        );
        let rollup: Value =
            serde_json::from_slice(&tree.files[Path::new("rollup.json")]).expect("parse rollup");
        assert_eq!(rollup["verdicts_by_day"]["2026-04-01"]["fail"], 1);
        assert!(rollup.get("velocity_by_day").is_none());
        let queue = String::from_utf8(tree.files[Path::new("queue.jsonl")].clone())
            .expect("queue is UTF-8");
        assert!(!queue.contains("private_note"));
        let merge = String::from_utf8(tree.files[Path::new("merge.jsonl")].clone())
            .expect("merge facts are UTF-8");
        assert_eq!(merge.lines().count(), 1);
        assert!(!merge.contains("login"));
        assert!(!merge.contains("placeholder-operator"));
    }

    #[test]
    fn unchanged_tree_reuses_the_previous_publication_time() {
        let root = tempdir().expect("prior publication checkout");
        fs::create_dir(root.path().join("gate")).expect("create gate directory");
        let current_manifest = json!({
            "published_at": "2026-08-01T00:05:00Z",
            "stable": "placeholder",
        });
        let previous_manifest = json!({
            "published_at": "2026-07-31T00:05:00Z",
            "stable": "placeholder",
        });
        fs::write(
            root.path().join("manifest.json"),
            pretty_json(&previous_manifest),
        )
        .expect("write prior manifest");
        fs::write(root.path().join("queue.jsonl"), b"").expect("write prior queue");
        let mut tree = DerivedTree {
            files: BTreeMap::from([
                (
                    PathBuf::from("manifest.json"),
                    pretty_json(&current_manifest),
                ),
                (PathBuf::from("queue.jsonl"), Vec::new()),
            ]),
        };

        reuse_previous_time_if_unchanged(root.path(), &mut tree)
            .expect("compare stable publication");

        let manifest: Value = serde_json::from_slice(&tree.files[Path::new("manifest.json")])
            .expect("parse reused manifest");
        assert_eq!(manifest["published_at"], "2026-07-31T00:05:00Z");
    }
}
