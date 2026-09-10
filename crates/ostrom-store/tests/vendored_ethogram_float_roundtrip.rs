//! The vendored ethogram must build with `serde_json/float_roundtrip`.
//!
//! ethogram's own manifest calls that feature REQUIRED rather than an
//! optimisation: without it serde_json's fast float parser reads
//! `0.09765190000000001` -- a real captured `costUsd` -- as one ULP below the
//! value JavaScript parses, then faithfully re-emits the *wrong* number as
//! `0.0976519`. That is a silent cross-SDK disagreement on money, and nothing
//! else in this repository would notice it.
//!
//! Vendoring makes that reachable. `ethogram/crates/ethogram/Cargo.toml`
//! declares `serde_json.workspace = true`, so which workspace claims the
//! vendored crate decides which serde_json features it gets. Without
//! `exclude = ["ethogram"]` at ostrom's workspace root, Cargo binds it to
//! ostrom's workspace, the crate silently takes ostrom's `preserve_order`-only
//! spec instead of its own, and this test fails. A byte-identical vendored
//! tree does not give a byte-identical build.

/// A value that survives a round trip only under `float_roundtrip`.
const ULP_SENSITIVE_COST: &str = "0.09765190000000001";

#[test]
fn the_vendored_ethogram_round_trips_a_ulp_sensitive_cost() {
    let wire = format!(
        r#"{{"v":1,"type":"run.finished","runId":"run-float","seq":1,"ts":"2026-09-10T00:00:00.000Z","payload":{{"costUsd":{ULP_SENSITIVE_COST},"outcome":"completed"}}}}"#
    );

    let event = ethogram::parse_event(&wire).expect("the capture parses");
    let out = ethogram::serialise_event(&event).expect("the event serialises");

    assert!(
        out.contains(ULP_SENSITIVE_COST),
        "the vendored ethogram lost serde_json/float_roundtrip: {ULP_SENSITIVE_COST} \
         re-emitted as something else.\n  serialised: {out}"
    );
}
