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

/// Authenticates legacy moodboard preference-model bundle/event/FANN
/// evidence for the V21 attachment cutover.
///
/// Implemented by `khive-pack-moodboard` when that pack is compiled in. When
/// no implementation is installed and legacy rows exist, the cutover fails
/// closed rather than silently dropping the legacy column with unmigrated
/// models still on disk.
#[async_trait]
pub trait LegacyPreferenceVerifier: Send + Sync {
    async fn verify_legacy_preference_attachments(
        &self,
        sql: &dyn SqlAccess,
        hydrator: &BlobHydrator,
    ) -> Result<Vec<VerifiedModelNetworkAttachment>, RuntimeError>;
}
