use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Component, Path, PathBuf},
    process::Command,
    sync::Once,
};

use chrono::{DateTime, Utc};
use ostrom_core::{
    ActorDecl, CheckDefinition, LoopDecl, OperationDecl, PolicyManifest, RuleDecl, SelectorFinding,
    SelectorUniverse,
};
use ostrom_store::{OstromPaths, PolicyBundle, PolicyExplanation, SweepFixture};
use serde::Deserialize;
use serde_json::Value as JsonValue;
use serde_yaml::{Mapping, Value};
use thiserror::Error;

pub(crate) fn run_validate(path: &Path, normalized: bool) -> Result<(), PolicyLoadError> {
    let loaded = load(path)?;
    // Resolve the ladder even when normalized output was not requested. This
    // catches a present environment value with the wrong declared type while
    // never placing its raw value in diagnostics.
    loaded
        .resolve_inputs(ostrom_store::environment::declared_input)
        .map_err(|error| PolicyLoadError::Validation(error.to_string()))?;

    let verbs = command_verbs()
        .map(str::to_owned)
        .chain(loaded.operations.keys().cloned());
    let universe = SelectorUniverse::from_manifest(&loaded, verbs);
    let findings = loaded.selector_findings(&universe);
    for finding in &findings {
        if let SelectorFinding::Empty {
            rule,
            selector,
            repository,
            message,
            ..
        } = finding
        {
            eprintln!(
                "finding: rule `{rule}` selector `{selector}`{}: {message}",
                repository
                    .as_deref()
                    .map_or_else(String::new, |value| format!(" in `{value}`"))
            );
        }
    }
    if let Some(finding) = findings.iter().find(|finding| finding.is_error()) {
        return Err(PolicyLoadError::Selector(format_finding(finding)));
    }

    if normalized {
        print!(
            "{}",
            loaded
                .to_yaml()
                .map_err(|error| PolicyLoadError::Validation(error.to_string()))?
        );
    } else {
        println!("valid: {}", path.display());
    }
    Ok(())
}

pub(crate) fn run_sign(
    path: &Path,
    key_id: &str,
    private_key: &Path,
) -> Result<(), PolicyLoadError> {
    let path = normalize_manifest_path(path);
    let manifest = if path.file_name().is_some_and(|name| name == "config.yaml") {
        load_composed(&path)?
    } else {
        load_unverified(&path)?
    };
    let signature = ostrom_store::sign_policy_manifest(&manifest, &path, key_id, private_key)?;
    println!("signed: {}", signature.display());
    Ok(())
}

pub(crate) fn load_bundle(
    paths: &OstromPaths,
    path: &Path,
) -> Result<PolicyBundle, PolicyLoadError> {
    let mut manifest = load(path)?;
    let overlay_path = paths.private_config_file();
    if !overlay_path.is_file() || overlay_path == path {
        return Ok(PolicyBundle::repository(manifest));
    }

    let overlay = load_composed(&overlay_path)?;
    verify(&overlay, &overlay_path)?;
    if overlay.manifest_version != manifest.manifest_version {
        return Err(PolicyLoadError::OverlayVersion {
            path: overlay_path,
            version: overlay.manifest_version,
            expected: manifest.manifest_version,
        });
    }
    if let Some(rule) = overlay.grants.keys().next() {
        return Err(PolicyLoadError::OverlayGrant {
            path: overlay_path,
            rule: rule.clone(),
        });
    }
    let overlay_denies = merge_overlay(&mut manifest, overlay);
    validate_manifest(&manifest)?;
    Ok(PolicyBundle::layered(manifest, overlay_denies))
}

pub(crate) fn default_manifest_path(paths: &OstromPaths, cwd: &Path) -> Option<PathBuf> {
    let repository = repository_root(cwd);
    let directories = repository.as_ref().map_or_else(
        || cwd.ancestors().map(Path::to_path_buf).collect::<Vec<_>>(),
        |root| {
            let mut directories = Vec::new();
            for directory in cwd.ancestors() {
                directories.push(directory.to_path_buf());
                if directory == root {
                    break;
                }
            }
            directories
        },
    );

    if let Some(path) = directories
        .iter()
        .map(|directory| directory.join("ostrom.yaml"))
        .find(|path| path.is_file())
    {
        return Some(path);
    }
    if let Some(path) = directories
        .iter()
        .map(|directory| directory.join(".ostrom/manifest.yml"))
        .find(|path| path.is_file())
    {
        warn_legacy_manifest(&path);
        return Some(path);
    }

    if repository.is_none() {
        let path = paths.config.join("manifest.yml");
        if path.is_file() {
            warn_legacy_manifest(&path);
            return Some(path);
        }
    }
    None
}

pub(crate) fn load_optional_bundle(
    paths: &OstromPaths,
    cwd: &Path,
) -> Result<Option<PolicyBundle>, PolicyLoadError> {
    // Absence is not an error here. Until the cutover in #364 the retired
    // surfaces are still the running system, and `sweep` and the check planner
    // must keep working in a repository that has no manifest yet. Discovery is
    // still strict — it never falls through to the operator's own policy — so
    // `None` means ungoverned, not "somebody else's rules".
    let Some(path) = default_manifest_path(paths, cwd) else {
        return Ok(None);
    };
    load_bundle(paths, &path).map(Some)
}

fn repository_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .find(|directory| directory.join(".git").exists())
        .map(Path::to_path_buf)
}

fn repository_name(cwd: &Path) -> PathBuf {
    repository_root(cwd).unwrap_or_else(|| cwd.to_path_buf())
}

fn warn_legacy_manifest(path: &Path) {
    static NOTICE: Once = Once::new();
    NOTICE.call_once(|| {
        eprintln!(
            "warning: legacy policy manifest `{}` is deprecated; move repository policy to `ostrom.yaml`",
            path.display()
        );
    });
}

fn merge_overlay(repository: &mut PolicyManifest, overlay: PolicyManifest) -> BTreeSet<String> {
    let shipped = ostrom_core::ManifestDefaults::default();
    if repository.defaults.stalls_after == shipped.stalls_after {
        repository.defaults.stalls_after = overlay.defaults.stalls_after;
    }
    if repository.defaults.check == shipped.check {
        repository.defaults.check = overlay.defaults.check;
    }
    if repository.defaults.grant == shipped.grant {
        repository.defaults.grant = overlay.defaults.grant;
    }
    if repository.defaults.deny == shipped.deny {
        repository.defaults.deny = overlay.defaults.deny;
    }
    if repository.defaults.r#loop.concurrent.is_none() {
        repository.defaults.r#loop.concurrent = overlay.defaults.r#loop.concurrent;
    }
    if repository.defaults.r#loop.spend_usd.is_none() {
        repository.defaults.r#loop.spend_usd = overlay.defaults.r#loop.spend_usd;
    }
    if repository.defaults.r#loop.tokens.is_none() {
        repository.defaults.r#loop.tokens = overlay.defaults.r#loop.tokens;
    }

    merge_fallback(&mut repository.inputs, overlay.inputs);
    merge_fallback(&mut repository.actors, overlay.actors);
    merge_fallback(&mut repository.checks, overlay.checks);
    merge_fallback(&mut repository.operations, overlay.operations);
    merge_fallback(&mut repository.loops, overlay.loops);
    let mut overlay_denies = BTreeSet::new();
    for (id, declaration) in overlay.denies {
        if let std::collections::btree_map::Entry::Vacant(entry) = repository.denies.entry(id) {
            overlay_denies.insert(entry.key().clone());
            entry.insert(declaration);
        }
    }
    overlay_denies
}

fn merge_fallback<T>(target: &mut BTreeMap<String, T>, fallback: BTreeMap<String, T>) {
    for (id, value) in fallback {
        target.entry(id).or_insert(value);
    }
}

pub(crate) struct ExplainOptions<'a> {
    pub paths: &'a OstromPaths,
    pub working_directory: &'a Path,
    pub target: &'a str,
    pub manifest: Option<&'a Path>,
    pub fixture: Option<&'a Path>,
    pub observed_at: DateTime<Utc>,
    pub actor: &'a str,
    pub operation: &'a str,
}

pub(crate) fn run_explain(options: &ExplainOptions<'_>) -> Result<String, PolicyLoadError> {
    let manifest_path = options
        .manifest
        .map(Path::to_path_buf)
        .or_else(|| default_manifest_path(options.paths, options.working_directory))
        .ok_or_else(|| {
            PolicyLoadError::UngovernedRepository(repository_name(options.working_directory))
        })?;
    let bundle = load_bundle(options.paths, &manifest_path)?;
    let target = ExplainTarget::parse(options.target)?;
    let pull_request = if let Some(fixture) = options.fixture {
        fixture_pull_request(fixture, &target)?
    } else {
        acquire_pull_request(&target, options.working_directory)?
    };
    let explanation = bundle.explain_pull_request(
        &target.repository,
        &pull_request,
        options.actor,
        options.operation,
    );
    let first_held = read_first_held(options.paths, &target.full);
    Ok(render_explanation(
        &target,
        &explanation,
        first_held,
        options.observed_at,
    ))
}

struct ExplainTarget {
    repository: String,
    number: u64,
    full: String,
}

impl ExplainTarget {
    fn parse(value: &str) -> Result<Self, PolicyLoadError> {
        let Some((repository, number)) = value.rsplit_once('#') else {
            return Err(PolicyLoadError::InvalidTarget);
        };
        let mut parts = repository.split('/');
        if !matches!(
            (parts.next(), parts.next(), parts.next()),
            (Some(owner), Some(repo), None) if !owner.is_empty() && !repo.is_empty()
        ) || number.starts_with('0')
        {
            return Err(PolicyLoadError::InvalidTarget);
        }
        let number = number
            .parse::<u64>()
            .ok()
            .filter(|number| *number > 0)
            .ok_or(PolicyLoadError::InvalidTarget)?;
        Ok(Self {
            repository: repository.to_owned(),
            number,
            full: value.to_owned(),
        })
    }
}

fn fixture_pull_request(path: &Path, target: &ExplainTarget) -> Result<JsonValue, PolicyLoadError> {
    let source = fs::read(path).map_err(|source| PolicyLoadError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let fixture = serde_json::from_slice::<SweepFixture>(&source).map_err(|source| {
        PolicyLoadError::Fixture {
            path: path.to_path_buf(),
            source,
        }
    })?;
    fixture
        .repositories
        .into_iter()
        .find(|snapshot| snapshot.repo.as_str() == target.repository)
        .and_then(|snapshot| {
            snapshot.open_prs.into_iter().find(|pull_request| {
                pull_request.get("number").and_then(JsonValue::as_u64) == Some(target.number)
            })
        })
        .ok_or_else(|| PolicyLoadError::PullRequestNotFound(target.full.clone()))
}

fn acquire_pull_request(
    target: &ExplainTarget,
    working_directory: &Path,
) -> Result<JsonValue, PolicyLoadError> {
    let output = Command::new("gh")
        .current_dir(working_directory)
        .args([
            "pr",
            "view",
            &target.number.to_string(),
            "--repo",
            &target.repository,
            "--json",
            "number,title,labels,files,statusCheckRollup,state",
        ])
        .output()
        .map_err(PolicyLoadError::GitHub)?;
    if !output.status.success() {
        return Err(PolicyLoadError::GitHubResponse(
            String::from_utf8_lossy(&output.stderr)
                .trim()
                .chars()
                .take(500)
                .collect(),
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|source| PolicyLoadError::Fixture {
        path: PathBuf::from("gh pr view"),
        source,
    })
}

fn read_first_held(paths: &OstromPaths, id: &str) -> Option<DateTime<Utc>> {
    let state = fs::read(paths.sweep_state_file()).ok()?;
    let state = serde_json::from_slice::<JsonValue>(&state).ok()?;
    let timestamp = state
        .pointer(&format!("/policy_holds/{}", escape_pointer(id)))?
        .get("first_held")?
        .as_str()?;
    DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

fn escape_pointer(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

fn render_explanation(
    target: &ExplainTarget,
    explanation: &PolicyExplanation,
    first_held: Option<DateTime<Utc>>,
    observed_at: DateTime<Utc>,
) -> String {
    let mut output = format!("{}\n\nSUBJECT RULES\n", target.full);
    for rule in &explanation.rules {
        let selectors = rule
            .selectors
            .iter()
            .filter(|selector| selector.projection == "subject")
            .map(|selector| selector.selector.as_str())
            .collect::<Vec<_>>();
        let predicate = if selectors.is_empty() {
            "*".to_owned()
        } else {
            selectors.join(", ")
        };
        output.push_str(&format!(
            "  {:10} {:5} {:24} {:38} {}{}\n",
            rule.layer.name(),
            rule.kind,
            rule.id,
            predicate,
            match_word(rule.subject_matched),
            if rule.subject_matched {
                String::new()
            } else {
                format!("  unmatched: {}", unmatched_name(rule.unmatched))
            }
        ));
    }
    output.push_str(&format!(
        "\nACTOR RULES ({} / {})\n",
        explanation.actor, explanation.operation
    ));
    for rule in &explanation.rules {
        output.push_str(&format!(
            "  {:10} {:5} {:24} {:38} {}\n",
            rule.layer.name(),
            rule.kind,
            rule.id,
            format!(
                "actor={} operation={}",
                explanation.actor, explanation.operation
            ),
            match_word(rule.actor_matched)
        ));
    }
    output.push_str("\nAGGREGATE\n");
    output.push_str(&format!(
        "  decide       {:12} {}\n",
        if explanation.granted {
            explanation.actor.as_str()
        } else {
            "principal"
        },
        explanation.decision_source
    ));
    if explanation.floor {
        output
            .push_str("               no rule granted this pull request; principal is the floor\n");
    }
    for rule in explanation.rules.iter().filter(|rule| rule.matched) {
        if let Some(requirement) = &rule.requirement {
            output.push_str(&format!(
                "  requires     {:12} {:12} {} (rule {})\n",
                requirement.check, requirement.status, requirement.source, rule.id
            ));
        }
    }
    if !explanation.granted {
        let held_seconds = first_held.map_or(0, |first| {
            observed_at
                .signed_duration_since(first)
                .num_seconds()
                .max(0) as u64
        });
        let held_days = held_seconds / 86_400;
        let stalled = held_seconds >= explanation.stalls_after.as_seconds();
        output.push_str(&format!(
            "  held         {held_days}d of {}{}\n",
            explanation.stalls_after,
            if stalled { "  STALLED" } else { "" }
        ));
        output.push_str(&format!(
            "  stalls_after {:12} {}\n",
            explanation.stalls_after, explanation.stalls_source
        ));
        if first_held.is_none() {
            output.push_str("               first-held time has not yet been recorded by sweep\n");
        }
    }
    output.push_str(&format!(
        "  verdict      {}\n",
        if explanation.granted { "MERGE" } else { "HOLD" }
    ));
    output
}

fn match_word(matched: bool) -> &'static str {
    if matched { "MATCH" } else { "no match" }
}

fn unmatched_name(policy: ostrom_core::UnmatchedPolicy) -> &'static str {
    match policy {
        ostrom_core::UnmatchedPolicy::Block => "block",
        ostrom_core::UnmatchedPolicy::Warn => "warn",
        ostrom_core::UnmatchedPolicy::Pass => "pass",
    }
}

fn format_finding(finding: &SelectorFinding) -> String {
    match finding {
        SelectorFinding::Error {
            rule,
            selector,
            repository,
            message,
            ..
        }
        | SelectorFinding::Empty {
            rule,
            selector,
            repository,
            message,
            ..
        } => format!(
            "rule `{rule}` selector `{selector}`{}: {message}",
            repository
                .as_deref()
                .map_or_else(String::new, |value| format!(" in `{value}`"))
        ),
    }
}

fn command_verbs() -> impl Iterator<Item = &'static str> {
    [
        "audit",
        "check",
        "config",
        "credential",
        "dispatch",
        "doctor",
        "explain",
        "excuse",
        "gate",
        "hook",
        "implement",
        "lease",
        "local-drift",
        "loop",
        "loops",
        "migrate",
        "parity",
        "pass",
        "plan",
        "queue",
        "repair-prs",
        "replay",
        "select-work",
        "sign",
        "sweep",
        "trace",
        "validate",
        "work-order",
    ]
    .into_iter()
}

pub(crate) fn load(path: &Path) -> Result<PolicyManifest, PolicyLoadError> {
    let path = normalize_manifest_path(path);
    let manifest = load_unverified(&path)?;
    verify(&manifest, &path)?;
    Ok(manifest)
}

fn verify(manifest: &PolicyManifest, path: &Path) -> Result<(), PolicyLoadError> {
    let trusted_keys = ostrom_store::environment::OSTROM_POLICY_TRUSTED_KEYS
        .value_os()
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .ok_or(PolicyLoadError::TrustedKeysUnset)?;
    ostrom_store::verify_policy_manifest(manifest, path, &trusted_keys)?;
    Ok(())
}

fn normalize_manifest_path(path: &Path) -> Cow<'_, Path> {
    if path
        .parent()
        .is_some_and(|parent| parent.as_os_str().is_empty())
    {
        Cow::Owned(Path::new(".").join(path))
    } else {
        Cow::Borrowed(path)
    }
}

fn load_unverified(path: &Path) -> Result<PolicyManifest, PolicyLoadError> {
    let manifest = load_composed(path)?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

fn load_composed(path: &Path) -> Result<PolicyManifest, PolicyLoadError> {
    let source = read(path)?;
    let mut manifest =
        PolicyManifest::parse_yaml(&source).map_err(|source| PolicyLoadError::Yaml {
            path: path.to_path_buf(),
            source,
        })?;
    let includes = std::mem::take(&mut manifest.includes);
    let mut origins = Origins::from_root(&manifest, path);
    let parent = path.parent().unwrap_or_else(|| Path::new("."));

    for pattern in includes {
        let matches = expand_include(parent, &pattern)?;
        if matches.is_empty() {
            return Err(PolicyLoadError::NoIncludeMatch {
                manifest: path.to_path_buf(),
                pattern,
            });
        }
        for include in matches {
            merge_include(&mut manifest, &mut origins, &include)?;
        }
    }
    Ok(manifest)
}

fn validate_manifest(manifest: &PolicyManifest) -> Result<(), PolicyLoadError> {
    manifest
        .validate()
        .map_err(|error| PolicyLoadError::Validation(error.to_string()))?;
    validate_operation_names(manifest)?;
    validate_check_requirements(manifest)
}

fn validate_operation_names(manifest: &PolicyManifest) -> Result<(), PolicyLoadError> {
    for name in manifest.operations.keys() {
        let valid = !name.is_empty()
            && name.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
            });
        if !valid {
            return Err(PolicyLoadError::Validation(format!(
                "operation name `{name}` must contain only lowercase letters, digits, `-`, or `_`"
            )));
        }
        if command_verbs().any(|verb| verb == name) || name == "operations" {
            return Err(PolicyLoadError::Validation(format!(
                "operation name `{name}` conflicts with a built-in ostrom command"
            )));
        }
    }
    Ok(())
}

fn validate_check_requirements(manifest: &PolicyManifest) -> Result<(), PolicyLoadError> {
    let mut requirements = manifest
        .operations
        .iter()
        .flat_map(|(operation, declaration)| {
            declaration.steps.iter().filter_map(move |step| {
                step.requires
                    .as_deref()
                    .map(|check| ("operation", operation.as_str(), check))
            })
        })
        .collect::<Vec<_>>();
    requirements.extend(manifest.grants.iter().filter_map(|(grant, declaration)| {
        declaration
            .requires
            .as_deref()
            .map(|check| ("grant", grant.as_str(), check))
    }));
    requirements.extend(manifest.denies.iter().filter_map(|(deny, declaration)| {
        declaration
            .requires
            .as_deref()
            .map(|check| ("deny", deny.as_str(), check))
    }));
    for (kind, owner, check) in requirements {
        if !manifest.checks.contains_key(check) {
            return Err(PolicyLoadError::UnknownCheck {
                kind,
                owner: owner.to_owned(),
                check: check.to_owned(),
            });
        }
    }
    Ok(())
}

fn read(path: &Path) -> Result<String, PolicyLoadError> {
    fs::read_to_string(path).map_err(|source| PolicyLoadError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn merge_include(
    manifest: &mut PolicyManifest,
    origins: &mut Origins,
    path: &Path,
) -> Result<(), PolicyLoadError> {
    let source = read(path)?;
    let value: Value = serde_yaml::from_str(&source).map_err(|source| PolicyLoadError::Yaml {
        path: path.to_path_buf(),
        source,
    })?;
    let mapping = value
        .as_mapping()
        .ok_or_else(|| PolicyLoadError::LeafShape {
            path: path.to_path_buf(),
            message: "included document must be a map".to_owned(),
        })?;
    let markers = ["actor", "check", "operation", "grant", "deny", "loop"]
        .into_iter()
        .filter(|marker| mapping.contains_key(Value::String((*marker).to_owned())))
        .collect::<Vec<_>>();
    if markers.len() > 1 {
        return Err(PolicyLoadError::LeafShape {
            path: path.to_path_buf(),
            message: format!("included leaf has multiple identity keys: {markers:?}"),
        });
    }
    if let Some(marker) = markers.first() {
        merge_leaf(manifest, origins, path, mapping.clone(), marker)
    } else {
        let fragment: IncludeFragment =
            serde_yaml::from_value(value).map_err(|source| PolicyLoadError::Yaml {
                path: path.to_path_buf(),
                source,
            })?;
        merge_map(
            &mut manifest.actors,
            fragment.actors,
            "actor",
            path,
            &mut origins.actors,
        )?;
        merge_map(
            &mut manifest.checks,
            fragment.checks,
            "check",
            path,
            &mut origins.checks,
        )?;
        merge_map(
            &mut manifest.operations,
            fragment.operations,
            "operation",
            path,
            &mut origins.operations,
        )?;
        merge_map(
            &mut manifest.grants,
            fragment.grants,
            "grant",
            path,
            &mut origins.grants,
        )?;
        merge_map(
            &mut manifest.denies,
            fragment.denies,
            "deny",
            path,
            &mut origins.denies,
        )?;
        merge_map(
            &mut manifest.loops,
            fragment.loops,
            "loop",
            path,
            &mut origins.loops,
        )
    }
}

fn merge_leaf(
    manifest: &mut PolicyManifest,
    origins: &mut Origins,
    path: &Path,
    mut mapping: Mapping,
    marker: &str,
) -> Result<(), PolicyLoadError> {
    let key = Value::String(marker.to_owned());
    let id = mapping
        .remove(&key)
        .and_then(|value| value.as_str().map(str::to_owned))
        .ok_or_else(|| PolicyLoadError::LeafShape {
            path: path.to_path_buf(),
            message: format!("`{marker}` identity must be a string"),
        })?;
    let body = Value::Mapping(mapping);
    match marker {
        "actor" => insert_leaf(
            &mut manifest.actors,
            &mut origins.actors,
            id,
            parse_leaf(body, path)?,
            "actor",
            path,
        ),
        "check" => insert_leaf(
            &mut manifest.checks,
            &mut origins.checks,
            id,
            parse_leaf(body, path)?,
            "check",
            path,
        ),
        "operation" => insert_leaf(
            &mut manifest.operations,
            &mut origins.operations,
            id,
            parse_leaf(body, path)?,
            "operation",
            path,
        ),
        "grant" => insert_leaf(
            &mut manifest.grants,
            &mut origins.grants,
            id,
            parse_leaf(body, path)?,
            "grant",
            path,
        ),
        "deny" => insert_leaf(
            &mut manifest.denies,
            &mut origins.denies,
            id,
            parse_leaf(body, path)?,
            "deny",
            path,
        ),
        "loop" => insert_leaf(
            &mut manifest.loops,
            &mut origins.loops,
            id,
            parse_leaf(body, path)?,
            "loop",
            path,
        ),
        _ => unreachable!("marker came from closed list"),
    }
}

fn parse_leaf<T: for<'de> Deserialize<'de>>(
    body: Value,
    path: &Path,
) -> Result<T, PolicyLoadError> {
    serde_yaml::from_value(body).map_err(|source| PolicyLoadError::Yaml {
        path: path.to_path_buf(),
        source,
    })
}

fn insert_leaf<T>(
    target: &mut BTreeMap<String, T>,
    origins: &mut BTreeMap<String, PathBuf>,
    id: String,
    value: T,
    kind: &'static str,
    path: &Path,
) -> Result<(), PolicyLoadError> {
    if let Some(first) = origins.get(&id) {
        return Err(PolicyLoadError::Collision {
            kind,
            id,
            first: first.clone(),
            second: path.to_path_buf(),
        });
    }
    target.insert(id.clone(), value);
    origins.insert(id, path.to_path_buf());
    Ok(())
}

fn merge_map<T>(
    target: &mut BTreeMap<String, T>,
    incoming: BTreeMap<String, T>,
    kind: &'static str,
    path: &Path,
    origins: &mut BTreeMap<String, PathBuf>,
) -> Result<(), PolicyLoadError> {
    for (id, value) in incoming {
        insert_leaf(target, origins, id, value, kind, path)?;
    }
    Ok(())
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct IncludeFragment {
    #[serde(default)]
    actors: BTreeMap<String, ActorDecl>,
    #[serde(default)]
    checks: BTreeMap<String, CheckDefinition>,
    #[serde(default)]
    operations: BTreeMap<String, OperationDecl>,
    #[serde(default)]
    grants: BTreeMap<String, RuleDecl>,
    #[serde(default)]
    denies: BTreeMap<String, RuleDecl>,
    #[serde(default)]
    loops: BTreeMap<String, LoopDecl>,
}

#[derive(Debug, Default)]
struct Origins {
    actors: BTreeMap<String, PathBuf>,
    checks: BTreeMap<String, PathBuf>,
    operations: BTreeMap<String, PathBuf>,
    grants: BTreeMap<String, PathBuf>,
    denies: BTreeMap<String, PathBuf>,
    loops: BTreeMap<String, PathBuf>,
}

impl Origins {
    fn from_root(manifest: &PolicyManifest, path: &Path) -> Self {
        let origins = |keys: Vec<String>| {
            keys.into_iter()
                .map(|key| (key, path.to_path_buf()))
                .collect()
        };
        Self {
            actors: origins(manifest.actors.keys().cloned().collect()),
            checks: origins(manifest.checks.keys().cloned().collect()),
            operations: origins(manifest.operations.keys().cloned().collect()),
            grants: origins(manifest.grants.keys().cloned().collect()),
            denies: origins(manifest.denies.keys().cloned().collect()),
            loops: origins(manifest.loops.keys().cloned().collect()),
        }
    }
}

fn expand_include(parent: &Path, pattern: &str) -> Result<Vec<PathBuf>, PolicyLoadError> {
    let pattern_path = Path::new(pattern);
    if pattern_path.is_absolute()
        || pattern_path
            .components()
            .any(|component| component == Component::ParentDir)
    {
        return Err(PolicyLoadError::UnsafeInclude(pattern.to_owned()));
    }
    if !pattern.contains(['*', '?']) {
        return Ok(vec![parent.join(pattern)]);
    }
    let wildcard = pattern.find(['*', '?']).expect("glob contains a wildcard");
    let prefix = Path::new(&pattern[..wildcard]);
    let base = parent.join(prefix.parent().unwrap_or_else(|| Path::new(".")));
    if !base.exists() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    collect_files(&base, &mut files)?;
    files.retain(|path| {
        path.strip_prefix(parent)
            .ok()
            .and_then(Path::to_str)
            .is_some_and(|relative| include_glob(relative, pattern))
    });
    files.sort();
    Ok(files)
}

fn collect_files(directory: &Path, files: &mut Vec<PathBuf>) -> Result<(), PolicyLoadError> {
    for entry in fs::read_dir(directory).map_err(|source| PolicyLoadError::Io {
        path: directory.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| PolicyLoadError::Io {
            path: directory.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|source| PolicyLoadError::Io {
            path: path.clone(),
            source,
        })?;
        if file_type.is_dir() && !file_type.is_symlink() {
            if path.file_name().is_none_or(|name| name != ".git") {
                collect_files(&path, files)?;
            }
        } else if file_type.is_file() {
            files.push(path);
        }
    }
    Ok(())
}

fn include_glob(value: &str, pattern: &str) -> bool {
    let value = value.as_bytes();
    let pattern = pattern.as_bytes();
    let mut states = BTreeSet::from([(0_usize, 0_usize)]);
    let mut seen = BTreeSet::new();
    while let Some((value_index, pattern_index)) = states.pop_first() {
        if !seen.insert((value_index, pattern_index)) {
            continue;
        }
        if pattern_index == pattern.len() {
            if value_index == value.len() {
                return true;
            }
            continue;
        }
        if pattern[pattern_index] == b'*' {
            let double = pattern.get(pattern_index + 1) == Some(&b'*');
            let next_pattern = pattern_index + usize::from(double) + 1;
            let skip_pattern = if double && pattern.get(next_pattern) == Some(&b'/') {
                next_pattern + 1
            } else {
                next_pattern
            };
            states.insert((value_index, skip_pattern));
            if value_index < value.len() && (double || value[value_index] != b'/') {
                states.insert((value_index + 1, pattern_index));
            }
        } else if value_index < value.len()
            && (pattern[pattern_index] == value[value_index]
                || (pattern[pattern_index] == b'?' && value[value_index] != b'/'))
        {
            states.insert((value_index + 1, pattern_index + 1));
        }
    }
    false
}

#[derive(Debug, Error)]
pub(crate) enum PolicyLoadError {
    #[error(
        "repository `{}` is ungoverned: no `ostrom.yaml` was found at or below its `.git` boundary",
        .0.display()
    )]
    UngovernedRepository(PathBuf),
    #[error(
        "private overlay `{}` may deny authority but may not grant it; offending rule `grants.{rule}`",
        path.display()
    )]
    OverlayGrant { path: PathBuf, rule: String },
    #[error(
        "private overlay `{}` has manifest_version {version}; expected {expected}",
        path.display()
    )]
    OverlayVersion {
        path: PathBuf,
        version: u32,
        expected: u32,
    },
    #[error("pull request must have the shape owner/repository#N")]
    InvalidTarget,
    #[error("pull request `{0}` was not present in the fixture")]
    PullRequestNotFound(String),
    #[error("could not run gh: {0}")]
    GitHub(io::Error),
    #[error("gh could not read the pull request: {0}")]
    GitHubResponse(String),
    #[error("could not parse pull-request fixture `{}`: {source}", path.display())]
    Fixture {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("could not read `{}`: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not parse `{}`: {source}", path.display())]
    Yaml {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },
    #[error(
        "{kind} `{owner}` requires undefined check `{check}`; define it under `checks:` or add its file to `includes:`"
    )]
    UnknownCheck {
        kind: &'static str,
        owner: String,
        check: String,
    },
    #[error("invalid policy manifest: {0}")]
    Validation(String),
    #[error("invalid selector: {0}")]
    Selector(String),
    #[error("OSTROM_POLICY_TRUSTED_KEYS is required to load a policy manifest")]
    TrustedKeysUnset,
    #[error(transparent)]
    Signature(#[from] ostrom_store::PolicySignatureError),
    #[error("unsafe include path `{0}`")]
    UnsafeInclude(String),
    #[error("include `{pattern}` from `{}` matched no files", manifest.display())]
    NoIncludeMatch { manifest: PathBuf, pattern: String },
    #[error("invalid included leaf `{}`: {message}", path.display())]
    LeafShape { path: PathBuf, message: String },
    #[error(
        "duplicate {kind} id `{id}` in `{}` and `{}`",
        first.display(),
        second.display()
    )]
    Collision {
        kind: &'static str,
        id: String,
        first: PathBuf,
        second: PathBuf,
    },
}

#[cfg(test)]
mod tests {
    use std::{env, fs, process::Command};

    use tempfile::tempdir;

    use super::default_manifest_path;
    use ostrom_store::OstromPaths;

    const LEGACY_NOTICE_CHILD: &str = "OSTROM_TEST_LEGACY_NOTICE_CHILD";

    #[test]
    fn legacy_notice_is_emitted_once_for_repeated_lookup() {
        if env::var_os(LEGACY_NOTICE_CHILD).is_some() {
            let root = tempdir().expect("temporary repository");
            fs::create_dir(root.path().join(".git")).expect("repository boundary");
            fs::create_dir(root.path().join(".ostrom")).expect("legacy directory");
            fs::write(
                root.path().join(".ostrom/manifest.yml"),
                "manifest_version: 1\n",
            )
            .expect("legacy manifest");
            let paths = OstromPaths {
                config: root.path().join("config"),
                state: root.path().join("state"),
            };
            assert!(default_manifest_path(&paths, root.path()).is_some());
            assert!(default_manifest_path(&paths, root.path()).is_some());
            return;
        }

        let output = Command::new(env::current_exe().expect("current test executable"))
            .env(LEGACY_NOTICE_CHILD, "1")
            .args([
                "--exact",
                "policy_manifest::tests::legacy_notice_is_emitted_once_for_repeated_lookup",
                "--nocapture",
            ])
            .output()
            .expect("run repeated lookup child");
        assert!(output.status.success());
        let stderr = String::from_utf8(output.stderr).expect("UTF-8 warning");
        assert_eq!(stderr.matches("deprecated").count(), 1, "{stderr}");
        assert!(stderr.contains("ostrom.yaml"), "{stderr}");
    }
}
