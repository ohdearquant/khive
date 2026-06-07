//! khive-pack-template — reference scaffold for new packs.
//!
//! See `docs/design.md` for the step-by-step guide to creating a new pack.

pub mod handlers;
mod pack;
pub mod vocab;

use khive_runtime::KhiveRuntime;
use khive_types::{HandlerDef, Pack};

pub(crate) use pack::TEMPLATE_HANDLERS;

/// Canonical pack name. Must match the factory below and `PackFactory::name()`.
pub(crate) const PACK_NAME: &str = "template";

/// Template pack — replace with your pack's struct name and logic.
pub struct TemplatePack {
    runtime: KhiveRuntime,
}

impl Pack for TemplatePack {
    const NAME: &'static str = PACK_NAME;
    /// Declare note kinds this pack contributes. Must not overlap with other packs.
    const NOTE_KINDS: &'static [&'static str] = vocab::NOTE_KINDS;
    /// Declare entity kinds this pack contributes. Must not overlap with other packs.
    const ENTITY_KINDS: &'static [&'static str] = vocab::ENTITY_KINDS;
    /// Handler table. Each entry is one verb or subhandler the pack can dispatch.
    const HANDLERS: &'static [HandlerDef] = &TEMPLATE_HANDLERS;
    /// Pack dependencies. The named packs must be in the configured `KHIVE_PACKS` list.
    const REQUIRES: &'static [&'static str] = &["kg"];
}

impl TemplatePack {
    /// Constructs the template pack with a runtime handle.
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self { runtime }
    }
    pub(crate) fn runtime(&self) -> &KhiveRuntime {
        &self.runtime
    }
}
