//! Secret-gate content-manifest exemption finalizer (ADR-115 Amendment 1).
//!
//! The [`boundary`] module is the production entry point for entity, note,
//! and knowledge candidates. The remaining modules retain the typed outcome
//! harness and failure seams used by ADR-115's generated acceptance matrix.
#![allow(dead_code)] // acceptance-harness types are intentionally test-driven

#[cfg(test)]
mod acceptance;
pub(crate) mod boundary;
pub(crate) mod declaration;
pub(crate) mod faults;
pub(crate) mod log_sink;
pub(crate) mod manifest;
pub(crate) mod matrix;
pub(crate) mod outcome;
pub(crate) mod transaction;
