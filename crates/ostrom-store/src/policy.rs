use ostrom_core::{
    CheckDefinition, InconclusivePolicy, PolicyCandidate, PolicyManifest, RuleDecl, SelectorPrefix,
    StallDuration, UnmatchedPolicy,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PolicyLayer {
    Repository,
    Overlay,
    Default,
}

impl PolicyLayer {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Repository => "repository",
            Self::Overlay => "overlay",
            Self::Default => "default",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PolicyBundle {
    pub manifest: PolicyManifest,
    overlay_denies: BTreeSet<String>,
}

impl PolicyBundle {
    #[must_use]
    pub fn repository(manifest: PolicyManifest) -> Self {
        Self {
            manifest,
            overlay_denies: BTreeSet::new(),
        }
    }

    #[must_use]
    pub fn layered(manifest: PolicyManifest, overlay_denies: BTreeSet<String>) -> Self {
        Self {
            manifest,
            overlay_denies,
        }
    }

    fn rule_layer(&self, kind: &str, id: &str) -> PolicyLayer {
        if kind == "deny" && self.overlay_denies.contains(id) {
            PolicyLayer::Overlay
        } else {
            PolicyLayer::Repository
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
        for (kind, declarations) in [
            ("grant", &self.manifest.grants),
            ("deny", &self.manifest.denies),
        ] {
            for (id, declaration) in declarations {
                let unmatched = declaration.unmatched.unwrap_or_else(|| {
                    if kind == "grant" {
                        self.manifest.defaults.grant.unmatched
                    } else {
                        self.manifest.defaults.deny.unmatched
                    }
                });
                rules.push(explain_rule(
                    kind,
                    id,
                    self.rule_layer(kind, id),
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

        rules.sort_by_key(|rule| (rule.layer, rule.kind, rule.id.clone()));

        let matching_grants = rule_ids(&rules, "grant", |rule| rule.matched);
        let matching_denies = rule_ids(&rules, "deny", |rule| rule.matched);
        let deciding_layer = [PolicyLayer::Repository, PolicyLayer::Overlay]
            .into_iter()
            .find(|layer| {
                rules
                    .iter()
                    .any(|rule| rule.layer == *layer && rule.matched)
            })
            .unwrap_or(PolicyLayer::Default);
        let effective_grants = rule_ids(&rules, "grant", |rule| {
            rule.layer == deciding_layer
                && rule.matched
                && rule
                    .requirement
                    .as_ref()
                    .is_none_or(|requirement| requirement.allows)
        });
        let deciding_denies = rule_ids(&rules, "deny", |rule| {
            rule.layer == deciding_layer && rule.matched
        });
        let granted = !effective_grants.is_empty() && deciding_denies.is_empty();
        let floor = deciding_layer == PolicyLayer::Default;

        let hold_rule = if granted {
            None
        } else {
            deciding_denies.first().cloned().or_else(|| {
                rules
                    .iter()
                    .find(|rule| {
                        rule.layer == deciding_layer
                            && rule.kind == "grant"
                            && rule.matched
                            && rule
                                .requirement
                                .as_ref()
                                .is_some_and(|requirement| !requirement.allows)
                    })
                    .map(|rule| rule.id.clone())
            })
        };
        let hold_declaration = hold_rule.as_ref().and_then(|id| {
            self.manifest
                .denies
                .get(id)
                .or_else(|| self.manifest.grants.get(id))
        });
        let stalls_after = hold_declaration
            .and_then(|rule| rule.stalls_after.clone())
            .unwrap_or_else(|| self.manifest.defaults.stalls_after.clone());
        let stalls_source = hold_rule
            .as_ref()
            .and_then(|id| {
                hold_declaration
                    .and_then(|rule| rule.stalls_after.as_ref())
                    .map(|_| {
                        if self.manifest.denies.contains_key(id) {
                            format!("denies.{id}.stalls_after")
                        } else {
                            format!("grants.{id}.stalls_after")
                        }
                    })
            })
            .unwrap_or_else(|| "defaults.stalls_after".to_owned());
        let decision_source = if granted {
            effective_grants.first().map_or_else(
                || "default floor".to_owned(),
                |id| format!("{} grants.{id}", deciding_layer.name()),
            )
        } else if let Some(id) = &hold_rule {
            if self.manifest.denies.contains_key(id) {
                format!("{} denies.{id}", deciding_layer.name())
            } else {
                format!("{} grants.{id}.requires", deciding_layer.name())
            }
        } else {
            "default floor (no grant matched)".to_owned()
        };

        PolicyExplanation {
            actor: actor.to_owned(),
            operation: operation.to_owned(),
            rules,
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
                SelectorPrefix::Label | SelectorPrefix::Path | SelectorPrefix::Type => "subject",
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
        checks
            .iter()
            .find(|candidate| check_name(candidate) == expected)
            .map_or("FAIL", check_status)
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
    PolicyCandidate {
        repository: repository.to_owned(),
        labels: names(pull_request.get("labels"), "name"),
        paths: names(pull_request.get("files"), "path"),
        commit_type: pull_request
            .get("title")
            .and_then(Value::as_str)
            .and_then(commit_type),
        actor: Some(actor.to_owned()),
        verb: Some(operation.to_owned()),
    }
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
            "repository grants.R-rust-green"
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
            "denies.R-plugin-manifest.stalls_after"
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
            "default floor (no grant matched)"
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
