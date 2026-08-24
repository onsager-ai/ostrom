use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use ostrom_core::{GateConfig, PolicyCandidate, PolicyManifest, ResolvedLoopCeilings};
use ostrom_store::{
    GateReplaySnapshot, OstromPaths, PolicyBundle, PublishTarget, RepositorySnapshot, SweepFixture,
    SweepMode, SweepOptions, acquire_gate_replay_snapshot, evaluate_gate_replay,
    gate_config_needs_diff_content, gate_replay_invariant_verdict, load_config, load_gate_config,
    run_sweep_with_mirror,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tempfile::Builder;
use thiserror::Error;

use crate::{policy_manifest, policy_version};

const BUILDER: &str = "builder";
const WORK: &str = "work";
const GATEKEEPER: &str = "gatekeeper";
const MERGE: &str = "merge";

#[derive(Debug, Clone)]
pub(crate) struct CutoverReplayOptions {
    pub scratch_root: PathBuf,
    pub legacy: PathBuf,
    pub manifest: PathBuf,
    pub snapshot: Option<PathBuf>,
    pub snapshot_output: Option<PathBuf>,
    pub executable: PathBuf,
    pub plugin_root: PathBuf,
    pub started_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CutoverSnapshot {
    repositories: Vec<RepositorySnapshot>,
    #[serde(default)]
    gates: BTreeMap<String, GateReplaySnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClassificationAnswer {
    authority: &'static str,
    detail: String,
}

#[derive(Debug, Clone, PartialEq)]
struct LoopAnswer {
    actor: String,
    schedule: Vec<String>,
    ceilings: ResolvedLoopCeilings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Difference {
    item: String,
    legacy: String,
    manifest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GateEvidence {
    name: &'static str,
    repositories: BTreeSet<String>,
    item_count: usize,
    coverage_gaps: Vec<String>,
    differences: Vec<Difference>,
}

impl GateEvidence {
    fn render(&self) -> String {
        let repositories = self.repositories.iter().cloned().collect::<Vec<_>>();
        let mut output = format!(
            "gate {}: repositories={} items={}\n  covered: {}\n",
            self.name,
            repositories.len(),
            self.item_count,
            repositories.join(", ")
        );
        for gap in &self.coverage_gaps {
            output.push_str(&format!("  COVERAGE GAP: {gap}\n"));
        }
        for difference in &self.differences {
            output.push_str(&format!(
                "  DIFF {}: legacy={} manifest={}\n",
                difference.item, difference.legacy, difference.manifest
            ));
        }
        if self.coverage_gaps.is_empty() && self.differences.is_empty() {
            output.push_str("  diff: empty\n");
        } else {
            output.push_str(&format!(
                "  diff: NON-EMPTY (coverage_gaps={} differences={})\n",
                self.coverage_gaps.len(),
                self.differences.len()
            ));
        }
        output
    }

    fn is_empty(&self) -> bool {
        self.coverage_gaps.is_empty() && self.differences.is_empty()
    }
}

#[derive(Debug, Error)]
pub(crate) enum CutoverReplayError {
    #[error("cutover replay requires an explicit scratch OSTROM_HOME")]
    ScratchHomeRequired,
    #[error("cutover replay refuses the live legacy Ostrom home at {0}")]
    LiveHome(String),
    #[error("cutover replay scratch OSTROM_HOME is not a directory: {0}")]
    ScratchHomeMissing(String),
    #[error("cutover replay requires either --snapshot or --snapshot-output")]
    SnapshotModeRequired,
    #[error("cutover replay cannot combine --snapshot with --snapshot-output")]
    SnapshotModeConflict,
    #[error("cutover replay policy load failed for {side}: {detail}")]
    PolicyLoad { side: &'static str, detail: String },
    #[error("cutover replay acquisition failed: {0}")]
    Acquisition(String),
    #[error("cutover replay snapshot `{}` is invalid: {detail}", path.display())]
    Snapshot { path: PathBuf, detail: String },
    #[error("cutover replay loop surface failed: {0}")]
    Loops(String),
    #[error("cutover replay found non-empty evidence\n{0}")]
    NonEmpty(String),
    #[error("cutover replay scratch failure: {0}")]
    Scratch(String),
}

pub(crate) fn scratch_home_from_environment() -> Result<PathBuf, CutoverReplayError> {
    let scratch = ostrom_store::environment::OSTROM_HOME
        .value_os()
        .filter(|value| !value.to_string_lossy().trim().is_empty())
        .map(PathBuf::from)
        .ok_or(CutoverReplayError::ScratchHomeRequired)?;
    if let Some(home) = ostrom_store::environment::HOME.value_os() {
        let live = PathBuf::from(home).join(".claude/ostrom");
        if same_path(&scratch, &live) {
            return Err(CutoverReplayError::LiveHome(scratch.display().to_string()));
        }
    }
    if !scratch.is_dir() {
        return Err(CutoverReplayError::ScratchHomeMissing(
            scratch.display().to_string(),
        ));
    }
    Ok(scratch)
}

pub(crate) fn run(options: &CutoverReplayOptions) -> Result<String, CutoverReplayError> {
    match (&options.snapshot, &options.snapshot_output) {
        (Some(_), Some(_)) => return Err(CutoverReplayError::SnapshotModeConflict),
        (None, None) => return Err(CutoverReplayError::SnapshotModeRequired),
        (Some(_), None) | (None, Some(_)) => {}
    }
    let manifest = load_manifest(&options.manifest)?;
    let bundle = PolicyBundle::repository(manifest.clone());
    let scratch = Builder::new()
        .prefix("cutover-replay-")
        .tempdir_in(&options.scratch_root)
        .map_err(|error| CutoverReplayError::Scratch(error.to_string()))?;
    let legacy_home = scratch.path().join("legacy");
    fs::create_dir(&legacy_home).map_err(|error| CutoverReplayError::Scratch(error.to_string()))?;
    let legacy_config = legacy_config_root(&options.legacy)?;
    for name in ["mandates.yaml", "gate.yaml"] {
        fs::copy(legacy_config.join(name), legacy_home.join(name))
            .map_err(|error| CutoverReplayError::Scratch(error.to_string()))?;
    }
    let paths = OstromPaths {
        config: legacy_home.clone(),
        state: legacy_home.clone(),
    };
    let mandate =
        load_config(&paths, &legacy_home).map_err(|error| CutoverReplayError::PolicyLoad {
            side: "legacy mandates.yaml",
            detail: error.to_string(),
        })?;
    let gate =
        load_gate_config(&paths, &legacy_home).map_err(|error| CutoverReplayError::PolicyLoad {
            side: "legacy gate.yaml",
            detail: error,
        })?;
    let roster = mandate
        .projects
        .iter()
        .map(|project| project.repo.as_str().to_owned())
        .collect::<BTreeSet<_>>();
    if roster.is_empty() {
        return Err(CutoverReplayError::PolicyLoad {
            side: "legacy mandates.yaml",
            detail: "roster is empty".to_owned(),
        });
    }

    let snapshot = if let Some(path) = &options.snapshot {
        read_snapshot(path)?
    } else {
        let snapshot = acquire_snapshot(options, &paths, &legacy_home, &gate)?;
        let path = options
            .snapshot_output
            .as_ref()
            .expect("snapshot output was selected");
        write_snapshot(path, &snapshot)?;
        snapshot
    };
    let fixture_path = scratch.path().join("github.json");
    let fixture = SweepFixture {
        repositories: snapshot.repositories.clone(),
    };
    fs::write(
        &fixture_path,
        serde_json::to_vec_pretty(&fixture).map_err(|error| CutoverReplayError::Snapshot {
            path: fixture_path.clone(),
            detail: error.to_string(),
        })?,
    )
    .map_err(|error| CutoverReplayError::Snapshot {
        path: fixture_path.clone(),
        detail: error.to_string(),
    })?;
    run_sweep_with_mirror(&SweepOptions {
        paths: paths.clone(),
        working_directory: legacy_home.clone(),
        executable: options.executable.clone(),
        plugin_root: options.plugin_root.clone(),
        started_at: options.started_at,
        requested_mode: SweepMode::Full,
        fixture: Some(fixture_path),
        publish: PublishTarget::Disabled,
        policy: None,
    })
    .map_err(|error| CutoverReplayError::Acquisition(error.to_string()))?;

    let state = read_json(&paths.sweep_state_file())?;
    let classification = classification_evidence(&roster, &state, &bundle, &manifest);
    let gate_verdict = gate_evidence(&roster, &gate, &snapshot, &bundle, &manifest)?;
    let loops = loop_evidence(&roster, &options.legacy, &manifest)?;
    let evidence = [classification, gate_verdict, loops];
    let output = evidence
        .iter()
        .map(GateEvidence::render)
        .collect::<String>();
    if evidence.iter().all(GateEvidence::is_empty) {
        Ok(output)
    } else {
        Err(CutoverReplayError::NonEmpty(output))
    }
}

fn acquire_snapshot(
    options: &CutoverReplayOptions,
    paths: &OstromPaths,
    legacy_home: &Path,
    gate: &GateConfig,
) -> Result<CutoverSnapshot, CutoverReplayError> {
    let (outcome, repositories) = run_sweep_with_mirror(&SweepOptions {
        paths: paths.clone(),
        working_directory: legacy_home.to_path_buf(),
        executable: options.executable.clone(),
        plugin_root: options.plugin_root.clone(),
        started_at: options.started_at,
        requested_mode: SweepMode::Full,
        fixture: None,
        publish: PublishTarget::Disabled,
        policy: None,
    })
    .map_err(|error| CutoverReplayError::Acquisition(error.to_string()))?;
    if !outcome.faults.is_empty() {
        return Err(CutoverReplayError::Acquisition(format!(
            "sweep returned acquisition gaps: {}",
            outcome.faults.join("; ")
        )));
    }
    let needs_diff_content = gate_config_needs_diff_content(gate);
    let mut gates = BTreeMap::new();
    for repository in &repositories {
        for pull_request in &repository.open_prs {
            let number = pull_request
                .get("number")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    CutoverReplayError::Acquisition(format!(
                        "{} open pull request has no number",
                        repository.repo
                    ))
                })?;
            let target = format!("{}#{number}", repository.repo);
            let acquisition =
                acquire_gate_replay_snapshot(&target, pull_request.clone(), needs_diff_content)
                    .map_err(|error| CutoverReplayError::Acquisition(error.to_string()))?;
            let gaps = acquisition.gaps(needs_diff_content);
            if !gaps.is_empty() {
                return Err(CutoverReplayError::Acquisition(format!(
                    "{target}: {}",
                    gaps.join("; ")
                )));
            }
            gates.insert(target, acquisition);
        }
    }
    Ok(CutoverSnapshot {
        repositories,
        gates,
    })
}

fn classification_evidence(
    roster: &BTreeSet<String>,
    state: &Value,
    bundle: &PolicyBundle,
    manifest: &PolicyManifest,
) -> GateEvidence {
    let mut gaps = policy_coverage_gaps(roster, manifest, BUILDER, WORK, "manifest");
    let mut differences = Vec::new();
    let mut item_count = 0;
    for repository in roster {
        let Some(repo_state) = state.pointer(&format!("/repos/{}", escape_pointer(repository)))
        else {
            gaps.push(format!(
                "legacy classification omitted repository {repository}"
            ));
            continue;
        };
        let Some(items) = repo_state.get("items").and_then(Value::as_object) else {
            gaps.push(format!(
                "legacy classification has no item map for repository {repository}"
            ));
            continue;
        };
        let Some(records) = repo_state.get("records").and_then(Value::as_object) else {
            gaps.push(format!(
                "legacy classification has no frozen records for repository {repository}"
            ));
            continue;
        };
        for (id, item) in items {
            item_count += 1;
            let Some(legacy_detail) = item.get("classification").and_then(Value::as_str) else {
                gaps.push(format!("legacy classification is missing for {id}"));
                continue;
            };
            let legacy_authority = match legacy_detail {
                "delegated" => "delegated",
                "reserved" | "tripwire" | "excluded" | "unclassified" => "principal",
                unknown => {
                    gaps.push(format!(
                        "legacy classification `{unknown}` is unknown for {id}"
                    ));
                    continue;
                }
            };
            let legacy = ClassificationAnswer {
                authority: legacy_authority,
                detail: legacy_detail.to_owned(),
            };
            let Some(record) = records.get(id) else {
                gaps.push(format!("legacy classification record is missing for {id}"));
                continue;
            };
            let decision = bundle.decide(BUILDER, WORK, &candidate(repository, record));
            let manifest_answer = ClassificationAnswer {
                authority: if decision.granted {
                    "delegated"
                } else {
                    "principal"
                },
                detail: if decision.granted {
                    format!("grant {}", decision.matching_grants.join(","))
                } else if !decision.matching_denies.is_empty() {
                    format!("deny {}", decision.matching_denies.join(","))
                } else {
                    "default deny".to_owned()
                },
            };
            if legacy.authority != manifest_answer.authority {
                differences.push(Difference {
                    item: id.clone(),
                    legacy: format!("{} ({})", legacy.authority, legacy.detail),
                    manifest: format!("{} ({})", manifest_answer.authority, manifest_answer.detail),
                });
            }
        }
    }
    GateEvidence {
        name: "1 classification",
        repositories: roster.clone(),
        item_count,
        coverage_gaps: gaps,
        differences,
    }
}

fn gate_evidence(
    roster: &BTreeSet<String>,
    legacy: &GateConfig,
    snapshot: &CutoverSnapshot,
    bundle: &PolicyBundle,
    manifest: &PolicyManifest,
) -> Result<GateEvidence, CutoverReplayError> {
    let mut gaps = policy_coverage_gaps(roster, manifest, GATEKEEPER, MERGE, "manifest");
    let legacy_repositories = legacy
        .projects
        .iter()
        .map(|project| project.repo.as_str().to_owned())
        .collect::<BTreeSet<_>>();
    for repository in roster.difference(&legacy_repositories) {
        gaps.push(format!(
            "legacy gate.yaml omitted roster repository {repository}"
        ));
    }
    let snapshot_repositories = snapshot
        .repositories
        .iter()
        .map(|repository| repository.repo.as_str().to_owned())
        .collect::<BTreeSet<_>>();
    for repository in roster.difference(&snapshot_repositories) {
        gaps.push(format!(
            "GitHub snapshot omitted roster repository {repository}"
        ));
    }
    let needs_diff_content = gate_config_needs_diff_content(legacy);
    let mut differences = Vec::new();
    let mut item_count = 0;
    for repository in &snapshot.repositories {
        if !roster.contains(repository.repo.as_str()) {
            continue;
        }
        for pull_request in &repository.open_prs {
            let Some(number) = pull_request.get("number").and_then(Value::as_u64) else {
                gaps.push(format!(
                    "{} snapshot contains an open pull request with no number",
                    repository.repo
                ));
                continue;
            };
            item_count += 1;
            let target = format!("{}#{number}", repository.repo);
            let Some(acquisition) = snapshot.gates.get(&target) else {
                gaps.push(format!("gate acquisition is missing for {target}"));
                continue;
            };
            if !acquisition.matches_sweep_row(pull_request) {
                gaps.push(format!(
                    "{target}: gate acquisition differs from the frozen sweep row"
                ));
            }
            for gap in acquisition.gaps(needs_diff_content) {
                gaps.push(format!("{target}: {gap}"));
            }
            let legacy_verdict = evaluate_gate_replay(legacy, &target, acquisition)
                .map_err(|error| CutoverReplayError::Acquisition(error.to_string()))?;
            let manifest_verdict =
                manifest_gate_verdict(bundle, repository.repo.as_str(), acquisition);
            if legacy_verdict != manifest_verdict {
                differences.push(Difference {
                    item: target,
                    legacy: legacy_verdict.to_owned(),
                    manifest: manifest_verdict.to_owned(),
                });
            }
        }
    }
    Ok(GateEvidence {
        name: "2 gate verdict",
        repositories: roster.clone(),
        item_count,
        coverage_gaps: gaps,
        differences,
    })
}

fn manifest_gate_verdict(
    bundle: &PolicyBundle,
    repository: &str,
    acquisition: &GateReplaySnapshot,
) -> &'static str {
    let explanation =
        bundle.explain_pull_request(repository, acquisition.metadata(), GATEKEEPER, MERGE);
    let policy = if explanation.granted {
        "pass"
    } else {
        let requirements = explanation
            .rules
            .iter()
            .filter(|rule| rule.kind == "grant" && rule.matched)
            .filter_map(|rule| rule.requirement.as_ref())
            .collect::<Vec<_>>();
        if requirements
            .iter()
            .any(|requirement| requirement.status == "INCONCLUSIVE")
        {
            "inconclusive"
        } else {
            "fail"
        }
    };
    combine_verdicts(gate_replay_invariant_verdict(acquisition), policy)
}

fn combine_verdicts(left: &'static str, right: &'static str) -> &'static str {
    if left == "fail" || right == "fail" {
        "fail"
    } else if left == "inconclusive" || right == "inconclusive" {
        "inconclusive"
    } else {
        "pass"
    }
}

fn loop_evidence(
    roster: &BTreeSet<String>,
    legacy_root: &Path,
    manifest: &PolicyManifest,
) -> Result<GateEvidence, CutoverReplayError> {
    let legacy = legacy_loops(legacy_root)?;
    let mut resolved = BTreeMap::new();
    let mut gaps = roster_policy_coverage_gaps(roster, manifest, "manifest");
    for name in manifest.loops.keys() {
        let value = manifest
            .resolve_loop(name)
            .map_err(|error| CutoverReplayError::Loops(error.to_string()))?;
        resolved.insert(
            name.clone(),
            LoopAnswer {
                actor: value.actor,
                schedule: value.every.on_calendars(),
                ceilings: value.ceilings,
            },
        );
    }
    let names = legacy
        .keys()
        .chain(resolved.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut differences = Vec::new();
    for name in &names {
        match (legacy.get(name), resolved.get(name)) {
            (Some(left), Some(right)) => {
                compare_loop_field(&mut differences, name, "actor", &left.actor, &right.actor);
                compare_loop_field(
                    &mut differences,
                    name,
                    "schedule",
                    &left.schedule.join(","),
                    &right.schedule.join(","),
                );
                compare_loop_field(
                    &mut differences,
                    name,
                    "ceiling.concurrent",
                    &render_option(left.ceilings.concurrent),
                    &render_option(right.ceilings.concurrent),
                );
                compare_loop_field(
                    &mut differences,
                    name,
                    "ceiling.spend_usd",
                    &render_float(left.ceilings.spend_usd),
                    &render_float(right.ceilings.spend_usd),
                );
                compare_loop_field(
                    &mut differences,
                    name,
                    "ceiling.tokens",
                    &render_option(left.ceilings.tokens),
                    &render_option(right.ceilings.tokens),
                );
            }
            (Some(_), None) => gaps.push(format!("manifest omitted legacy loop {name}")),
            (None, Some(_)) => gaps.push(format!("legacy enabled-timers omitted loop {name}")),
            (None, None) => unreachable!("name came from one loop map"),
        }
    }
    Ok(GateEvidence {
        name: "3 loop equivalence",
        repositories: roster.clone(),
        item_count: names.len(),
        coverage_gaps: gaps,
        differences,
    })
}

fn compare_loop_field(
    differences: &mut Vec<Difference>,
    loop_name: &str,
    field: &str,
    legacy: &str,
    manifest: &str,
) {
    if legacy != manifest {
        differences.push(Difference {
            item: format!("{loop_name}.{field}"),
            legacy: legacy.to_owned(),
            manifest: manifest.to_owned(),
        });
    }
}

fn legacy_loops(root: &Path) -> Result<BTreeMap<String, LoopAnswer>, CutoverReplayError> {
    let enabled_path = root.join("systemd/enabled-timers");
    let enabled = fs::read_to_string(&enabled_path).map_err(|error| {
        CutoverReplayError::Loops(format!(
            "could not read {}: {error}",
            enabled_path.display()
        ))
    })?;
    let units = root.join("systemd");
    let mut loops = BTreeMap::new();
    for timer_name in enabled
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        let timer_name = timer_name
            .strip_prefix("systemd/")
            .unwrap_or(timer_name)
            .to_owned();
        if !matches!(
            timer_name.as_str(),
            "mandate-sweep.timer" | "ostrom-builder-pass.timer" | "ostrom-gatekeeper-pass.timer"
        ) && !timer_name.starts_with("ostrom-loop-")
        {
            continue;
        }
        let timer_path = units.join(&timer_name);
        let service_name = timer_name
            .strip_suffix(".timer")
            .map(|stem| format!("{stem}.service"))
            .ok_or_else(|| {
                CutoverReplayError::Loops(format!(
                    "enabled timer `{timer_name}` does not end in .timer"
                ))
            })?;
        let service_path = units.join(&service_name);
        let timer = fs::read_to_string(&timer_path).map_err(|error| {
            CutoverReplayError::Loops(format!("could not read {}: {error}", timer_path.display()))
        })?;
        let service = fs::read_to_string(&service_path).map_err(|error| {
            CutoverReplayError::Loops(format!(
                "could not read {}: {error}",
                service_path.display()
            ))
        })?;
        let stem = timer_name.trim_end_matches(".timer");
        let base_name = stem
            .strip_prefix("ostrom-loop-")
            .or_else(|| stem.strip_prefix("ostrom-"))
            .or_else(|| stem.strip_prefix("mandate-"))
            .unwrap_or(stem)
            .to_owned();
        let mut schedule = values(&timer, "OnCalendar=");
        schedule.sort();
        if schedule.is_empty() {
            return Err(CutoverReplayError::Loops(format!(
                "{} has no OnCalendar schedule",
                timer_path.display()
            )));
        }
        let actor = environment_value(&service, "OSTROM_ACTOR")
            .or_else(|| actor_from_exec_start(&service))
            .or_else(|| legacy_actor(stem))
            .or_else(|| role_actor(root, &base_name))
            .ok_or_else(|| {
                CutoverReplayError::Loops(format!(
                    "{} and its role settings do not name an actor",
                    service_path.display()
                ))
            })?;
        let ceilings = ResolvedLoopCeilings {
            concurrent: parse_environment(&service, "MANDATE_MAX_IMPLEMENTERS")?,
            spend_usd: parse_environment(&service, "MANDATE_DAILY_CAP_USD")?,
            tokens: parse_environment(&service, "MANDATE_ORDER_TOKEN_CEILING")?,
        };
        let entries = legacy_loop_entries(stem, &base_name, &schedule)?;
        for (name, schedule) in entries {
            if loops
                .insert(
                    name.clone(),
                    LoopAnswer {
                        actor: actor.clone(),
                        schedule,
                        ceilings,
                    },
                )
                .is_some()
            {
                return Err(CutoverReplayError::Loops(format!(
                    "enabled-timers contains duplicate loop {name}"
                )));
            }
        }
    }
    if loops.is_empty() {
        return Err(CutoverReplayError::Loops(
            "enabled-timers contains no loops".to_owned(),
        ));
    }
    Ok(loops)
}

fn legacy_loop_entries(
    stem: &str,
    base_name: &str,
    schedule: &[String],
) -> Result<Vec<(String, Vec<String>)>, CutoverReplayError> {
    if stem == "ostrom-builder-pass" && schedule.len() == 2 {
        return schedule
            .iter()
            .map(|calendar| {
                let name = match calendar.as_str() {
                    value if value.contains("08..21") => "builder-day",
                    value if value.contains("23,02,05") => "builder-night",
                    unknown => {
                        return Err(CutoverReplayError::Loops(format!(
                            "ostrom-builder-pass.timer has an unknown schedule `{unknown}`"
                        )));
                    }
                };
                Ok((name.to_owned(), vec![calendar.clone()]))
            })
            .collect();
    }
    let name = match stem {
        "ostrom-gatekeeper-pass" => "gatekeeper",
        "mandate-sweep" => "sweep",
        _ => base_name,
    };
    Ok(vec![(name.to_owned(), schedule.to_vec())])
}

fn values(source: &str, prefix: &str) -> Vec<String> {
    source
        .lines()
        .map(str::trim)
        .filter_map(|line| line.strip_prefix(prefix).map(str::to_owned))
        .collect()
}

fn environment_value(source: &str, name: &str) -> Option<String> {
    values(source, "Environment=")
        .into_iter()
        .filter_map(|assignment| {
            let assignment = assignment.trim_matches('"');
            assignment
                .split_once('=')
                .filter(|(candidate, _)| *candidate == name)
                .map(|(_, value)| value.to_owned())
        })
        .next()
}

fn actor_from_exec_start(source: &str) -> Option<String> {
    let command = values(source, "ExecStart=").into_iter().next()?;
    [BUILDER, GATEKEEPER]
        .into_iter()
        .find_map(|actor| (command.contains(&format!("pass {actor}"))).then(|| actor.to_owned()))
}

fn legacy_actor(stem: &str) -> Option<String> {
    match stem {
        "ostrom-builder-pass" => Some(BUILDER.to_owned()),
        "ostrom-gatekeeper-pass" => Some(GATEKEEPER.to_owned()),
        // The legacy sweep has no agent settings file, but its acquisition
        // credential is explicitly minted for the gatekeeper role. That is
        // the authority identity being preserved, not a default for an absent
        // value.
        "mandate-sweep" => Some(GATEKEEPER.to_owned()),
        _ => None,
    }
}

fn parse_environment<T>(source: &str, name: &str) -> Result<Option<T>, CutoverReplayError>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    environment_value(source, name)
        .map(|value| {
            value.parse::<T>().map_err(|error| {
                CutoverReplayError::Loops(format!(
                    "environment {name} has invalid value `{value}`: {error}"
                ))
            })
        })
        .transpose()
}

fn role_actor(root: &Path, loop_name: &str) -> Option<String> {
    let role = loop_name.split('-').next()?;
    let relative = PathBuf::from("roles").join(format!("{role}.settings.json"));
    let source = [
        root.join(&relative),
        root.join("claude/ostrom").join(relative),
    ]
    .into_iter()
    .find_map(|path| fs::read(path).ok())?;
    let value = serde_json::from_slice::<Value>(&source).ok()?;
    value
        .pointer("/env/OSTROM_ACTOR")?
        .as_str()
        .map(str::to_owned)
}

fn policy_coverage_gaps(
    roster: &BTreeSet<String>,
    manifest: &PolicyManifest,
    actor: &str,
    operation: &str,
    side: &str,
) -> Vec<String> {
    roster
        .iter()
        .filter(|repository| {
            !manifest
                .grants
                .values()
                .chain(manifest.denies.values())
                .any(|rule| {
                    (rule.repositories.is_empty()
                        || rule.repositories.iter().any(|value| value == *repository))
                        && (rule.actors.is_empty()
                            || rule.actors.iter().any(|value| value == actor))
                        && (rule.operations.is_empty()
                            || rule.operations.iter().any(|value| value == operation))
                })
        })
        .map(|repository| {
            format!(
                "{side} has no {actor}/{operation} policy entry for roster repository {repository}"
            )
        })
        .collect()
}

fn roster_policy_coverage_gaps(
    roster: &BTreeSet<String>,
    manifest: &PolicyManifest,
    side: &str,
) -> Vec<String> {
    roster
        .iter()
        .filter(|repository| {
            !manifest
                .grants
                .values()
                .chain(manifest.denies.values())
                .any(|rule| {
                    rule.repositories.is_empty()
                        || rule.repositories.iter().any(|value| value == *repository)
                })
        })
        .map(|repository| format!("{side} has no policy entry for roster repository {repository}"))
        .collect()
}

fn load_manifest(path: &Path) -> Result<PolicyManifest, CutoverReplayError> {
    let result = if path.is_dir() {
        policy_version::load_explicit_version(path).map_err(|error| error.to_string())
    } else {
        policy_manifest::load(path).map_err(|error| error.to_string())
    };
    result.map_err(|detail| CutoverReplayError::PolicyLoad {
        side: "manifest",
        detail,
    })
}

fn candidate(repository: &str, record: &Value) -> PolicyCandidate {
    PolicyCandidate {
        repository: repository.to_owned(),
        labels: strings(record.get("labels")),
        paths: strings(record.get("files")),
        commit_type: record
            .get("title")
            .and_then(Value::as_str)
            .and_then(commit_type),
        actor: Some(BUILDER.to_owned()),
        verb: Some(WORK.to_owned()),
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

fn commit_type(title: &str) -> Option<String> {
    let prefix = title.split_once(':')?.0;
    let kind = prefix.split_once('(').map_or(prefix, |(kind, _)| kind);
    (!kind.is_empty()).then(|| kind.to_owned())
}

fn read_snapshot(path: &Path) -> Result<CutoverSnapshot, CutoverReplayError> {
    let source = fs::read(path).map_err(|error| CutoverReplayError::Snapshot {
        path: path.to_path_buf(),
        detail: error.to_string(),
    })?;
    serde_json::from_slice(&source).map_err(|error| CutoverReplayError::Snapshot {
        path: path.to_path_buf(),
        detail: error.to_string(),
    })
}

fn write_snapshot(path: &Path, snapshot: &CutoverSnapshot) -> Result<(), CutoverReplayError> {
    let source =
        serde_json::to_vec_pretty(snapshot).map_err(|error| CutoverReplayError::Snapshot {
            path: path.to_path_buf(),
            detail: error.to_string(),
        })?;
    fs::write(path, source).map_err(|error| CutoverReplayError::Snapshot {
        path: path.to_path_buf(),
        detail: error.to_string(),
    })
}

fn read_json(path: &Path) -> Result<Value, CutoverReplayError> {
    let source = fs::read(path).map_err(|error| CutoverReplayError::Snapshot {
        path: path.to_path_buf(),
        detail: error.to_string(),
    })?;
    serde_json::from_slice(&source).map_err(|error| CutoverReplayError::Snapshot {
        path: path.to_path_buf(),
        detail: error.to_string(),
    })
}

fn legacy_config_root(root: &Path) -> Result<PathBuf, CutoverReplayError> {
    [root.to_path_buf(), root.join("claude/ostrom")]
        .into_iter()
        .find(|candidate| {
            candidate.join("mandates.yaml").is_file() && candidate.join("gate.yaml").is_file()
        })
        .ok_or_else(|| CutoverReplayError::PolicyLoad {
            side: "legacy",
            detail: format!(
                "no mandates.yaml and gate.yaml pair found at {} or {}",
                root.display(),
                root.join("claude/ostrom").display()
            ),
        })
}

fn escape_pointer(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

fn render_option<T: ToString>(value: Option<T>) -> String {
    value.map_or_else(|| "unbounded".to_owned(), |value| value.to_string())
}

fn render_float(value: Option<f64>) -> String {
    value.map_or_else(
        || "unbounded".to_owned(),
        |value| {
            if value.fract() == 0.0 {
                format!("{value:.0}")
            } else {
                value.to_string()
            }
        },
    )
}

fn same_path(left: &Path, right: &Path) -> bool {
    let absolute = |path: &Path| {
        fs::canonicalize(path).unwrap_or_else(|_| {
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("/"))
                    .join(path)
            }
        })
    };
    absolute(left) == absolute(right)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn policy(extra: &str) -> PolicyManifest {
        PolicyManifest::from_yaml(&format!(
            "manifest_version: 1\nactors: {{builder: {{}}, gatekeeper: {{}}}}\noperations: {{work: {{steps: []}}, merge: {{steps: []}}}}\n{extra}"
        ))
        .expect("test policy")
    }

    #[test]
    fn seeded_classification_disagreement_fails_and_names_both_answers() {
        let manifest = policy(
            "grants:\n  release: {actors: builder, operations: work, repositories: placeholder-org/alpha, where: type:ops}\n",
        );
        let state = serde_json::json!({
            "repos": {"placeholder-org/alpha": {
                "items": {"placeholder-org/alpha#397": {"classification": "tripwire"}},
                "records": {"placeholder-org/alpha#397": {
                    "title": "ops(release): publish placeholder",
                    "labels": [],
                    "files": []
                }}
            }}
        });
        let roster = BTreeSet::from(["placeholder-org/alpha".to_owned()]);
        let gate = classification_evidence(
            &roster,
            &state,
            &PolicyBundle::repository(manifest.clone()),
            &manifest,
        );
        assert!(!gate.is_empty());
        let output = gate.render();
        assert!(output.contains("placeholder-org/alpha#397"), "{output}");
        assert!(output.contains("principal (tripwire)"), "{output}");
        assert!(output.contains("delegated (grant release)"), "{output}");
    }

    #[test]
    fn seeded_gate_verdict_disagreement_fails_and_names_both_verdicts() {
        let legacy = GateConfig::from_yaml(
            "provider: file\nprojects:\n  - repo: placeholder-org/alpha\n    required_checks: [verify-linux]\n",
        )
        .expect("legacy gate");
        let manifest = policy(
            "checks:\n  renamed:\n    uses: gh/check-run\n    with: {name: verify-renamed}\ngrants:\n  merge: {actors: gatekeeper, operations: merge, repositories: placeholder-org/alpha, requires: renamed}\n",
        );
        let acquisition: GateReplaySnapshot = serde_json::from_value(serde_json::json!({
            "metadata_ready": true,
            "metadata": {
                "number": 12,
                "title": "fix: placeholder",
                "author": {"login": "placeholder-author"},
                "headRefOid": "aaaaaaaa",
                "labels": [],
                "closingIssuesReferences": [],
                "mergeable": "MERGEABLE",
                "isDraft": false,
                "files": [{"path": "src/lib.rs"}],
                "checks": [{"name": "verify-linux", "conclusion": "SUCCESS"}]
            },
            "metadata_error": "",
            "head_sha": "aaaaaaaa",
            "checks_ready": true,
            "checks": [{"name": "verify-linux", "conclusion": "SUCCESS"}],
            "checks_error": "",
            "checks_partial_error": "",
            "diff_ready": true,
            "paths": ["src/lib.rs"],
            "diff_error": "",
            "diff_content_ready": false,
            "diff_content": "",
            "diff_content_error": "diff content was not requested",
            "threads_ready": true,
            "threads": [],
            "threads_error": "",
            "thread_author": "placeholder-author"
        }))
        .expect("gate acquisition");
        let snapshot = CutoverSnapshot {
            repositories: vec![RepositorySnapshot {
                repo: ostrom_core::RepositoryName::new("placeholder-org/alpha")
                    .expect("repository"),
                issues: Vec::new(),
                issue_etag: None,
                issue_not_modified: false,
                open_prs: vec![acquisition.metadata().clone()],
                merged_prs: Vec::new(),
                default_branch: None,
                branches: Vec::new(),
                branch_read_degraded: false,
                ci_runs: Vec::new(),
                warnings: Vec::new(),
            }],
            gates: BTreeMap::from([("placeholder-org/alpha#12".to_owned(), acquisition)]),
        };
        let roster = BTreeSet::from(["placeholder-org/alpha".to_owned()]);
        let gate = gate_evidence(
            &roster,
            &legacy,
            &snapshot,
            &PolicyBundle::repository(manifest.clone()),
            &manifest,
        )
        .expect("compare gate");
        assert!(!gate.is_empty());
        let output = gate.render();
        assert!(output.contains("placeholder-org/alpha#12"), "{output}");
        assert!(output.contains("legacy=pass manifest=fail"), "{output}");
    }

    #[test]
    fn legacy_builder_timer_splits_day_and_night_and_ignores_unmanaged_timers() {
        let root = tempdir().expect("legacy loop root");
        fs::create_dir(root.path().join("systemd")).expect("systemd directory");
        fs::write(
            root.path().join("systemd/enabled-timers"),
            "unrelated.timer\nostrom-builder-pass.timer\n",
        )
        .expect("enabled timers");
        fs::write(
            root.path().join("systemd/ostrom-builder-pass.timer"),
            "[Timer]\nOnCalendar=*-*-* 08..21:15:00\nOnCalendar=*-*-* 23,02,05:15:00\n",
        )
        .expect("builder timer");
        fs::write(
            root.path().join("systemd/ostrom-builder-pass.service"),
            "[Service]\nExecStart=/usr/bin/ostrom pass builder\n",
        )
        .expect("builder service");
        let manifest = policy(
            "grants:\n  work: {actors: builder, operations: work, repositories: placeholder-org/alpha}\nloops:\n  builder-day: {actor: builder, operation: work, target: placeholder-org/alpha, every: '08:15..21:15'}\n  builder-night: {actor: builder, operation: work, target: placeholder-org/alpha, every: ['23:15', '02:15', '05:15']}\n",
        );
        let roster = BTreeSet::from(["placeholder-org/alpha".to_owned()]);
        let gate = loop_evidence(&roster, root.path(), &manifest).expect("compare loops");
        assert!(gate.is_empty(), "{}", gate.render());
        assert_eq!(gate.item_count, 2);
    }

    fn loop_fixture(manifest_loop: &str) -> (tempfile::TempDir, GateEvidence) {
        let root = tempdir().expect("legacy loop root");
        fs::create_dir(root.path().join("systemd")).expect("systemd directory");
        fs::write(
            root.path().join("systemd/enabled-timers"),
            "ostrom-loop-builder.timer\n",
        )
        .expect("enabled timers");
        fs::write(
            root.path().join("systemd/ostrom-loop-builder.timer"),
            "[Timer]\nOnCalendar=hourly\n",
        )
        .expect("timer");
        fs::write(
            root.path().join("systemd/ostrom-loop-builder.service"),
            "[Service]\nEnvironment=OSTROM_ACTOR=builder\nEnvironment=MANDATE_MAX_IMPLEMENTERS=6\nEnvironment=MANDATE_DAILY_CAP_USD=50\nEnvironment=MANDATE_ORDER_TOKEN_CEILING=200000\n",
        )
        .expect("service");
        let manifest = policy(&format!(
            "grants:\n  work: {{actors: builder, operations: work, repositories: placeholder-org/alpha}}\nloops:\n  builder:\n{manifest_loop}"
        ));
        let roster = BTreeSet::from(["placeholder-org/alpha".to_owned()]);
        let gate = loop_evidence(&roster, root.path(), &manifest).expect("compare loops");
        (root, gate)
    }

    #[test]
    fn seeded_loop_ceiling_disagreement_is_detected() {
        let (_root, gate) = loop_fixture(
            "    actor: builder\n    operation: work\n    target: placeholder-org/alpha\n    every: hourly\n    concurrent: 5\n    spend_usd: 50\n    tokens: 200000\n",
        );
        let output = gate.render();
        assert!(!gate.is_empty());
        assert!(output.contains("builder.ceiling.concurrent"), "{output}");
    }

    #[test]
    fn seeded_loop_actor_disagreement_is_detected() {
        let (_root, gate) = loop_fixture(
            "    actor: gatekeeper\n    operation: work\n    target: placeholder-org/alpha\n    every: hourly\n    concurrent: 6\n    spend_usd: 50\n    tokens: 200000\n",
        );
        let output = gate.render();
        assert!(!gate.is_empty());
        assert!(output.contains("builder.actor"), "{output}");
    }

    #[test]
    fn seeded_loop_schedule_disagreement_is_detected() {
        let (_root, gate) = loop_fixture(
            "    actor: builder\n    operation: work\n    target: placeholder-org/alpha\n    every: '*:45'\n    concurrent: 6\n    spend_usd: 50\n    tokens: 200000\n",
        );
        let output = gate.render();
        assert!(!gate.is_empty());
        assert!(output.contains("builder.schedule"), "{output}");
    }

    #[test]
    fn dropped_repository_is_a_failure_not_a_ten_repository_comparison() {
        let manifest = policy(
            "grants:\n  alpha: {actors: builder, operations: work, repositories: placeholder-org/alpha}\n",
        );
        let state = serde_json::json!({"repos": {"placeholder-org/alpha": {
            "items": {}, "records": {}
        }}});
        let roster = BTreeSet::from([
            "placeholder-org/alpha".to_owned(),
            "placeholder-org/missing".to_owned(),
        ]);
        let gate = classification_evidence(
            &roster,
            &state,
            &PolicyBundle::repository(manifest.clone()),
            &manifest,
        );
        assert!(!gate.is_empty());
        let output = gate.render();
        assert!(output.contains("COVERAGE GAP"), "{output}");
        assert!(output.contains("placeholder-org/missing"), "{output}");
        assert!(!output.contains("diff: empty"), "{output}");
    }
}
