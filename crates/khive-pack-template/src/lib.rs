//! Reference scaffold for a dynamically registered khive pack.

pub mod handlers;
mod pack;
pub mod vocab;

use khive_runtime::KhiveRuntime;
use khive_types::{HandlerDef, Pack};

pub(crate) use pack::TEMPLATE_HANDLERS;

/// Canonical pack name. Must match the factory below and `PackFactory::name()`.
pub(crate) const PACK_NAME: &str = "template";

/// Example pack joining vocabulary, handlers, dependencies, and a runtime handle.
///
/// See `crates/khive-pack-template/docs/api/pack-scaffold.md`.
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
    /// Bind the template pack to the runtime used by its handlers.
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self { runtime }
    }
    pub(crate) fn runtime(&self) -> &KhiveRuntime {
        &self.runtime
    }
}
