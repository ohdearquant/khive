//! Legacy moodboard preference-model verification, inverted behind a trait.
//!
//! `khive-mcp`'s V21 attachment cutover (ADR-121/ADR-160) must be able to
//! authenticate legacy `khive-pack-moodboard` preference-model bundles
//! without depending on that pack directly — the pack is opt-in and its
//! crate dependency is feature-gated. `khive-runtime` already sits below
//! both `khive-mcp` and `khive-pack-moodboard` in the dependency graph and
//! already owns [`BlobHydrator`], so the trait lives here.

use async_trait::async_trait;
use khive_storage::{ContentRef, SqlAccess};
use uuid::Uuid;

use crate::{BlobHydrator, RuntimeError};

/// One authenticated network role for the attachment cutover coordinator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedModelNetworkAttachment {
    pub model_id: Uuid,
    pub network_content_ref: ContentRef,
    pub size_bytes: u64,
}

/// Restricts implementations of [`LegacyPreferenceVerifier`].
///
/// The verifier trait is `pub` because its one implementor lives in another
/// crate, `khive-pack-moodboard`. It is not an extension point: the public
/// commitment is the method shape the cutover calls, not open implementation.
/// This supertrait is the marker that says so.
///
/// Note on what this does and does not enforce. Rust has no notion of
/// "workspace-visible", so a marker a sibling crate can name is a marker any
/// crate can name. This is the conventional sealing idiom — it prevents
/// accidental implementation and documents intent — not a barrier a determined
/// downstream cannot cross. Hard enforcement is unavailable while the
/// implementor is a separate crate.
#[doc(hidden)]
pub mod sealed {
    pub trait Sealed {}
}

/// Authenticates legacy moodboard preference-model bundle/event/FANN
/// evidence for the V21 attachment cutover.
///
/// Implemented by `khive-pack-moodboard` when that pack is compiled in. When
/// no implementation is installed and legacy rows exist, the cutover fails
/// closed rather than silently dropping the legacy column with unmigrated
/// models still on disk.
///
/// Sealed via [`sealed::Sealed`]; see that module for the scope of the seal.
#[async_trait]
pub trait LegacyPreferenceVerifier: sealed::Sealed + Send + Sync {
    async fn verify_legacy_preference_attachments(
        &self,
        sql: &dyn SqlAccess,
        hydrator: &BlobHydrator,
    ) -> Result<Vec<VerifiedModelNetworkAttachment>, RuntimeError>;
}
