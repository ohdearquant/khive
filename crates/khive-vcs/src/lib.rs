// Copyright 2026 khive contributors. Licensed under Apache-2.0.
//
//! KG versioning — content-addressed snapshot hashing, git-native core types,
//! and the NDJSON-to-SQLite sync library boundary.
//!
//! v1 versioning is git-native (ADR-010, ADR-020): KG state lives as sorted
//! NDJSON files in a git repository. The legacy snapshot/branch/merge pipeline
//! (`KgSnapshot`, `KgBranch`, `RemoteConfig`, custom push/pull) was superseded
//! by ADR-020. This crate retains:
//!
//! - [`types`] — `SnapshotId`, `SnapshotCoverage`, `VcsState`
//! - [`hash`] — canonical JSON serialization + SHA-256 snapshot hashing
//! - [`sync`] — NDJSON-to-SQLite rebuild library (ADR-010/ADR-020, F106)
//! - [`error`] — `VcsError` type

pub mod error;
pub mod hash;
pub mod sync;
pub mod types;

pub use error::VcsError;
pub use types::{SnapshotCoverage, SnapshotId, VcsState, KG_V1_COVERAGE};
