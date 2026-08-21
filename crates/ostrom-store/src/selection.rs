use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use ostrom_core::{MandateConfig, QueueItem, WorkEdgeSource, WorkGraph, mechanical_ranking};
use serde_json::{Map, Value, json};
use thiserror::Error;

use crate::{
    Clock, OstromPaths, StoreError, SweepError, TraceAppend, append_trace, load_config_or_defaults,
    read_queue,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectAction {
    List,
    Select {
        owner: String,
        attempted: BTreeSet<String>,
    },
}

#[derive(Debug, Clone)]
pub struct SelectRequest {
    pub paths: OstromPaths,
    pub working_directory: PathBuf,
    pub action: SelectAction,
    pub clock: Clock,
}

/// Empty is a successful, known result. Every inability to establish that
/// fact travels through `SelectError`, so callers cannot turn a read fault
/// into the same value as a quiet portfolio.
#[derive(Debug, Clone, PartialEq)]
pub enum SelectOutcome {
    Items(Vec<Value>),
    Selected(Value),
    Empty,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanApplication {
    Absent,
    Applied,
    Rejected,
}

#[must_use]
pub fn encode_selection(outcome: &SelectOutcome) -> Vec<u8> {
    let mut output = Vec::new();
    match outcome {
        SelectOutcome::Items(rows) => {
            for row in rows {
                serde_json::to_writer(&mut output, row).expect("selection JSON serializes");
                output.push(b'\n');
            }
        }
        SelectOutcome::Selected(row) => {
            serde_json::to_writer(&mut output, row).expect("selection JSON serializes");
            output.push(b'\n');
        }
        SelectOutcome::Empty => {}
    }
    output
}

#[derive(Debug, Error)]
pub enum SelectError {
    #[error("mandate selection: could not load mandate config: {0}")]
    Config(#[from] SweepError),
    #[error("mandate selection: could not read queue: {0}")]
    Queue(#[from] StoreError),
    #[error(
        "mandate selection: dependency graph has no sweep state; run sweep before selecting work"
    )]
    MissingState,
    #[error("mandate selection: cannot read {path}")]
    StateRead { path: String },
    #[error(
        "mandate selection: dependency graph is absent, stale, or invalid; run sweep before selecting work"
    )]
    InvalidGraph,
    #[error(
        "mandate selection: active work_ranking differs from the last sweep; run sweep before selecting work"
    )]
    RankingMismatch,
    #[error("mandate selection: stale work_ranking item {0} no longer exists")]
    StaleRanking(String),
    #[error("mandate selection: could not read plan.json")]
    PlanRead,
    #[error("mandate selection: could not write selection trace: {0}")]
    Trace(StoreError),
}

pub fn run_selection(request: &SelectRequest) -> Result<(SelectOutcome, Vec<String>), SelectError> {
    let config = load_config_or_defaults(&request.paths, &request.working_directory)?;
    let documents = read_queue(&request.paths.queue_file())?;
    let queue = documents
        .iter()
        .map(|document| document.value().clone())
        .collect::<Vec<_>>();
    let state_path = request.paths.state.join("state.json");
    if !state_path.exists() || fs::metadata(&state_path).is_ok_and(|metadata| metadata.len() == 0) {
        return Err(SelectError::MissingState);
    }
    let state: Value =
        serde_json::from_slice(&fs::read(&state_path).map_err(|_| SelectError::StateRead {
            path: state_path.display().to_string(),
        })?)
        .map_err(|_| SelectError::StateRead {
            path: state_path.display().to_string(),
        })?;
    let graph = validate_graph(&state, &config, &queue)?;
    validate_ranking(&state, &config)?;

    let graph_by_id = graph
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect::<BTreeMap<_, _>>();
    let queue_items = queue
        .iter()
        .map(|row| queue_item(row, &graph_by_id))
        .collect::<Result<Vec<_>, _>>()?;
    let authorized = queue_items
        .iter()
        .enumerate()
        .filter(|(_, item)| authorized(item))
        .collect::<Vec<_>>();
    let candidates = authorized
        .iter()
        .filter(|(_, item)| item.graph_dispatchable)
        .map(|(index, _)| *index)
        .collect::<Vec<_>>();

    let mut diagnostics = graph
        .faults
        .iter()
        .map(|fault| {
            format!(
                "mandate selection: dependency graph fault {}: {}",
                fault.name,
                fault.nodes.join(", ")
            )
        })
        .collect::<Vec<_>>();
    let plan_path = request.paths.state.join("plan.json");
    let (plan_application, plan_order, rejection_clause) =
        evaluate_plan(&plan_path, &config, &queue_items, &candidates)?;
    if plan_application == PlanApplication::Rejected {
        let clause = rejection_clause
            .as_deref()
            .expect("rejected plans name a rejection clause");
        diagnostics.push(format!(
            "mandate selection: plan.json rejected ({clause}); using mechanical ranking"
        ));
    }

    let candidate_items = candidates
        .iter()
        .map(|index| queue_items[*index].clone())
        .collect::<Vec<_>>();
    let order = if plan_application == PlanApplication::Applied {
        ranked_with_plan(&candidate_items, &config.work_ranking, &plan_order)
    } else {
        mechanical_ranking(&candidate_items, &config.work_ranking)
    };
    let by_id = queue
        .iter()
        .filter_map(|row| row.get("id").and_then(Value::as_str).map(|id| (id, row)))
        .collect::<BTreeMap<_, _>>();
    let ordered = order
        .iter()
        .filter_map(|id| by_id.get(id.as_str()).map(|row| (*row).clone()))
        .collect::<Vec<_>>();

    match &request.action {
        SelectAction::List => {
            let mut fact = Map::new();
            fact.insert("action".to_owned(), json!("list"));
            append_plan_selection_trace(
                &request.paths.trace_file(),
                &selection_trace_timestamp(),
                fact,
                plan_application,
                rejection_clause.as_deref(),
            )?;
            let outcome = if ordered.is_empty() {
                SelectOutcome::Empty
            } else {
                SelectOutcome::Items(ordered)
            };
            Ok((outcome, diagnostics))
        }
        SelectAction::Select { owner, attempted } => {
            let Some(selected) = ordered
                .iter()
                .find(|row| row["id"].as_str().is_some_and(|id| !attempted.contains(id)))
            else {
                let mut fact = Map::new();
                fact.insert("owner".to_owned(), json!(owner));
                fact.insert("action".to_owned(), json!("select"));
                fact.insert("outcome".to_owned(), json!("empty"));
                append_plan_selection_trace(
                    &request.paths.trace_file(),
                    &selection_trace_timestamp(),
                    fact,
                    plan_application,
                    rejection_clause.as_deref(),
                )?;
                return Ok((SelectOutcome::Empty, diagnostics));
            };
            append_selection_traces(
                request,
                owner,
                attempted,
                &queue,
                &queue_items,
                &authorized,
                &candidates,
                &ordered,
                selected,
                &graph,
                &config.work_ranking,
                plan_application,
                &plan_order,
                rejection_clause.as_deref(),
            )?;
            Ok((SelectOutcome::Selected(selected.clone()), diagnostics))
        }
    }
}

fn validate_graph(
    state: &Value,
    config: &MandateConfig,
    queue: &[Value],
) -> Result<WorkGraph, SelectError> {
    let graph: WorkGraph = state
        .get("dependency_graph")
        .cloned()
        .ok_or(SelectError::InvalidGraph)
        .and_then(|value| serde_json::from_value(value).map_err(|_| SelectError::InvalidGraph))?;
    let mut configured = config
        .projects
        .iter()
        .map(|project| project.repo.as_str().to_owned())
        .collect::<Vec<_>>();
    configured.sort();
    if graph.graph_version != 1 || graph.configured_repositories != configured {
        return Err(SelectError::InvalidGraph);
    }
    let nodes = graph
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect::<BTreeMap<_, _>>();
    for row in queue {
        let id = row["id"].as_str().ok_or(SelectError::InvalidGraph)?;
        if !nodes.contains_key(id) {
            return Err(SelectError::InvalidGraph);
        }
        let mut row_dependencies = row["blocked_by"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();
        let mut edge_dependencies = graph
            .edges
            .iter()
            .filter(|edge| edge.item == id && edge.sources.contains(&WorkEdgeSource::Body))
            .map(|edge| edge.dependency.as_str())
            .collect::<Vec<_>>();
        row_dependencies.sort_unstable();
        edge_dependencies.sort_unstable();
        if row_dependencies != edge_dependencies {
            return Err(SelectError::InvalidGraph);
        }
    }
    Ok(graph)
}

fn validate_ranking(state: &Value, config: &MandateConfig) -> Result<(), SelectError> {
    if config.work_ranking.is_empty() {
        return Ok(());
    }
    if state.get("work_ranking") != Some(&json!(config.work_ranking)) {
        return Err(SelectError::RankingMismatch);
    }
    if let Some(faults) = state.get("work_ranking_faults").and_then(Value::as_array) {
        if let Some(item) = faults.first().and_then(Value::as_str) {
            return Err(SelectError::StaleRanking(item.to_owned()));
        }
    }
    Ok(())
}

fn queue_item(
    row: &Value,
    graph: &BTreeMap<&str, &ostrom_core::WorkGraphNode>,
) -> Result<QueueItem, SelectError> {
    let string = |field: &str| {
        row.get(field)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or(SelectError::InvalidGraph)
    };
    let id = string("id")?;
    let node = graph.get(id.as_str()).ok_or(SelectError::InvalidGraph)?;
    Ok(QueueItem {
        id,
        opened: string("opened")?,
        kind: string("kind")?,
        state: string("state")?,
        blocked_by: row["blocked_by"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        graph_dispatchable: node.dispatchable,
        unblocking_power: node.unblocking_power,
    })
}

fn authorized(item: &QueueItem) -> bool {
    item.kind != "parked"
        && item.state != "deferred"
        && (matches!(item.kind.as_str(), "moved" | "stuck")
            || (item.state == "approved" && matches!(item.kind.as_str(), "tripwire" | "decision")))
}

fn evaluate_plan(
    path: &Path,
    config: &MandateConfig,
    queue: &[QueueItem],
    candidates: &[usize],
) -> Result<(PlanApplication, Vec<String>, Option<String>), SelectError> {
    if !path.exists() || fs::metadata(path).is_ok_and(|metadata| metadata.len() == 0) {
        return Ok((PlanApplication::Absent, Vec::new(), None));
    }
    let bytes = fs::read(path).map_err(|_| SelectError::PlanRead)?;
    let Ok(plan) = serde_json::from_slice::<Value>(&bytes) else {
        return Ok((
            PlanApplication::Rejected,
            Vec::new(),
            Some("malformed_json".to_owned()),
        ));
    };
    let basis = serde_json::to_value(queue).expect("queue basis serializes");
    let candidate_ids = candidates
        .iter()
        .map(|index| queue[*index].id.clone())
        .collect::<Vec<_>>();
    let ordered_values = plan.pointer("/ranking/ordered").and_then(Value::as_array);
    let ordered = ordered_values.and_then(|values| {
        values
            .iter()
            .map(|value| value.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
    });
    let clause = if plan.get("plan_version") != Some(&json!(1)) {
        Some("plan_version")
    } else if plan.get("queue_basis") != Some(&basis) {
        Some("queue_basis")
    } else if plan.pointer("/ranking/work_ranking") != Some(&json!(config.work_ranking)) {
        Some("work_ranking")
    } else if !plan
        .pointer("/ranking/ordered")
        .is_some_and(Value::is_array)
    {
        Some("ordered_not_array")
    } else if ordered_values.is_some_and(|items| {
        items
            .iter()
            .enumerate()
            .any(|(index, item)| items[..index].contains(item))
    }) {
        Some("ordered_duplicates")
    } else {
        let mut expected = candidate_ids;
        let mut actual = ordered.clone().unwrap_or_default();
        expected.sort();
        actual.sort();
        (ordered.is_none() || actual != expected).then_some("candidate_set_mismatch")
    };
    if let Some(clause) = clause {
        Ok((
            PlanApplication::Rejected,
            Vec::new(),
            Some(clause.to_owned()),
        ))
    } else {
        Ok((PlanApplication::Applied, ordered.unwrap_or_default(), None))
    }
}

fn ranked_with_plan(queue: &[QueueItem], principal: &[String], plan: &[String]) -> Vec<String> {
    let mut items = queue.iter().collect::<Vec<_>>();
    items.sort_by_key(|item| {
        if let Some(rank) = principal.iter().position(|id| id == &item.id) {
            (0, rank, 0_isize, &item.opened, &item.id)
        } else if let Some(rank) = plan.iter().position(|id| id == &item.id) {
            (1, rank, 0, &item.opened, &item.id)
        } else {
            (
                2,
                0,
                -(item.unblocking_power as isize),
                &item.opened,
                &item.id,
            )
        }
    });
    items.into_iter().map(|item| item.id.clone()).collect()
}

#[allow(clippy::too_many_arguments)]
fn append_selection_traces(
    request: &SelectRequest,
    owner: &str,
    attempted: &BTreeSet<String>,
    raw_queue: &[Value],
    queue: &[QueueItem],
    authorized: &[(usize, &QueueItem)],
    candidates: &[usize],
    ordered: &[Value],
    selected: &Value,
    graph: &WorkGraph,
    principal: &[String],
    plan_application: PlanApplication,
    plan_order: &[String],
    rejection_clause: Option<&str>,
) -> Result<(), SelectError> {
    let trace_path = request.paths.trace_file();
    let timestamp = request.clock.timestamp();
    let selected_id = selected["id"].as_str().ok_or(SelectError::InvalidGraph)?;
    let graph_by_id = graph
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect::<BTreeMap<_, _>>();
    let oldest_authorized = authorized
        .iter()
        .copied()
        .filter(|(_, item)| !attempted.contains(&item.id))
        .min_by_key(|(_, item)| (&item.opened, &item.id));
    if let Some((_, gated)) =
        oldest_authorized.filter(|(_, item)| item.id != selected_id && !item.graph_dispatchable)
    {
        let node = graph_by_id
            .get(gated.id.as_str())
            .ok_or(SelectError::InvalidGraph)?;
        let cycle = graph
            .faults
            .iter()
            .any(|fault| fault.name == "dependency_cycle" && fault.nodes.contains(&gated.id));
        append_fact(
            &trace_path,
            &timestamp,
            "work-graph-gated",
            json!({
                "owner": owner,
                "action": "dependency-graph-gate",
                "selected": selected_id,
                "gated": gated.id,
                "unsatisfied": node.unsatisfied,
                "children": node.children,
                "cycle": cycle
            }),
        )?;
    }

    let oldest_candidate = candidates
        .iter()
        .map(|index| &queue[*index])
        .filter(|item| !attempted.contains(&item.id))
        .min_by_key(|item| (&item.opened, &item.id));
    if let Some(displaced) = oldest_candidate.filter(|item| item.id != selected_id) {
        let (ranking, position) =
            if let Some(rank) = principal.iter().position(|id| id == selected_id) {
                ("work_ranking", Some(rank + 1))
            } else if plan_application == PlanApplication::Applied {
                plan_order
                    .iter()
                    .position(|id| id == selected_id)
                    .map_or(("dependency-unblocks", None), |rank| {
                        ("goal-plan", Some(rank + 1))
                    })
            } else {
                ("dependency-unblocks", None)
            };
        let mut fact = Map::new();
        fact.insert("owner".to_owned(), json!(owner));
        fact.insert("repo".to_owned(), selected["repo"].clone());
        fact.insert("ref".to_owned(), selected["ref"].clone());
        fact.insert("action".to_owned(), json!("delegated-selection"));
        fact.insert("selected".to_owned(), json!(selected_id));
        fact.insert("displaced".to_owned(), json!(displaced.id));
        fact.insert("ranking".to_owned(), json!(ranking));
        if let Some(position) = position {
            fact.insert("ranking_position".to_owned(), json!(position));
        }
        append_fact(&trace_path, &timestamp, "work-ranked", Value::Object(fact))?;
    }

    let mut fact = Map::new();
    fact.insert("owner".to_owned(), json!(owner));
    fact.insert("repo".to_owned(), selected["repo"].clone());
    fact.insert("ref".to_owned(), selected["ref"].clone());
    fact.insert("action".to_owned(), json!("delegated-selection"));
    fact.insert("selected".to_owned(), json!(selected_id));
    append_plan_selection_trace(
        &trace_path,
        &timestamp,
        fact,
        plan_application,
        rejection_clause,
    )?;
    let _ = (raw_queue, ordered);
    Ok(())
}

fn selection_trace_timestamp() -> String {
    std::env::var("MANDATE_TRACE_TIME").unwrap_or_else(|_| {
        chrono::DateTime::<Utc>::from(SystemTime::now())
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    })
}

fn append_plan_selection_trace(
    path: &Path,
    timestamp: &str,
    mut fact: Map<String, Value>,
    plan_application: PlanApplication,
    rejection_clause: Option<&str>,
) -> Result<(), SelectError> {
    let status = match plan_application {
        PlanApplication::Absent => "absent",
        PlanApplication::Applied => "applied",
        PlanApplication::Rejected => "rejected",
    };
    fact.insert("plan_status".to_owned(), json!(status));
    if let Some(clause) = rejection_clause {
        fact.insert("plan_rejection_clause".to_owned(), json!(clause));
    }
    append_fact(path, timestamp, "plan-selection", Value::Object(fact))
}

fn append_fact(path: &Path, timestamp: &str, kind: &str, fact: Value) -> Result<(), SelectError> {
    append_trace(
        path,
        &TraceAppend {
            ts: timestamp.to_owned(),
            kind: kind.to_owned(),
            fact: fact
                .as_object()
                .expect("selection facts are objects")
                .clone(),
            narration: Map::new(),
        },
    )
    .map(|_| ())
    .map_err(SelectError::Trace)
}
