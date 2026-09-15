use std::collections::{BTreeMap, BTreeSet};

use ostrom_core::{
    DefaultDisposition, MandateConfig, PolicyCandidate, PolicyManifest, ProjectMandate,
    RepositoryName, ResolvedLoop,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::environment;

pub const REPOSITORY_NOT_AVAILABLE: &str = "repository-not-available";
pub const REPOSITORY_NOT_GRANTED: &str = "repository-not-granted";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkippedRepository {
    pub repository: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EffectiveRepositories {
    pub repositories: Vec<String>,
    pub skipped: Vec<SkippedRepository>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AvailableRepositoriesError {
    #[error("repository entry `{0}` must have the shape owner/name")]
    Invalid(String),
}

pub fn parse_repository_list(
    supplied: &str,
) -> Result<BTreeSet<String>, AvailableRepositoriesError> {
    supplied
        .split(',')
        .map(str::trim)
        .filter(|repository| !repository.is_empty())
        .map(validated_name)
        .collect()
}

pub fn inherited_repository_scope() -> Result<Option<BTreeSet<String>>, AvailableRepositoriesError>
{
    environment::OSTROM_EFFECTIVE_REPOSITORIES
        .value()
        .as_deref()
        .map(parse_repository_list)
        .transpose()
}

/// Resolve the repositories supplied by the operator environment, or derive
/// the compatibility roster from policy rules and mandate projects.
pub fn available_repositories(
    manifest: Option<&PolicyManifest>,
    mandates: &MandateConfig,
) -> Result<BTreeSet<String>, AvailableRepositoriesError> {
    resolve_available_repositories(
        environment::OSTROM_AVAILABLE_REPOSITORIES
            .value()
            .as_deref(),
        manifest,
        mandates,
    )
}

pub fn resolve_available_repositories(
    supplied: Option<&str>,
    manifest: Option<&PolicyManifest>,
    mandates: &MandateConfig,
) -> Result<BTreeSet<String>, AvailableRepositoriesError> {
    if let Some(supplied) = supplied {
        return parse_repository_list(supplied);
    }

    let mut repositories = mandates
        .projects
        .iter()
        .map(|project| project.repo.as_str().to_owned())
        .collect::<BTreeSet<_>>();
    if let Some(manifest) = manifest {
        for repository in manifest
            .grants
            .values()
            .chain(manifest.denies.values())
            .flat_map(|rule| rule.repositories.iter())
        {
            repositories.insert(validated_name(repository)?.to_owned());
        }
    }
    Ok(repositories)
}

fn validated_name(repository: &str) -> Result<String, AvailableRepositoriesError> {
    RepositoryName::new(repository.to_owned())
        .map(|name| name.as_str().to_owned())
        .map_err(|_| AvailableRepositoriesError::Invalid(repository.to_owned()))
}

/// Make the sweep roster exactly the available set. A repository not named in
/// mandates receives the conservative default project: active, unclassified,
/// with no selectors, reserved references, or custom concurrency ceiling.
#[must_use]
pub fn project_available_mandates(
    mandates: &MandateConfig,
    available: &BTreeSet<String>,
) -> MandateConfig {
    let authored = mandates
        .projects
        .iter()
        .map(|project| (project.repo.as_str(), project))
        .collect::<BTreeMap<_, _>>();
    let mut projected = mandates.clone();
    projected.projects = available
        .iter()
        .map(|repository| {
            authored.get(repository.as_str()).map_or_else(
                || ProjectMandate {
                    repo: RepositoryName::new(repository.clone())
                        .expect("available repositories were validated"),
                    paused: false,
                    default: DefaultDisposition::Unclassified,
                    delegated: Vec::new(),
                    excluded: Vec::new(),
                    reserved: Vec::new(),
                    bounce: Vec::new(),
                    max_implementers_per_repository: None,
                },
                |project| (**project).clone(),
            )
        })
        .collect();
    projected
}

/// Intersect a loop declaration with availability, then apply its actor and
/// operation grant independently to each remaining repository.
#[must_use]
pub fn effective_repositories(
    resolved: &ResolvedLoop,
    available: &BTreeSet<String>,
    manifest: &PolicyManifest,
) -> EffectiveRepositories {
    let requested = if resolved.repositories.is_empty() {
        available.iter().cloned().collect::<Vec<_>>()
    } else {
        resolved.repositories.iter().cloned().collect::<Vec<_>>()
    };
    let mut effective = EffectiveRepositories::default();
    for repository in requested {
        if !available.contains(&repository) {
            effective.skipped.push(SkippedRepository {
                repository,
                reason: REPOSITORY_NOT_AVAILABLE.to_owned(),
            });
            continue;
        }
        let candidate = PolicyCandidate {
            repository: repository.clone(),
            actor: Some(resolved.actor.clone()),
            verb: Some(resolved.operation.clone()),
            ..PolicyCandidate::default()
        };
        if manifest
            .decide(&resolved.actor, &resolved.operation, &candidate)
            .granted
        {
            effective.repositories.push(repository);
        } else {
            effective.skipped.push(SkippedRepository {
                repository,
                reason: REPOSITORY_NOT_GRANTED.to_owned(),
            });
        }
    }
    effective.repositories.sort();
    effective.repositories.dedup();
    effective.skipped.sort_by(|left, right| {
        (&left.repository, &left.reason).cmp(&(&right.repository, &right.reason))
    });
    effective.skipped.dedup();
    effective
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mandates() -> MandateConfig {
        MandateConfig::from_yaml(
            "cadence_hours: 1\nstuck_after_days: 7\nprojects:\n  - repo: placeholder-org/mandate\n",
        )
        .unwrap()
    }

    #[test]
    fn absent_operator_input_unions_rules_and_mandates() {
        let manifest = PolicyManifest::from_yaml(
            "manifest_version: 1\nactors: {builder: {}}\noperations: {work: {steps: []}}\ngrants: {work: {actors: builder, operations: work, repositories: placeholder-org/grant}}\ndenies: {hold: {actors: builder, operations: work, repositories: placeholder-org/deny}}\n",
        )
        .unwrap();
        assert_eq!(
            resolve_available_repositories(None, Some(&manifest), &mandates()).unwrap(),
            BTreeSet::from([
                "placeholder-org/deny".to_owned(),
                "placeholder-org/grant".to_owned(),
                "placeholder-org/mandate".to_owned(),
            ])
        );
    }

    #[test]
    fn loop_scope_intersects_availability_and_skips_ungranted_repositories() {
        let manifest = PolicyManifest::from_yaml(
            "manifest_version: 1\nactors: {builder: {}}\noperations: {work: {steps: []}}\ngrants: {work: {actors: builder, operations: work, repositories: placeholder-org/granted}}\nloops: {day: {actor: builder, operation: work, repositories: [placeholder-org/granted, placeholder-org/ungranted, placeholder-org/unavailable], every: hourly}}\n",
        )
        .unwrap();
        let resolved = manifest.resolve_loop("day").unwrap();
        let result = effective_repositories(
            &resolved,
            &BTreeSet::from([
                "placeholder-org/granted".to_owned(),
                "placeholder-org/ungranted".to_owned(),
            ]),
            &manifest,
        );
        assert_eq!(result.repositories, ["placeholder-org/granted"]);
        assert_eq!(
            result.skipped,
            [
                SkippedRepository {
                    repository: "placeholder-org/unavailable".to_owned(),
                    reason: REPOSITORY_NOT_AVAILABLE.to_owned(),
                },
                SkippedRepository {
                    repository: "placeholder-org/ungranted".to_owned(),
                    reason: REPOSITORY_NOT_GRANTED.to_owned(),
                },
            ]
        );
    }

    #[test]
    fn absent_or_empty_loop_scope_resolves_to_every_available_repository() {
        for repositories in ["", "repositories: []"] {
            let manifest = PolicyManifest::from_yaml(&format!(
                "manifest_version: 1\nactors: {{builder: {{}}}}\noperations: {{work: {{steps: []}}}}\ngrants: {{work: {{actors: builder, operations: work}}}}\nloops:\n  day:\n    actor: builder\n    operation: work\n    every: hourly\n    {repositories}\n"
            ))
            .unwrap();
            let result = effective_repositories(
                &manifest.resolve_loop("day").unwrap(),
                &BTreeSet::from([
                    "placeholder-org/alpha".to_owned(),
                    "placeholder-org/beta".to_owned(),
                ]),
                &manifest,
            );
            assert_eq!(
                result.repositories,
                ["placeholder-org/alpha", "placeholder-org/beta"]
            );
            assert!(result.skipped.is_empty());
        }
    }
}
