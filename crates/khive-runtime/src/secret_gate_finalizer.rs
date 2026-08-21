//! Secret-gate content-manifest exemption finalizer (ADR-115 Amendment 1).
//!
//! This module owns only the execution/outcome slice of the first acceptance
//! rung: the five typed outcomes, their injectable failure seams, the
//! store-independent audit-gap sink for a second-order failure-audit
//! failure, and the atomic-with-rollback transaction that produces them.
//! The runtime-owned entry-point declaration, the universal reservation
//! contract, and the manifest-lookup mechanism are separate, independently
//! owned files under this same module per ADR-115 Amendment 1 and are
//! not authored here.
//!
//! Nothing in this module is wired to a caller yet — that integration is the
//! runtime-ingress/declaration lane's job. Until it lands, every item here
//! is reachable only from this module's own tests.
#![allow(dead_code)]

#[cfg(test)]
mod acceptance;
pub(crate) mod declaration;
pub(crate) mod faults;
pub(crate) mod log_sink;
pub(crate) mod manifest;
pub(crate) mod matrix;
pub(crate) mod outcome;
pub(crate) mod transaction;
