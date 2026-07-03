//! `kkernel sync` — thin wrapper around the `khive_vcs::sync` library boundary.
//!
//! The NDJSON-to-SQLite rebuild logic lives in `khive_vcs::sync::run_sync`.
//! This module re-exports the types and function so the `kkernel` binary CLI
//! layer can call them with minimal indirection.

pub use khive_vcs::sync::{
    run_sync, run_sync_remote, RemoteConfig, RemoteName, RemoteSyncReport, SyncReport,
};
