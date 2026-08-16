// Keep the store port substrate-neutral: a path in the port would mean a
// database-backed implementation cannot satisfy the trait honestly.
#![deny(clippy::disallowed_types)]

//! Pure Ostrom domain types and the public store port.
//!
//! This crate deliberately has no filesystem, environment, or network API.
//! Out-of-tree consumers should currently pin a Git revision. Publishing the
//! `ostrom-core` name is a public registration and remains a principal
//! decision. Until that decision, the API follows pre-1.0 semver: compatible
//! additions may land in minor releases; breaking changes require a minor
//! version bump and migration notes.

mod check;
mod domain;
mod plan;
mod store;

#[cfg(feature = "conformance")]
pub mod conformance;

pub use check::{
    ActionDefinition, CHECKS_VERSION, Catalogue, CatalogueEnumeration, CheckBasis,
    CheckContractError, CheckDefinition, CheckDocument, CheckEvaluation, CheckFault, CheckReceipt,
    CheckState, CheckVerdict, DefinitionDigest, Evidence, FreshnessError, RESULT_VERSION,
    ResolvedCheck, RunnerStamp, resolve_check, resolve_fresh_for, select_check,
};
pub use domain::{
    ConfigError, DefaultDisposition, GateConfig, GateProject, Mandate, MandateConfig,
    ProjectMandate, RepositoryName, Role, Selector, SelectorError, Verdict,
};
pub use plan::{
    Acknowledgement, AcknowledgementResponse, Assessment, AssessmentDraft, AssessmentError,
    Because, Consequence, EvaluatedCheck, GOALS_VERSION, Goal, GoalAction, GoalActionVerb,
    GoalFacts, GoalService, GoalState, GoalsDocument, GoalsError, Impediment, MetWhenStatus,
    MilestoneFact, MilestoneInput, MovementFact, PLAN_VERSION, ProgressFact, QueueItem, Reading,
    cited_fact_basis, compose_ranking, consequence, derive_goal_facts, fact_table,
    mechanical_ranking, validate_assessment,
};
pub use store::{
    AttemptOutcome, CHECK_STORE_SCHEMA_VERSION, CheckRun, CheckRunId, CheckStore, CheckStoreFault,
    GateFact, PassAttempt, PassId, QueueFact, QueueKind, QueueState, RepoStateFact,
    STORE_SCHEMA_VERSION, StoreFault, SweepPass, SweepStore, WriteDisposition,
};
