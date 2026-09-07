use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// Ostrom's escalation dossier before it is converted to the ethogram wire
/// representation at the runtime edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dossier {
    pub question: String,
    pub options_ruled_out: Vec<String>,
    pub recommended_action: String,
    pub blast_radius: String,
}

/// One answer Ostrom offers for a decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionOption {
    pub id: String,
    pub label: String,
}

/// One condition in Ostrom's gate result.
///
/// The free-form detail remains opaque. An excused condition must carry the
/// exception reason that made it excused, matching the consumer contract.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GateCondition {
    pub name: String,
    pub result: String,
    pub tier: Vec<String>,
    pub detail: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exception_reason: Option<String>,
}

#[derive(Deserialize)]
struct GateConditionWire {
    name: String,
    result: String,
    tier: Vec<String>,
    detail: Value,
    exception_reason: Option<String>,
}

impl<'de> Deserialize<'de> for GateCondition {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = GateConditionWire::deserialize(deserializer)?;
        if wire.result == "excused" && wire.exception_reason.is_none() {
            return Err(serde::de::Error::missing_field("exception_reason"));
        }
        Ok(Self {
            name: wire.name,
            result: wire.result,
            tier: wire.tier,
            detail: wire.detail,
            exception_reason: wire.exception_reason,
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{DecisionOption, Dossier, GateCondition};

    #[test]
    fn dossier_and_options_are_ostrom_owned_wire_shapes() {
        let dossier = Dossier {
            question: "May the placeholder proceed?".to_owned(),
            options_ruled_out: vec!["Auto-proceed".to_owned()],
            recommended_action: "Review the placeholder".to_owned(),
            blast_radius: "One placeholder".to_owned(),
        };
        assert_eq!(
            serde_json::to_value(dossier).expect("serialize dossier"),
            json!({
                "question": "May the placeholder proceed?",
                "options_ruled_out": ["Auto-proceed"],
                "recommended_action": "Review the placeholder",
                "blast_radius": "One placeholder",
            })
        );
        assert_eq!(
            serde_json::to_value(DecisionOption {
                id: "approve".to_owned(),
                label: "Approve".to_owned(),
            })
            .expect("serialize option"),
            json!({"id": "approve", "label": "Approve"})
        );
    }

    #[test]
    fn gate_condition_agrees_with_consumer_contract_fixture() {
        let fixture = include_str!("../tests/fixtures/gate-condition.json");
        let expected: Value = serde_json::from_str(fixture).expect("parse shared fixture");
        let condition: GateCondition =
            serde_json::from_value(expected.clone()).expect("parse gate condition");

        assert_eq!(
            serde_json::to_value(&condition).expect("serialize gate condition"),
            expected
        );

        let mut missing_exception = expected;
        missing_exception
            .as_object_mut()
            .expect("condition object")
            .remove("exception_reason");
        assert!(
            serde_json::from_value::<GateCondition>(missing_exception).is_err(),
            "an excused condition without its exception reason disagrees with the consumer contract"
        );
    }
}
