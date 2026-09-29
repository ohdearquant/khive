//! Pack-owned ANN candidate source for the note-substrate search vector leg.
//!
//! The runtime owns this neutral seam so its note search does not depend on a
//! higher-layer pack. The memory pack installs the provider for its backend.

use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use khive_storage::types::VectorSearchHit;

use crate::{KhiveRuntime, NamespaceToken, RuntimeResult};

static ANN_ROUTE_TOTAL: AtomicU64 = AtomicU64::new(0);
static FALLBACK_ROUTE_TOTAL: AtomicU64 = AtomicU64::new(0);

/// An installed graph candidate source for one database backend.
///
/// `None` means this model has no graph this consumer may serve and licenses the
/// existing exact sqlite-vec fallback. Errors propagate rather than silently
/// changing route. Implementations merge the registered consumer's fresh tail
/// before returning candidates.
#[async_trait]
pub trait NoteSearchAnnProvider: Send + Sync {
    /// Backend identity whose note vectors the graph indexes.
    fn backend_id(&self) -> &str;

    /// Match the actual opened backend, not only its logical name. Two
    /// independently opened databases may both have the default `main` ID.
    fn serves_backend(&self, runtime: &KhiveRuntime) -> bool;

    /// Return ANN plus fresh-tail candidates in the same score and ordering
    /// contract as `VectorStore::search`.
    async fn search(
        &self,
        token: &NamespaceToken,
        model: &str,
        query_embedding: &[f32],
        top_k: u32,
    ) -> RuntimeResult<Option<Vec<VectorSearchHit>>>;
}

fn increment(counter: &AtomicU64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        Some(value.saturating_add(1))
    });
}

pub(crate) fn record_ann_route() {
    increment(&ANN_ROUTE_TOTAL);
}

pub(crate) fn record_fallback_route() {
    increment(&FALLBACK_ROUTE_TOTAL);
}

/// Process-lifetime note-search vector route counts. The pair does not reset
/// when a database pool is reopened or diagnostics is collected.
pub fn route_totals() -> (u64, u64) {
    (
        ANN_ROUTE_TOTAL.load(Ordering::Relaxed),
        FALLBACK_ROUTE_TOTAL.load(Ordering::Relaxed),
    )
}
