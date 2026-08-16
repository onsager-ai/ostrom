use std::collections::{BTreeMap, BTreeSet, HashSet};

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

use crate::{CheckBasis, CheckEvaluation, CheckFault, CheckReceipt, CheckState, ResolvedCheck};

pub const GOALS_VERSION: u32 = 1;
pub const PLAN_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalsDocument {
    pub goals_version: u32,
    #[serde(default)]
    pub goals: Vec<Goal>,
    #[serde(default)]
    pub actions: Vec<GoalAction>,
    #[serde(default)]
    pub acknowledgements: Vec<Acknowledgement>,
}

impl GoalsDocument {
    pub fn from_yaml(input: &str) -> Result<Self, GoalsError> {
        let document: Self = serde_yaml::from_str(input).map_err(GoalsError::Yaml)?;
        if document.goals_version != GOALS_VERSION {
            return Err(GoalsError::UnsupportedVersion);
        }
        let mut ids = HashSet::new();
        for goal in &document.goals {
            if goal.id.trim().is_empty() || goal.intent.trim().is_empty() {
                return Err(GoalsError::EmptyGoalField);
            }
            if !ids.insert(goal.id.as_str()) {
                return Err(GoalsError::DuplicateGoal(goal.id.clone()));
            }
            if goal.serves.iter().any(|item| !valid_item_id(&item.epic))
                || goal.met_when.iter().any(|check| check.trim().is_empty())
            {
                return Err(GoalsError::InvalidReference(goal.id.clone()));
            }
            let unique_checks = goal.met_when.iter().collect::<HashSet<_>>();
            if unique_checks.len() != goal.met_when.len() {
                return Err(GoalsError::DuplicateCheck(goal.id.clone()));
            }
        }
        for action in &document.actions {
            if !ids.contains(action.goal.as_str()) {
                return Err(GoalsError::UnknownGoal(action.goal.clone()));
            }
            if action.note.trim().is_empty() {
                return Err(GoalsError::EmptyActionNote(action.goal.clone()));
            }
        }
        for acknowledgement in &document.acknowledgements {
            if !ids.contains(acknowledgement.goal.as_str()) {
                return Err(GoalsError::UnknownGoal(acknowledgement.goal.clone()));
            }
        }
        Ok(document)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Goal {
    pub id: String,
    pub intent: String,
    pub state: GoalState,
    #[serde(default)]
    pub serves: Vec<GoalService>,
    #[serde(default)]
    pub met_when: Vec<String>,
    #[serde(default)]
    pub horizon: Option<NaiveDate>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalService {
    pub epic: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GoalState {
    Active,
    Paused,
    Met,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalAction {
    pub goal: String,
    pub verb: GoalActionVerb,
    pub note: String,
    #[serde(default)]
    pub until: Option<NaiveDate>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GoalActionVerb {
    Promote,
    Pause,
    Demote,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Acknowledgement {
    pub goal: String,
    pub reading: Reading,
    pub response: AcknowledgementResponse,
    #[serde(default)]
    pub until: Option<NaiveDate>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reading {
    OnTrack,
    AtRisk,
    OffTrack,
    Blocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AcknowledgementResponse {
    Accepted,
    Disputed,
    Deferred,
}

#[derive(Debug, Error)]
pub enum GoalsError {
    #[error("could not parse goals YAML: {0}")]
    Yaml(serde_yaml::Error),
    #[error("unsupported goals_version")]
    UnsupportedVersion,
    #[error("goal id and intent must not be empty")]
    EmptyGoalField,
    #[error("duplicate goal id: {0}")]
    DuplicateGoal(String),
    #[error("goal has an invalid work or check reference: {0}")]
    InvalidReference(String),
    #[error("goal repeats a met_when check: {0}")]
    DuplicateCheck(String),
    #[error("unknown goal: {0}")]
    UnknownGoal(String),
    #[error("goal action note must not be empty: {0}")]
    EmptyActionNote(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluatedCheck {
    pub evaluation: CheckEvaluation,
    pub basis: CheckBasis,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_age_seconds: Option<u64>,
}

impl EvaluatedCheck {
    #[must_use]
    pub fn from_contract(
        check: &ResolvedCheck,
        receipts: &[CheckReceipt],
        evaluated_at: DateTime<Utc>,
    ) -> Self {
        let evaluation = check.evaluate(receipts, evaluated_at);
        let observation_age_seconds = receipts
            .iter()
            .filter(|receipt| receipt.check == check.id)
            .max_by_key(|receipt| receipt.observed_at)
            .and_then(|receipt| {
                evaluated_at
                    .signed_duration_since(receipt.observed_at)
                    .num_seconds()
                    .try_into()
                    .ok()
            });
        Self {
            evaluation,
            basis: check.basis,
            observation_age_seconds,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MilestoneInput {
    pub id: String,
    pub epic: String,
    pub opened: String,
    #[serde(default)]
    pub updated: String,
    #[serde(default)]
    pub blocked_by: Vec<String>,
    pub open: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueItem {
    pub id: String,
    pub opened: String,
    pub kind: String,
    pub state: String,
    #[serde(default)]
    pub blocked_by: Vec<String>,
}

impl QueueItem {
    #[must_use]
    pub fn dispatchable(&self) -> bool {
        self.kind != "parked"
            && self.state != "deferred"
            && (matches!(self.kind.as_str(), "moved" | "stuck")
                || (self.state == "approved"
                    && matches!(self.kind.as_str(), "tripwire" | "decision")))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalFacts {
    pub goal: String,
    pub milestones: Vec<MilestoneFact>,
    pub progress: ProgressFact,
    pub movement: MovementFact,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    pub met_when_status: Vec<MetWhenStatus>,
    pub impediments: Vec<Impediment>,
    pub met: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MilestoneFact {
    pub id: String,
    pub opened: String,
    pub updated: String,
    pub blocked_by: Vec<String>,
    pub dispatchable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressFact {
    pub complete: usize,
    pub total: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MovementFact {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_at: Option<String>,
    pub open: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetWhenStatus {
    pub check: String,
    pub state: CheckState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fault: Option<CheckFault>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub basis: Option<CheckBasis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observation_age_seconds: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Impediment {
    pub item: String,
    pub fact: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssessmentDraft {
    pub goal: String,
    pub reading: Reading,
    pub because: Vec<Because>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Because {
    pub fact: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assessment {
    pub goal: String,
    pub reading: Reading,
    pub because: Vec<Because>,
    pub consequence: Consequence,
    pub escalation_suppressed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Consequence {
    pub promote: Vec<String>,
    pub escalate: Vec<String>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AssessmentError {
    #[error("assessment goal does not match the requested goal")]
    GoalMismatch,
    #[error("assessment must cite at least one computed fact")]
    Uncited,
    #[error("assessment cites an unknown fact: {0}")]
    InventedFact(String),
    #[error("assessment detail must not be empty")]
    EmptyDetail,
}

#[must_use]
pub fn derive_goal_facts(
    goal: &Goal,
    milestones: &[MilestoneInput],
    queue: &[QueueItem],
    checks: &BTreeMap<String, EvaluatedCheck>,
) -> GoalFacts {
    let served = goal
        .serves
        .iter()
        .map(|service| service.epic.as_str())
        .collect::<BTreeSet<_>>();
    let related = milestones
        .iter()
        .filter(|milestone| served.contains(milestone.epic.as_str()))
        .collect::<Vec<_>>();
    let latest_at = related
        .iter()
        .map(|milestone| {
            if milestone.updated.is_empty() {
                milestone.opened.clone()
            } else {
                milestone.updated.clone()
            }
        })
        .max();
    let complete = related.iter().filter(|milestone| !milestone.open).count();
    let open = related
        .iter()
        .filter(|milestone| milestone.open)
        .copied()
        .collect::<Vec<_>>();
    let open_ids = open
        .iter()
        .map(|milestone| milestone.id.as_str())
        .collect::<BTreeSet<_>>();
    let queue_by_id = queue
        .iter()
        .map(|item| (item.id.as_str(), item))
        .collect::<BTreeMap<_, _>>();
    let mut ordered = Vec::new();
    let mut remaining = open;
    while !remaining.is_empty() {
        let index = remaining
            .iter()
            .enumerate()
            .filter(|(_, milestone)| {
                milestone.blocked_by.iter().all(|dependency| {
                    !open_ids.contains(dependency.as_str())
                        || ordered
                            .iter()
                            .any(|ordered: &&MilestoneInput| ordered.id == *dependency)
                })
            })
            .min_by_key(|(_, milestone)| (&milestone.opened, &milestone.id))
            .map_or(0, |(index, _)| index);
        ordered.push(remaining.remove(index));
    }
    let milestones = ordered
        .iter()
        .map(|milestone| MilestoneFact {
            id: milestone.id.clone(),
            opened: milestone.opened.clone(),
            updated: milestone.updated.clone(),
            blocked_by: milestone.blocked_by.clone(),
            dispatchable: queue_by_id
                .get(milestone.id.as_str())
                .is_some_and(|item| item.dispatchable()),
        })
        .collect::<Vec<_>>();
    let next = ordered
        .iter()
        .find(|milestone| {
            milestone
                .blocked_by
                .iter()
                .all(|dependency| !open_ids.contains(dependency.as_str()))
        })
        .map(|milestone| milestone.id.clone());
    let impediments = next
        .iter()
        .filter_map(|id| queue_by_id.get(id.as_str()))
        .flat_map(|item| {
            let mut impediments = item
                .blocked_by
                .iter()
                .filter(|dependency| open_ids.contains(dependency.as_str()))
                .map(|dependency| Impediment {
                    item: item.id.clone(),
                    fact: "next.unsatisfied_dependency".to_owned(),
                    detail: dependency.clone(),
                })
                .collect::<Vec<_>>();
            if !item.dispatchable() {
                let fact = if item.kind == "parked" {
                    "next.hold"
                } else if item.kind == "tripwire" && item.state != "approved" {
                    "next.tripwire"
                } else if item.state == "deferred" {
                    "next.deferred"
                } else {
                    "next.unauthorized"
                };
                impediments.push(Impediment {
                    item: item.id.clone(),
                    fact: fact.to_owned(),
                    detail: format!("kind={} state={}", item.kind, item.state),
                });
            }
            impediments
        })
        .collect::<Vec<_>>();
    let met_when_status = goal
        .met_when
        .iter()
        .map(|check| {
            checks.get(check).map_or_else(
                || MetWhenStatus {
                    check: check.clone(),
                    state: CheckState::NeverRun,
                    fault: Some(CheckFault {
                        name: "unresolved_check".to_owned(),
                        detail: None,
                    }),
                    basis: None,
                    observation_age_seconds: None,
                },
                |check_status| MetWhenStatus {
                    check: check.clone(),
                    state: check_status.evaluation.state,
                    fault: check_status.evaluation.fault.clone(),
                    basis: Some(check_status.basis),
                    observation_age_seconds: check_status.observation_age_seconds,
                },
            )
        })
        .collect::<Vec<_>>();
    let checks_met = !goal.met_when.is_empty()
        && met_when_status
            .iter()
            .all(|status| status.state == CheckState::Passing && status.fault.is_none());
    GoalFacts {
        goal: goal.id.clone(),
        milestones,
        progress: ProgressFact {
            complete,
            total: related.len(),
        },
        movement: MovementFact {
            latest_at,
            open: ordered.len(),
        },
        next,
        met_when_status,
        impediments,
        met: goal.state == GoalState::Met || (goal.state == GoalState::Active && checks_met),
    }
}

#[must_use]
pub fn fact_table(facts: &GoalFacts, queue: &[QueueItem]) -> BTreeMap<String, Value> {
    let mut table = BTreeMap::new();
    table.insert(
        "progress.complete".to_owned(),
        json!(facts.progress.complete),
    );
    table.insert("progress.total".to_owned(), json!(facts.progress.total));
    table.insert("movement.open".to_owned(), json!(facts.movement.open));
    table.insert(
        "movement.latest_at".to_owned(),
        json!(facts.movement.latest_at),
    );
    table.insert(
        "milestones.order".to_owned(),
        json!(
            facts
                .milestones
                .iter()
                .map(|milestone| &milestone.id)
                .collect::<Vec<_>>()
        ),
    );
    table.insert("goal.met".to_owned(), json!(facts.met));
    if let Some(next) = &facts.next {
        table.insert("next.id".to_owned(), json!(next));
        if let Some(item) = queue.iter().find(|item| &item.id == next) {
            table.insert("next.dispatchable".to_owned(), json!(item.dispatchable()));
            table.insert("next.kind".to_owned(), json!(item.kind));
            table.insert("next.state".to_owned(), json!(item.state));
        }
    }
    let mut checks_by_state = BTreeMap::<String, Vec<String>>::new();
    let mut faulted_checks = Vec::new();
    for status in &facts.met_when_status {
        let state = serde_json::to_value(status.state).expect("check state serializes");
        checks_by_state
            .entry(state.as_str().unwrap_or("unknown").to_owned())
            .or_default()
            .push(status.check.clone());
        table.insert(format!("met_when.{}.state", status.check), state);
        if let Some(fault) = &status.fault {
            table.insert(format!("met_when.{}.fault", status.check), json!(fault));
            faulted_checks.push(status.check.clone());
        }
    }
    for (state, mut checks) in checks_by_state {
        checks.sort();
        table.insert(format!("met_when.{state}"), json!(checks));
    }
    if !faulted_checks.is_empty() {
        faulted_checks.sort();
        table.insert("met_when.fault".to_owned(), json!(faulted_checks));
    }
    let mut impediments = BTreeMap::<String, Vec<String>>::new();
    for impediment in &facts.impediments {
        impediments
            .entry(impediment.fact.clone())
            .or_default()
            .push(impediment.detail.clone());
    }
    for (fact, mut details) in impediments {
        details.sort();
        table.insert(fact, json!(details));
    }
    table
}

pub fn validate_assessment(
    goal: &Goal,
    facts: &GoalFacts,
    queue: &[QueueItem],
    assessment: AssessmentDraft,
) -> Result<AssessmentDraft, AssessmentError> {
    if assessment.goal != goal.id {
        return Err(AssessmentError::GoalMismatch);
    }
    if assessment.because.is_empty() {
        return Err(AssessmentError::Uncited);
    }
    let table = fact_table(facts, queue);
    for clause in &assessment.because {
        if clause.detail.trim().is_empty() {
            return Err(AssessmentError::EmptyDetail);
        }
        if !table.contains_key(&clause.fact) {
            return Err(AssessmentError::InventedFact(clause.fact.clone()));
        }
    }
    Ok(assessment)
}

#[must_use]
pub fn cited_fact_basis(
    assessment: &AssessmentDraft,
    facts: &GoalFacts,
    queue: &[QueueItem],
) -> BTreeMap<String, Value> {
    let table = fact_table(facts, queue);
    assessment
        .because
        .iter()
        .filter_map(|clause| {
            table
                .get(&clause.fact)
                .cloned()
                .map(|value| (clause.fact.clone(), value))
        })
        .collect()
}

#[must_use]
pub fn consequence(facts: &GoalFacts, queue: &[QueueItem]) -> Consequence {
    let Some(next) = &facts.next else {
        return Consequence::default();
    };
    let Some(item) = queue.iter().find(|item| &item.id == next) else {
        return Consequence {
            promote: Vec::new(),
            escalate: vec![next.clone()],
        };
    };
    if item.dispatchable() && facts.impediments.is_empty() {
        Consequence {
            promote: vec![next.clone()],
            escalate: Vec::new(),
        }
    } else {
        Consequence {
            promote: Vec::new(),
            escalate: vec![next.clone()],
        }
    }
}

#[must_use]
pub fn compose_ranking(
    queue: &[QueueItem],
    principal: &[String],
    computed: &[String],
    demoted: &BTreeSet<String>,
) -> Vec<String> {
    let dispatchable = queue
        .iter()
        .filter(|item| item.dispatchable())
        .collect::<Vec<_>>();
    let allowed = dispatchable
        .iter()
        .map(|item| item.id.as_str())
        .collect::<BTreeSet<_>>();
    let mut result = Vec::new();
    for id in principal.iter().chain(computed) {
        if allowed.contains(id.as_str()) && !result.contains(id) && !demoted.contains(id) {
            result.push(id.clone());
        }
    }
    let mut remainder = dispatchable
        .iter()
        .filter(|item| !result.contains(&item.id) && !demoted.contains(&item.id))
        .copied()
        .collect::<Vec<_>>();
    remainder.sort_by_key(|item| (&item.opened, &item.id));
    result.extend(remainder.into_iter().map(|item| item.id.clone()));
    let mut tail = dispatchable
        .iter()
        .filter(|item| demoted.contains(&item.id))
        .copied()
        .collect::<Vec<_>>();
    tail.sort_by_key(|item| (&item.opened, &item.id));
    result.extend(tail.into_iter().map(|item| item.id.clone()));
    result
}

/// Reproduce the pre-plan selector exactly: a principal prefix, then the
/// dependency-unblocks heuristic introduced with that prefix, with age as the
/// final floor. Plan uses this whenever no goal consequence speaks.
#[must_use]
pub fn mechanical_ranking(queue: &[QueueItem], principal: &[String]) -> Vec<String> {
    let mut dispatchable = queue
        .iter()
        .filter(|item| item.dispatchable())
        .collect::<Vec<_>>();
    if principal.is_empty() {
        dispatchable.sort_by_key(|item| (&item.opened, &item.id));
        return dispatchable
            .into_iter()
            .map(|item| item.id.clone())
            .collect();
    }
    dispatchable.sort_by_key(|item| {
        let rank = principal.iter().position(|id| id == &item.id);
        let unblocks = queue
            .iter()
            .filter(|candidate| candidate.state != "deferred" && candidate.kind != "parked")
            .filter(|candidate| candidate.blocked_by.contains(&item.id))
            .count();
        match rank {
            Some(rank) => (0, rank, 0_isize, &item.opened, &item.id),
            None => (1, 0, -(unblocks as isize), &item.opened, &item.id),
        }
    });
    dispatchable
        .into_iter()
        .map(|item| item.id.clone())
        .collect()
}

fn valid_item_id(value: &str) -> bool {
    if value.chars().any(char::is_whitespace) {
        return false;
    }
    let Some((repository, number)) = value.rsplit_once('#') else {
        return false;
    };
    let mut repository = repository.split('/');
    matches!((repository.next(), repository.next(), repository.next()), (Some(owner), Some(name), None) if !owner.is_empty() && !name.is_empty())
        && number.starts_with(|character: char| character.is_ascii_digit() && character != '0')
        && number.chars().all(|character| character.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use chrono::{TimeZone, Utc};
    use serde_json::json;

    use super::*;
    use crate::{
        ActionDefinition, Catalogue, CatalogueEnumeration, CheckDocument, CheckVerdict,
        RunnerStamp, resolve_check,
    };

    const GOALS: &str = r#"
goals_version: 1
goals:
  - id: rust-cli
    intent: ostrom installs and runs as a product
    state: active
    serves:
      - epic: example-org/example-repo#115
    met_when: [sweep-parity]
actions: []
acknowledgements: []
"#;

    fn goal() -> Goal {
        GoalsDocument::from_yaml(GOALS)
            .expect("fixture parses")
            .goals
            .remove(0)
    }

    fn queue(id: &str, kind: &str, state: &str, opened: &str) -> QueueItem {
        QueueItem {
            id: id.to_owned(),
            opened: opened.to_owned(),
            kind: kind.to_owned(),
            state: state.to_owned(),
            blocked_by: Vec::new(),
        }
    }

    fn resolved() -> ResolvedCheck {
        let document = CheckDocument::from_yaml(
            "checks_version: 1\nchecks:\n  sweep-parity:\n    uses: fixture/observe\n    with: {}\n",
        )
        .expect("check document");
        let action = ActionDefinition {
            uses: "fixture/observe".to_owned(),
            producer: "fixture".to_owned(),
            default_fresh_for_seconds: 300,
            definition: json!({}),
            source_revision: "fixture-revision".to_owned(),
        };
        resolve_check(
            "sweep-parity",
            &CatalogueEnumeration {
                catalogues: vec![Catalogue { document }],
                complete: true,
            },
            &action,
        )
        .expect("resolved check")
    }

    #[test]
    fn strict_goals_reject_unknown_and_derived_fields() {
        for field in [
            "reading: on-track",
            "progress: 1",
            "basis: mechanical",
            "unknown: true",
        ] {
            let yaml = GOALS.replace(
                "    state: active",
                &format!("    state: active\n    {field}"),
            );
            assert!(GoalsDocument::from_yaml(&yaml).is_err(), "accepted {field}");
        }
    }

    #[test]
    fn closed_children_do_not_override_never_run_truth() {
        let now = Utc.with_ymd_and_hms(2030, 1, 2, 3, 4, 5).unwrap();
        let check = resolved();
        let checks = BTreeMap::from([(
            "sweep-parity".to_owned(),
            EvaluatedCheck::from_contract(&check, &[], now),
        )]);
        let facts = derive_goal_facts(
            &goal(),
            &[MilestoneInput {
                id: "example-org/example-repo#118".to_owned(),
                epic: "example-org/example-repo#115".to_owned(),
                opened: now.to_rfc3339(),
                updated: now.to_rfc3339(),
                blocked_by: Vec::new(),
                open: false,
            }],
            &[],
            &checks,
        );
        assert_eq!(
            facts.progress,
            ProgressFact {
                complete: 1,
                total: 1
            }
        );
        assert_eq!(facts.met_when_status[0].state, CheckState::NeverRun);
        assert!(!facts.met);
    }

    #[test]
    fn faulted_pass_is_not_met() {
        let now = Utc.with_ymd_and_hms(2030, 1, 2, 3, 4, 5).unwrap();
        let check = resolved();
        let receipt = RunnerStamp {
            resolved: &check,
            attempt_id: "fixture-attempt",
            observed_at: now,
            completed_at: now,
        }
        .stamp(json!({"result_version": 1, "verdict": CheckVerdict::Pass}))
        .expect("receipt");
        let fault = RunnerStamp {
            resolved: &check,
            attempt_id: "fixture-fault",
            observed_at: now,
            completed_at: now,
        }
        .fault("runner_unavailable", None);
        let checks = BTreeMap::from([(
            "sweep-parity".to_owned(),
            EvaluatedCheck::from_contract(&check, &[receipt, fault], now),
        )]);
        assert!(!derive_goal_facts(&goal(), &[], &[], &checks).met);
    }

    #[test]
    fn assessment_requires_real_citations() {
        let facts = derive_goal_facts(&goal(), &[], &[], &BTreeMap::new());
        let uncited = AssessmentDraft {
            goal: "rust-cli".to_owned(),
            reading: Reading::OffTrack,
            because: Vec::new(),
        };
        assert_eq!(
            validate_assessment(&goal(), &facts, &[], uncited).unwrap_err(),
            AssessmentError::Uncited
        );
        let invented = AssessmentDraft {
            goal: "rust-cli".to_owned(),
            reading: Reading::OffTrack,
            because: vec![Because {
                fact: "issue.prose_sounds_urgent".to_owned(),
                detail: "invented".to_owned(),
            }],
        };
        assert_eq!(
            validate_assessment(&goal(), &facts, &[], invented).unwrap_err(),
            AssessmentError::InventedFact("issue.prose_sounds_urgent".to_owned())
        );
    }

    #[test]
    fn ranking_never_authorizes_tripwires_holds_or_deferred_work() {
        let items = vec![
            queue(
                "example-org/example-repo#1",
                "tripwire",
                "pending",
                "2030-01-01",
            ),
            queue(
                "example-org/example-repo#2",
                "parked",
                "pending",
                "2030-01-02",
            ),
            queue(
                "example-org/example-repo#3",
                "moved",
                "deferred",
                "2030-01-03",
            ),
            queue(
                "example-org/example-repo#4",
                "moved",
                "pending",
                "2030-01-04",
            ),
        ];
        assert_eq!(
            compose_ranking(
                &items,
                &[],
                &[
                    "example-org/example-repo#1".to_owned(),
                    "example-org/example-repo#2".to_owned(),
                    "example-org/example-repo#3".to_owned(),
                    "example-org/example-repo#4".to_owned(),
                ],
                &BTreeSet::new(),
            ),
            vec!["example-org/example-repo#4"]
        );
    }

    #[test]
    fn principal_ranking_wins_and_computed_only_orders_the_remainder() {
        let items = vec![
            queue(
                "example-org/example-repo#1",
                "moved",
                "pending",
                "2030-01-01",
            ),
            queue(
                "example-org/example-repo#2",
                "moved",
                "pending",
                "2030-01-02",
            ),
            queue(
                "example-org/example-repo#3",
                "moved",
                "pending",
                "2030-01-03",
            ),
        ];
        assert_eq!(
            compose_ranking(
                &items,
                &["example-org/example-repo#3".to_owned()],
                &["example-org/example-repo#2".to_owned()],
                &BTreeSet::new(),
            ),
            vec![
                "example-org/example-repo#3",
                "example-org/example-repo#2",
                "example-org/example-repo#1",
            ]
        );
    }

    #[test]
    fn milestones_follow_dependencies_and_slow_vs_stuck_is_mechanical() {
        let goal = goal();
        let milestones = vec![
            MilestoneInput {
                id: "example-org/example-repo#118".to_owned(),
                epic: "example-org/example-repo#115".to_owned(),
                opened: "2030-01-02T00:00:00Z".to_owned(),
                updated: "2030-01-03T00:00:00Z".to_owned(),
                blocked_by: Vec::new(),
                open: true,
            },
            MilestoneInput {
                id: "example-org/example-repo#119".to_owned(),
                epic: "example-org/example-repo#115".to_owned(),
                opened: "2030-01-01T00:00:00Z".to_owned(),
                updated: "2030-01-04T00:00:00Z".to_owned(),
                blocked_by: vec!["example-org/example-repo#118".to_owned()],
                open: true,
            },
        ];
        let slow_queue = vec![queue(
            "example-org/example-repo#118",
            "moved",
            "pending",
            "2030-01-02T00:00:00Z",
        )];
        let slow = derive_goal_facts(&goal, &milestones, &slow_queue, &BTreeMap::new());
        assert_eq!(
            slow.milestones
                .iter()
                .map(|milestone| milestone.id.as_str())
                .collect::<Vec<_>>(),
            vec![
                "example-org/example-repo#118",
                "example-org/example-repo#119"
            ]
        );
        assert_eq!(slow.next.as_deref(), Some("example-org/example-repo#118"));
        assert_eq!(
            consequence(&slow, &slow_queue),
            Consequence {
                promote: vec!["example-org/example-repo#118".to_owned()],
                escalate: Vec::new(),
            }
        );

        let stuck_queue = vec![queue(
            "example-org/example-repo#118",
            "parked",
            "pending",
            "2030-01-02T00:00:00Z",
        )];
        let stuck = derive_goal_facts(&goal, &milestones, &stuck_queue, &BTreeMap::new());
        assert_eq!(stuck.impediments[0].fact, "next.hold");
        assert_eq!(
            consequence(&stuck, &stuck_queue),
            Consequence {
                promote: Vec::new(),
                escalate: vec!["example-org/example-repo#118".to_owned()],
            }
        );
    }

    #[test]
    fn mechanical_ranking_retains_the_pre_plan_dependency_order() {
        let mut blocker = queue(
            "example-org/example-repo#4",
            "drift",
            "pending",
            "2030-01-04",
        );
        blocker.blocked_by = vec!["example-org/example-repo#2".to_owned()];
        let items = vec![
            queue(
                "example-org/example-repo#1",
                "moved",
                "pending",
                "2030-01-01",
            ),
            queue(
                "example-org/example-repo#2",
                "moved",
                "pending",
                "2030-01-02",
            ),
            queue(
                "example-org/example-repo#3",
                "moved",
                "pending",
                "2030-01-03",
            ),
            blocker,
        ];
        assert_eq!(
            mechanical_ranking(&items, &["example-org/example-repo#3".to_owned()]),
            vec![
                "example-org/example-repo#3",
                "example-org/example-repo#2",
                "example-org/example-repo#1",
            ]
        );
    }
}
