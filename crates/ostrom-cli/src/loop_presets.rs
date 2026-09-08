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
    description: Publishes the portfolio sweep. Reads widely, writes only what publication allows.
    permission_mode: auto
operations:
  portfolio-sweep:
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
    ]))
}

pub(crate) fn render(json: bool) -> Result<String, Box<dyn std::error::Error>> {
    let presets = catalogue()?;
    if json {
        return Ok(format!("{}\n", serde_json::to_string_pretty(&presets)?));
    }
    let mut fragment = PolicyManifest::parse_yaml("manifest_version: 1\n")?;
    for preset in presets.into_values() {
        fragment.actors.extend(preset.fragment.actors);
        fragment.operations.extend(preset.fragment.operations);
        fragment.grants.extend(preset.fragment.grants);
        fragment.loops.extend(preset.fragment.loops);
    }
    Ok(format!(
        "# Optional loop presets: merge the declarations you want into ostrom.yaml.\n\
         # Replace every placeholder-org/portfolio with your repository.\n\
         # Agent prompt files are written by ostrom init.\n{}",
        fragment.to_yaml()?
    ))
}
