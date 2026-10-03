//! Blob verb pack — thin MCP verbs over the existing `BlobStore` CAS.
//!
//! Direct put, staged upload, get and stat verbs expose the installed
//! content-addressed service. `put` and `stat` use the raw store's mutation and
//! metadata capabilities; `get` enters the paired runtime `BlobHydrator` for
//! backend-verified, shared-admission whole-buffer reads. This pack adds no
//! entity/note kind, schema, or storage backend of its own. Physical
//! `delete`/`orphan_sweep` stay admin-only (ADR-111 §8) and are deliberately
//! not verbs here.

mod file_handlers;
pub mod handlers;
mod pack;
pub mod uploads;
pub mod vocab;

pub use uploads::UploadManager;

use std::sync::Arc;

use khive_runtime::KhiveRuntime;
use khive_types::{HandlerDef, Pack};

pub(crate) use pack::BLOB_HANDLERS;

/// Canonical pack name — verbs are exposed as `blob.<verb>`.
pub(crate) const PACK_NAME: &str = "blob";

/// Blob pack: thin verb surface over the runtime's installed `BlobStore`.
pub struct BlobPack {
    runtime: KhiveRuntime,
    uploads: Arc<UploadManager>,
    file_transfers_enabled: bool,
}

impl Pack for BlobPack {
    const NAME: &'static str = PACK_NAME;
    const NOTE_KINDS: &'static [&'static str] = vocab::NOTE_KINDS;
    const ENTITY_KINDS: &'static [&'static str] = vocab::ENTITY_KINDS;
    const HANDLERS: &'static [HandlerDef] = &BLOB_HANDLERS;
    const REQUIRES: &'static [&'static str] = &[];
}

impl BlobPack {
    pub fn new(runtime: KhiveRuntime) -> Self {
        let uploads = Arc::new(UploadManager::new(runtime.clone()));
        let file_transfers_enabled = runtime.config().blob.file_transfers;
        Self {
            runtime,
            uploads,
            file_transfers_enabled,
        }
    }

    pub(crate) fn runtime(&self) -> &KhiveRuntime {
        &self.runtime
    }
}
