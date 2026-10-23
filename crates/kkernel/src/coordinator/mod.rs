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
