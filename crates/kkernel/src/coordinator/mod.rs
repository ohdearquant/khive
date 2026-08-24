//! SubstrateCoordinator — cross-backend dispatch layer (D2-D4).

mod dispatch;
mod locator;
mod registry;
pub mod service;

pub use dispatch::{
    BackendSearchFailure, BackendSearchFailureKind, BackendSearchResult, SubstrateCoordinator,
};
pub use locator::LocatorCache;
pub use registry::{BackendEntry, BackendRegistry};
pub use service::SubstrateCoordinatorService;

#[cfg(test)]
mod tests;
