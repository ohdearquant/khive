// Copyright 2026 khive contributors. Licensed under Apache-2.0.
//
//! KG versioning — content-addressed snapshot hashing and core types.
//!
//! The full snapshot/branch/merge pipeline was superseded by ADR-048
//! (git-native KG versioning via Deno CLI). This crate retains only the
//! foundational primitives still referenced by the wider workspace.
//!
//! # Crate layout
//!
//! - [`types`] — `KgSnapshot`, `KgBranch`, `SnapshotId`, `RemoteConfig`
//! - [`hash`] — canonical JSON serialization + SHA-256 snapshot hashing
//! - [`error`] — `VcsError` type

pub mod error;
pub mod hash;
pub mod types;

pub use error::VcsError;
pub use types::{KgBranch, KgSnapshot, RemoteAuth, RemoteConfig, SnapshotId};
