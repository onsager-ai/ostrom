use ethogram::DecisionKind;
use ostrom_core::{DecisionOption, Dossier};

use crate::{Clock, OstromPaths, generated_run_id, run_events::DecisionRequest};

pub(crate) fn decision_request(
    paths: &OstromPaths,
    clock: &Clock,
    ceiling_usd: f64,
    spend: &str,
) -> DecisionRequest {
    // The daily ceiling covers the operator account sharing this state root,
    // rather than an individual actor or repository.
    let account = paths.state.display();
    DecisionRequest {
        decision_id: generated_run_id("budget", clock),
        kind: DecisionKind::Budget,
        subject: format!("account:{account}"),
        dossier: Dossier {
            question: format!(
                "The operator account at {account} has reached its daily ceiling of {ceiling_usd} USD for {} ({spend}). Raise the ceiling or wait?",
                clock.date()
            ),
            options_ruled_out: vec!["Proceeding under the current spend ceiling".to_owned()],
            recommended_action: "Wait for budget to become available, or have the operator author a policy version with a higher ceiling.".to_owned(),
            blast_radius: format!("All runs charged to the operator account at {account}."),
        },
        options: vec![
            DecisionOption {
                id: "raise".to_owned(),
                label: format!(
                    "Author a policy version with a higher ceiling, superseding {}",
                    paths.current_policy_version().display()
                ),
            },
            DecisionOption {
                id: "wait".to_owned(),
                label: "Wait for budget to become available".to_owned(),
            },
        ],
    }
}
