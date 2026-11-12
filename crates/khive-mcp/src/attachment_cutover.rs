//! Host-runtime coordination for ADR-121/ADR-160's V21 attachment cutover.
//!
//! The coordinator itself lives in [`khive_runtime::attachment_cutover`] so
//! embedding hosts can bring a pre-V21 database forward without depending on
//! this crate. This module keeps the one piece that must stay here: the
//! feature-gated legacy-moodboard verifier seam, which keeps the optional
//! `khive-pack-moodboard` dependency out of the runtime.

use std::sync::Arc;

use khive_runtime::LegacyPreferenceVerifier;

pub(crate) use khive_runtime::attachment_cutover::{
    coordinate_attachment_cutover, coordinate_empty_secondary_attachment_cutover,
    legacy_preference_model_count, require_secondary_attachment_empty,
};

#[cfg(test)]
pub(crate) use khive_runtime::attachment_cutover::create_v20_database_fixture;

/// The V21 legacy-moodboard verifier for this build, or `None` when compiled
/// without the `pack-moodboard` feature.
///
/// Boot call sites use this instead of naming `khive-pack-moodboard`
/// directly, so the optional dependency stays confined to this one seam.
#[cfg(feature = "pack-moodboard")]
pub(crate) fn legacy_preference_verifier() -> Option<Arc<dyn LegacyPreferenceVerifier>> {
    Some(Arc::new(
        khive_pack_moodboard::MoodboardLegacyPreferenceVerifier,
    ))
}

/// See the `pack-moodboard` variant above; this build ships no verifier.
#[cfg(not(feature = "pack-moodboard"))]
pub(crate) fn legacy_preference_verifier() -> Option<Arc<dyn LegacyPreferenceVerifier>> {
    None
}

#[cfg(all(test, feature = "pack-moodboard"))]
mod tests {
    use khive_db::migrations::{AttachmentCutoverStatus, ATTACHMENT_CUTOVER_VERSION};
    use khive_db::stores::blob::FsBlobStore;
    use khive_runtime::StorageBackend;
    use khive_storage::BlobStore as _;

    use super::*;

    /// With the `pack-moodboard` feature enabled, a verifier is installed but
    /// no `BlobHydrator` is configured: the pre-existing hydrator-required
    /// error must still fire, unchanged, for that case (only reachable past
    /// the new fail-closed arm once a verifier exists).
    #[tokio::test]
    async fn legacy_model_with_verifier_but_no_hydrator_stays_incomplete_and_gc_refuses_to_run() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("legacy-model.db");
        create_v20_database_fixture(&db_path, "moodboard_model", None);
        let backend = Arc::new(StorageBackend::sqlite_for_test(&db_path).unwrap());
        assert_eq!(
            backend.prepare_core_schema().unwrap(),
            ATTACHMENT_CUTOVER_VERSION - 1
        );

        let verifier: Arc<dyn LegacyPreferenceVerifier> =
            Arc::new(khive_pack_moodboard::MoodboardLegacyPreferenceVerifier);
        let error = coordinate_attachment_cutover(Arc::clone(&backend), None, Some(verifier))
            .await
            .expect_err("legacy model evidence requires configured blob hydration");
        assert!(error.to_string().contains("no BlobHydrator is configured"));
        assert_eq!(
            backend.attachment_cutover_status().unwrap(),
            AttachmentCutoverStatus::Incomplete,
            "a failed application stage must remain durably resumable"
        );

        let store = FsBlobStore::new(temp.path().join("blobs"), 0).unwrap();
        let sweep_error = store
            .transactional_orphan_sweep(backend.sql().as_ref(), false)
            .await
            .expect_err("GC must not run over an incomplete dual representation");
        assert!(sweep_error.to_string().contains("fencing"));
    }
}
