use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use chrono::{DateTime, Utc};
use ostrom_core::{
    Acknowledgement, Assessment, AssessmentDraft, CheckFault, CheckStoreFault, EvaluatedCheck,
    GoalActionVerb, GoalFacts, GoalState, GoalsDocument, GoalsError, MilestoneInput, PLAN_VERSION,
    QueueItem, ResolvedCheck, WorkGraph, WorkGraphNode, cited_fact_basis, compose_ranking,
    consequence, derive_goal_facts, fact_table, mechanical_ranking, validate_assessment,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tempfile::tempdir;
use thiserror::Error;

use crate::sweep::run_sweep_with_mirror;
use crate::{
    JsonlCheckStore, QueueDocument, RepositorySnapshot, StoreError, SweepError, SweepOptions,
    io_error, read_queue, set_private_file_mode,
};

const ASSESSMENTS_PER_PLAN: usize = 20;
const ASSESSMENT_COST_CEILING_USD: &str = "1";
const ASSESSMENT_TOKEN_CEILING: &str = "25000";
const ASSESSMENT_SCHEMA: &str = r#"{"type":"object","additionalProperties":false,"required":["goal","reading","because"],"properties":{"goal":{"type":"string"},"reading":{"enum":["on-track","at-risk","off-track","blocked"]},"because":{"type":"array","minItems":1,"items":{"type":"object","additionalProperties":false,"required":["fact","detail"],"properties":{"fact":{"type":"string"},"detail":{"type":"string","minLength":1}}}}}}"#;

pub trait AssessmentDeriver {
    fn derive(
        &mut self,
        input: &AssessmentInput,
    ) -> Result<AssessmentDraft, AssessmentDeriverError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssessmentDeriverError {
    name: &'static str,
    detail: String,
}

impl AssessmentDeriverError {
    fn new(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            detail: detail.into(),
        }
    }
}

pub struct ExecutableAssessmentDeriver {
    executable: PathBuf,
}

impl ExecutableAssessmentDeriver {
    #[must_use]
    pub fn new(executable: PathBuf) -> Self {
        Self { executable }
    }
}

impl AssessmentDeriver for ExecutableAssessmentDeriver {
    fn derive(
        &mut self,
        input: &AssessmentInput,
    ) -> Result<AssessmentDraft, AssessmentDeriverError> {
        let mut child = Command::new(&self.executable)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                AssessmentDeriverError::new(
                    "assessment_deriver_unavailable",
                    format!("could not start deriver: {error}"),
                )
            })?;
        let bytes = serde_json::to_vec(input).expect("assessment input serializes");
        child
            .stdin
            .take()
            .ok_or_else(|| {
                AssessmentDeriverError::new(
                    "assessment_deriver_failed",
                    "deriver stdin unavailable",
                )
            })?
            .write_all(&bytes)
            .map_err(|error| {
                AssessmentDeriverError::new(
                    "assessment_deriver_failed",
                    format!("could not write deriver input: {error}"),
                )
            })?;
        let output = child.wait_with_output().map_err(|error| {
            AssessmentDeriverError::new(
                "assessment_deriver_failed",
                format!("could not wait for deriver: {error}"),
            )
        })?;
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr);
            return Err(AssessmentDeriverError::new(
                "assessment_deriver_failed",
                format!("deriver exited with {}; {}", output.status, detail.trim()),
            ));
        }
        serde_json::from_slice(&output.stdout).map_err(|error| {
            AssessmentDeriverError::new(
                "assessment_invalid_output",
                format!("deriver output is invalid: {error}"),
            )
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssessmentHarness {
    Claude,
    Codex,
    Copilot,
}

impl AssessmentHarness {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Copilot => "copilot",
        }
    }
}

pub struct HarnessAssessmentDeriver {
    harness: AssessmentHarness,
    executable: PathBuf,
    attempts: usize,
}

impl HarnessAssessmentDeriver {
    #[must_use]
    pub fn new(harness: AssessmentHarness, executable: PathBuf) -> Self {
        Self {
            harness,
            executable,
            attempts: 0,
        }
    }

    fn command(&self, scratch: &Path) -> Result<Command, AssessmentDeriverError> {
        let mut command = Command::new(&self.executable);
        command.current_dir(scratch);
        match self.harness {
            AssessmentHarness::Claude => {
                command.args([
                    "--print",
                    "--output-format",
                    "json",
                    "--json-schema",
                    ASSESSMENT_SCHEMA,
                    "--permission-mode",
                    "dontAsk",
                    "--tools",
                    "",
                    "--disallowedTools",
                    "mcp__*",
                    "--max-turns",
                    "1",
                    "--max-budget-usd",
                    ASSESSMENT_COST_CEILING_USD,
                    "--no-session-persistence",
                ]);
                command.env("CLAUDE_CODE_MAX_OUTPUT_TOKENS", ASSESSMENT_TOKEN_CEILING);
            }
            AssessmentHarness::Codex => {
                let schema = scratch.join("assessment-schema.json");
                fs::write(&schema, ASSESSMENT_SCHEMA).map_err(|error| {
                    AssessmentDeriverError::new(
                        "assessment_harness_failed",
                        format!("could not prepare Codex output schema: {error}"),
                    )
                })?;
                command.args([
                    "exec",
                    "--disable",
                    "shell_tool",
                    "--disable",
                    "unified_exec",
                    "--disable",
                    "shell_snapshot",
                    "--ephemeral",
                    "--skip-git-repo-check",
                    "--ignore-user-config",
                    "--ignore-rules",
                    "--color",
                    "never",
                    "--sandbox",
                    "read-only",
                    "--config",
                    "approval_policy=\"never\"",
                    "--config",
                    "web_search=\"disabled\"",
                    "--config",
                    "model_reasoning_effort=\"low\"",
                    "--config",
                    "model_context_window=25000",
                    "--config",
                    "model_auto_compact_token_limit=25000",
                    "--output-schema",
                ]);
                command.arg(schema);
            }
            AssessmentHarness::Copilot => {
                command.args([
                    "--silent",
                    "--no-ask-user",
                    "--available-tools=",
                    "--disable-builtin-mcps",
                    "--no-custom-instructions",
                    "--no-auto-update",
                    "--no-remote",
                    "--no-remote-export",
                    "--max-ai-credits=1",
                    "--stream=off",
                ]);
            }
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Ok(command)
    }
}

impl AssessmentDeriver for HarnessAssessmentDeriver {
    fn derive(
        &mut self,
        input: &AssessmentInput,
    ) -> Result<AssessmentDraft, AssessmentDeriverError> {
        if self.attempts == ASSESSMENTS_PER_PLAN {
            return Err(AssessmentDeriverError::new(
                "assessment_budget_exhausted",
                format!(
                    "{} assessor reached the {ASSESSMENTS_PER_PLAN}-goal plan ceiling",
                    self.harness.name()
                ),
            ));
        }
        self.attempts += 1;
        let encoded = serde_json::to_string(input).expect("assessment input serializes");
        let prompt = format!(
            "Assess exactly one authored goal using only the supplied JSON. Do not use tools or seek other context. The only allowed readings are on-track, at-risk, off-track, and blocked. Return only an object matching this schema: {ASSESSMENT_SCHEMA}. Every because.fact must exactly name a key in input.facts, because must contain at least one citation, and because.detail must explain how that fact supports the reading. Do not invent, default, synthesize, or repair citations. Input: {encoded}"
        );
        let scratch = tempdir().map_err(|error| {
            AssessmentDeriverError::new(
                "assessment_harness_failed",
                format!("could not create isolated assessor directory: {error}"),
            )
        })?;
        let mut child = self.command(scratch.path())?.spawn().map_err(|error| {
            AssessmentDeriverError::new(
                "assessment_harness_unavailable",
                format!(
                    "{} assessor is configured but {} could not be started: {error}",
                    self.harness.name(),
                    self.executable.display()
                ),
            )
        })?;
        child
            .stdin
            .take()
            .ok_or_else(|| {
                AssessmentDeriverError::new(
                    "assessment_harness_failed",
                    format!("{} assessor stdin is unavailable", self.harness.name()),
                )
            })?
            .write_all(prompt.as_bytes())
            .map_err(|error| {
                AssessmentDeriverError::new(
                    "assessment_harness_failed",
                    format!(
                        "could not write {} assessor input: {error}",
                        self.harness.name()
                    ),
                )
            })?;
        let output = child.wait_with_output().map_err(|error| {
            AssessmentDeriverError::new(
                "assessment_harness_failed",
                format!(
                    "could not wait for {} assessor: {error}",
                    self.harness.name()
                ),
            )
        })?;
        if !output.status.success() {
            return Err(AssessmentDeriverError::new(
                "assessment_harness_failed",
                format!(
                    "{} assessor exited with {}; {}",
                    self.harness.name(),
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            ));
        }
        decode_harness_output(self.harness, &output.stdout)
    }
}

fn decode_harness_output(
    harness: AssessmentHarness,
    output: &[u8],
) -> Result<AssessmentDraft, AssessmentDeriverError> {
    if let Ok(draft) = serde_json::from_slice(output) {
        return Ok(draft);
    }
    if harness == AssessmentHarness::Claude {
        let envelope: Value = serde_json::from_slice(output).map_err(|error| {
            AssessmentDeriverError::new(
                "assessment_invalid_output",
                format!("claude assessor output is invalid: {error}"),
            )
        })?;
        return serde_json::from_value(envelope["structured_output"].clone()).map_err(|error| {
            AssessmentDeriverError::new(
                "assessment_invalid_output",
                format!("claude assessor structured output is invalid: {error}"),
            )
        });
    }
    Err(AssessmentDeriverError::new(
        "assessment_invalid_output",
        format!(
            "{} assessor output does not match the assessment contract",
            harness.name()
        ),
    ))
}

pub struct UnavailableAssessmentDeriver;

impl AssessmentDeriver for UnavailableAssessmentDeriver {
    fn derive(
        &mut self,
        _input: &AssessmentInput,
    ) -> Result<AssessmentDraft, AssessmentDeriverError> {
        Err(AssessmentDeriverError::new(
            "assessment_unavailable",
            "no semantic assessment deriver is configured",
        ))
    }
}

#[derive(Debug, Clone)]
pub struct PlanOptions {
    pub sweep: SweepOptions,
    pub resolved_checks: BTreeMap<String, ResolvedCheck>,
    pub check_resolution_faults: BTreeMap<String, CheckFault>,
    pub catalogue_fault: Option<CheckFault>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AssessmentInput {
    pub goal: String,
    pub intent: String,
    pub facts: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanDocument {
    pub plan_version: u32,
    pub generated_at: DateTime<Utc>,
    pub sweep: PlanSweep,
    pub goals: Vec<GoalPlan>,
    pub ranking: PlanRanking,
    pub queue_basis: Vec<QueueItem>,
    pub faults: Vec<PlanFault>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanSweep {
    pub projects: usize,
    pub queue_changes: usize,
    pub mode: String,
    pub check_runs: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalPlan {
    pub id: String,
    pub intent: String,
    pub authored_state: GoalState,
    pub facts: GoalFacts,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assessment: Option<Assessment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanRanking {
    pub work_ranking: Vec<String>,
    pub computed: Vec<String>,
    pub ordered: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanFault {
    pub stage: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    pub name: String,
    pub detail: String,
}

#[derive(Debug, Error)]
pub enum PlanError {
    #[error(transparent)]
    Sweep(#[from] SweepError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("could not mirror check receipts: {0}")]
    Checks(#[from] CheckStoreFault),
    #[error("could not read goals: {0}")]
    GoalsRead(String),
    #[error(transparent)]
    Goals(#[from] GoalsError),
    #[error("could not read plan input: {0}")]
    Input(String),
    #[error("could not read acknowledgement state: {0}")]
    Acknowledgements(String),
    #[error("could not write plan: {0}")]
    Write(String),
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AcknowledgementLedger {
    #[serde(default)]
    entries: BTreeMap<String, BTreeMap<String, Value>>,
}

pub fn run_plan(
    options: &PlanOptions,
    deriver: &mut dyn AssessmentDeriver,
) -> Result<PlanDocument, PlanError> {
    let goals = load_goals(
        &options.sweep.paths.config,
        &options.sweep.working_directory,
    )?;
    let (outcome, mirror) = run_sweep_with_mirror(&options.sweep)?;
    let mut milestone_input = load_milestones(&mirror);
    let (check_runs, check_mirror_fault) =
        match JsonlCheckStore::new(&options.sweep.paths).snapshot() {
            Ok(runs) => (runs, None),
            Err(error) => (Vec::new(), Some(error.to_string())),
        };
    let receipts = check_runs
        .iter()
        .flat_map(|run| run.receipts.iter().cloned())
        .collect::<Vec<_>>();
    let mut evaluated_checks = options
        .resolved_checks
        .values()
        .map(|check| {
            (
                check.id.clone(),
                EvaluatedCheck::from_contract(check, &receipts, options.sweep.started_at),
            )
        })
        .collect::<BTreeMap<_, _>>();
    evaluated_checks.extend(options.check_resolution_faults.iter().map(|(id, fault)| {
        (
            id.clone(),
            EvaluatedCheck::from_resolution_fault(fault.clone()),
        )
    }));
    if let Some(fault) = &options.catalogue_fault {
        for check in goals.goals.iter().flat_map(|goal| &goal.met_when) {
            evaluated_checks
                .entry(check.clone())
                .or_insert_with(|| EvaluatedCheck::from_resolution_fault(fault.clone()));
        }
    }
    let queue_documents = read_queue(&options.sweep.paths.queue_file())?;
    let dependency_graph = read_dependency_graph(&options.sweep.paths.state.join("state.json"))?;
    let graph_nodes = dependency_graph
        .as_ref()
        .into_iter()
        .flat_map(|graph| graph.nodes.iter())
        .map(|node| (node.id.as_str(), node))
        .collect::<BTreeMap<_, _>>();
    let queue = queue_documents
        .iter()
        .map(|document| {
            queue_item(
                document,
                graph_nodes
                    .get(document.value()["id"].as_str().unwrap_or_default())
                    .copied(),
                dependency_graph.is_some(),
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    for milestone in &mut milestone_input {
        if let Some(item) = queue.iter().find(|item| item.id == milestone.id) {
            milestone.blocked_by.clone_from(&item.blocked_by);
        }
    }
    let principal = read_work_ranking(&options.sweep.paths.state.join("state.json"))?;
    let ledger_path = options.sweep.paths.state.join("plan-acknowledgements.json");
    let mut ledger = read_ledger(&ledger_path)?;
    let mut faults = outcome
        .faults
        .iter()
        .map(|detail| PlanFault {
            stage: "mirror".to_owned(),
            goal: None,
            name: "sweep_fault".to_owned(),
            detail: detail.clone(),
        })
        .collect::<Vec<_>>();
    if let Some(detail) = check_mirror_fault {
        faults.push(PlanFault {
            stage: "mirror".to_owned(),
            goal: None,
            name: "check_store_fault".to_owned(),
            detail,
        });
    }
    if let Some(graph) = &dependency_graph {
        faults.extend(graph.faults.iter().map(|fault| PlanFault {
            stage: "graph".to_owned(),
            goal: None,
            name: fault.name.clone(),
            detail: fault.nodes.join(", "),
        }));
    }
    let mut plans = Vec::new();
    let mut computed = Vec::new();
    let mut demoted = BTreeSet::new();
    let today = options.sweep.started_at.date_naive();

    for goal in &goals.goals {
        let facts = derive_goal_facts(goal, &milestone_input, &queue, &evaluated_checks);
        for status in &facts.met_when_status {
            if let Some(fault) = &status.fault {
                faults.push(PlanFault {
                    stage: "evaluate".to_owned(),
                    goal: Some(goal.id.clone()),
                    name: fault.name.clone(),
                    detail: format!("{} is not admissible", status.check),
                });
            }
        }
        let action = goals.actions.iter().rev().find(|action| {
            action.goal == goal.id && action.until.is_none_or(|until| until >= today)
        });
        let paused = goal.state != GoalState::Active
            || action.is_some_and(|action| action.verb == GoalActionVerb::Pause);
        if let Some(next) = &facts.next {
            if action.is_some_and(|action| action.verb == GoalActionVerb::Promote)
                && queue
                    .iter()
                    .find(|item| &item.id == next)
                    .is_some_and(QueueItem::dispatchable)
            {
                computed.push(next.clone());
            } else if action.is_some_and(|action| action.verb == GoalActionVerb::Demote) {
                demoted.insert(next.clone());
            }
        }
        let assessment = if facts.met || paused {
            None
        } else {
            let input = AssessmentInput {
                goal: goal.id.clone(),
                intent: goal.intent.clone(),
                facts: fact_table(&facts, &queue),
            };
            match deriver.derive(&input).and_then(|draft| {
                validate_assessment(goal, &facts, &queue, draft).map_err(|error| {
                    AssessmentDeriverError::new("assessment_invalid_output", error.to_string())
                })
            }) {
                Ok(draft) => {
                    let mut consequence = consequence(&facts, &queue);
                    if action.is_some_and(|action| action.verb == GoalActionVerb::Promote) {
                        if let Some(next) = &facts.next {
                            if queue
                                .iter()
                                .find(|item| &item.id == next)
                                .is_some_and(QueueItem::dispatchable)
                                && !consequence.promote.contains(next)
                            {
                                consequence.promote.push(next.clone());
                            }
                        }
                    }
                    if action.is_some_and(|action| action.verb == GoalActionVerb::Demote) {
                        if let Some(next) = &facts.next {
                            demoted.insert(next.clone());
                            consequence.promote.retain(|item| item != next);
                        }
                    }
                    let basis = cited_fact_basis(&draft, &facts, &queue);
                    let suppressed = matching_acknowledgement(
                        &goals.acknowledgements,
                        &draft,
                        today,
                        &basis,
                        &mut ledger,
                    );
                    if suppressed {
                        consequence.escalate.clear();
                    }
                    computed.extend(consequence.promote.iter().cloned());
                    Some(Assessment {
                        goal: draft.goal,
                        reading: draft.reading,
                        because: draft.because,
                        consequence,
                        escalation_suppressed: suppressed,
                    })
                }
                Err(error) => {
                    faults.push(PlanFault {
                        stage: "assess".to_owned(),
                        goal: Some(goal.id.clone()),
                        name: error.name.to_owned(),
                        detail: error.detail,
                    });
                    None
                }
            }
        };
        plans.push(GoalPlan {
            id: goal.id.clone(),
            intent: goal.intent.clone(),
            authored_state: goal.state,
            facts,
            assessment,
        });
    }

    let mut seen = BTreeSet::new();
    computed.retain(|item| !demoted.contains(item) && seen.insert(item.clone()));
    let ordered = if computed.is_empty() && demoted.is_empty() {
        mechanical_ranking(&queue, &principal)
    } else {
        compose_ranking(&queue, &principal, &computed, &demoted)
    };
    let document = PlanDocument {
        plan_version: PLAN_VERSION,
        generated_at: options.sweep.started_at,
        sweep: PlanSweep {
            projects: outcome.project_count,
            queue_changes: outcome.queue_changes,
            mode: format!("{:?}", outcome.mode).to_lowercase(),
            check_runs: check_runs.len(),
        },
        goals: plans,
        ranking: PlanRanking {
            work_ranking: principal,
            computed,
            ordered,
        },
        queue_basis: queue,
        faults,
    };
    write_json_private(&options.sweep.paths.state.join("plan.json"), &document)?;
    write_json_private(&ledger_path, &ledger)?;
    Ok(document)
}

fn load_goals(config_root: &Path, cwd: &Path) -> Result<GoalsDocument, PlanError> {
    let user = config_root.join("goals.yaml");
    let repository = cwd.join(".ostrom/goals.yaml");
    let path = if repository.exists() {
        Some(repository)
    } else if user.exists() {
        Some(user)
    } else {
        None
    };
    let Some(path) = path else {
        return Ok(GoalsDocument {
            goals_version: 1,
            goals: Vec::new(),
            actions: Vec::new(),
            acknowledgements: Vec::new(),
        });
    };
    let text = fs::read_to_string(&path)
        .map_err(|error| PlanError::GoalsRead(format!("{}: {error}", path.display())))?;
    GoalsDocument::from_yaml(&text).map_err(PlanError::Goals)
}

fn load_milestones(snapshots: &[RepositorySnapshot]) -> Vec<MilestoneInput> {
    let mut milestones = Vec::new();
    for repository in snapshots {
        let repo = repository.repo.as_str();
        let items = repository.issues.iter().chain(&repository.open_prs);
        for item in items {
            let Some(number) = item["number"].as_u64() else {
                continue;
            };
            let id = format!("{repo}#{number}");
            if let Some(epic) = parent_epic(repo, item) {
                milestones.push(MilestoneInput {
                    id: id.clone(),
                    epic,
                    opened: string_field(item, &["createdAt", "created_at"]).to_owned(),
                    updated: string_field(item, &["updatedAt", "updated_at"]).to_owned(),
                    blocked_by: dependency_array(item),
                    open: !string_field(item, &["state"]).eq_ignore_ascii_case("closed"),
                });
            }
            for child in item["subIssues"].as_array().into_iter().flatten() {
                if let Some(child_number) = child["number"].as_u64() {
                    milestones.push(MilestoneInput {
                        id: format!("{repo}#{child_number}"),
                        epic: id.clone(),
                        opened: string_field(child, &["createdAt", "created_at"]).to_owned(),
                        updated: string_field(child, &["updatedAt", "updated_at"]).to_owned(),
                        blocked_by: dependency_array(child),
                        open: !string_field(child, &["state"]).eq_ignore_ascii_case("closed"),
                    });
                }
            }
        }
    }
    milestones.sort_by(|left, right| left.id.cmp(&right.id));
    milestones.dedup_by(|left, right| left.id == right.id && left.epic == right.epic);
    milestones
}

fn parent_epic(repo: &str, item: &Value) -> Option<String> {
    if let Some(epic) = item["epic"].as_str() {
        return Some(if epic.starts_with('#') {
            format!("{repo}{epic}")
        } else {
            epic.to_owned()
        });
    }
    for field in ["parent", "parentIssue", "parent_issue"] {
        if let Some(number) = item[field]["number"].as_u64() {
            return Some(format!("{repo}#{number}"));
        }
    }
    item["parent_issue_url"]
        .as_str()
        .and_then(|url| url.rsplit('/').next())
        .and_then(|number| number.parse::<u64>().ok())
        .map(|number| format!("{repo}#{number}"))
}

fn dependency_array(item: &Value) -> Vec<String> {
    item["blocked_by"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

fn queue_item(
    document: &QueueDocument,
    graph: Option<&WorkGraphNode>,
    graph_required: bool,
) -> Result<QueueItem, PlanError> {
    let value = document.value();
    let field = |name: &str| {
        value[name]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| PlanError::Input(format!("queue item has no {name}")))
    };
    Ok(QueueItem {
        id: field("id")?,
        opened: field("opened")?,
        kind: field("kind")?,
        state: field("state")?,
        item_type: value
            .get("item_type")
            .and_then(Value::as_str)
            .map(str::to_owned),
        blocked_by: value["blocked_by"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        graph_dispatchable: graph.map_or(!graph_required, |node| node.dispatchable),
        unblocking_power: graph.map_or(0, |node| node.unblocking_power),
    })
}

fn read_dependency_graph(path: &Path) -> Result<Option<WorkGraph>, PlanError> {
    let value: Value = serde_json::from_slice(
        &fs::read(path).map_err(|error| PlanError::Input(error.to_string()))?,
    )
    .map_err(|error| PlanError::Input(error.to_string()))?;
    value
        .get("dependency_graph")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| PlanError::Input(format!("dependency graph is malformed: {error}")))
}

fn read_work_ranking(path: &Path) -> Result<Vec<String>, PlanError> {
    let value: Value = serde_json::from_slice(
        &fs::read(path).map_err(|error| PlanError::Input(error.to_string()))?,
    )
    .map_err(|error| PlanError::Input(error.to_string()))?;
    Ok(value["work_ranking"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect())
}

fn matching_acknowledgement(
    acknowledgements: &[Acknowledgement],
    assessment: &AssessmentDraft,
    today: chrono::NaiveDate,
    basis: &BTreeMap<String, Value>,
    ledger: &mut AcknowledgementLedger,
) -> bool {
    let Some(acknowledgement) = acknowledgements.iter().find(|acknowledgement| {
        acknowledgement.goal == assessment.goal
            && acknowledgement.reading == assessment.reading
            && acknowledgement.until.is_none_or(|until| until >= today)
    }) else {
        return false;
    };
    let key = acknowledgement_key(acknowledgement);
    match ledger.entries.get(&key) {
        Some(previous) => previous == basis,
        None => {
            ledger.entries.insert(key, basis.clone());
            true
        }
    }
}

fn acknowledgement_key(acknowledgement: &Acknowledgement) -> String {
    format!(
        "{}|{:?}|{:?}|{}",
        acknowledgement.goal,
        acknowledgement.reading,
        acknowledgement.response,
        acknowledgement
            .until
            .map_or_else(String::new, |until| until.to_string())
    )
}

fn read_ledger(path: &Path) -> Result<AcknowledgementLedger, PlanError> {
    if !path.exists() {
        return Ok(AcknowledgementLedger::default());
    }
    serde_json::from_slice(
        &fs::read(path).map_err(|error| PlanError::Acknowledgements(error.to_string()))?,
    )
    .map_err(|error| PlanError::Acknowledgements(error.to_string()))
}

fn write_json_private(path: &Path, value: &impl Serialize) -> Result<(), PlanError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| PlanError::Write(error.to_string()))?;
    let temporary = path.with_extension(format!("tmp.{}", std::process::id()));
    let bytes = serde_json::to_vec_pretty(value).expect("plan value serializes");
    fs::write(&temporary, bytes).map_err(|error| io_error("write plan", &temporary, error))?;
    set_private_file_mode(&temporary)?;
    fs::rename(&temporary, path).map_err(|error| io_error("install plan", path, error))?;
    Ok(())
}

fn string_field<'a>(value: &'a Value, fields: &[&str]) -> &'a str {
    fields
        .iter()
        .find_map(|field| value.get(field).and_then(Value::as_str))
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::{TimeZone, Utc};
    use ostrom_core::{
        ActionDefinition, Because, CHECK_STORE_SCHEMA_VERSION, Catalogue, CatalogueEnumeration,
        CheckDocument, CheckRun, CheckRunId, CheckStore, CheckVerdict, Reading, RunnerStamp,
        resolve_check,
    };
    use serde_json::json;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn acknowledgement_only_suppresses_unchanged_cited_facts() {
        let acknowledgement = Acknowledgement {
            goal: "rust-cli".to_owned(),
            reading: Reading::OffTrack,
            response: ostrom_core::AcknowledgementResponse::Accepted,
            until: None,
        };
        let assessment = AssessmentDraft {
            goal: "rust-cli".to_owned(),
            reading: Reading::OffTrack,
            because: vec![Because {
                fact: "met_when.never_run".to_owned(),
                detail: "no receipt".to_owned(),
            }],
        };
        let today = chrono::NaiveDate::from_ymd_opt(2030, 1, 2).unwrap();
        let first = BTreeMap::from([("met_when.never_run".to_owned(), json!("check-a"))]);
        let changed = BTreeMap::from([("met_when.never_run".to_owned(), json!("check-b"))]);
        let mut ledger = AcknowledgementLedger::default();
        assert!(matching_acknowledgement(
            std::slice::from_ref(&acknowledgement),
            &assessment,
            today,
            &first,
            &mut ledger
        ));
        let directory = tempfile::tempdir().expect("ledger directory");
        let path = directory.path().join("plan-acknowledgements.json");
        write_json_private(&path, &ledger).expect("persist ledger");
        let mut ledger = read_ledger(&path).expect("reload ledger");
        assert!(matching_acknowledgement(
            std::slice::from_ref(&acknowledgement),
            &assessment,
            today,
            &first,
            &mut ledger
        ));
        assert!(!matching_acknowledgement(
            &[acknowledgement],
            &assessment,
            today,
            &changed,
            &mut ledger
        ));
    }

    #[tokio::test]
    async fn plan_mirrors_receipts_and_evaluates_them_through_the_check_contract() {
        let fixture = tempdir().expect("plan fixture");
        let paths = crate::OstromPaths {
            config: fixture.path().to_path_buf(),
            state: fixture.path().to_path_buf(),
        };
        fs::write(
            fixture.path().join("mandates.yaml"),
            "provider: file\ncadence_hours: 1\nstuck_after_days: 7\nprojects:\n  - repo: example-org/example-repo\n    default: delegated\n",
        )
        .expect("mandates");
        fs::write(
            fixture.path().join("goals.yaml"),
            "goals_version: 1\ngoals:\n  - id: rust-cli\n    intent: ostrom runs\n    state: active\n    met_when: [sweep-parity]\n",
        )
        .expect("goals");
        let sweep_fixture = fixture.path().join("sweep.json");
        fs::write(
            &sweep_fixture,
            serde_json::to_vec(&json!({"repositories": [{"repo": "example-org/example-repo"}]}))
                .expect("fixture JSON"),
        )
        .expect("sweep fixture");
        let document = CheckDocument::from_yaml(
            "checks_version: 1\nchecks:\n  sweep-parity:\n    uses: fixture/observe\n    with: {}\n",
        )
        .expect("check document");
        let resolved = resolve_check(
            "sweep-parity",
            &CatalogueEnumeration {
                catalogues: vec![Catalogue { document }],
                complete: true,
            },
            &ActionDefinition {
                uses: "fixture/observe".to_owned(),
                producer: "fixture".to_owned(),
                default_fresh_for_seconds: 300,
                definition: json!({}),
                source_revision: "fixture-revision".to_owned(),
            },
        )
        .expect("resolve check");
        let now = Utc.with_ymd_and_hms(2030, 1, 2, 3, 4, 5).unwrap();
        let receipt = RunnerStamp {
            resolved: &resolved,
            attempt_id: "fixture-attempt",
            observed_at: now,
            completed_at: now,
        }
        .stamp(json!({"result_version": 1, "verdict": CheckVerdict::Pass}))
        .expect("pass receipt");
        JsonlCheckStore::new(&paths)
            .write_run(&CheckRun {
                schema_version: CHECK_STORE_SCHEMA_VERSION,
                run_id: CheckRunId("fixture-run".to_owned()),
                completed_at: now.to_rfc3339(),
                receipts: vec![receipt],
            })
            .await
            .expect("write check run");
        let options = PlanOptions {
            sweep: SweepOptions {
                paths,
                working_directory: fixture.path().to_path_buf(),
                executable: std::env::current_exe().expect("test executable"),
                plugin_root: fixture.path().to_path_buf(),
                started_at: now,
                requested_mode: crate::SweepMode::Full,
                fixture: Some(sweep_fixture),
                publish: crate::PublishTarget::Disabled,
            },
            resolved_checks: BTreeMap::from([("sweep-parity".to_owned(), resolved)]),
            check_resolution_faults: BTreeMap::new(),
            catalogue_fault: None,
        };
        let plan = run_plan(&options, &mut UnavailableAssessmentDeriver).expect("run plan");
        assert_eq!(plan.sweep.check_runs, 1);
        assert!(plan.goals[0].facts.met);
        assert_eq!(
            plan.goals[0].facts.met_when_status[0].state,
            ostrom_core::CheckState::Passing
        );
        assert!(plan.goals[0].assessment.is_none());
    }
}
