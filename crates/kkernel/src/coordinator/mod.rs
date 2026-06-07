//! SubstrateCoordinator — cross-backend dispatch layer (D1-D6).

mod dispatch;
mod locator;
mod registry;

pub use dispatch::{BackendSearchResult, SubstrateCoordinator};
pub use locator::LocatorCache;
pub use registry::{BackendEntry, BackendRegistry};

#[cfg(test)]
mod tests;
