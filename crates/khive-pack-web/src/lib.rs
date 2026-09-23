//! Web pack (ADR-191): `site`/`page`/`resource` ontology and the
//! `fetch`/`extract`/`ingest`/`search`/`refresh` verbs over HTTP(S) egress
//! policy, the runtime's blob store, and its create/update/link seam.
//! Supersedes ADR-175's local-manifest-only pack.

mod confinement;
mod egress;
mod entities;
mod extract;
mod fetch;
mod identity;
mod ingest;
mod namespace;
mod pack;
mod receipt;
mod refresh;
mod search;
mod vocab;

pub use pack::WebPack;

use khive_runtime::{KhiveRuntime, RuntimeError};
use khive_storage::BlobStore;
use std::sync::Arc;

/// The installed `BlobStore`, or an explicit `Unconfigured` refusal — the
/// same pattern and the same message shape `khive-pack-blob` uses, so an
/// operator sees one consistent error regardless of which pack's verb hit
/// the missing configuration first.
pub(crate) fn blob_store(runtime: &KhiveRuntime) -> Result<Arc<dyn BlobStore>, RuntimeError> {
    runtime.blob_store().ok_or_else(|| {
        RuntimeError::Unconfigured(
            "no BlobStore installed on this server (configure [storage.blob] in khive.toml, or KHIVE_BLOB_ROOT)"
                .to_string(),
        )
    })
}
