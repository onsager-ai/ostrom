//! Event-type names shared by runtime producers and consumers.

// Ethogram #5 landed, but it exports no event-type name constants; these
// local copies stay together here until it does.
pub(crate) const RUN_FINISHED: &str = "run.finished";
pub(crate) const AGENT_COMPLETED: &str = "agent.completed";
pub(crate) const AGENT_TOOL_USE: &str = "agent.tool_use";
pub(crate) const AGENT_TOOL_RESULT: &str = "agent.tool_result";
pub(crate) const AGENT_WARNING: &str = "agent.warning";

#[cfg(test)]
mod tests {
    use ethogram::parse_event;
    use serde_json::{Value, json};

    use super::*;

    fn wire(event_type: &str, payload: Value) -> String {
        json!({
            "v": 1,
            "type": event_type,
            "runId": "run",
            "seq": 1,
            "ts": "2026-09-07T00:00:00.000Z",
            "payload": payload,
        })
        .to_string()
    }

    #[test]
    fn run_finished_constant_still_names_a_type_ethogram_validates() {
        // Ethogram validates payloads only for recognised event types. If this
        // malformed payload parses, the local copy has drifted from its vocabulary.
        assert!(parse_event(&wire(RUN_FINISHED, json!({ "outcome": 12345 }))).is_err());
    }

    #[test]
    fn agent_tool_use_constant_still_names_a_type_ethogram_validates() {
        // This omits the required `tool` and gives `toolUseId` the wrong type.
        // Ethogram validates payloads only for recognised event types, so a
        // drifted constant becomes unknown, skips validation, and fails this guard.
        assert!(
            parse_event(&wire(
                AGENT_TOOL_USE,
                json!({ "toolUseId": 12345, "name": 98765 }),
            ))
            .is_err()
        );
    }

    #[test]
    fn agent_tool_result_constant_still_names_a_type_ethogram_validates() {
        // This omits the required `tool` and gives `toolUseId` the wrong type.
        // Ethogram validates payloads only for recognised event types, so a
        // drifted constant becomes unknown, skips validation, and fails this guard.
        assert!(
            parse_event(&wire(
                AGENT_TOOL_RESULT,
                json!({ "toolUseId": 12345, "name": 98765 }),
            ))
            .is_err()
        );
    }

    #[test]
    fn agent_completed_constant_still_names_a_type_ethogram_validates() {
        // Unknown fields are tolerated on `agent.completed`, so this payload
        // violates its own `turns` field instead. If the constant drifts, the
        // type becomes unknown, validation is skipped, and this guard fails.
        assert!(parse_event(&wire(AGENT_COMPLETED, json!({ "turns": "not-a-count" }),)).is_err());
    }
}
