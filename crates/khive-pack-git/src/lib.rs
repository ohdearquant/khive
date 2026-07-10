//! `khive-pack-git` — the git-lifecycle pack (ADR-088, amended by ADR-088
//! Amendment 1).
//!
//! Contributes three note kinds (`commit`, `issue`, `pull_request`) that make
//! repository provenance queryable through the KG graph, and one
//! agent-facing verb, `git.digest(source, project?, max_items?, include?)`
//! (`handlers`), that drives the batch, cursor-based ingester (`ingest`)
//! against either a local path or a remote `https://` URL (cloned/fetched
//! into a daemon-owned scratch cache, `cache`). See
//! `docs/adr/ADR-088-git-lifecycle-pack.md` and
//! `docs/adr/ADR-088-amendment-1-git-digest.md`.
//!
//! | Verb | Args | What it does |
//! | ---- | ---- | ------------ |
//! | `git.digest` | `source`, `project?`, `max_items?`, `include?` | Ingest commit/issue/PR provenance from a local path or `https://` URL, bounded and cursor-resumable |
//!
//! `kkernel git-ingest` remains the unbounded, all-kinds admin CLI path over
//! the same shared `ingest::run_ingest` core.

pub mod cache;
pub mod handlers;
pub mod hook;
pub mod ingest;
mod pack;
#[cfg(test)]
mod recovery_tests;
pub mod refs;
pub mod source;
pub(crate) mod vocab;

pub use pack::GitPack;
