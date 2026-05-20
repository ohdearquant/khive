// Copyright 2026 khive contributors. Licensed under Apache-2.0.
//
//! KG versioning — snapshots, branches, and remote sync.
//!
//! Implements the versioning model from ADR-015 and the implementation details
//! from ADR-042: content-addressed snapshots, named branch pointers, and the
//! remote push/pull protocol.
//!
//! # Crate layout
//!
//! - [`types`] — `KgSnapshot`, `KgBranch`, `SnapshotId`, `RemoteConfig`
//! - [`hash`] — canonical JSON serialization + SHA-256 snapshot hashing
//! - [`snapshot`] — `commit()` operation
//! - [`branch`] — `branch()`, `checkout()`, `list_branches()`
//! - [`log`] — `log()` operation (snapshot history)
//! - [`merge_engine`] — `MergeEngine` trait + `NoOpMergeEngine`
//! - [`error`] — `VcsError` type

pub mod branch;
pub mod error;
pub mod hash;
pub mod log;
pub mod merge_engine;
pub mod migrations;
pub mod snapshot;
pub mod types;

pub use error::VcsError;
pub use merge_engine::{MergeEngine, MergeResult, MergeStrategy, NoOpMergeEngine};
pub use types::{KgBranch, KgSnapshot, RemoteAuth, RemoteConfig, SnapshotId};
