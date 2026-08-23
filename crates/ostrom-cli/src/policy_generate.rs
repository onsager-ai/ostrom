use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
};

use ostrom_core::{
    CheckDefinition, DefaultDisposition, GateConfig, MandateConfig, NormalizedList,
    PolicyCandidate, PolicyManifest, PolicySelector, RuleDecl, glob_matches,
};
use ostrom_store::{OstromPaths, PolicyBundle, load_central_config, load_central_gate_config};
use serde_json::{Value, json};
use thiserror::Error;

use crate::policy_manifest;

#[derive(Debug, Default)]
struct SourceComments {
    fields: BTreeMap<(Option<String>, String), Vec<String>>,
    selectors: BTreeMap<(Option<String>, String, String), Vec<String>>,
    projects: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Default)]
struct DenyGroup {
    selectors: BTreeSet<String>,
    descriptions: BTreeSet<String>,
}

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
    let mandate_comments = read_comments(&mandates_path)?;
    let gate_comments = read_comments(&gate_path)?;
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
            let bundle =
                policy_manifest::load_unsigned_bundle(paths, &output).map_err(|error| {
                    GenerateError::Resolution {
                        path: output.clone(),
                        message: error.to_string(),
                    }
                })?;
            report_portability_lints(&manifest, &output);
            let items = open_items(&state, repository)?;
            verify_repository(&mandates, &gate, project, &bundle, &manifest, &items)?;
            println!("verified: {repository} ({} open items)", items.len());
        }
        return Ok(());
    }

    for project in &mandates.projects {
        let repository = project.repo.as_str();
        let manifest = generate_manifest(&gate, project, &mandate_comments, &gate_comments)?;
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
    let count = mandates.projects.len();
    let noun = if count == 1 { "manifest" } else { "manifests" };
    println!("\nWARNING: generated {count} unsigned policy {noun}.");
    println!(concat!(
        "These manifests are not yet in effect. Sign every generated `ostrom.yaml` with ",
        "`ostrom sign --key-id <key-id> --key <private-key.pem> <manifest>` and configure ",
        "`OSTROM_POLICY_TRUSTED_KEYS` with the matching public keys before running ",
        "`ostrom sweep`; until then, commands that discover these files will refuse to load them."
    ));
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
    gate: &GateConfig,
    project: &ostrom_core::ProjectMandate,
    mandate_comments: &SourceComments,
    gate_comments: &SourceComments,
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

    let mut denies = BTreeMap::<String, DenyGroup>::new();
    add_deny_groups(
        &mut denies,
        repository,
        "bounce",
        "needs-review",
        project.bounce.iter().map(|selector| selector.as_str()),
        mandate_comments,
    );
    if let Some(gate_project) = gate
        .projects
        .iter()
        .find(|candidate| candidate.repo.as_str() == repository)
    {
        add_deny_groups(
            &mut denies,
            repository,
            "bounce",
            "needs-review",
            gate_project.bounce.iter().map(|selector| selector.as_str()),
            gate_comments,
        );
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
    add_deny_groups(
        &mut denies,
        repository,
        "excluded",
        "excluded",
        project.excluded.iter().map(|selector| selector.as_str()),
        mandate_comments,
    );
    for (id, group) in denies {
        manifest.denies.insert(
            id,
            rule(
                choose_description(&group.descriptions),
                selectors(group.selectors.iter().map(String::as_str))?,
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

fn add_deny_groups<'a>(
    groups: &mut BTreeMap<String, DenyGroup>,
    repository: &str,
    field: &str,
    suffix: &str,
    values: impl IntoIterator<Item = &'a str>,
    comments: &SourceComments,
) {
    let mut by_subject = BTreeMap::<String, Vec<&str>>::new();
    for selector in values {
        by_subject
            .entry(selector_subject(selector))
            .or_default()
            .push(selector);
    }
    let one_subject = by_subject.len() == 1;
    let mut concerns = BTreeMap::<(bool, String), (Vec<String>, DenyGroup)>::new();
    for (subject, selectors) in by_subject {
        let descriptions = comments
            .descriptions_for(repository, field, &subject, &selectors, one_subject)
            .into_iter()
            .collect::<BTreeSet<_>>();
        let description = choose_description(&descriptions);
        let key = description
            .as_ref()
            .map_or_else(|| (false, subject.clone()), |reason| (true, reason.clone()));
        let (subjects, concern) = concerns.entry(key).or_default();
        subjects.push(subject);
        concern
            .selectors
            .extend(selectors.iter().map(|selector| (*selector).to_owned()));
        concern.descriptions.extend(description);
    }
    for (_, (subjects, concern)) in concerns {
        let id = format!("{}-{suffix}", subjects.join("-and-"));
        let group = groups.entry(id).or_default();
        group.selectors.extend(concern.selectors);
        group.descriptions.extend(concern.descriptions);
    }
}

fn selector_subject(selector: &str) -> String {
    let (prefix, value) = selector.split_once(':').unwrap_or(("rule", selector));
    let value = match prefix {
        "label" => value
            .strip_prefix("area:")
            .or_else(|| value.strip_prefix("risk:"))
            .unwrap_or(value),
        _ => value,
    };
    let segment = if prefix == "path" {
        value
            .split('/')
            .rev()
            .find(|segment| segment.chars().any(char::is_alphanumeric))
            .unwrap_or(value)
    } else {
        value
    };
    let subject = slug(segment);
    if subject.is_empty() {
        prefix.to_owned()
    } else if prefix == "ref" && subject.chars().all(|character| character.is_ascii_digit()) {
        format!("ref-{subject}")
    } else {
        subject
    }
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
        // `ref:` names one specific item. That is the operator's queue, not a
        // property of the repository — the same reason `reserved:` does not
        // travel into a repository manifest. #358 excluded the prefix from the
        // manifest vocabulary deliberately, and its closed-set test asserts it.
        .filter(|selector| !selector.starts_with("ref:"))
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

fn read_comments(path: &Path) -> Result<SourceComments, GenerateError> {
    let source = fs::read_to_string(path).map_err(|source| GenerateError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(extract_comments(&source))
}

fn extract_comments(source: &str) -> SourceComments {
    let mut comments = SourceComments::default();
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
            if let Some(comment) = take_comment(&mut pending) {
                comments
                    .projects
                    .entry(name.to_owned())
                    .or_default()
                    .push(comment);
            }
            repository = Some(name.to_owned());
            field = None;
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
            if let Some(comment) = take_comment(&mut pending) {
                comments
                    .fields
                    .entry((owner.clone(), name.clone()))
                    .or_default()
                    .push(comment);
            }
            if let Some(comment) = inline_comment.filter(|comment| !comment.is_empty()) {
                comments
                    .fields
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
                let selector = trimmed.strip_prefix('-').map(str::trim).unwrap_or_default();
                let mut attached = Vec::new();
                if let Some(comment) = take_comment(&mut pending) {
                    attached.push(comment);
                }
                if let Some(comment) = inline_comment.filter(|comment| !comment.is_empty()) {
                    attached.push(comment.to_owned());
                }
                if !selector.is_empty() && !attached.is_empty() {
                    comments
                        .selectors
                        .entry((owner, name.clone(), unquote(selector).to_owned()))
                        .or_default()
                        .extend(attached);
                }
            }
        } else {
            field = None;
            pending.clear();
        }
    }
    comments
}

fn take_comment(lines: &mut Vec<String>) -> Option<String> {
    if lines.is_empty() {
        None
    } else {
        Some(std::mem::take(lines).join(" "))
    }
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(value)
}

impl SourceComments {
    fn descriptions_for(
        &self,
        repository: &str,
        field: &str,
        subject: &str,
        selectors: &[&str],
        one_subject: bool,
    ) -> Vec<String> {
        let owner = Some(repository.to_owned());
        let field_comments = self
            .fields
            .get(&(owner.clone(), field.to_owned()))
            .into_iter()
            .flatten();
        let selector_comments = selectors.iter().flat_map(|selector| {
            self.selectors
                .get(&(owner.clone(), field.to_owned(), (*selector).to_owned()))
                .into_iter()
                .flatten()
        });
        let scoped = field_comments
            .chain(selector_comments)
            .flat_map(|comment| comment_sentences(comment))
            .filter(|sentence| {
                purpose_sentence(sentence)
                    && (one_subject || sentence_names_subject(sentence, subject))
            });
        let project = self
            .projects
            .get(repository)
            .into_iter()
            .flatten()
            .flat_map(|comment| comment_sentences(comment))
            .filter(|sentence| {
                purpose_sentence(sentence) && sentence_names_subject(sentence, subject)
            });
        scoped.chain(project).collect()
    }
}

fn comment_sentences(comment: &str) -> Vec<String> {
    let mut sentences = Vec::new();
    let mut start = 0;
    let mut characters = comment.char_indices().peekable();
    while let Some((index, character)) = characters.next() {
        if matches!(character, '.' | '!' | '?')
            && characters
                .peek()
                .is_none_or(|(_, next)| next.is_whitespace())
        {
            let end = index + character.len_utf8();
            let sentence = comment[start..end].trim();
            if !sentence.is_empty() {
                sentences.push(sentence.to_owned());
            }
            start = end;
        }
    }
    let remainder = comment[start..].trim();
    if !remainder.is_empty() {
        sentences.push(remainder.to_owned());
    }
    sentences
}

fn purpose_sentence(sentence: &str) -> bool {
    description_score(sentence) > 0
}

fn sentence_names_subject(sentence: &str, subject: &str) -> bool {
    let sentence = slug(sentence);
    subject
        .split('-')
        .filter(|part| part.len() > 2)
        .all(|part| {
            sentence
                .split('-')
                .any(|word| word == part || word.strip_suffix('s') == Some(part))
        })
}

fn choose_description(descriptions: &BTreeSet<String>) -> Option<String> {
    descriptions
        .iter()
        .max_by(|left, right| {
            description_score(left)
                .cmp(&description_score(right))
                .then_with(|| right.cmp(left))
        })
        .cloned()
}

fn description_score(sentence: &str) -> i32 {
    let sentence = sentence.to_ascii_lowercase();
    let purpose = [
        (" because ", 4),
        (" require", 4),
        (" review", 4),
        (" gate", 2),
        (" protect", 3),
        (" principal", 4),
        (" human", 4),
        (" irreversible", 3),
        (" outside delegat", 4),
        (" not delegat", 4),
        (" stay gated", 3),
    ]
    .iter()
    .filter(|(marker, _)| sentence.contains(marker))
    .map(|(_, score)| score)
    .sum::<i32>();
    let historical = [
        "added ",
        "discovered ",
        " incident",
        "landed ",
        "previously ",
        "removed ",
        "retired ",
        "shipped ",
        "used to ",
    ]
    .iter()
    .filter(|marker| sentence.contains(*marker))
    .count() as i32;
    let dated = sentence
        .split(|character: char| !character.is_ascii_digit())
        .any(|part| part.len() == 4 && part.starts_with("20"));
    purpose - (historical * 8) - i32::from(dated) * 8
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
    bundle: &PolicyBundle,
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
        // A reservation holds one specific item. It is operator state — a queue
        // decision that outlives any policy file — so it is deliberately not
        // carried into a repository manifest, and comparing a central policy
        // that applies it against a generated one that cannot is comparing two
        // different questions. The hold still applies; it simply is not policy.
        if item
            .get("refs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_u64)
            .any(|number| project.reserved.contains(&number))
        {
            continue;
        }
        let candidate = policy_candidate(repository, item);
        let central = central_granted(mandates, gate, project, gate_project, &candidate)?;
        let generated = bundle.decide("", "", &candidate).granted;
        if central != generated {
            differences.push(format!(
                "{id}: authority central={} generated={}",
                verdict(central),
                verdict(generated)
            ));
        }
        let has_required_checks = gate_project
            .is_some_and(|project| !project.required_checks.is_empty())
            || !manifest.checks.is_empty();
        if item.get("type").and_then(Value::as_str) == Some("pr") && has_required_checks {
            // A pull request with no check runs is an ordinary state, not absent
            // evidence: a release pull request often has no CI, and a repository
            // may have none at all. Both sides observe the same empty list and
            // therefore agree. Evidence genuinely never gathered is a different
            // condition, reported per repository by `VerificationEvidence`.
            const NO_CHECK_RUNS: &Vec<Value> = &Vec::new();
            let observed = item
                .get("checks")
                .and_then(Value::as_array)
                .unwrap_or(NO_CHECK_RUNS);
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
        commit_type: title.as_deref().and_then(conventional_type),
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
    #[error("could not resolve generated manifest `{}`: {message}", path.display())]
    Resolution { path: PathBuf, message: String },
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
    use std::collections::BTreeSet;

    use super::{choose_description, extract_comments, selector_subject};

    #[test]
    fn source_comments_are_attached_to_their_repository_field() {
        let comments = extract_comments(
            "projects:\n  - repo: placeholder-org/repo\n    # Copy changes need a human because wording is irreversible.\n    bounce:\n      - label:copy\n",
        );
        assert_eq!(
            comments.fields[&(Some("placeholder-org/repo".to_owned()), "bounce".to_owned())],
            ["Copy changes need a human because wording is irreversible."]
        );
    }

    #[test]
    fn rule_subjects_group_equivalent_selector_vocabulary() {
        assert_eq!(selector_subject("type:release"), "release");
        assert_eq!(selector_subject("label:risk:release"), "release");
        assert_eq!(
            selector_subject("path:.github/workflows/release*"),
            "release"
        );
        assert_eq!(selector_subject("label:area:copy"), "copy");
        assert_eq!(selector_subject("path:infra/**"), "infra");
    }

    #[test]
    fn descriptions_prefer_a_purpose_over_an_incident() {
        let descriptions = BTreeSet::from([
            "Added 2026-08-01 because an earlier release escaped review.".to_owned(),
            "Releases require review because publication is irreversible.".to_owned(),
        ]);
        assert_eq!(
            choose_description(&descriptions).as_deref(),
            Some("Releases require review because publication is irreversible.")
        );
    }
}
