use std::collections::BTreeSet;

use ostrom_core::{PolicyCandidate, PolicyManifest};
use serde::Deserialize;

#[derive(Deserialize)]
struct LegacyProfiles {
    builder: LegacyProfile,
    gatekeeper: LegacyProfile,
}

#[derive(Deserialize)]
struct LegacyProfile {
    permissions: LegacyPermissions,
}

#[derive(Deserialize)]
struct LegacyPermissions {
    allow: Vec<String>,
    deny: Vec<String>,
}

const POLICY: &str = "manifest_version: 1\nactors: {builder: {}, gatekeeper: {}}\noperations:\n  common: {steps: []}\n  merge:\n    steps:\n      - uses: gh/merge-pr\n        requires: ready\n  work: {steps: []}\ngrants:\n  both-common: {actors: [builder, gatekeeper], operations: common}\n  builder-work: {actors: builder, operations: work}\n  gatekeeper-merge: {actors: gatekeeper, operations: merge}\n";

#[test]
fn all_224_legacy_entries_replay_through_the_three_operation_grants() {
    let profiles =
        serde_json::from_str::<LegacyProfiles>(include_str!("fixtures/legacy-role-settings.json"))
            .expect("sanitized legacy settings fixture");
    let manifest = PolicyManifest::from_yaml(POLICY).expect("operation policy");
    let entries = profiles
        .builder
        .permissions
        .allow
        .iter()
        .chain(&profiles.builder.permissions.deny)
        .chain(&profiles.gatekeeper.permissions.allow)
        .chain(&profiles.gatekeeper.permissions.deny)
        .collect::<Vec<_>>();
    assert_eq!(
        entries.len(),
        224,
        "the replay must include both deny lists"
    );

    let candidate = PolicyCandidate::default();
    for entry in entries {
        let sample = representative(entry);
        let legacy_builder = profiles.builder.permissions.allows(&sample);
        let legacy_gatekeeper = profiles.gatekeeper.permissions.allows(&sample);
        let operation = match (legacy_builder, legacy_gatekeeper) {
            (true, false) => Some("work"),
            (false, true) => Some("merge"),
            (true, true) => Some("common"),
            (false, false) => None,
        };
        let replayed_builder = operation
            .is_some_and(|operation| manifest.decide("builder", operation, &candidate).granted);
        let replayed_gatekeeper = operation
            .is_some_and(|operation| manifest.decide("gatekeeper", operation, &candidate).granted);
        assert_eq!(
            (replayed_builder, replayed_gatekeeper),
            (legacy_builder, legacy_gatekeeper),
            "legacy permission entry `{entry}`"
        );
    }
}

#[test]
fn fixture_retains_the_measured_15_2_38_separation() {
    let profiles =
        serde_json::from_str::<LegacyProfiles>(include_str!("fixtures/legacy-role-settings.json"))
            .expect("sanitized legacy settings fixture");
    let builder_allow = set(&profiles.builder.permissions.allow);
    let builder_deny = set(&profiles.builder.permissions.deny);
    let gatekeeper_allow = set(&profiles.gatekeeper.permissions.allow);
    let gatekeeper_deny = set(&profiles.gatekeeper.permissions.deny);

    assert_eq!(builder_allow.intersection(&gatekeeper_deny).count(), 15);
    assert_eq!(gatekeeper_allow.intersection(&builder_deny).count(), 2);
    assert_eq!(builder_allow.intersection(&gatekeeper_allow).count(), 16);
    assert_eq!(builder_deny.intersection(&gatekeeper_deny).count(), 22);
}

fn set(values: &[String]) -> BTreeSet<&str> {
    values.iter().map(String::as_str).collect()
}

impl LegacyPermissions {
    fn allows(&self, sample: &str) -> bool {
        self.allow.iter().any(|rule| rule_matches(rule, sample))
            && !self.deny.iter().any(|rule| rule_matches(rule, sample))
    }
}

fn representative(rule: &str) -> String {
    rule.replace('*', "placeholder")
}

fn rule_matches(rule: &str, sample: &str) -> bool {
    glob_matches(
        sample.as_bytes(),
        rule.as_bytes(),
        0,
        0,
        &mut BTreeSet::new(),
    )
}

fn glob_matches(
    value: &[u8],
    pattern: &[u8],
    value_index: usize,
    pattern_index: usize,
    visited: &mut BTreeSet<(usize, usize)>,
) -> bool {
    if !visited.insert((value_index, pattern_index)) {
        return false;
    }
    if pattern_index == pattern.len() {
        return value_index == value.len();
    }
    if pattern[pattern_index] == b'*' {
        return glob_matches(value, pattern, value_index, pattern_index + 1, visited)
            || (value_index < value.len()
                && glob_matches(value, pattern, value_index + 1, pattern_index, visited));
    }
    value_index < value.len()
        && value[value_index] == pattern[pattern_index]
        && glob_matches(value, pattern, value_index + 1, pattern_index + 1, visited)
}
