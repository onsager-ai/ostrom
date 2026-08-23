use ostrom_core::{
    CheckDefinition, InconclusivePolicy, PolicyCandidate, PolicyManifest, RuleDecl, SelectorPrefix,
    StallDuration, UnmatchedPolicy, glob_matches,
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PolicyLayer {
    Repository,
    Operator,
    Default,
}

impl PolicyLayer {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Repository => "repository",
            Self::Operator => "operator",
            Self::Default => "default",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PolicyOrigins {
    pub root: PathBuf,
    pub actors: BTreeMap<String, PathBuf>,
    pub checks: BTreeMap<String, PathBuf>,
    pub operations: BTreeMap<String, PathBuf>,
    pub grants: BTreeMap<String, PathBuf>,
    pub denies: BTreeMap<String, PathBuf>,
    pub loops: BTreeMap<String, PathBuf>,
}

impl PolicyOrigins {
    #[must_use]
    pub fn from_root(manifest: &PolicyManifest, path: &Path) -> Self {
        let origins = |keys: Vec<String>| {
            keys.into_iter()
                .map(|key| (key, path.to_path_buf()))
                .collect()
        };
        Self {
            root: path.to_path_buf(),
            actors: origins(manifest.actors.keys().cloned().collect()),
            checks: origins(manifest.checks.keys().cloned().collect()),
            operations: origins(manifest.operations.keys().cloned().collect()),
            grants: origins(manifest.grants.keys().cloned().collect()),
            denies: origins(manifest.denies.keys().cloned().collect()),
            loops: origins(manifest.loops.keys().cloned().collect()),
        }
    }

    pub fn rebase(&mut self, from: &Path, to: &Path) {
        let rebase = |path: &PathBuf| {
            path.strip_prefix(from)
                .map_or_else(|_| path.clone(), |relative| to.join(relative))
        };
        self.root = rebase(&self.root);
        for origins in [
            &mut self.actors,
            &mut self.checks,
            &mut self.operations,
            &mut self.grants,
            &mut self.denies,
            &mut self.loops,
        ] {
            for path in origins.values_mut() {
                *path = rebase(path);
            }
        }
    }

    fn rule(&self, kind: &str, id: &str) -> PathBuf {
        let origins = if kind == "grant" {
            &self.grants
        } else {
            &self.denies
        };
        origins
            .get(id)
            .cloned()
            .unwrap_or_else(|| self.root.clone())
    }
}

#[derive(Debug, Clone)]
struct PolicyDocument {
    layer: PolicyLayer,
    manifest: PolicyManifest,
    origins: PolicyOrigins,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsultedScope {
    pub layer: PolicyLayer,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InertDeclaration {
    pub kind: &'static str,
    pub id: String,
    pub layer: PolicyLayer,
    pub source: PathBuf,
}

#[derive(Debug, Clone)]
pub struct PolicyBundle {
    pub manifest: PolicyManifest,
    documents: Vec<PolicyDocument>,
    inert_declarations: Vec<InertDeclaration>,
}

impl PolicyBundle {
    #[must_use]
    pub fn repository(manifest: PolicyManifest) -> Self {
        let origins = PolicyOrigins::from_root(&manifest, Path::new("<repository>"));
        Self {
            manifest: manifest.clone(),
            documents: vec![PolicyDocument {
                layer: PolicyLayer::Repository,
                manifest,
                origins,
            }],
            inert_declarations: Vec::new(),
        }
    }

    #[must_use]
    pub fn scoped(
        manifest: PolicyManifest,
        repository: PolicyManifest,
        repository_origins: PolicyOrigins,
        operator: Option<(PolicyManifest, PolicyOrigins)>,
    ) -> Self {
        let mut inert_declarations = repository
            .operations
            .keys()
            .map(|id| InertDeclaration {
                kind: "operation",
                id: id.clone(),
                layer: PolicyLayer::Repository,
                source: repository_origins
                    .operations
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| repository_origins.root.clone()),
            })
            .chain(repository.loops.keys().map(|id| {
                InertDeclaration {
                    kind: "loop",
                    id: id.clone(),
                    layer: PolicyLayer::Repository,
                    source: repository_origins
                        .loops
                        .get(id)
                        .cloned()
                        .unwrap_or_else(|| repository_origins.root.clone()),
                }
            }))
            .collect::<Vec<_>>();
        inert_declarations.sort_by_key(|declaration| {
            (
                declaration.kind,
                declaration.id.clone(),
                declaration.source.clone(),
            )
        });

        let mut documents = vec![PolicyDocument {
            layer: PolicyLayer::Repository,
            manifest: repository,
            origins: repository_origins,
        }];
        if let Some((operator, origins)) = operator {
            documents.push(PolicyDocument {
                layer: PolicyLayer::Operator,
                manifest: operator,
                origins,
            });
        }
        Self {
            manifest,
            documents,
            inert_declarations,
        }
    }

    #[must_use]
    pub fn operator(manifest: PolicyManifest, origins: PolicyOrigins) -> Self {
        Self {
            manifest: manifest.clone(),
            documents: vec![PolicyDocument {
                layer: PolicyLayer::Operator,
                manifest,
                origins,
            }],
            inert_declarations: Vec::new(),
        }
    }

    #[must_use]
    pub fn decide(
        &self,
        actor: &str,
        operation: &str,
        candidate: &PolicyCandidate,
    ) -> ostrom_core::PolicyDecision {
        let mut matching_grants = Vec::new();
        let mut matching_denies = Vec::new();
        for document in &self.documents {
            let decision = document.manifest.decide(actor, operation, candidate);
            matching_grants.extend(decision.matching_grants);
            matching_denies.extend(decision.matching_denies);
        }
        ostrom_core::PolicyDecision {
            granted: !matching_grants.is_empty() && matching_denies.is_empty(),
            matching_grants,
            matching_denies,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectorProjection {
    pub selector: String,
    pub projection: &'static str,
    pub matched: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequirementExplanation {
    pub check: String,
    pub status: &'static str,
    pub allows: bool,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleExplanation {
    pub kind: &'static str,
    pub id: String,
    pub layer: PolicyLayer,
    pub source: PathBuf,
    pub subject_matched: bool,
    pub actor_matched: bool,
    pub matched: bool,
    pub selectors: Vec<SelectorProjection>,
    pub requirement: Option<RequirementExplanation>,
    pub stalls_after: Option<StallDuration>,
    pub unmatched: UnmatchedPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyExplanation {
    pub actor: String,
    pub operation: String,
    pub rules: Vec<RuleExplanation>,
    pub consulted_scopes: Vec<ConsultedScope>,
    pub inert_declarations: Vec<InertDeclaration>,
    pub matching_grants: Vec<String>,
    pub effective_grants: Vec<String>,
    pub matching_denies: Vec<String>,
    pub granted: bool,
    pub floor: bool,
    pub decision_source: String,
    pub hold_rule: Option<String>,
    pub stalls_after: StallDuration,
    pub stalls_source: String,
}

impl PolicyBundle {
    #[must_use]
    pub fn explain_pull_request(
        &self,
        repository: &str,
        pull_request: &Value,
        actor: &str,
        operation: &str,
    ) -> PolicyExplanation {
        let candidate = pull_request_candidate(repository, pull_request, actor, operation);
        let checks = pull_request
            .get("statusCheckRollup")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let mut rules = Vec::new();
        for document in &self.documents {
            for (kind, declarations) in [
                ("grant", &document.manifest.grants),
                ("deny", &document.manifest.denies),
            ] {
                for (id, declaration) in declarations {
                    let unmatched = declaration.unmatched.unwrap_or_else(|| {
                        if kind == "grant" {
                            document.manifest.defaults.grant.unmatched
                        } else {
                            document.manifest.defaults.deny.unmatched
                        }
                    });
                    rules.push(explain_rule(
                        kind,
                        id,
                        document.layer,
                        document.origins.rule(kind, id),
                        declaration,
                        &candidate,
                        actor,
                        operation,
                        &self.manifest.checks,
                        self.manifest.defaults.check.inconclusive_policy,
                        checks,
                        unmatched,
                    ));
                }
            }
        }

        rules.sort_by_key(|rule| (rule.layer, rule.kind, rule.id.clone(), rule.source.clone()));

        let matching_grants = rule_ids(&rules, "grant", |rule| rule.matched);
        let matching_denies = rule_ids(&rules, "deny", |rule| rule.matched);
        let effective_grants = rule_ids(&rules, "grant", |rule| {
            rule.matched
                && rule
                    .requirement
                    .as_ref()
                    .is_none_or(|requirement| requirement.allows)
        });
        let granted = !effective_grants.is_empty() && matching_denies.is_empty();
        let floor = matching_grants.is_empty() && matching_denies.is_empty();

        let deciding_rule = if granted {
            rules.iter().find(|rule| {
                rule.kind == "grant"
                    && rule.matched
                    && rule
                        .requirement
                        .as_ref()
                        .is_none_or(|requirement| requirement.allows)
            })
        } else {
            rules
                .iter()
                .find(|rule| rule.kind == "deny" && rule.matched)
                .or_else(|| {
                    rules.iter().find(|rule| {
                        rule.kind == "grant"
                            && rule.matched
                            && rule
                                .requirement
                                .as_ref()
                                .is_some_and(|requirement| !requirement.allows)
                    })
                })
        };
        let hold_rule = (!granted)
            .then(|| deciding_rule.map(|rule| rule.id.clone()))
            .flatten();
        let stalls_after = deciding_rule
            .and_then(|rule| rule.stalls_after.clone())
            .unwrap_or_else(|| self.manifest.defaults.stalls_after.clone());
        let stalls_source = deciding_rule
            .filter(|rule| rule.stalls_after.is_some())
            .map_or_else(
                || "defaults.stalls_after".to_owned(),
                |rule| {
                    let section = if rule.kind == "deny" {
                        "denies"
                    } else {
                        "grants"
                    };
                    format!(
                        "{section}.{}.stalls_after in {}",
                        rule.id,
                        rule.source.display()
                    )
                },
            );
        let consulted_scopes = self
            .documents
            .iter()
            .map(|document| ConsultedScope {
                layer: document.layer,
                path: document.origins.root.clone(),
            })
            .collect::<Vec<_>>();
        let decision_source = deciding_rule.map_or_else(
            || {
                let consulted = consulted_scopes
                    .iter()
                    .map(|scope| format!("{} {}", scope.layer.name(), scope.path.display()))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("default deny (no grant matched; consulted {consulted})")
            },
            |rule| {
                let section = if rule.kind == "deny" {
                    "denies"
                } else {
                    "grants"
                };
                let requirement = if rule.kind == "grant"
                    && rule.requirement.as_ref().is_some_and(|value| !value.allows)
                {
                    ".requires"
                } else {
                    ""
                };
                format!(
                    "{} {section}.{}{} in {}",
                    rule.layer.name(),
                    rule.id,
                    requirement,
                    rule.source.display()
                )
            },
        );

        PolicyExplanation {
            actor: actor.to_owned(),
            operation: operation.to_owned(),
            rules,
            consulted_scopes,
            inert_declarations: self.inert_declarations.clone(),
            matching_grants,
            effective_grants,
            matching_denies,
            granted,
            floor,
            decision_source,
            hold_rule,
            stalls_after,
            stalls_source,
        }
    }
}

fn rule_ids(
    rules: &[RuleExplanation],
    kind: &str,
    predicate: impl Fn(&RuleExplanation) -> bool,
) -> Vec<String> {
    rules
        .iter()
        .filter(|rule| rule.kind == kind && predicate(rule))
        .map(|rule| rule.id.clone())
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn explain_rule(
    kind: &'static str,
    id: &str,
    layer: PolicyLayer,
    source: PathBuf,
    declaration: &RuleDecl,
    candidate: &PolicyCandidate,
    actor: &str,
    operation: &str,
    definitions: &BTreeMap<String, CheckDefinition>,
    default_inconclusive_policy: InconclusivePolicy,
    checks: &[Value],
    unmatched: UnmatchedPolicy,
) -> RuleExplanation {
    let repository_matched = declaration.repositories.is_empty()
        || declaration
            .repositories
            .iter()
            .any(|repository| repository == &candidate.repository);
    let actor_dimension_matched = declaration.actors.is_empty()
        || declaration
            .actors
            .iter()
            .any(|candidate| candidate == actor);
    let operation_dimension_matched = declaration.operations.is_empty()
        || declaration
            .operations
            .iter()
            .any(|candidate| candidate == operation);
    let selectors = declaration
        .selectors
        .iter()
        .map(|selector| SelectorProjection {
            selector: selector.to_string(),
            projection: match selector.prefix() {
                SelectorPrefix::Label
                | SelectorPrefix::Path
                | SelectorPrefix::Ref
                | SelectorPrefix::Scope
                | SelectorPrefix::Substance
                | SelectorPrefix::Title
                | SelectorPrefix::Type => "subject",
                SelectorPrefix::Actor | SelectorPrefix::Verb => "actor",
            },
            matched: selector.matches(candidate),
        })
        .collect::<Vec<_>>();
    let subject_selectors = selectors
        .iter()
        .filter(|selector| selector.projection == "subject")
        .collect::<Vec<_>>();
    let actor_selectors = selectors
        .iter()
        .filter(|selector| selector.projection == "actor")
        .collect::<Vec<_>>();
    let subject_matched = repository_matched
        && (subject_selectors.is_empty()
            || subject_selectors.iter().any(|selector| selector.matched));
    let actor_matched = actor_dimension_matched
        && operation_dimension_matched
        && (actor_selectors.is_empty() || actor_selectors.iter().any(|selector| selector.matched));
    let matched = declaration.matches(actor, operation, candidate);
    let requirement = declaration
        .requires
        .as_deref()
        .map(|check| explain_requirement(check, definitions, default_inconclusive_policy, checks));
    RuleExplanation {
        kind,
        id: id.to_owned(),
        layer,
        source,
        subject_matched,
        actor_matched,
        matched,
        selectors,
        requirement,
        stalls_after: declaration.stalls_after.clone(),
        unmatched,
    }
}

fn explain_requirement(
    check: &str,
    definitions: &BTreeMap<String, CheckDefinition>,
    default_inconclusive_policy: InconclusivePolicy,
    checks: &[Value],
) -> RequirementExplanation {
    let Some(definition) = definitions.get(check) else {
        return RequirementExplanation {
            check: check.to_owned(),
            status: "INCONCLUSIVE",
            allows: false,
            source: format!("checks.{check}: undefined check"),
        };
    };
    let policy = definition
        .inconclusive_policy
        .unwrap_or(default_inconclusive_policy);
    let status = if definition.uses == "gh/check-run" {
        let expected = definition
            .with
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let selected = checks
            .iter()
            .filter(|candidate| glob_matches(check_name(candidate), expected, false))
            .collect::<Vec<_>>();
        if selected.is_empty() || selected.iter().any(|check| check_status(check) == "FAIL") {
            "FAIL"
        } else if selected
            .iter()
            .any(|check| check_status(check) == "INCONCLUSIVE")
        {
            "INCONCLUSIVE"
        } else {
            "PASS"
        }
    } else {
        "INCONCLUSIVE"
    };
    let allows = match status {
        "PASS" => true,
        "FAIL" => false,
        _ => policy != InconclusivePolicy::Block,
    };
    let source = if definition.uses == "gh/check-run" {
        format!(
            "checks.{check}: gh/check-run name={}",
            definition
                .with
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("(missing)")
        )
    } else {
        format!("checks.{check}: {}", definition.uses)
    };
    RequirementExplanation {
        check: check.to_owned(),
        status,
        allows,
        source,
    }
}

fn check_name(check: &Value) -> &str {
    check
        .get("name")
        .or_else(|| check.get("context"))
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn check_status(check: &Value) -> &'static str {
    let state = check
        .get("bucket")
        .or_else(|| check.get("conclusion"))
        .or_else(|| check.get("state"))
        .or_else(|| check.get("status"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if matches_ignore_ascii_case(
        state,
        &["pass", "skipping", "success", "neutral", "skipped"],
    ) {
        "PASS"
    } else if matches_ignore_ascii_case(
        state,
        &[
            "fail",
            "failure",
            "error",
            "cancel",
            "cancelled",
            "timed_out",
            "action_required",
            "stale",
        ],
    ) {
        "FAIL"
    } else {
        "INCONCLUSIVE"
    }
}

fn matches_ignore_ascii_case(value: &str, candidates: &[&str]) -> bool {
    candidates
        .iter()
        .any(|candidate| value.eq_ignore_ascii_case(candidate))
}

fn pull_request_candidate(
    repository: &str,
    pull_request: &Value,
    actor: &str,
    operation: &str,
) -> PolicyCandidate {
    let title = pull_request
        .get("title")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut refs = pull_request
        .get("number")
        .and_then(Value::as_u64)
        .into_iter()
        .chain(
            pull_request
                .pointer("/closingIssuesReferences/nodes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|issue| issue.get("number").and_then(Value::as_u64)),
        )
        .map(|number| format!("#{number}"))
        .collect::<Vec<_>>();
    refs.sort();
    refs.dedup();
    PolicyCandidate {
        repository: repository.to_owned(),
        labels: names(pull_request.get("labels"), "name"),
        paths: names(pull_request.get("files"), "path"),
        refs,
        scopes: title.as_deref().map_or_else(Vec::new, conventional_scopes),
        substances: names(pull_request.get("substances"), "name"),
        commit_type: title.as_deref().and_then(commit_type),
        title,
        actor: Some(actor.to_owned()),
        verb: Some(operation.to_owned()),
    }
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

fn names(value: Option<&Value>, key: &str) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            item.as_str()
                .or_else(|| item.get(key).and_then(Value::as_str))
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        })
        .collect()
}

fn commit_type(title: &str) -> Option<String> {
    let prefix = title.split_once(':')?.0;
    let kind = prefix
        .split_once('(')
        .map_or(prefix, |(kind, _)| kind)
        .trim();
    (!kind.is_empty()
        && kind
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'))
    .then(|| kind.to_owned())
}

#[cfg(test)]
mod tests {
    use ostrom_core::{InconclusivePolicy, PolicyManifest};
    use serde_json::json;

    use super::PolicyBundle;

    fn bundle() -> PolicyBundle {
        PolicyBundle::repository(
            PolicyManifest::from_yaml(
                r#"
manifest_version: 1
defaults: {stalls_after: 7d}
actors: {builder: {}}
operations: {work: {steps: []}}
checks:
  rust-green:
    uses: gh/check-run
    with: {name: placeholder-ci}
grants:
  R-rust-green:
    actors: builder
    operations: work
    repositories: placeholder-org/repository
    where: label:delegated
    requires: rust-green
denies:
  R-plugin-manifest:
    actors: builder
    operations: work
    repositories: placeholder-org/repository
    where: path:**/.claude-plugin/plugin.json
    stalls_after: 12d
"#,
            )
            .expect("manifest"),
        )
    }

    #[test]
    fn explanation_names_the_grant_requirement_and_ladder_source() {
        let explanation = bundle().explain_pull_request(
            "placeholder-org/repository",
            &json!({
                "title": "feat: placeholder",
                "labels": [{"name": "delegated"}],
                "files": [{"path": "src/lib.rs"}],
                "statusCheckRollup": [{"name": "placeholder-ci", "conclusion": "SUCCESS"}],
            }),
            "builder",
            "work",
        );
        assert!(explanation.granted);
        assert_eq!(
            explanation.decision_source,
            "repository grants.R-rust-green in <repository>"
        );
        let grant = explanation
            .rules
            .iter()
            .find(|rule| rule.id == "R-rust-green")
            .expect("grant explanation");
        assert_eq!(
            grant.requirement.as_ref().map(|value| value.status),
            Some("PASS")
        );
        assert_eq!(
            grant
                .requirement
                .as_ref()
                .map(|value| value.source.as_str()),
            Some("checks.rust-green: gh/check-run name=placeholder-ci")
        );
    }

    #[test]
    fn deny_hold_keeps_the_verdict_and_uses_its_stall_override() {
        let explanation = bundle().explain_pull_request(
            "placeholder-org/repository",
            &json!({
                "title": "chore: placeholder",
                "labels": [],
                "files": [{"path": ".claude-plugin/plugin.json"}],
                "statusCheckRollup": [],
            }),
            "builder",
            "work",
        );
        assert!(!explanation.granted);
        assert_eq!(explanation.hold_rule.as_deref(), Some("R-plugin-manifest"));
        assert_eq!(explanation.stalls_after.to_string(), "12d");
        assert_eq!(
            explanation.stalls_source,
            "denies.R-plugin-manifest.stalls_after in <repository>"
        );
    }

    #[test]
    fn no_matching_rule_reports_the_principal_floor() {
        let explanation = bundle().explain_pull_request(
            "placeholder-org/repository",
            &json!({
                "title": "chore: placeholder",
                "labels": [],
                "files": [{"path": "README.md"}],
                "statusCheckRollup": [],
            }),
            "builder",
            "work",
        );
        assert!(!explanation.granted);
        assert!(explanation.floor);
        assert_eq!(
            explanation.decision_source,
            "default deny (no grant matched; consulted repository <repository>)"
        );
        assert_eq!(explanation.stalls_after.to_string(), "7d");
    }

    #[test]
    fn an_allowed_inconclusive_requirement_proceeds_but_stays_visible() {
        let mut bundle = bundle();
        bundle.manifest.defaults.check.inconclusive_policy = InconclusivePolicy::Pass;
        let explanation = bundle.explain_pull_request(
            "placeholder-org/repository",
            &json!({
                "title": "feat: placeholder",
                "labels": [{"name": "delegated"}],
                "files": [{"path": "src/lib.rs"}],
                "statusCheckRollup": [{"name": "placeholder-ci", "status": "IN_PROGRESS"}],
            }),
            "builder",
            "work",
        );
        assert!(explanation.granted);
        let requirement = explanation
            .rules
            .iter()
            .find(|rule| rule.id == "R-rust-green")
            .and_then(|rule| rule.requirement.as_ref())
            .expect("requirement");
        assert_eq!(requirement.status, "INCONCLUSIVE");
        assert!(requirement.allows);
    }
}
