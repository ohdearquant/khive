//! SubstrateCoordinator — cross-backend dispatch layer (D2-D4).

mod dispatch;
mod locator;
mod ranking;
mod registry;
mod search_profile;
pub mod service;

pub use dispatch::{
    BackendSearchFailure, BackendSearchFailureKind, BackendSearchResult, SubstrateCoordinator,
};
pub use locator::LocatorCache;
pub use registry::{BackendEntry, BackendRegistrationError, BackendRegistry};
pub use service::SubstrateCoordinatorService;

fn ensure_search_active() -> khive_runtime::RuntimeResult<()> {
    khive_storage::ensure_request_read_active("search")
        .map_err(|error| khive_runtime::RuntimeError::from(error).before_requested_write())
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SearchMetadataPhase {
    Finalize,
    Hydrate,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SearchMetadataRead {
    pub(super) phase: SearchMetadataPhase,
    pub(super) substrate: khive_pack_kg::handlers::SearchSubstrate,
    pub(super) id: uuid::Uuid,
}

#[cfg(test)]
tokio::task_local! {
    static SEARCH_METADATA_OBSERVER: std::sync::Arc<dyn Fn(SearchMetadataRead) + Send + Sync>;
}

#[cfg(test)]
pub(super) async fn scope_search_metadata_observer<F: std::future::Future>(
    observer: std::sync::Arc<dyn Fn(SearchMetadataRead) + Send + Sync>,
    future: F,
) -> F::Output {
    SEARCH_METADATA_OBSERVER.scope(observer, future).await
}

#[cfg(test)]
fn notify_search_metadata_read(
    phase: SearchMetadataPhase,
    substrate: khive_pack_kg::handlers::SearchSubstrate,
    id: uuid::Uuid,
) {
    let _ = SEARCH_METADATA_OBSERVER.try_with(|observe| {
        observe(SearchMetadataRead {
            phase,
            substrate,
            id,
        });
    });
}

#[cfg(test)]
mod fusion_label_tests;
#[cfg(test)]
mod ranking_tests;
#[cfg(test)]
mod search_profile_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod search_audit_tests;

#[cfg(test)]
mod search_cancellation_tests;

// This separate hook pauses before the actual read without holding a store
// connection. The existing observer records only completed metadata reads.
#[cfg(test)]
type SearchMetadataHook = std::sync::Arc<
    dyn Fn(SearchMetadataRead) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        + Send
        + Sync,
>;

#[cfg(test)]
tokio::task_local! {
    static SEARCH_METADATA_BEFORE: SearchMetadataHook;
}

#[cfg(test)]
async fn scope_before_search_metadata<F: std::future::Future>(
    hook: SearchMetadataHook,
    future: F,
) -> F::Output {
    SEARCH_METADATA_BEFORE.scope(hook, future).await
}

#[cfg(test)]
async fn before_search_metadata_read(
    phase: SearchMetadataPhase,
    substrate: khive_pack_kg::handlers::SearchSubstrate,
    id: uuid::Uuid,
) {
    let paused = SEARCH_METADATA_BEFORE
        .try_with(|hook| {
            hook(SearchMetadataRead {
                phase,
                substrate,
                id,
            })
        })
        .ok();
    if let Some(paused) = paused {
        paused.await;
    }
}
