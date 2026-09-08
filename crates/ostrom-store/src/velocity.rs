//! Private observation ledger and the single merge-fact definition (#343).

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The same machine predicate applies to authors and mergers. GraphQL exposes
/// Bot through __typename; the CLI's author shape exposes is_bot instead.
pub(crate) fn is_machine(actor: &Value) -> bool {
    let login = actor.get("login").and_then(Value::as_str).unwrap_or("");
    let is_bot = ["is_bot", "isBot"]
        .iter()
        .any(|key| actor.get(key).and_then(Value::as_bool) == Some(true))
        || actor.get("__typename").and_then(Value::as_str) == Some("Bot");
    is_bot || login.ends_with("[bot]")
}

pub(crate) fn actor_observed(actor: &Value) -> bool {
    actor
        .get("login")
        .and_then(Value::as_str)
        .is_some_and(|login| !login.is_empty())
        || matches!(
            actor.get("__typename").and_then(Value::as_str),
            Some("User" | "Bot")
        )
        || ["is_bot", "isBot"]
            .iter()
            .any(|key| actor.get(key).is_some_and(Value::is_boolean))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Attribution {
    LoopToLoop,
    LoopToPrincipal,
    Principal,
}

impl Attribution {
    pub(crate) fn classify(author: &Value, merger: &Value) -> Self {
        if !is_machine(author) {
            Self::Principal
        } else if is_machine(merger) {
            Self::LoopToLoop
        } else {
            Self::LoopToPrincipal
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MergeFact {
    pub pr: String,
    pub order_id: Option<String>,
    pub opened_at: DateTime<Utc>,
    pub merged_at: DateTime<Utc>,
    pub attribution: Attribution,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ObservedPull {
    pub repository: String,
    pub opened_at: DateTime<Utc>,
    /// None means machine-authored with no observed merger yet. It is pending,
    /// never an unattended delivery inferred from authorship alone.
    pub attribution: Option<Attribution>,
    pub merge: Option<MergeFact>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VelocityLedger {
    pub observed_days: BTreeMap<NaiveDate, BTreeSet<String>>,
    pub pulls: BTreeMap<String, ObservedPull>,
}

impl VelocityLedger {
    pub(crate) fn from_state(state: &Value) -> Result<Self, serde_json::Error> {
        state.get("velocity").map_or_else(
            || Ok(Self::default()),
            |value| serde_json::from_value(value.clone()),
        )
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::fs;

    use serde_json::json;
    use tempfile::{TempDir, tempdir};

    use super::*;
    use crate::{OstromPaths, PublishTarget, SweepMode, SweepOptions, run_sweep};

    pub(crate) const LOGINS: [&str; 4] = [
        "placeholder-distinct-author-machine",
        "placeholder-distinct-author-human",
        "placeholder-distinct-merger-machine[bot]",
        "placeholder-distinct-merger-human",
    ];

    pub(crate) fn pulls() -> Vec<Value> {
        (1..=9).map(|number| {
            let human = matches!(number, 3 | 4 | 9);
            json!({
                "number": number, "title": "Placeholder delivery", "state": "MERGED",
                "author": {"login": LOGINS[usize::from(human)], "__typename": if human {"User"} else {"Bot"}},
                "mergedBy": {"login": LOGINS[if matches!(number, 2 | 4) {3} else {2}], "__typename": "User"},
                "createdAt": if number == 6 {"2026-07-29T06:00:00Z".to_owned()} else {format!("2026-08-01T{number:02}:00:00Z")},
                "mergedAt": match number {
                    5 => json!("2026-08-02T05:00:00Z"),
                    6 => json!("2026-07-30T06:00:00Z"),
                    7 => json!("2026-08-03T08:00:00Z"),
                    8 | 9 => Value::Null,
                    _ => json!(format!("2026-08-03T{number:02}:00:00Z")),
                },
                "headRefName": format!("placeholder-branch-{number}"),
            })
        }).collect()
    }

    pub(crate) fn fixture() -> TempDir {
        let root = tempdir().expect("velocity fixture");
        let paths = OstromPaths {
            config: root.path().into(),
            state: root.path().into(),
        };
        fs::write(
            root.path().join("mandates.yaml"),
            "projects:\n  - repo: placeholder-org/velocity\n",
        )
        .expect("write roster");
        fs::create_dir(root.path().join("work-orders")).expect("orders directory");
        fs::write(root.path().join("work-orders/placeholder.json"),
            r#"{"repository":"placeholder-org/velocity","branch_name":"placeholder-branch-1","item_id":"placeholder-org/velocity#10","order_id":"placeholder-order-branch"}"#).expect("write order");
        crate::append_trace(&paths.trace_file(), &crate::TraceAppend {
            ts: "2026-08-01T10:00:00Z".to_owned(), kind: "work-completed".to_owned(),
            fact: json!({"order_id": "placeholder-order-completion", "pr_url": "https://example.invalid/placeholder-org/velocity/pull/2"}).as_object().unwrap().clone(),
            narration: serde_json::Map::new(),
        }).expect("completion link");
        let pulls = pulls();
        let mut options = SweepOptions {
            paths: paths.clone(),
            working_directory: root.path().into(),
            executable: root.path().join("unused"),
            plugin_root: root.path().into(),
            started_at: "2026-08-01T12:00:00Z".parse().unwrap(),
            requested_mode: SweepMode::Full,
            fixture: Some(root.path().join("fixture.json")),
            publish: PublishTarget::Disabled,
            policy: None,
        };
        for day in [1, 3, 4] {
            options.started_at = format!("2026-08-{day:02}T12:00:00Z").parse().unwrap();
            let (open, merged): (Vec<_>, Vec<_>) = pulls.iter().cloned().partition(|pull| {
                pull["mergedAt"]
                    .as_str()
                    .is_none_or(|time| time.parse::<DateTime<Utc>>().unwrap() > options.started_at)
            });
            let open = open
                .into_iter()
                .map(|mut pull| {
                    pull["state"] = json!("OPEN");
                    pull.as_object_mut().unwrap().remove("mergedAt");
                    pull.as_object_mut().unwrap().remove("mergedBy");
                    pull
                })
                .collect::<Vec<_>>();
            fs::write(
                options.fixture.as_ref().unwrap(),
                serde_json::to_vec(&json!({"repositories": [{
                    "repo": "placeholder-org/velocity", "open_prs": open, "merged_prs": merged,
                }]}))
                .unwrap(),
            )
            .unwrap();
            run_sweep(&options).expect("velocity sweep");
        }
        // Every merge is queried repeatedly; the trace must still have one fact per PR.
        let before = fs::read(paths.trace_file()).unwrap();
        let successful_state = fs::read(paths.sweep_state_file()).unwrap();
        // Simulate a failed state write after the facts were appended. The
        // retained trace must repair the ledger without re-appending facts.
        fs::write(paths.sweep_state_file(), b"{}").unwrap();
        run_sweep(&options).expect("retry after lost state");
        assert_eq!(before, fs::read(paths.trace_file()).unwrap());
        // Restore coverage from the successful generation for aggregation tests.
        fs::write(paths.sweep_state_file(), successful_state).unwrap();
        root
    }

    #[test]
    fn machine_predicate_and_attribution_cover_both_actor_shapes() {
        let human = json!({"login": "placeholder-person", "__typename": "User"});
        for bot in [
            json!({"login": "placeholder-machine", "is_bot": true}),
            json!({"login": "placeholder-machine", "isBot": true}),
            json!({"login": "placeholder-machine", "__typename": "Bot"}),
            json!({"login": "placeholder-machine[bot]"}),
        ] {
            assert_eq!(Attribution::classify(&bot, &bot), Attribution::LoopToLoop);
            assert_eq!(
                Attribution::classify(&bot, &human),
                Attribution::LoopToPrincipal
            );
            assert_eq!(Attribution::classify(&human, &bot), Attribution::Principal);
        }
        assert_eq!(
            Attribution::classify(&human, &human),
            Attribution::Principal
        );
    }

    #[test]
    fn missing_merge_evidence_refuses_before_writing_any_generation() {
        let root = fixture();
        let paths = OstromPaths {
            config: root.path().into(),
            state: root.path().into(),
        };
        let before = [
            paths.trace_file(),
            paths.sweep_state_file(),
            paths.queue_file(),
        ]
        .map(|path| (fs::read(&path).unwrap(), path));
        let options = SweepOptions {
            paths,
            working_directory: root.path().into(),
            executable: root.path().join("unused"),
            plugin_root: root.path().into(),
            started_at: "2026-08-04T12:00:00Z".parse().unwrap(),
            requested_mode: SweepMode::Full,
            fixture: Some(root.path().join("invalid.json")),
            publish: PublishTarget::Disabled,
            policy: None,
        };
        for (field, value) in [
            ("author", Value::Null),
            ("mergedBy", Value::Null),
            ("mergedBy", json!({})),
            ("createdAt", Value::Null),
            ("mergedAt", Value::Null),
            ("mergedAt", json!("2026-07-01T00:00:00Z")),
        ] {
            let mut pull = pulls().remove(0);
            pull["number"] = json!(99);
            pull[field] = value;
            fs::write(
                options.fixture.as_ref().unwrap(),
                serde_json::to_vec(&json!({"repositories": [{
                    "repo": "placeholder-org/velocity", "merged_prs": [pull],
                }]}))
                .unwrap(),
            )
            .unwrap();
            let error = run_sweep(&options).expect_err("unknown merge evidence must refuse");
            assert!(error.to_string().contains(field), "{error}");
            for (bytes, path) in &before {
                assert_eq!(*bytes, fs::read(path).unwrap());
            }
        }
    }

    #[test]
    fn sweep_records_merges_once_with_optional_order_and_github_time() {
        let root = fixture();
        let trace = fs::read_to_string(root.path().join("sprint.jsonl")).unwrap();
        for login in LOGINS {
            assert!(!trace.contains(login), "trace leaked {login}");
        }
        let rows = trace
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|row| row["kind"] == "pr-merged")
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 7);
        for row in &rows {
            assert_eq!(
                row.as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect::<BTreeSet<_>>(),
                BTreeSet::from(["ts", "kind", "fact", "narration"])
            );
        }
        let fact = |number| {
            &rows
                .iter()
                .find(|row| row["fact"]["pr"] == format!("placeholder-org/velocity#{number}"))
                .unwrap()["fact"]
        };
        assert_eq!(fact(1)["attribution"], "loop_to_loop");
        assert_eq!(fact(2)["attribution"], "loop_to_principal");
        assert_eq!(fact(3)["attribution"], "principal");
        assert_eq!(fact(4)["attribution"], "principal");
        assert_eq!(fact(1)["order_id"], "placeholder-order-branch");
        assert_eq!(fact(2)["order_id"], "placeholder-order-completion");
        assert!(fact(3)["order_id"].is_null());
        assert_eq!(fact(5)["merged_at"], "2026-08-02T05:00:00Z");
        assert_eq!(fact(5)["opened_at"], "2026-08-01T05:00:00Z");
        assert_eq!(
            rows.iter()
                .find(|row| row["fact"]["pr"] == "placeholder-org/velocity#5")
                .unwrap()["ts"],
            "2026-08-03T12:00:00Z"
        );
    }
}
