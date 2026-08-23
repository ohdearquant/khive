//! Shared test fixture for GTD pack integration tests.

use khive_pack_gtd::GtdPack;
use khive_pack_kg::KgPack;
#[allow(unused_imports)] // REASON: used by metadata.rs tests
use khive_runtime::pack::HandlerDef;
use khive_runtime::{KhiveRuntime, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use serde_json::Value;

pub fn rt() -> KhiveRuntime {
    KhiveRuntime::memory().expect("memory runtime")
}

/// An in-memory runtime configured with a specific `[display] timezone`
/// (ADR-169), for tests that exercise date-only `due` anchoring.
// REASON: used by assign.rs tests only; each test binary sees its own dead_code analysis
#[allow(dead_code)]
pub fn rt_with_timezone(tz: &str) -> KhiveRuntime {
    let display_timezone = tz
        .parse()
        .unwrap_or_else(|e| panic!("{tz:?} must be a valid IANA zone name: {e}"));
    KhiveRuntime::new(khive_runtime::RuntimeConfig {
        display_timezone,
        db_path: None,
        packs: vec!["kg".to_string()],
        brain_profile: None,
        actor_id: None,
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .expect("memory runtime with configured display timezone")
}

/// Test fixture: a `VerbRegistry` containing a freshly registered `GtdPack`,
/// with pass-through metadata methods so existing tests keep working.
pub struct Fixture {
    pub registry: VerbRegistry,
}

impl Fixture {
    pub async fn dispatch(&self, verb: &str, args: Value) -> Result<Value, RuntimeError> {
        self.registry.dispatch(verb, args).await
    }

    // REASON: used by metadata.rs tests only; each test binary sees its own dead_code analysis
    #[allow(dead_code)]
    pub fn verbs(&self) -> Vec<&'static HandlerDef> {
        self.registry.all_verbs()
    }

    // REASON: used by metadata.rs tests only
    #[allow(dead_code)]
    pub fn note_kinds(&self) -> Vec<&'static str> {
        self.registry.all_note_kinds()
    }

    // REASON: part of Fixture helper API; may be used in future tests
    #[allow(dead_code)]
    pub fn entity_kinds(&self) -> Vec<&'static str> {
        self.registry.all_entity_kinds()
    }

    // REASON: used by metadata.rs tests only
    #[allow(dead_code)]
    pub fn name(&self) -> &'static str {
        "gtd"
    }
}

pub fn pack(rt: KhiveRuntime) -> Fixture {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(GtdPack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");
    rt.install_edge_rules(registry.all_edge_rules());
    Fixture { registry }
}

pub async fn assign(pack: &Fixture, body: Value) -> Value {
    pack.dispatch("gtd.assign", body).await.expect("assign ok")
}
