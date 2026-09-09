//! Optional declarations an operator may merge into policy; no actor set is required.
//!
//! Actor settings profiles are derived from grants, not declared on actors.
//! Secrets can only be declared as manifest-level `inputs` with `secret: true`;
//! secret names below are consumer metadata, never fields inside the fragment.
//! `secret_names` lists credential lookup names in `secrets.yaml`; each lookup
//! accepts the existing `shared` fallback. These are not environment variables
//! or new manifest inputs. Harness authentication remains operator-configured.
//! Placeholder paths are JSON Pointers relative to each preset's `fragment`.
//! Map keys use BTreeMap order and metadata fields use struct declaration order,
//! including when serde_json's preserve_order feature is enabled.

use std::collections::BTreeMap;

use ostrom_core::PolicyManifest;
use serde::Serialize;
use thiserror::Error;

#[derive(Serialize)]
struct Preset {
    fragment: PolicyManifest,
    secret_names: &'static [&'static str],
    placeholder_paths: &'static [&'static str],
}

fn catalogue() -> Result<BTreeMap<&'static str, Preset>, serde_yaml::Error> {
    Ok(BTreeMap::from([
        (
            "builder",
            Preset {
                fragment: PolicyManifest::parse_yaml(
                    r#"manifest_version: 1
actors:
  builder:
    description: Writes work orders and dispatches implementers, unattended.
    permission_mode: auto
operations:
  build-pass:
    description: One builder pass over the portfolio queue.
    steps:
      - uses: agent/claude
        with:
          prompt: {from: ./prompts/work.md}
grants:
  builder-build:
    actors: builder
    operations: build-pass
loops:
  builder-day:
    actor: builder
    operation: build-pass
    target: placeholder-org/portfolio
    every: 08:15..21:15
    spend_usd: 20
    concurrent: 1
"#,
                )?,
                secret_names: &["builder"],
                placeholder_paths: &["/loops/builder-day/target"],
            },
        ),
        (
            "gatekeeper",
            Preset {
                fragment: PolicyManifest::parse_yaml(
                    r#"manifest_version: 1
actors:
  gatekeeper:
    description: Judges finished work. Acts only on confirmation.
    permission_mode: manual
operations:
  gate-pass:
    description: One gatekeeper pass over open pull requests.
    steps:
      - uses: agent/claude
        with:
          prompt: {from: ./prompts/gatekeep.md}
grants:
  gatekeeper-gate:
    actors: gatekeeper
    operations: gate-pass
loops:
  gatekeeper:
    actor: gatekeeper
    operation: gate-pass
    target: placeholder-org/portfolio
    every: hourly
"#,
                )?,
                secret_names: &["gatekeeper"],
                placeholder_paths: &["/loops/gatekeeper/target"],
            },
        ),
        (
            "sweep",
            Preset {
                // `ostrom sweep` is the publication sweep. `cmd/run` is already
                // in the closed action catalogue in ostrom-core/src/operation.rs;
                // this introduces no new action. Publication remains opt-in via
                // the sweep command's explicit destination argument.
                fragment: PolicyManifest::parse_yaml(
                    r#"manifest_version: 1
actors:
  sweeper:
    description: Publishes the portfolio sweep. Reads widely, writes only what publication allows. The loops.sweep entry schedules the sweep from a local cron-style scheduler on one machine; a hosted substrate schedules the sweep itself and should not adopt that loop.
    permission_mode: auto
operations:
  portfolio-sweep:
    description: Runs the publication sweep over the portfolio queue.
    steps:
      - uses: cmd/run
        with:
          script: ostrom sweep
grants:
  sweep:
    actors: sweeper
    operations: portfolio-sweep
    repositories: placeholder-org/portfolio
loops:
  sweep:
    actor: sweeper
    operation: portfolio-sweep
    target: placeholder-org/portfolio
    every: "*:45"
"#,
                )?,
                // sweep::organization_token_request uses this credential name.
                secret_names: &["gatekeeper"],
                placeholder_paths: &["/grants/sweep/repositories/0", "/loops/sweep/target"],
            },
        ),
        (
            "triage",
            Preset {
                fragment: PolicyManifest::parse_yaml(
                    r#"manifest_version: 1
actors:
  triage:
    description: Orders the portfolio queue. Classifies readiness, blockers and stale work.
    permission_mode: auto
operations:
  queue-triage:
    description: One triage pass over the portfolio queue.
    steps:
      - uses: agent/claude
        with:
          prompt: {from: ./prompts/triage.md}
grants:
  triage-queue:
    actors: triage
    operations: queue-triage
loops:
  unattended-triage:
    actor: triage
    operation: queue-triage
    target: placeholder-org/portfolio
    every: hourly
"#,
                )?,
                secret_names: &["triage"],
                placeholder_paths: &["/loops/unattended-triage/target"],
            },
        ),
    ]))
}

/// Two presets declared the same key in the same section of the manifest.
///
/// This is deliberately reachable only through [`merge_disjoint`], never as an
/// assertion buried inside `render()` over the hardcoded catalogue: the real
/// catalogue is static and never collides, so a guard living inside `render()`
/// could never be exercised by a test. Extracting the merge lets a test build
/// a synthetic, colliding catalogue and prove the guard actually trips.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("preset collision: {section} `{key}` is declared by both `{first}` and `{second}`")]
pub(crate) struct PresetCollision {
    section: &'static str,
    key: String,
    first: String,
    second: String,
}

/// Merge each preset's manifest fragment, refusing a collision on any shared
/// `actors`, `operations`, `grants` or `loops` key rather than letting
/// [`BTreeMap::extend`] silently let the later preset win.
fn merge_disjoint<'a>(
    presets: impl IntoIterator<Item = (&'a str, &'a PolicyManifest)>,
) -> Result<PolicyManifest, PresetCollision> {
    let mut merged =
        PolicyManifest::parse_yaml("manifest_version: 1\n").expect("literal fragment parses");
    let mut actor_origins = BTreeMap::<String, String>::new();
    let mut operation_origins = BTreeMap::<String, String>::new();
    let mut grant_origins = BTreeMap::<String, String>::new();
    let mut loop_origins = BTreeMap::<String, String>::new();
    let mut sweep_origin = None;
    for (name, fragment) in presets {
        if let Some(sweep) = &fragment.sweep {
            if let Some(first) = sweep_origin {
                return Err(PresetCollision {
                    section: "sweep",
                    key: "sweep".to_owned(),
                    first,
                    second: name.to_owned(),
                });
            }
            merged.sweep = Some(sweep.clone());
            sweep_origin = Some(name.to_owned());
        }
        merge_section(
            &mut merged.actors,
            &mut actor_origins,
            &fragment.actors,
            "actors",
            name,
        )?;
        merge_section(
            &mut merged.operations,
            &mut operation_origins,
            &fragment.operations,
            "operations",
            name,
        )?;
        merge_section(
            &mut merged.grants,
            &mut grant_origins,
            &fragment.grants,
            "grants",
            name,
        )?;
        merge_section(
            &mut merged.loops,
            &mut loop_origins,
            &fragment.loops,
            "loops",
            name,
        )?;
    }
    Ok(merged)
}

fn merge_section<T: Clone>(
    target: &mut BTreeMap<String, T>,
    origins: &mut BTreeMap<String, String>,
    incoming: &BTreeMap<String, T>,
    section: &'static str,
    preset: &str,
) -> Result<(), PresetCollision> {
    for (key, value) in incoming {
        if let Some(first) = origins.get(key) {
            return Err(PresetCollision {
                section,
                key: key.clone(),
                first: first.clone(),
                second: preset.to_owned(),
            });
        }
        target.insert(key.clone(), value.clone());
        origins.insert(key.clone(), preset.to_owned());
    }
    Ok(())
}

pub(crate) fn render(json: bool) -> Result<String, Box<dyn std::error::Error>> {
    let presets = catalogue()?;
    if json {
        return Ok(format!("{}\n", serde_json::to_string_pretty(&presets)?));
    }
    let fragment = merge_disjoint(
        presets
            .iter()
            .map(|(name, preset)| (*name, &preset.fragment)),
    )?;
    Ok(format!(
        "# Optional loop presets: merge the declarations you want into ostrom.yaml.\n\
         # Replace every placeholder-org/portfolio with your repository.\n\
         # Agent prompt files are written by ostrom init.\n{}",
        fragment.to_yaml()?
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_catalogue_merges_without_collision() {
        let presets = catalogue().expect("catalogue parses");
        let merged = merge_disjoint(
            presets
                .iter()
                .map(|(name, preset)| (*name, &preset.fragment)),
        )
        .expect("the four shipped presets do not collide");
        for actor in ["builder", "gatekeeper", "sweeper", "triage"] {
            assert!(merged.actors.contains_key(actor), "missing actor {actor}");
        }
        for operation in ["build-pass", "gate-pass", "portfolio-sweep", "queue-triage"] {
            assert!(
                merged.operations.contains_key(operation),
                "missing operation {operation}"
            );
        }
        for grant in ["builder-build", "gatekeeper-gate", "sweep", "triage-queue"] {
            assert!(merged.grants.contains_key(grant), "missing grant {grant}");
        }
        for loop_name in ["builder-day", "gatekeeper", "sweep", "unattended-triage"] {
            assert!(
                merged.loops.contains_key(loop_name),
                "missing loop {loop_name}"
            );
        }
    }

    // The presets endpoint passes this description through verbatim, so its
    // bytes are what an operator reads before applying the preset. #527 ruled
    // that it must say a hosted substrate schedules the sweep itself; pinning
    // the whole string is what turns that ruling into a guard.
    #[test]
    fn the_sweep_preset_description_says_a_hosted_substrate_schedules_its_own_sweep() {
        let presets = catalogue().expect("catalogue parses");
        let sweeper = presets
            .get("sweep")
            .expect("the sweep preset is shipped")
            .fragment
            .actors
            .get("sweeper")
            .expect("the sweep preset declares the sweeper actor");

        assert_eq!(
            sweeper.description.as_deref(),
            Some(
                "Publishes the portfolio sweep. Reads widely, writes only what publication \
                 allows. The loops.sweep entry schedules the sweep from a local cron-style \
                 scheduler on one machine; a hosted substrate schedules the sweep itself and \
                 should not adopt that loop."
            )
        );
    }

    #[test]
    fn colliding_presets_are_refused_by_name() {
        let first = PolicyManifest::parse_yaml(
            r#"manifest_version: 1
actors:
  builder:
    permission_mode: auto
"#,
        )
        .expect("first fragment parses");
        let second = PolicyManifest::parse_yaml(
            r#"manifest_version: 1
actors:
  builder:
    permission_mode: manual
"#,
        )
        .expect("second fragment parses");

        let error = merge_disjoint([("alpha", &first), ("beta", &second)])
            .expect_err("a shared actor key must be refused, not silently overwritten");

        assert_eq!(
            error,
            PresetCollision {
                section: "actors",
                key: "builder".to_owned(),
                first: "alpha".to_owned(),
                second: "beta".to_owned(),
            }
        );
        assert_eq!(
            error.to_string(),
            "preset collision: actors `builder` is declared by both `alpha` and `beta`"
        );
    }
    #[test]
    fn sweep_section_is_preserved_and_collisions_refuse() {
        let first = PolicyManifest::from_yaml("manifest_version: 1\nsweep: {}\n").unwrap();
        let second =
            PolicyManifest::from_yaml("manifest_version: 1\nsweep: {max_age: 1h}\n").unwrap();
        assert_eq!(
            merge_disjoint([("alpha", &first)]).unwrap().sweep,
            first.sweep
        );
        let error = merge_disjoint([("alpha", &first), ("beta", &second)])
            .expect_err("shared sweep section must refuse");
        assert_eq!(
            error.to_string(),
            "preset collision: sweep `sweep` is declared by both `alpha` and `beta`"
        );
    }
}
