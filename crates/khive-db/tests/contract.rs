//! Contract tests for the sqlite backend (ADR-009 §backend-contract-tests).
//!
//! Exercises the eight storage capability traits (`SqlAccess`, `EntityStore`,
//! `GraphStore`, `NoteStore`, `EventStore`, `VectorStore`, `SparseStore`,
//! `TextSearch`) against both in-memory and file-backed SQLite backends.
//! The harness is structured to become a cross-backend conformance suite when
//! a second backend ships (e.g. `khive-db-postgres`).

#[path = "contract/vector_filter.rs"]
mod vector_filter;

#[path = "contract/backend.rs"]
mod backend;
