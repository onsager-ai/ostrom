use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

pub const WORK_GRAPH_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkEdgeSource {
    Body,
    PullRequest,
    SubIssue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkEdge {
    pub dependency: String,
    pub item: String,
    pub sources: Vec<WorkEdgeSource>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkNodeInput {
    pub id: String,
    pub open: bool,
    pub body_dependencies: Vec<String>,
    pub parent: Option<String>,
    pub children: Vec<String>,
    pub closes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkGraphNode {
    pub id: String,
    pub open: bool,
    pub dependencies: Vec<String>,
    pub unsatisfied: Vec<String>,
    pub children: Vec<String>,
    pub dispatchable: bool,
    pub unblocking_power: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkGraphFault {
    pub name: String,
    pub nodes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkGraph {
    pub graph_version: u32,
    pub configured_repositories: Vec<String>,
    pub nodes: Vec<WorkGraphNode>,
    pub edges: Vec<WorkEdge>,
    pub faults: Vec<WorkGraphFault>,
}

/// Merge all dependency-bearing GitHub surfaces into one roster-wide graph.
///
/// An absent node in a configured repository is satisfied: the sweep's open
/// listing is exhaustive, so absence is positive closure evidence. A pointer
/// outside the roster remains unsatisfied because its state was not observed.
#[must_use]
pub fn build_work_graph(
    inputs: &[WorkNodeInput],
    configured_repositories: &BTreeSet<String>,
) -> WorkGraph {
    let mut nodes = BTreeMap::<String, bool>::new();
    for input in inputs {
        nodes
            .entry(input.id.clone())
            .and_modify(|open| *open |= input.open)
            .or_insert(input.open);
    }

    let mut edge_sources = BTreeMap::<(String, String), BTreeSet<WorkEdgeSource>>::new();
    let mut structural_children = BTreeMap::<String, BTreeSet<String>>::new();
    {
        let mut add_edge = |dependency: &str, item: &str, source: WorkEdgeSource| {
            if dependency == item && source != WorkEdgeSource::Body {
                return;
            }
            edge_sources
                .entry((dependency.to_owned(), item.to_owned()))
                .or_default()
                .insert(source);
        };

        for input in inputs {
            for dependency in &input.body_dependencies {
                add_edge(dependency, &input.id, WorkEdgeSource::Body);
            }
            if let Some(parent) = &input.parent {
                add_edge(&input.id, parent, WorkEdgeSource::SubIssue);
                structural_children
                    .entry(parent.clone())
                    .or_default()
                    .insert(input.id.clone());
            }
            for child in &input.children {
                add_edge(child, &input.id, WorkEdgeSource::SubIssue);
                structural_children
                    .entry(input.id.clone())
                    .or_default()
                    .insert(child.clone());
            }
            for issue in &input.closes {
                add_edge(&input.id, issue, WorkEdgeSource::PullRequest);
            }
        }
    }

    let edges = edge_sources
        .iter()
        .map(|((dependency, item), sources)| WorkEdge {
            dependency: dependency.clone(),
            item: item.clone(),
            sources: sources.iter().cloned().collect(),
        })
        .collect::<Vec<_>>();

    let open = nodes
        .iter()
        .filter(|(_, open)| **open)
        .map(|(id, _)| id.as_str())
        .collect::<BTreeSet<_>>();
    let open_edges = edges
        .iter()
        .filter(|edge| open.contains(edge.dependency.as_str()) && open.contains(edge.item.as_str()))
        .map(|edge| (edge.dependency.as_str(), edge.item.as_str()))
        .collect::<Vec<_>>();
    let cycle_nodes = open
        .iter()
        .filter(|start| reaches(start, start, &open_edges, &mut BTreeSet::new(), false))
        .map(|id| (*id).to_owned())
        .collect::<BTreeSet<_>>();

    let mut graph_nodes = nodes
        .iter()
        .map(|(id, is_open)| {
            let dependencies = edges
                .iter()
                .filter(|edge| edge.item == *id)
                .map(|edge| edge.dependency.clone())
                .collect::<Vec<_>>();
            let unsatisfied = dependencies
                .iter()
                .filter(|dependency| {
                    nodes.get(*dependency).copied().unwrap_or_else(|| {
                        repository_of(dependency)
                            .is_none_or(|repo| !configured_repositories.contains(repo))
                    })
                })
                .cloned()
                .collect::<Vec<_>>();
            let children = structural_children
                .get(id)
                .into_iter()
                .flat_map(BTreeSet::iter)
                .cloned()
                .collect::<Vec<_>>();
            WorkGraphNode {
                id: id.clone(),
                open: *is_open,
                dispatchable: *is_open
                    && unsatisfied.is_empty()
                    && children.is_empty()
                    && !cycle_nodes.contains(id),
                dependencies,
                unsatisfied,
                children,
                unblocking_power: 0,
            }
        })
        .collect::<Vec<_>>();

    let unsatisfied_by_item = graph_nodes
        .iter()
        .map(|node| node.unsatisfied.clone())
        .collect::<Vec<_>>();
    for node in &mut graph_nodes {
        node.unblocking_power = unsatisfied_by_item
            .iter()
            .filter(|unsatisfied| unsatisfied.as_slice() == [node.id.as_str()])
            .count();
    }

    WorkGraph {
        graph_version: WORK_GRAPH_VERSION,
        configured_repositories: configured_repositories.iter().cloned().collect(),
        nodes: graph_nodes,
        edges,
        faults: if cycle_nodes.is_empty() {
            Vec::new()
        } else {
            vec![WorkGraphFault {
                name: "dependency_cycle".to_owned(),
                nodes: cycle_nodes.into_iter().collect(),
            }]
        },
    }
}

fn repository_of(id: &str) -> Option<&str> {
    id.rsplit_once('#').map(|(repository, _)| repository)
}

fn reaches<'a>(
    current: &'a str,
    target: &str,
    edges: &[(&'a str, &'a str)],
    visited: &mut BTreeSet<&'a str>,
    moved: bool,
) -> bool {
    if moved && current == target {
        return true;
    }
    if !visited.insert(current) {
        return false;
    }
    edges
        .iter()
        .filter(|(dependency, _)| *dependency == current)
        .any(|(_, item)| reaches(item, target, edges, visited, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str) -> WorkNodeInput {
        WorkNodeInput {
            id: id.to_owned(),
            open: true,
            body_dependencies: Vec::new(),
            parent: None,
            children: Vec::new(),
            closes: Vec::new(),
        }
    }

    #[test]
    fn merges_edges_gates_open_dependencies_and_self_clears_closed_ones() {
        let mut item = node("another-example-org/another-repo#2");
        item.body_dependencies = vec!["example-org/example-repo#1".to_owned()];
        let repositories = BTreeSet::from([
            "example-org/example-repo".to_owned(),
            "another-example-org/another-repo".to_owned(),
        ]);
        let open = build_work_graph(
            &[node("example-org/example-repo#1"), item.clone()],
            &repositories,
        );
        assert!(
            !open
                .nodes
                .iter()
                .find(|node| node.id == item.id)
                .unwrap()
                .dispatchable
        );

        let closed = build_work_graph(&[item], &repositories);
        assert!(closed.nodes[0].dispatchable);
    }

    #[test]
    fn structural_children_cycles_and_unblocking_power_are_explicit() {
        let mut parent = node("example-org/example-repo#10");
        parent.children = vec!["example-org/example-repo#11".to_owned()];
        let mut child = node("example-org/example-repo#11");
        child.parent = Some(parent.id.clone());
        let mut downstream = node("example-org/example-repo#12");
        downstream.body_dependencies = vec![child.id.clone()];
        let graph = build_work_graph(
            &[parent, child, downstream],
            &BTreeSet::from(["example-org/example-repo".to_owned()]),
        );
        let parent = graph
            .nodes
            .iter()
            .find(|node| node.id.ends_with("#10"))
            .unwrap();
        let child = graph
            .nodes
            .iter()
            .find(|node| node.id.ends_with("#11"))
            .unwrap();
        assert!(!parent.dispatchable);
        assert_eq!(child.unblocking_power, 2);

        let mut left = node("example-org/example-repo#20");
        let mut right = node("example-org/example-repo#21");
        left.body_dependencies.push(right.id.clone());
        right.body_dependencies.push(left.id.clone());
        let cycle = build_work_graph(
            &[left, right],
            &BTreeSet::from(["example-org/example-repo".to_owned()]),
        );
        assert_eq!(cycle.faults[0].name, "dependency_cycle");
        assert!(cycle.nodes.iter().all(|node| !node.dispatchable));
    }

    #[test]
    fn duplicate_edges_retain_every_source_once() {
        let mut parent = node("example-org/example-repo#30");
        parent.body_dependencies = vec!["example-org/example-repo#31".to_owned()];
        parent.children = vec!["example-org/example-repo#31".to_owned()];
        let mut implementation = node("example-org/example-repo#31");
        implementation.parent = Some(parent.id.clone());
        implementation.closes = vec![parent.id.clone(), parent.id.clone()];
        let graph = build_work_graph(
            &[parent, implementation],
            &BTreeSet::from(["example-org/example-repo".to_owned()]),
        );
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(
            graph.edges[0].sources,
            vec![
                WorkEdgeSource::Body,
                WorkEdgeSource::PullRequest,
                WorkEdgeSource::SubIssue,
            ]
        );
    }
}
