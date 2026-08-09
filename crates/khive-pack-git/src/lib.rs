//! `khive-pack-git` — the git-lifecycle pack (ADR-088, amended by ADR-088
//! Amendments 1 and 2, plus ADR-108).
//!
//! Contributes three note kinds (`commit`, `issue`, `pull_request`) that make
//! repository provenance queryable through the KG graph, one read/ingest
//! agent-facing verb, `git.digest(source, project?, max_items?, include?)`
//! (`handlers`), that drives the batch, cursor-based ingester (`ingest`)
//! against either a local path or a remote `https://` URL (cloned/fetched
//! into a daemon-owned scratch cache, `cache`), and three write verbs
//! (`write_handlers`, ADR-108) that shell to system git with hardened,
//! allowlisted argv construction (`write_argv`). See
//! `docs/adr/ADR-088-git-lifecycle-pack.md`,
//! `docs/adr/ADR-088-amendment-1-git-digest.md`,
//! `docs/adr/ADR-088-amendment-2-anchor-identity.md`, and
//! `docs/adr/ADR-108-git-write-surface.md`.
//!
//! | Verb | Args | What it does |
//! | ---- | ---- | ------------ |
//! | `git.digest` | `source`, `project?`, `max_items?`, `include?` | Ingest commit/issue/PR provenance from a local path or `https://` URL, bounded and cursor-resumable |
//! | `git.commit` | `repo`, `message`, `paths?`, `author?` | Stage and commit against a local repo; returns the resulting SHA |
//! | `git.branch` | `repo`, `name`, `from?` | Create a branch, optionally from a named ref/SHA |
//! | `git.push` | `repo`, `branch`, `remote?` | Push a branch to a remote; force-push is always denied |
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
pub mod write_argv;
pub mod write_handlers;
#[cfg(test)]
mod write_handlers_tests;
pub mod write_policy;

pub use pack::GitPack;

use khive_runtime::{NamespaceToken, RequestIdentity, RuntimeError, VerbRegistry};
use serde_json::Value;

/// Re-enter the registry for one Git-pack child operation without replacing
/// the caller's authority or storage namespace with the warm registry's baked
/// defaults.
///
/// `git.digest` performs its writes through the canonical KG verbs so their
/// validation, gate, audit, and indexing contracts remain authoritative. A
/// bare `VerbRegistry::dispatch`, however, would mint a fresh default token:
/// explicit non-local requests would read through `token` but write and audit
/// through `local`, and a warm daemon could substitute its own actor. Carry
/// both the explicit transport namespace and the token-derived identity. The
/// registry consumes/removes `namespace` before forwarding these KG params.
pub(crate) async fn dispatch_from_token(
    registry: &VerbRegistry,
    token: &NamespaceToken,
    verb: &str,
    mut params: Value,
) -> Result<Value, RuntimeError> {
    let object = params.as_object_mut().ok_or_else(|| {
        RuntimeError::InvalidInput(format!(
            "nested {verb} dispatch requires an object argument"
        ))
    })?;
    object.insert(
        "namespace".to_string(),
        Value::String(token.namespace().as_str().to_string()),
    );
    registry
        .dispatch_with_identity(verb, params, Some(RequestIdentity::from_token(token)))
        .await
}
