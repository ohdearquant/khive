//! Blob verb pack — thin MCP verbs over the existing `BlobStore` CAS.
//!
//! Phase 1 of the blob-consumer surface: three verbs — `blob.put`, `blob.get`,
//! `blob.stat` — mapped directly
//! onto `khive_storage::BlobStore::{put,get,exists}`. This pack adds no
//! entity/note kind, no schema, and no storage backend of its own; it only
//! exposes the pre-existing content-addressed store on the MCP `request`
//! surface. Physical `delete`/`orphan_sweep` stay admin-only (ADR-111 §8)
//! and are deliberately not verbs here.

pub mod handlers;
mod pack;
pub mod vocab;

use khive_runtime::KhiveRuntime;
use khive_types::{HandlerDef, Pack};

pub(crate) use pack::BLOB_HANDLERS;

/// Canonical pack name — verbs are exposed as `blob.<verb>`.
pub(crate) const PACK_NAME: &str = "blob";

/// Blob pack: thin verb surface over the runtime's installed `BlobStore`.
pub struct BlobPack {
    runtime: KhiveRuntime,
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
        Self { runtime }
    }

    pub(crate) fn runtime(&self) -> &KhiveRuntime {
        &self.runtime
    }
}
