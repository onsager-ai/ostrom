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
mod dispatch;
mod domain;
mod plan;
mod policy;
mod store;
mod work_graph;

#[cfg(feature = "conformance")]
pub mod conformance;

pub use check::{
    ActionDefinition, AgentParameters, CHECKS_VERSION, Catalogue, CatalogueEnumeration, CheckBasis,
    CheckContractError, CheckDefinition, CheckDocument, CheckEvaluation, CheckFault, CheckReceipt,
    CheckState, CheckVerdict, DefinitionDigest, Evidence, EvidenceBundleItem, EvidenceReference,
    FreshnessError, InconclusivePolicy, JudgeStamp, JudgmentClause, JudgmentInput,
    JudgmentRunnerStamp, RESULT_VERSION, RecordedOutput, ResolvedCheck, RunnerStamp,
    agent_parameters, receipt_digest, resolve_check, resolve_fresh_for, select_check, sha256_hex,
};
pub use dispatch::{
    BranchListing, BranchListingFault, BranchListingOutcome, RemoteBranch, WorkOrder,
    WorkOrderError, resolve_exact_branch,
};
pub use domain::{
    ConfigError, DefaultDisposition, GateConfig, GateProject, GateSelector, Mandate, MandateConfig,
    ProjectMandate, RepositoryName, Role, Selector, SelectorError, Verdict,
};
pub use plan::{
    Acknowledgement, AcknowledgementResponse, Assessment, AssessmentDraft, AssessmentError,
    Because, Consequence, EvaluatedCheck, GOALS_VERSION, Goal, GoalAction, GoalActionVerb,
    GoalBasis, GoalFacts, GoalService, GoalState, GoalsDocument, GoalsError, Impediment,
    MetWhenStatus, MilestoneFact, MilestoneInput, MovementFact, PLAN_VERSION, ProgressFact,
    QueueItem, Reading, cited_fact_basis, compose_ranking, consequence, derive_goal_facts,
    fact_table, mechanical_ranking, validate_assessment,
};
pub use policy::{
    ActorDecl, InputDecl, InputResolutionError, InputType, LoopDecl, ManifestDefaults,
    ManifestError, ManifestValidationError, NormalizedList, OperationDecl, PolicyCandidate,
    PolicyDecision, PolicyManifest, PolicySelector, PolicySelectorError, ResolvedInput, RuleDecl,
    RuleDefaults, SelectorFinding, SelectorMatch, SelectorPrefix, SelectorResolutionError,
    SelectorUniverse, StepDecl, UnmatchedPolicy,
};
pub use store::{
    AttemptOutcome, CHECK_STORE_SCHEMA_VERSION, CheckRun, CheckRunId, CheckStore, CheckStoreFault,
    EVENT_VERSION, EventEnvelope, EventInput, EventPayload, EventPayloadFault, EventRunId,
    EventStore, EventStoreFault, EventType, EventTypeFault, GateFact, PassAttempt, PassId,
    QueueFact, QueueKind, QueueState, RepoStateFact, STORE_SCHEMA_VERSION, StoreFault, SweepPass,
    SweepStore, WriteDisposition,
};
pub use work_graph::{
    WORK_GRAPH_VERSION, WorkEdge, WorkEdgeSource, WorkGraph, WorkGraphFault, WorkGraphNode,
    WorkNodeInput, build_work_graph,
};

/// The `User-Agent` every outbound HTTP request must carry.
///
/// GitHub's REST API rejects a request without one — not with 401, which would
/// point at credentials, but with **403 and an administrative-rules message**.
/// `reqwest` sends no default, so a client built without this fails every call
/// while looking like a permission problem.
///
/// This cost a production incident on 2026-08-18. The native App minter had
/// never authenticated successfully; its tests pass because a local fixture
/// server accepts requests with no User-Agent, and the failure only appears
/// against real GitHub. The sweep then wrote an 11-row queue over a 135-row one.
///
/// It lives in core so both the store's minter and the checks crate's HTTP
/// action share one definition rather than each remembering.
pub const USER_AGENT: &str = concat!("ostrom/", env!("CARGO_PKG_VERSION"));
