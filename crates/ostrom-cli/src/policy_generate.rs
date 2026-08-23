use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
};

use ostrom_core::{
    CheckDefinition, DefaultDisposition, GateConfig, MandateConfig, NormalizedList,
    PolicyCandidate, PolicyManifest, PolicySelector, RuleDecl, glob_matches,
};
use ostrom_store::{OstromPaths, load_central_config, load_central_gate_config};
use serde_json::{Value, json};
use thiserror::Error;

use crate::policy_manifest;

type CommentMap = BTreeMap<(Option<String>, String), Vec<String>>;

pub(crate) fn run(paths: &OstromPaths, verify: bool) -> Result<(), GenerateError> {
    let mandates_path = paths.config.join("mandates.yaml");
    let gate_path = paths.config.join("gate.yaml");
    let mandates = load_central_config(paths).map_err(|error| GenerateError::Policy {
        path: mandates_path.clone(),
        message: error.to_string(),
    })?;
    let gate = load_central_gate_config(paths).map_err(|message| GenerateError::Policy {
        path: gate_path.clone(),
        message,
    })?;
    let comments = merge_comments([read_comments(&mandates_path)?, read_comments(&gate_path)?]);
    let repositories = repository_paths(&mandates)?;

    if verify {
        let state = read_json(&paths.sweep_state_file())?;
        for project in &mandates.projects {
            let repository = project.repo.as_str();
            let output = repositories
                .get(repository)
                .expect("every roster entry has a resolved path")
                .join("ostrom.yaml");
            let source = fs::read_to_string(&output).map_err(|source| GenerateError::Io {
                path: output.clone(),
                source,
            })?;
            let manifest =
                PolicyManifest::from_yaml(&source).map_err(|error| GenerateError::Manifest {
                    path: output.clone(),
                    message: error.to_string(),
                })?;
            report_portability_lints(&manifest, &output);
            let items = open_items(&state, repository)?;
            verify_repository(&mandates, &gate, project, &manifest, &items)?;
            println!("verified: {repository} ({} open items)", items.len());
        }
        return Ok(());
    }

    for project in &mandates.projects {
        let repository = project.repo.as_str();
        let manifest = generate_manifest(&mandates, &gate, project, &comments)?;
        let output = repositories
            .get(repository)
            .expect("every roster entry has a resolved path")
            .join("ostrom.yaml");
        report_portability_lints(&manifest, &output);
        let yaml = manifest
            .to_yaml()
            .map_err(|error| GenerateError::Manifest {
                path: output.clone(),
                message: error.to_string(),
            })?;
        fs::write(&output, yaml).map_err(|source| GenerateError::Io {
            path: output.clone(),
            source,
        })?;
        println!("generated: {}", output.display());
    }
    Ok(())
}

fn report_portability_lints(manifest: &PolicyManifest, path: &Path) {
    for actor in policy_manifest::repository_actor_names(manifest) {
        eprintln!(
            "lint: repository actor `{actor}` named in `{}` is non-portable across operator rosters",
            path.display()
        );
    }
}

fn generate_manifest(
    mandates: &MandateConfig,
    gate: &GateConfig,
    project: &ostrom_core::ProjectMandate,
    comments: &CommentMap,
) -> Result<PolicyManifest, GenerateError> {
    let repository = project.repo.as_str();
    let mut manifest = PolicyManifest::from_yaml("manifest_version: 1\n")
        .expect("the empty version-one manifest is valid");
    let delegated = selectors(project.delegated.iter().map(|selector| selector.as_str()))?;
    if !delegated.is_empty() {
        manifest
            .grants
            .insert("delegated-changes".to_owned(), rule(None, delegated));
    }
    if project.default == DefaultDisposition::Delegated {
        manifest
            .grants
            .insert("remaining-changes".to_owned(), rule(None, Vec::new()));
    }

    let mut review = mandates
        .bounce_all
        .iter()
        .map(|selector| selector.as_str())
        .chain(project.bounce.iter().map(|selector| selector.as_str()))
        .chain(gate.bounce_all.iter().map(|selector| selector.as_str()))
        .collect::<Vec<_>>();
    if let Some(gate_project) = gate
        .projects
        .iter()
        .find(|candidate| candidate.repo.as_str() == repository)
    {
        review.extend(gate_project.bounce.iter().map(|selector| selector.as_str()));
        let mut used = BTreeSet::new();
        for pattern in &gate_project.required_checks {
            let id = unique_check_id(pattern, &mut used);
            manifest.checks.insert(
                id,
                CheckDefinition {
                    uses: "gh/check-run".to_owned(),
                    inconclusive_policy: None,
                    with: BTreeMap::from([("name".to_owned(), json!(pattern))]),
                },
            );
        }
    }
    review.sort_unstable();
    review.dedup();
    if !review.is_empty() {
        let rationale = comments_for(
            comments,
            repository,
            "bounce",
            "These changes require operator review before work proceeds.",
        );
        manifest.denies.insert(
            "operator-review".to_owned(),
            rule(Some(rationale), selectors(review)?),
        );
    }
    if !project.excluded.is_empty() {
        let rationale = comments_for(
            comments,
            repository,
            "excluded",
            "These changes are intentionally outside delegated repository work.",
        );
        manifest.denies.insert(
            "excluded-changes".to_owned(),
            rule(
                Some(rationale),
                selectors(project.excluded.iter().map(|selector| selector.as_str()))?,
            ),
        );
    }
    manifest
        .validate()
        .map_err(|error| GenerateError::Manifest {
            path: PathBuf::from(repository).join("ostrom.yaml"),
            message: error.to_string(),
        })?;
    Ok(manifest)
}

fn rule(description: Option<String>, selectors: Vec<PolicySelector>) -> RuleDecl {
    RuleDecl {
        description,
        selectors: NormalizedList::from(selectors),
        ..RuleDecl::default()
    }
}

fn selectors<'a>(
    values: impl IntoIterator<Item = &'a str>,
) -> Result<Vec<PolicySelector>, GenerateError> {
    values
        .into_iter()
        .map(|selector| {
            PolicySelector::new(selector).map_err(|error| GenerateError::Selector {
                selector: selector.to_owned(),
                message: error.to_string(),
            })
        })
        .collect()
}

fn unique_check_id(pattern: &str, used: &mut BTreeSet<String>) -> String {
    let subject = match slug(pattern) {
        subject if subject.is_empty() => "required-check".to_owned(),
        subject => subject,
    };
    let stem = format!("{subject}-green");
    let mut id = stem.clone();
    let mut qualifier = "required";
    while !used.insert(id.clone()) {
        id = format!("{subject}-{qualifier}-green");
        qualifier = "additional";
    }
    id
}

fn slug(value: &str) -> String {
    let mut slug = String::new();
    let mut separated = true;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
            separated = false;
        } else if !separated && !slug.is_empty() {
            slug.push('-');
            separated = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    slug
}

fn repository_paths(config: &MandateConfig) -> Result<BTreeMap<String, PathBuf>, GenerateError> {
    let mut candidates = Vec::new();
    for root in &config.search_roots {
        collect_repository_paths(Path::new(root), &mut candidates);
    }
    let mut resolved = BTreeMap::new();
    for project in &config.projects {
        let repository = project.repo.as_str();
        let suffix = Path::new(repository);
        let matches = candidates
            .iter()
            .filter(|candidate| candidate.ends_with(suffix))
            .cloned()
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [path] => {
                resolved.insert(repository.to_owned(), path.clone());
            }
            [] => {
                return Err(GenerateError::RepositoryNotFound {
                    repository: repository.to_owned(),
                    roots: config.search_roots.clone(),
                });
            }
            _ => {
                return Err(GenerateError::AmbiguousRepository {
                    repository: repository.to_owned(),
                    paths: matches,
                });
            }
        }
    }
    Ok(resolved)
}

fn collect_repository_paths(directory: &Path, repositories: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_name() == ".git" {
            if let Some(repository) = path.parent() {
                repositories.push(repository.to_path_buf());
            }
        } else if entry
            .file_type()
            .is_ok_and(|kind| kind.is_dir() && !kind.is_symlink())
        {
            collect_repository_paths(&path, repositories);
        }
    }
}

fn read_comments(path: &Path) -> Result<CommentMap, GenerateError> {
    let source = fs::read_to_string(path).map_err(|source| GenerateError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(extract_comments(&source))
}

fn extract_comments(source: &str) -> CommentMap {
    let mut comments = CommentMap::new();
    let mut pending = Vec::new();
    let mut repository = None::<String>;
    let mut field = None::<String>;
    for line in source.lines() {
        let raw = line.trim();
        let (trimmed, inline_comment) = raw
            .split_once(" #")
            .map_or((raw, None), |(content, comment)| {
                (content.trim_end(), Some(comment.trim()))
            });
        if trimmed.is_empty() {
            pending.clear();
            continue;
        }
        if let Some(comment) = trimmed.strip_prefix('#') {
            let comment = comment.trim();
            if !comment.is_empty() {
                pending.push(comment.to_owned());
            }
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if indent == 2
            && let Some(name) = trimmed.strip_prefix("- repo:").map(str::trim)
        {
            repository = Some(name.to_owned());
            field = None;
            pending.clear();
            continue;
        }
        let field_name = trimmed
            .split_once(':')
            .map(|(name, _)| name)
            .filter(|name| {
                matches!(
                    *name,
                    "bounce" | "bounce_all" | "delegated" | "excluded" | "required_checks"
                )
            })
            .map(str::to_owned);
        if let Some(name) = field_name {
            let owner = if indent == 0 {
                None
            } else {
                repository.clone()
            };
            comments
                .entry((owner.clone(), name.clone()))
                .or_default()
                .append(&mut pending);
            if let Some(comment) = inline_comment.filter(|comment| !comment.is_empty()) {
                comments
                    .entry((owner.clone(), name.clone()))
                    .or_default()
                    .push(comment.to_owned());
            }
            field = Some(name);
            continue;
        }
        if trimmed.starts_with('-') {
            if let Some(name) = &field {
                let owner = if indent <= 2 {
                    None
                } else {
                    repository.clone()
                };
                comments
                    .entry((owner.clone(), name.clone()))
                    .or_default()
                    .append(&mut pending);
                if let Some(comment) = inline_comment.filter(|comment| !comment.is_empty()) {
                    comments
                        .entry((owner, name.clone()))
                        .or_default()
                        .push(comment.to_owned());
                }
            }
        } else {
            field = None;
            pending.clear();
        }
    }
    comments
}

fn merge_comments(maps: impl IntoIterator<Item = CommentMap>) -> CommentMap {
    let mut merged = CommentMap::new();
    for map in maps {
        for (key, values) in map {
            merged.entry(key).or_default().extend(values);
        }
    }
    for values in merged.values_mut() {
        values.sort();
        values.dedup();
    }
    merged
}

fn comments_for(comments: &CommentMap, repository: &str, field: &str, base: &str) -> String {
    let global = (field == "bounce")
        .then(|| comments.get(&(None, "bounce_all".to_owned())))
        .flatten();
    let mut values = global
        .into_iter()
        .flatten()
        .chain(
            comments
                .get(&(Some(repository.to_owned()), field.to_owned()))
                .into_iter()
                .flatten(),
        )
        .cloned()
        .collect::<Vec<_>>();
    values.sort();
    values.dedup();
    if values.is_empty() {
        base.to_owned()
    } else {
        format!("{base} {}", values.join(" "))
    }
}

fn read_json(path: &Path) -> Result<Value, GenerateError> {
    let bytes = fs::read(path).map_err(|source| GenerateError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_slice(&bytes).map_err(|source| GenerateError::Json {
        path: path.to_path_buf(),
        source,
    })
}

fn open_items(state: &Value, repository: &str) -> Result<Vec<Value>, GenerateError> {
    let records = state
        .pointer(&format!(
            "/repos/{}/records",
            repository.replace('~', "~0").replace('/', "~1")
        ))
        .and_then(Value::as_object)
        .ok_or_else(|| GenerateError::VerificationEvidence(repository.to_owned()))?;
    let mut items = records.values().cloned().collect::<Vec<_>>();
    items.sort_by_key(item_id);
    Ok(items)
}

fn verify_repository(
    mandates: &MandateConfig,
    gate: &GateConfig,
    project: &ostrom_core::ProjectMandate,
    manifest: &PolicyManifest,
    items: &[Value],
) -> Result<(), GenerateError> {
    let repository = project.repo.as_str();
    let gate_project = gate
        .projects
        .iter()
        .find(|candidate| candidate.repo.as_str() == repository);
    let mut differences = Vec::new();
    for item in items {
        let id = item_id(item);
        let candidate = policy_candidate(repository, item);
        let central = central_granted(mandates, gate, project, gate_project, &candidate)?;
        let generated = manifest.decide("", "", &candidate).granted;
        if central != generated {
            differences.push(format!(
                "{id}: authority central={} generated={}",
                verdict(central),
                verdict(generated)
            ));
        }
        if item.get("type").and_then(Value::as_str) == Some("pr") {
            let observed = item
                .get("checks")
                .and_then(Value::as_array)
                .ok_or_else(|| GenerateError::CheckEvidence { item: id.clone() })?;
            let central_checks = gate_project
                .into_iter()
                .flat_map(|project| &project.required_checks)
                .map(|pattern| {
                    (
                        "gh/check-run".to_owned(),
                        pattern.clone(),
                        check_outcome(pattern, observed),
                    )
                })
                .collect::<Vec<_>>();
            let mut generated_checks = manifest
                .checks
                .values()
                .map(|definition| {
                    let pattern = definition
                        .with
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("<invalid>")
                        .to_owned();
                    (
                        definition.uses.clone(),
                        pattern.clone(),
                        check_outcome(&pattern, observed),
                    )
                })
                .collect::<Vec<_>>();
            generated_checks.sort();
            let mut central_checks = central_checks;
            central_checks.sort();
            if central_checks != generated_checks {
                differences.push(format!(
                    "{id}: checks central={central_checks:?} generated={generated_checks:?}"
                ));
            }
        }
    }
    if differences.is_empty() {
        Ok(())
    } else {
        Err(GenerateError::Divergence {
            repository: repository.to_owned(),
            differences: differences.join("\n"),
        })
    }
}

fn policy_candidate(repository: &str, item: &Value) -> PolicyCandidate {
    let title = item.get("title").and_then(Value::as_str).map(str::to_owned);
    PolicyCandidate {
        repository: repository.to_owned(),
        labels: strings(item.get("labels")),
        paths: strings(item.get("files")),
        refs: item
            .get("refs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_u64)
            .map(|number| format!("#{number}"))
            .collect(),
        scopes: title.as_deref().map_or_else(Vec::new, conventional_scopes),
        substances: strings(item.get("substances")),
        commit_type: title.as_deref().and_then(conventional_type),
        title,
        actor: None,
        verb: None,
    }
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

fn conventional_type(title: &str) -> Option<String> {
    title
        .split_once(':')
        .map(|(prefix, _)| prefix.split_once('(').map_or(prefix, |(kind, _)| kind))
        .filter(|kind| !kind.is_empty())
        .map(str::to_owned)
}

fn conventional_scopes(title: &str) -> Vec<String> {
    let Some(prefix) = title.split_once(':').map(|(prefix, _)| prefix) else {
        return Vec::new();
    };
    let Some((_, scopes)) = prefix.split_once('(') else {
        return Vec::new();
    };
    scopes
        .strip_suffix(')')
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .map(str::to_owned)
        .collect()
}

fn central_granted(
    mandates: &MandateConfig,
    gate: &GateConfig,
    project: &ostrom_core::ProjectMandate,
    gate_project: Option<&ostrom_core::GateProject>,
    candidate: &PolicyCandidate,
) -> Result<bool, GenerateError> {
    let denied = mandates
        .bounce_all
        .iter()
        .map(|selector| selector.as_str())
        .chain(project.bounce.iter().map(|selector| selector.as_str()))
        .chain(project.excluded.iter().map(|selector| selector.as_str()))
        .chain(gate.bounce_all.iter().map(|selector| selector.as_str()))
        .chain(
            gate_project
                .into_iter()
                .flat_map(|project| &project.bounce)
                .map(|selector| selector.as_str()),
        )
        .map(PolicySelector::new)
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .any(|selector| selector.matches(candidate));
    if denied {
        return Ok(false);
    }
    let delegated = project
        .delegated
        .iter()
        .map(|selector| PolicySelector::new(selector.as_str()))
        .collect::<Result<Vec<_>, _>>()?
        .iter()
        .any(|selector| selector.matches(candidate));
    Ok(delegated || project.default == DefaultDisposition::Delegated)
}

fn check_outcome(pattern: &str, observed: &[Value]) -> &'static str {
    let selected = observed
        .iter()
        .filter(|check| {
            check
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| glob_matches(name, pattern, false))
        })
        .collect::<Vec<_>>();
    if selected.is_empty()
        || selected.iter().any(|check| {
            matches!(
                check
                    .get("state")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_ascii_uppercase()
                    .as_str(),
                "FAILURE"
                    | "ERROR"
                    | "CANCELLED"
                    | "TIMED_OUT"
                    | "ACTION_REQUIRED"
                    | "STALE"
                    | "PENDING"
                    | "EXPECTED"
                    | "QUEUED"
                    | "IN_PROGRESS"
                    | "WAITING"
                    | "REQUESTED"
            )
        })
    {
        "fail"
    } else if selected.iter().all(|check| {
        matches!(
            check
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_ascii_uppercase()
                .as_str(),
            "SUCCESS" | "NEUTRAL" | "SKIPPED"
        )
    }) {
        "pass"
    } else {
        "inconclusive"
    }
}

fn item_id(item: &Value) -> String {
    item.get("id")
        .and_then(Value::as_str)
        .unwrap_or("<unknown-item>")
        .to_owned()
}

const fn verdict(granted: bool) -> &'static str {
    if granted { "granted" } else { "denied" }
}

#[derive(Debug, Error)]
pub(crate) enum GenerateError {
    #[error("could not load central policy `{}`: {message}", path.display())]
    Policy { path: PathBuf, message: String },
    #[error("could not read or write `{}`: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    #[error("could not parse sweep evidence `{}`: {source}", path.display())]
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("could not load generated manifest `{}`: {message}", path.display())]
    Manifest { path: PathBuf, message: String },
    #[error("legacy selector `{selector}` cannot be represented: {message}")]
    Selector { selector: String, message: String },
    #[error("repository `{repository}` was not found beneath search_roots {roots:?}")]
    RepositoryNotFound {
        repository: String,
        roots: Vec<String>,
    },
    #[error("repository `{repository}` resolves ambiguously: {paths:?}")]
    AmbiguousRepository {
        repository: String,
        paths: Vec<PathBuf>,
    },
    #[error("sweep evidence contains no open-item records for `{0}`; run a full sweep first")]
    VerificationEvidence(String),
    #[error("sweep evidence for `{item}` has no named check runs; run a full sweep first")]
    CheckEvidence { item: String },
    #[error("generated manifest for `{repository}` diverges:\n{differences}")]
    Divergence {
        repository: String,
        differences: String,
    },
}

impl From<ostrom_core::PolicySelectorError> for GenerateError {
    fn from(error: ostrom_core::PolicySelectorError) -> Self {
        Self::Selector {
            selector: "<unknown>".to_owned(),
            message: error.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::extract_comments;

    #[test]
    fn source_comments_are_attached_to_their_repository_field() {
        let comments = extract_comments(
            "projects:\n  - repo: placeholder-org/repo\n    # Copy changes need a human because wording is irreversible.\n    bounce:\n      - label:copy\n",
        );
        assert_eq!(
            comments[&(Some("placeholder-org/repo".to_owned()), "bounce".to_owned())],
            ["Copy changes need a human because wording is irreversible."]
        );
    }
}
