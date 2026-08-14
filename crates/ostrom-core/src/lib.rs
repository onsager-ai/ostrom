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

mod domain;
mod store;

#[cfg(feature = "conformance")]
pub mod conformance;

pub use domain::{
    ConfigError, DefaultDisposition, GateConfig, GateProject, Mandate, MandateConfig,
    ProjectMandate, RepositoryName, Role, Selector, SelectorError, Verdict,
};
pub use store::{
    AttemptOutcome, GateFact, PassAttempt, PassId, QueueFact, QueueKind, QueueState, RepoStateFact,
    STORE_SCHEMA_VERSION, StoreFault, SweepPass, SweepStore, WriteDisposition,
};
