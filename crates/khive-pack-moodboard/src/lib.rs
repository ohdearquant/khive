//! Experimental raster ingest and exact visual retrieval over Khive substrates.
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
mod preprocess;
pub mod vocab;

use khive_runtime::KhiveRuntime;
use khive_types::{EntityTypeDef, HandlerDef, Pack};

use model::VisionModelState;

pub(crate) const PACK_NAME: &str = "moodboard";

/// Opt-in Moodboard visual-retrieval pack.
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
