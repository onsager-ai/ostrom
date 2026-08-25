use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    sync::Once,
};

use chrono::{DateTime, Utc};
use ostrom_core::{
    ActorDecl, CheckDefinition, LoopDecl, OperationDecl, PolicyManifest, PromptValue,
    RepositoryName, RuleDecl, SelectorFinding, SelectorUniverse,
};
use ostrom_store::{
    ActorPortabilityFinding, OstromPaths, PolicyBundle, PolicyExplanation, PolicyOrigins,
    SweepFixture, read_commit_checks,
};
use serde::Deserialize;
use serde_json::Value as JsonValue;
use serde_yaml::{Mapping, Value};
use thiserror::Error;

pub(crate) fn run_validate(
    paths: &OstromPaths,
    path: &Path,
    normalized: bool,
) -> Result<(), PolicyLoadError> {
    let normalized_path = normalize_manifest_path(path);
    let loaded = load_composed(&normalized_path)?;
    validate_manifest(&loaded.manifest)?;
    verify(&loaded.manifest, &normalized_path)?;
    let operator_path = operator_manifest_path(paths)?;
    let bundle = if operator_path
        .as_deref()
        .is_some_and(|operator| same_file(operator, &normalized_path))
    {
        PolicyBundle::operator(loaded.manifest.clone(), loaded.origins)
    } else {
        PolicyBundle::repository_with_origins(loaded.manifest.clone(), loaded.origins)
    };
    report_actor_portability_findings(bundle.actor_portability_findings());
    // Resolve the ladder even when normalized output was not requested. This
    // catches a present environment value with the wrong declared type while
    // never placing its raw value in diagnostics.
    loaded
        .manifest
        .resolve_inputs(ostrom_store::environment::declared_input)
        .map_err(|error| PolicyLoadError::Validation(error.to_string()))?;

    let verbs = command_verbs()
        .map(str::to_owned)
        .chain(loaded.manifest.operations.keys().cloned());
    let universe = SelectorUniverse::from_manifest(&loaded.manifest, verbs);
    let findings = loaded.manifest.selector_findings(&universe);
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
    validate_adjacent_legacy_policy(path)?;

    if normalized {
        print!(
            "{}",
            loaded
                .manifest
                .to_yaml()
                .map_err(|error| PolicyLoadError::Validation(error.to_string()))?
        );
    } else {
        println!("valid: {}", path.display());
    }
    Ok(())
}

fn same_file(left: &Path, right: &Path) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn validate_adjacent_legacy_policy(path: &Path) -> Result<(), PolicyLoadError> {
    let path = normalize_manifest_path(path);
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    let legacy_policy_present = [
        directory.join("mandates.yaml"),
        directory.join("gate.yaml"),
        directory.join(".ostrom/mandates.yaml"),
        directory.join(".ostrom/gate.yaml"),
    ]
    .iter()
    .any(|candidate| candidate.is_file());
    if !legacy_policy_present {
        return Ok(());
    }
    let paths = OstromPaths {
        config: directory.to_path_buf(),
        state: directory.to_path_buf(),
    };
    ostrom_store::validate_roster_coverage(&paths, directory)
        .map_err(|error| PolicyLoadError::Validation(error.to_string()))
}

pub(crate) fn run_sign(
    path: &Path,
    key_id: &str,
    private_key: &Path,
) -> Result<(), PolicyLoadError> {
    let path = normalize_manifest_path(path);
    let manifest = load_composed(&path)?.manifest;
    let signature = ostrom_store::sign_policy_manifest(&manifest, &path, key_id, private_key)?;
    println!("signed: {}", signature.display());
    Ok(())
}

pub(crate) fn run_generate(
    paths: &OstromPaths,
    repository: &str,
    output: Option<&Path>,
) -> Result<(), PolicyLoadError> {
    let repository = RepositoryName::new(repository)
        .map_err(|_| PolicyLoadError::InvalidRepository(repository.to_owned()))?;
    let operator_path = adopting_manifest_path(paths)?;
    let operator = load(&operator_path)?;
    let generated = project_repository_manifest(operator, repository.as_str());
    let yaml = generated
        .to_yaml()
        .map_err(|error| PolicyLoadError::Validation(error.to_string()))?;
    if let Some(path) = output.filter(|path| *path != Path::new("-")) {
        fs::write(path, yaml).map_err(|source| PolicyLoadError::Write {
            path: path.to_path_buf(),
            source,
        })?;
    } else {
        print!("{yaml}");
    }
    Ok(())
}

fn project_repository_manifest(mut manifest: PolicyManifest, repository: &str) -> PolicyManifest {
    manifest.includes.clear();
    manifest.actors.clear();
    manifest.prompts.clear();
    project_rules(&mut manifest.grants, repository);
    project_rules(&mut manifest.denies, repository);
    manifest
        .loops
        .retain(|_, declaration| declaration.target == repository);
    manifest
}

fn project_rules(rules: &mut BTreeMap<String, RuleDecl>, repository: &str) {
    rules.retain(|_, declaration| {
        declaration.repositories.is_empty()
            || declaration
                .repositories
                .iter()
                .any(|candidate| candidate == repository)
    });
    for declaration in rules.values_mut() {
        declaration.repositories = Default::default();
    }
}

pub(crate) fn load_bundle(
    paths: &OstromPaths,
    path: &Path,
) -> Result<PolicyBundle, PolicyLoadError> {
    let repository_path = normalize_manifest_path(path).into_owned();
    let repository = load_composed(&repository_path)?;
    verify(&repository.manifest, &repository_path)?;
    build_bundle(paths, repository_path, repository)
}

/// Compose repository policy with the adopting operator scope, validating and
/// verifying every signed input before returning the effective manifest.
pub(crate) fn compose_manifest(
    paths: &OstromPaths,
    path: &Path,
) -> Result<PolicyManifest, PolicyLoadError> {
    let bundle = load_bundle(paths, path)?;
    validate_manifest(&bundle.manifest)?;
    Ok(bundle.manifest)
}

fn build_bundle(
    paths: &OstromPaths,
    repository_path: PathBuf,
    repository: LoadedManifest,
) -> Result<PolicyBundle, PolicyLoadError> {
    let operator_path = operator_manifest_path(paths)?;
    let operator = operator_path
        .filter(|operator_path| operator_path != &repository_path)
        .map(|operator_path| {
            let loaded = load_composed(&operator_path)?;
            verify(&loaded.manifest, &operator_path)?;
            Ok::<_, PolicyLoadError>((operator_path, loaded))
        })
        .transpose()?;
    if let Some((operator_path, operator)) = &operator
        && operator.manifest.manifest_version != repository.manifest.manifest_version
    {
        return Err(PolicyLoadError::OperatorVersion {
            path: operator_path.clone(),
            version: operator.manifest.manifest_version,
            expected: repository.manifest.manifest_version,
        });
    }
    validate_scoped_manifest(
        &repository.manifest,
        operator.as_ref().map(|(_, loaded)| &loaded.manifest),
    )?;
    if let Some((_, operator)) = &operator {
        validate_scoped_manifest(&operator.manifest, Some(&repository.manifest))?;
    }
    let manifest = compose_scopes(
        repository.manifest.clone(),
        operator.as_ref().map(|(_, loaded)| &loaded.manifest),
    );
    Ok(PolicyBundle::scoped(
        manifest,
        repository.manifest,
        repository.origins,
        operator.map(|(_, loaded)| (loaded.manifest, loaded.origins)),
    ))
}

pub(crate) fn load_bundle_at_base(
    paths: &OstromPaths,
    path: &Path,
    working_directory: &Path,
    base_sha: &str,
) -> Result<PolicyBundle, PolicyLoadError> {
    let Some(root) = repository_root(working_directory) else {
        return load_bundle(paths, path);
    };
    let path = normalize_manifest_path(path).into_owned();
    let Ok(relative) = path.strip_prefix(&root) else {
        return load_bundle(paths, &path);
    };
    let snapshot = tempfile::tempdir().map_err(PolicyLoadError::Snapshot)?;
    let archive = Command::new("git")
        .current_dir(&root)
        .args(["archive", "--format=tar", base_sha])
        .output()
        .map_err(PolicyLoadError::GitSnapshot)?;
    if !archive.status.success() {
        return Err(PolicyLoadError::GitSnapshotResponse(
            String::from_utf8_lossy(&archive.stderr)
                .trim()
                .chars()
                .take(500)
                .collect(),
        ));
    }
    let mut tar = Command::new("tar")
        .args(["-xf", "-", "-C"])
        .arg(snapshot.path())
        .stdin(Stdio::piped())
        .spawn()
        .map_err(PolicyLoadError::GitSnapshot)?;
    use std::io::Write as _;
    tar.stdin
        .take()
        .ok_or_else(|| {
            PolicyLoadError::GitSnapshotResponse("tar stdin was unavailable".to_owned())
        })?
        .write_all(&archive.stdout)
        .map_err(PolicyLoadError::GitSnapshot)?;
    let status = tar.wait().map_err(PolicyLoadError::GitSnapshot)?;
    if !status.success() {
        return Err(PolicyLoadError::GitSnapshotResponse(format!(
            "tar exited with {status}"
        )));
    }
    let snapshot_path = snapshot.path().join(relative);
    let snapshot_directory = snapshot_path.parent().unwrap_or_else(|| snapshot.path());
    let Some(snapshot_manifest) = named_manifest_path(snapshot_directory)? else {
        let manifest = PolicyManifest::parse_yaml("manifest_version: 1\n").map_err(|source| {
            PolicyLoadError::Yaml {
                path: path.clone(),
                source,
            }
        })?;
        return build_bundle(
            paths,
            path.clone(),
            LoadedManifest {
                origins: PolicyOrigins::from_root(&manifest, &path),
                manifest,
            },
        );
    };
    let repository_path = snapshot_manifest
        .strip_prefix(snapshot.path())
        .map_or_else(|_| path.clone(), |relative| root.join(relative));
    let mut repository = load_composed(&snapshot_manifest)?;
    verify(&repository.manifest, &snapshot_manifest)?;
    repository.origins.rebase(snapshot.path(), &root);
    build_bundle(paths, repository_path, repository)
}

pub(crate) fn default_manifest_path(
    paths: &OstromPaths,
    cwd: &Path,
) -> Result<Option<PathBuf>, PolicyLoadError> {
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

    for directory in &directories {
        if let Some(path) = named_manifest_path(directory)? {
            return Ok(Some(path));
        }
    }
    if let Some(path) = directories
        .iter()
        .map(|directory| directory.join(".ostrom/manifest.yml"))
        .find(|path| path.is_file())
    {
        warn_legacy_manifest(&path);
        return Ok(Some(path));
    }

    if repository.is_none() {
        let path = paths.config.join("manifest.yml");
        if path.is_file() {
            warn_legacy_manifest(&path);
            return Ok(Some(path));
        }
    }
    Ok(None)
}

fn named_manifest_path(directory: &Path) -> Result<Option<PathBuf>, PolicyLoadError> {
    let yaml = directory.join("ostrom.yaml");
    let yml = directory.join("ostrom.yml");
    match (yaml.is_file(), yml.is_file()) {
        (true, true) => Err(PolicyLoadError::AmbiguousManifest { yaml, yml }),
        (true, false) => Ok(Some(yaml)),
        (false, true) => Ok(Some(yml)),
        (false, false) => Ok(None),
    }
}

pub(crate) fn operator_manifest_path(
    paths: &OstromPaths,
) -> Result<Option<PathBuf>, PolicyLoadError> {
    if let Some(path) = ostrom_store::environment::OSTROM_POLICY_MANIFEST
        .value_os()
        .filter(|path| !path.is_empty())
    {
        return Ok(Some(PathBuf::from(path)));
    }
    if let Some(path) = named_manifest_path(&paths.config)? {
        return Ok(Some(path));
    }
    for legacy in ["policy.yaml", "config.yaml"] {
        let path = paths.config.join(legacy);
        if path.is_file() {
            warn_legacy_operator_manifest(&path);
            return Ok(Some(path));
        }
    }
    Ok(None)
}

pub(crate) fn adopting_manifest_path(paths: &OstromPaths) -> Result<PathBuf, PolicyLoadError> {
    operator_manifest_path(paths)?.ok_or_else(|| PolicyLoadError::AdoptingManifestNotFound {
        yaml: paths.config.join("ostrom.yaml"),
        yml: paths.config.join("ostrom.yml"),
    })
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
    let Some(path) = default_manifest_path(paths, cwd)? else {
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

fn warn_legacy_operator_manifest(path: &Path) {
    static NOTICE: Once = Once::new();
    NOTICE.call_once(|| {
        eprintln!(
            "warning: legacy operator policy manifest `{}` is deprecated; move it to `ostrom.yaml`",
            path.display()
        );
    });
}

fn compose_scopes(
    mut repository: PolicyManifest,
    operator: Option<&PolicyManifest>,
) -> PolicyManifest {
    let Some(operator) = operator else {
        repository.operations.clear();
        repository.prompts.clear();
        repository.loops.clear();
        return repository;
    };
    let shipped = ostrom_core::ManifestDefaults::default();
    if repository.defaults.stalls_after == shipped.stalls_after {
        repository.defaults.stalls_after = operator.defaults.stalls_after.clone();
    }
    if repository.defaults.check == shipped.check {
        repository.defaults.check = operator.defaults.check;
    }
    if repository.defaults.grant == shipped.grant {
        repository.defaults.grant = operator.defaults.grant;
    }
    if repository.defaults.deny == shipped.deny {
        repository.defaults.deny = operator.defaults.deny;
    }
    if repository.defaults.r#loop.concurrent.is_none() {
        repository.defaults.r#loop.concurrent = operator.defaults.r#loop.concurrent;
    }
    if repository.defaults.r#loop.spend_usd.is_none() {
        repository.defaults.r#loop.spend_usd = operator.defaults.r#loop.spend_usd;
    }
    if repository.defaults.r#loop.tokens.is_none() {
        repository.defaults.r#loop.tokens = operator.defaults.r#loop.tokens;
    }

    merge_fallback(&mut repository.inputs, operator.inputs.clone());
    merge_fallback(&mut repository.actors, operator.actors.clone());
    merge_fallback(&mut repository.checks, operator.checks.clone());
    merge_fallback(&mut repository.grants, operator.grants.clone());
    merge_fallback(&mut repository.denies, operator.denies.clone());
    repository.operations.clone_from(&operator.operations);
    repository.prompts.clone_from(&operator.prompts);
    repository.loops.clone_from(&operator.loops);
    repository
}

fn validate_scoped_manifest(
    manifest: &PolicyManifest,
    fallback: Option<&PolicyManifest>,
) -> Result<(), PolicyLoadError> {
    let mut validation = manifest.clone();
    if let Some(fallback) = fallback {
        merge_fallback(&mut validation.inputs, fallback.inputs.clone());
        merge_fallback(&mut validation.actors, fallback.actors.clone());
        merge_fallback(&mut validation.checks, fallback.checks.clone());
        merge_fallback(&mut validation.prompts, fallback.prompts.clone());
        merge_fallback(&mut validation.operations, fallback.operations.clone());
        merge_fallback(&mut validation.grants, fallback.grants.clone());
        merge_fallback(&mut validation.denies, fallback.denies.clone());
    }
    validate_manifest(&validation)
}

fn report_actor_portability_findings(findings: &[ActorPortabilityFinding]) {
    for finding in findings {
        eprintln!(
            "lint: repository actor `{}` declared in `{}` is non-portable across operator rosters",
            finding.actor,
            finding.source.display()
        );
    }
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
    let discovered = options.manifest.is_none();
    let manifest_path = if let Some(manifest) = options.manifest {
        manifest.to_path_buf()
    } else {
        default_manifest_path(options.paths, options.working_directory)?.ok_or_else(|| {
            PolicyLoadError::UngovernedRepository(repository_name(options.working_directory))
        })?
    };
    let target = ExplainTarget::parse(options.target)?;
    let pull_request = if let Some(fixture) = options.fixture {
        fixture_pull_request(fixture, &target)?
    } else {
        acquire_pull_request(&target, options.working_directory)?
    };
    let bundle = if discovered {
        pull_request
            .get("baseRefOid")
            .and_then(JsonValue::as_str)
            .filter(|sha| !sha.is_empty())
            .map_or_else(
                || load_bundle(options.paths, &manifest_path),
                |base_sha| {
                    load_bundle_at_base(
                        options.paths,
                        &manifest_path,
                        options.working_directory,
                        base_sha,
                    )
                },
            )?
    } else {
        load_bundle(options.paths, &manifest_path)?
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
            "number,title,labels,files,state,baseRefOid,headRefOid",
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
    let mut pull_request: JsonValue =
        serde_json::from_slice(&output.stdout).map_err(|source| PolicyLoadError::Fixture {
            path: PathBuf::from("gh pr view"),
            source,
        })?;
    let head_sha = pull_request
        .get("headRefOid")
        .and_then(JsonValue::as_str)
        .filter(|sha| !sha.is_empty())
        .ok_or_else(|| {
            PolicyLoadError::GitHubResponse("pull request response had no head SHA".to_owned())
        })?;
    let checks = read_commit_checks(&target.repository, head_sha, |endpoint| {
        let output = Command::new("gh")
            .current_dir(working_directory)
            .args(["api", endpoint])
            .output()
            .map_err(|error| error.to_string())?;
        if output.status.success() {
            Ok(output.stdout)
        } else {
            let error = String::from_utf8_lossy(&output.stderr)
                .trim()
                .chars()
                .take(500)
                .collect::<String>();
            Err(if error.is_empty() {
                "gh api failed without error detail".to_owned()
            } else {
                error
            })
        }
    })
    .map_err(|error| PolicyLoadError::GitHubResponse(error.to_string()))?;
    if let Some(error) = checks.statuses_error {
        eprintln!("ostrom explain: partial check read: {error}");
    }
    pull_request
        .as_object_mut()
        .ok_or_else(|| {
            PolicyLoadError::GitHubResponse("pull request response was not an object".to_owned())
        })?
        .insert("checks".to_owned(), JsonValue::Array(checks.checks));
    Ok(pull_request)
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
    let mut output = format!("{}\n\nSCOPES CONSULTED\n", target.full);
    for scope in &explanation.consulted_scopes {
        output.push_str(&format!(
            "  {:10} {}\n",
            scope.layer.name(),
            scope.path.display()
        ));
    }
    output.push_str("\nSUBJECT RULES\n");
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
            "  {:10} {:5} {:24} {:38} {}{}  source: {}\n",
            rule.layer.name(),
            rule.kind,
            rule.id,
            predicate,
            match_word(rule.subject_matched),
            if rule.subject_matched {
                String::new()
            } else {
                format!("  unmatched: {}", unmatched_name(rule.unmatched))
            },
            rule.source.display()
        ));
    }
    output.push_str(&format!(
        "\nACTOR RULES ({} / {})\n",
        explanation.actor, explanation.operation
    ));
    for rule in &explanation.rules {
        output.push_str(&format!(
            "  {:10} {:5} {:24} {:38} {}  source: {}\n",
            rule.layer.name(),
            rule.kind,
            rule.id,
            format!(
                "actor={} operation={}",
                explanation.actor, explanation.operation
            ),
            match_word(rule.actor_matched),
            rule.source.display()
        ));
    }
    if !explanation.inert_declarations.is_empty() {
        output.push_str("\nEXECUTABLE DECLARATIONS\n");
        for declaration in &explanation.inert_declarations {
            output.push_str(&format!(
                "  {:10} {:9} {:24} DECLARED BUT NOT ADOPTED  source: {}\n",
                declaration.layer.name(),
                declaration.kind,
                declaration.id,
                declaration.source.display()
            ));
        }
    }
    if !explanation.actor_portability_findings.is_empty() {
        output.push_str("\nACTOR PORTABILITY\n");
        for finding in &explanation.actor_portability_findings {
            output.push_str(&format!(
                "  {:10} actor     {:24} NON-PORTABLE  source: {}\n",
                finding.layer.name(),
                finding.actor,
                finding.source.display()
            ));
        }
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
        "compose",
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
        "rollback",
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
    let loaded = load_composed(path)?;
    validate_manifest(&loaded.manifest)?;
    Ok(loaded.manifest)
}

#[derive(Debug)]
struct LoadedManifest {
    manifest: PolicyManifest,
    origins: PolicyOrigins,
}

fn load_composed(path: &Path) -> Result<LoadedManifest, PolicyLoadError> {
    let source = read(path)?;
    let mut manifest =
        PolicyManifest::parse_yaml(&source).map_err(|source| PolicyLoadError::Yaml {
            path: path.to_path_buf(),
            source,
        })?;
    let includes = std::mem::take(&mut manifest.includes);
    let mut origins = PolicyOrigins::from_root(&manifest, path);
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
    materialize_prompt_files(&mut manifest, &origins)?;
    Ok(LoadedManifest { manifest, origins })
}

fn materialize_prompt_files(
    manifest: &mut PolicyManifest,
    origins: &PolicyOrigins,
) -> Result<(), PolicyLoadError> {
    let declared = manifest
        .prompts
        .iter()
        .filter_map(|(name, prompt)| prompt.file().map(|path| (name.clone(), path.to_owned())))
        .collect::<Vec<_>>();
    for (name, reference) in declared {
        let origin = origins.prompts.get(&name).unwrap_or(&origins.root).clone();
        let text = read_prompt_file(&origin, &reference)?;
        manifest.prompts.insert(name, PromptValue::Inline(text));
    }

    for (operation, declaration) in &mut manifest.operations {
        let origin = origins
            .operations
            .get(operation)
            .unwrap_or(&origins.root)
            .clone();
        for step in &mut declaration.steps {
            if !step.uses.starts_with("agent/") {
                continue;
            }
            let Some(value) = step.parameters.get("prompt").cloned() else {
                continue;
            };
            let prompt: PromptValue =
                serde_yaml::from_value(value).map_err(|source| PolicyLoadError::Yaml {
                    path: origin.clone(),
                    source,
                })?;
            let Some(reference) = prompt.file() else {
                continue;
            };
            let text = read_prompt_file(&origin, reference)?;
            step.parameters.insert(
                "prompt".to_owned(),
                serde_yaml::to_value(PromptValue::Inline(text)).expect("prompt serializes"),
            );
        }
    }
    Ok(())
}

fn read_prompt_file(origin: &Path, reference: &str) -> Result<String, PolicyLoadError> {
    let path = Path::new(reference);
    if path.is_absolute()
        || path
            .components()
            .any(|component| component == Component::ParentDir)
    {
        return Err(PolicyLoadError::UnsafePrompt(reference.to_owned()));
    }
    let parent = origin.parent().unwrap_or_else(|| Path::new("."));
    read(&parent.join(path))
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
            declaration.steps.iter().flat_map(move |step| {
                step.requires
                    .iter()
                    .map(|check| ("operation", operation.as_str(), check.as_str()))
            })
        })
        .collect::<Vec<_>>();
    requirements.extend(manifest.grants.iter().flat_map(|(grant, declaration)| {
        declaration
            .requires
            .iter()
            .map(|check| ("grant", grant.as_str(), check.as_str()))
    }));
    requirements.extend(manifest.denies.iter().flat_map(|(deny, declaration)| {
        declaration
            .requires
            .iter()
            .map(|check| ("deny", deny.as_str(), check.as_str()))
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
    origins: &mut PolicyOrigins,
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
    let markers = [
        "actor",
        "check",
        "prompt",
        "operation",
        "grant",
        "deny",
        "loop",
    ]
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
            &mut manifest.prompts,
            fragment.prompts,
            "prompt",
            path,
            &mut origins.prompts,
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
    origins: &mut PolicyOrigins,
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
        "prompt" => insert_leaf(
            &mut manifest.prompts,
            &mut origins.prompts,
            id,
            parse_leaf(body, path)?,
            "prompt",
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
    prompts: BTreeMap<String, PromptValue>,
    #[serde(default)]
    operations: BTreeMap<String, OperationDecl>,
    #[serde(default)]
    grants: BTreeMap<String, RuleDecl>,
    #[serde(default)]
    denies: BTreeMap<String, RuleDecl>,
    #[serde(default)]
    loops: BTreeMap<String, LoopDecl>,
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
        "repository `{}` is ungoverned: no `ostrom.yaml` or `ostrom.yml` was found at or below its `.git` boundary",
        .0.display()
    )]
    UngovernedRepository(PathBuf),
    #[error(
        "both policy manifest paths exist for the same document: `{}` and `{}`",
        yaml.display(),
        yml.display()
    )]
    AmbiguousManifest { yaml: PathBuf, yml: PathBuf },
    #[error(
        "no adopting operator manifest found at `{}` or `{}`",
        yaml.display(),
        yml.display()
    )]
    AdoptingManifestNotFound { yaml: PathBuf, yml: PathBuf },
    #[error(
        "operator manifest `{}` has manifest_version {version}; expected {expected}",
        path.display()
    )]
    OperatorVersion {
        path: PathBuf,
        version: u32,
        expected: u32,
    },
    #[error("pull request must have the shape owner/repository#N")]
    InvalidTarget,
    #[error("repository must have the shape owner/name: {0}")]
    InvalidRepository(String),
    #[error("pull request `{0}` was not present in the fixture")]
    PullRequestNotFound(String),
    #[error("could not run gh: {0}")]
    GitHub(io::Error),
    #[error("gh could not read the pull request: {0}")]
    GitHubResponse(String),
    #[error("could not create a policy snapshot: {0}")]
    Snapshot(io::Error),
    #[error("could not run the policy snapshot command: {0}")]
    GitSnapshot(io::Error),
    #[error("could not read policy from the pull request base: {0}")]
    GitSnapshotResponse(String),
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
    #[error("could not write generated policy manifest `{}`: {source}", path.display())]
    Write {
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
    #[error("unsafe prompt file path `{0}`")]
    UnsafePrompt(String),
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
    use std::{env, fs, path::Path, process::Command};

    use ostrom_core::{PolicyCandidate, PolicyManifest};
    use tempfile::tempdir;

    use super::{
        PolicyLoadError, compose_scopes, default_manifest_path, load_composed,
        project_repository_manifest, validate_scoped_manifest,
    };

    #[test]
    fn missing_and_unsafe_prompt_files_are_refused() {
        let root = tempdir().expect("prompt fixture");
        let manifest = root.path().join("ostrom.yaml");
        for (reference, expected) in [
            ("prompts/missing.md", "could not read"),
            ("../outside.md", "unsafe prompt file path"),
        ] {
            fs::write(
                &manifest,
                format!(
                    "manifest_version: 1\noperations:\n  inspect:\n    steps:\n      - uses: agent/claude\n        with: {{prompt: {{from: {reference}}}}}\n"
                ),
            )
            .expect("write manifest");
            let error = load_composed(&manifest).expect_err("prompt reference must fail");
            assert!(error.to_string().contains(expected), "{error}");
            assert!(matches!(
                error,
                PolicyLoadError::Io { .. } | PolicyLoadError::UnsafePrompt(_)
            ));
        }
    }
    use ostrom_store::{OstromPaths, PolicyBundle, PolicyOrigins};

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
            assert!(
                default_manifest_path(&paths, root.path())
                    .expect("manifest discovery")
                    .is_some()
            );
            assert!(
                default_manifest_path(&paths, root.path())
                    .expect("manifest discovery")
                    .is_some()
            );
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

    #[test]
    fn generated_repository_policy_round_trips_is_portable_and_preserves_verdicts() {
        let operator = PolicyManifest::from_yaml(
            r#"
manifest_version: 1
actors: {builder: {}}
checks:
  placeholder-green:
    uses: gh/check-run
    with: {name: placeholder-ci}
operations: {work: {steps: []}}
grants:
  target-grant:
    actors: builder
    operations: work
    repositories: placeholder-org/target
    where: label:delegated
  global-grant:
    actors: builder
    operations: work
    where: type:docs
  other-grant:
    actors: builder
    operations: work
    repositories: placeholder-org/other
denies:
  target-deny:
    actors: builder
    operations: work
    repositories: placeholder-org/target
    where: path:protected/**
loops:
  target-loop: {actor: builder, operation: work, target: placeholder-org/target, every: hourly}
  other-loop: {actor: builder, operation: work, target: placeholder-org/other, every: hourly}
"#,
        )
        .expect("operator policy");
        let generated = project_repository_manifest(operator.clone(), "placeholder-org/target");
        let yaml = generated.to_yaml().expect("generated YAML");
        let round_tripped = PolicyManifest::parse_yaml(&yaml).expect("generated manifest loads");

        assert!(round_tripped.actors.is_empty());
        assert!(!yaml.lines().any(|line| line == "actors:"), "{yaml}");
        assert!(round_tripped.grants.contains_key("target-grant"));
        assert!(round_tripped.grants.contains_key("global-grant"));
        assert!(!round_tripped.grants.contains_key("other-grant"));
        assert!(round_tripped.denies.contains_key("target-deny"));
        assert!(
            round_tripped
                .grants
                .values()
                .chain(round_tripped.denies.values())
                .all(|rule| rule.repositories.is_empty())
        );
        assert!(round_tripped.checks.contains_key("placeholder-green"));
        assert!(round_tripped.operations.contains_key("work"));
        assert!(round_tripped.loops.contains_key("target-loop"));
        assert!(!round_tripped.loops.contains_key("other-loop"));
        validate_scoped_manifest(&round_tripped, Some(&operator))
            .expect("generated repository layer loads with operator policy");

        let repository_origins =
            PolicyOrigins::from_root(&round_tripped, Path::new("generated/ostrom.yaml"));
        let generated_only = PolicyBundle::repository_with_origins(
            round_tripped.clone(),
            repository_origins.clone(),
        );
        assert!(generated_only.actor_portability_findings().is_empty());

        let operator_origins =
            PolicyOrigins::from_root(&operator, Path::new("operator/ostrom.yaml"));
        let operator_bundle = PolicyBundle::operator(operator.clone(), operator_origins.clone());
        let resolved = compose_scopes(round_tripped.clone(), Some(&operator));
        let layered_bundle = PolicyBundle::scoped(
            resolved,
            round_tripped,
            repository_origins,
            Some((operator, operator_origins)),
        );
        let cases = [
            PolicyCandidate {
                repository: "placeholder-org/target".to_owned(),
                labels: vec!["delegated".to_owned()],
                ..PolicyCandidate::default()
            },
            PolicyCandidate {
                repository: "placeholder-org/target".to_owned(),
                paths: vec!["protected/authority.txt".to_owned()],
                labels: vec!["delegated".to_owned()],
                ..PolicyCandidate::default()
            },
        ];
        assert!(operator_bundle.decide("builder", "work", &cases[0]).granted);
        assert!(!operator_bundle.decide("builder", "work", &cases[1]).granted);
        for candidate in &cases {
            assert_eq!(
                generated_only.decide("builder", "work", candidate).granted,
                operator_bundle.decide("builder", "work", candidate).granted
            );
            assert_eq!(
                layered_bundle.decide("builder", "work", candidate).granted,
                operator_bundle.decide("builder", "work", candidate).granted
            );
        }
    }
}
