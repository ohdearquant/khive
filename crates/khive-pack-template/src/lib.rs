//! khive-pack-template — reference scaffold for new packs (ADR-023 §8).
//!
//! See `docs/design.md` for the step-by-step guide to creating a new pack.

pub mod handlers;
pub mod vocab;

use async_trait::async_trait;
use serde_json::Value;

use khive_runtime::pack::PackRuntime;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry};
use khive_types::{HandlerDef, Pack, Visibility};

/// Canonical pack name. Must match the factory below and `PackFactory::name()`.
const PACK_NAME: &str = "template";

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

/// Handler table. Add one `HandlerDef` per verb.
///
/// `Visibility::Verb`       = exposed on the MCP `request` tool (agent-facing).
/// `Visibility::Subhandler` = CLI-only / internal; not on the MCP wire.
static TEMPLATE_HANDLERS: [HandlerDef; 1] = [HandlerDef {
    name: "template.my_verb",
    description: "Example pack-prefixed verb (ADR-023: non-kg packs must use pack.verb naming).",
    visibility: Visibility::Verb,
    category: khive_types::VerbCategory::Directive,
    params: &[],
}];

impl TemplatePack {
    /// Constructs the template pack with a runtime handle.
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self { runtime }
    }
    fn runtime(&self) -> &KhiveRuntime {
        &self.runtime
    }
}

// ── ADR-027: inventory self-registration ─────────────────────────────────────
//
// This block registers the pack factory so the linker includes it in the
// binary's inventory at startup. One `inventory::submit!` per pack crate.

struct TemplatePackFactory;

impl khive_runtime::PackFactory for TemplatePackFactory {
    fn name(&self) -> &'static str {
        PACK_NAME
    }
    fn requires(&self) -> &'static [&'static str] {
        &["kg"]
    }
    fn create(&self, runtime: KhiveRuntime) -> Box<dyn khive_runtime::PackRuntime> {
        Box::new(TemplatePack::new(runtime))
    }
}

inventory::submit! { khive_runtime::PackRegistration(&TemplatePackFactory) }

// ── PackRuntime impl ─────────────────────────────────────────────────────────

#[async_trait]
impl PackRuntime for TemplatePack {
    fn name(&self) -> &str {
        <TemplatePack as Pack>::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        <TemplatePack as Pack>::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        <TemplatePack as Pack>::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        &TEMPLATE_HANDLERS
    }
    fn requires(&self) -> &'static [&'static str] {
        <TemplatePack as Pack>::REQUIRES
    }

    /// Dispatch a verb call. Add a match arm for each entry in `HANDLERS`.
    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        _registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        match verb {
            "template.my_verb" => handlers::handle_my_verb(self.runtime(), token, params).await,
            _ => Err(RuntimeError::InvalidInput(format!(
                "{PACK_NAME} pack does not handle verb {verb:?}"
            ))),
        }
    }
}
