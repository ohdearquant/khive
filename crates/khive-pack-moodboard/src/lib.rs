//! Experimental raster ingest, exact visual retrieval, and pairwise preference learning over
//! Khive substrates.
//!
//! The pack is intentionally opt-in. Original bytes live in the configured
//! [`khive_storage::BlobStore`], graph identity lives in `visual_asset`
//! artifact entities, and immutable Lattice descriptor spaces live in named
//! Khive vector tables. In multi-backend configurations, entities use the
//! shared core graph while vector tables stay on the pack-selected runtime.
//! It does not register a text embedder provider.

pub mod handlers;
mod model;
mod pack;
mod preference;
mod preference_artifact;
mod preference_handlers;
mod preprocess;
pub mod vocab;

use khive_runtime::KhiveRuntime;
use khive_types::{EntityTypeDef, HandlerDef, Pack};

use model::VisionModelState;

pub use preference_artifact::{
    legacy_preference_model_count, verify_legacy_preference_attachments,
    MoodboardLegacyPreferenceVerifier, VerifiedModelNetworkAttachment,
};

pub(crate) const PACK_NAME: &str = "moodboard";
pub(crate) const LATTICE_VERSION: &str = "0.9.0";

/// Opt-in Moodboard visual-retrieval and preference-learning pack.
pub struct MoodboardPack {
    runtime: KhiveRuntime,
    model: VisionModelState,
}

impl Pack for MoodboardPack {
    const NAME: &'static str = PACK_NAME;
    const NOTE_KINDS: &'static [&'static str] = vocab::NOTE_KINDS;
    const ENTITY_KINDS: &'static [&'static str] = vocab::ENTITY_KINDS;
    const HANDLERS: &'static [HandlerDef] = &vocab::MOODBOARD_HANDLERS;
    const ENTITY_TYPES: &'static [EntityTypeDef] = &vocab::MOODBOARD_ENTITY_TYPES;
    const REQUIRES: &'static [&'static str] = &["kg"];
}

impl MoodboardPack {
    /// Bind a Moodboard pack to one runtime.
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self {
            runtime,
            model: VisionModelState::default(),
        }
    }

    pub(crate) fn runtime(&self) -> &KhiveRuntime {
        &self.runtime
    }

    pub(crate) fn model_state(&self) -> &VisionModelState {
        &self.model
    }
}

#[cfg(test)]
mod tests {
    use super::LATTICE_VERSION;

    #[test]
    fn lattice_provenance_version_matches_all_exact_workspace_pins() {
        let manifest: toml::Value = include_str!("../../Cargo.toml")
            .parse()
            .expect("workspace Cargo.toml must parse");
        let dependencies = manifest["workspace"]["dependencies"]
            .as_table()
            .expect("workspace.dependencies must be a table");

        for dependency in ["lattice-embed", "lattice-inference", "lattice-fann"] {
            let pin = match &dependencies[dependency] {
                toml::Value::String(version) => version.as_str(),
                toml::Value::Table(specification) => specification["version"]
                    .as_str()
                    .expect("lattice dependency table must contain a version"),
                value => panic!("unexpected {dependency} dependency shape: {value:?}"),
            };
            assert_eq!(pin, format!("={LATTICE_VERSION}"), "{dependency}");
        }
    }
}
