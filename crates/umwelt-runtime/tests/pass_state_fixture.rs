use std::{fs, path::PathBuf};

use tempfile::tempdir;
use umwelt_runtime::{PassState, read_pass_state, write_pass_state};

#[test]
fn shared_pass_state_fixture_round_trips_through_umwelt() {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pass-state");
    let expected = PassState {
        role_id: "89abcdef".to_owned(),
        wake: 42,
        dispatchability_hash: Some(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
        ),
    };
    assert_eq!(
        read_pass_state(&fixture, "builder").expect("read shared fixture"),
        Some(expected.clone())
    );

    let round_trip = tempdir().expect("round-trip directory");
    write_pass_state(round_trip.path(), "builder", &expected).expect("write round trip");
    for suffix in ["pass-id", "wake-counter", "dispatchability-hash"] {
        let name = format!("builder-{suffix}");
        assert_eq!(
            fs::read(round_trip.path().join(&name)).expect("read round-trip field"),
            fs::read(fixture.join(name)).expect("read fixture field")
        );
    }
}
