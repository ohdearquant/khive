//! Trusted transactional administration over authenticated durable grants.
//!
//! The composition root provisions identity admission, keys, catalog and clock.
//! This library installs no wire adapter or evidence-transaction admission seam.

mod authorize;
mod bootstrap;
mod current;
mod history;
mod model;
mod mutation;
mod request;

pub use bootstrap::{
    AuthorityHistory, BootstrapHistoricalChange, BootstrapManifestInput, BootstrapRuntime,
    BootstrapSlotInput, InitializationResult, InitializationSeed, InitializationSummary,
    LoadedAuthority, ValidatedBootstrapManifest,
};
pub use history::{HistoricalAdministration, HistoricalChange};
pub use model::{
    AdministrationOperation, AdmissionBinding, AdmissionContext, AdmissionVerifier, AdmittedCaller,
    AuthorityClock, AuthorityConfiguration, AuthorityRefusal, AuthorityResult, AuthorityStore,
    BodyDigest, MutationResult, PublicValidity, RecordOutcome, RecordRef, RecordedState,
};
pub use request::{GrantItem, GrantParent, GrantRequest, RetireAnchorRequest, RevokeRequest};

#[derive(Debug, thiserror::Error)]
pub enum AuthorityError {
    #[error("authority refused: {0}")]
    Refused(#[from] AuthorityRefusal),
    #[error(transparent)]
    Storage(#[from] khive_storage::StorageError),
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod bootstrap_tests;
