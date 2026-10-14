//! Secret-gate content-manifest exemption finalizer (ADR-115 Amendment 1).
//!
//! Entity constructors and direct ingest share exact-candidate preparation
//! and one caller-owned record/stamp/audit transaction. Production manifests
//! remain empty; nonempty fixtures exist only under cfg(test). Note routes
//! retain their mechanism harness and reservation-only production behavior.
#![allow(dead_code)]

#[cfg(test)]
mod acceptance;
pub(crate) mod declaration;
pub(crate) mod entity_admission;
mod entity_direct;
#[cfg(test)]
mod entity_route_tests;
pub(crate) mod entity_transaction;
pub use entity_admission::{
    EntityCandidateContext, EntityCandidateOrigin, EntityCandidatePrepared,
};
pub use entity_direct::EntityCandidateAdmission;
pub use entity_transaction::{EntityCandidateMutation, EntityFinalizationPlan};
pub(crate) mod faults;
pub(crate) mod log_sink;
pub(crate) mod manifest;
pub(crate) mod matrix;
pub(crate) mod outcome;
#[cfg(test)]
mod route_census;
pub(crate) mod transaction;
