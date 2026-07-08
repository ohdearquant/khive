//! `khive-pack-git` — the git-lifecycle pack (ADR-088).
//!
//! Contributes three note kinds (`commit`, `issue`, `pull_request`) that make
//! repository provenance queryable through the KG graph, populated by a
//! batch, cursor-based ingester (`ingest`) rather than any new agent-facing
//! verb. See `docs/adr/ADR-088-git-lifecycle-pack.md`.

pub mod hook;
pub mod ingest;
mod pack;
pub(crate) mod vocab;

pub use pack::GitPack;
