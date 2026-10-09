use async_trait::async_trait;
use khive_runtime::{NamespaceToken, PackRuntime, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use khive_types::{EdgeEndpointRule, EdgeRelation, EndpointKind, HandlerDef, Pack};
use serde_json::Value;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

const RULE: EdgeEndpointRule = EdgeEndpointRule {
    relation: EdgeRelation::DependsOn,
    source: EndpointKind::NoteOfKind("task"),
    target: EndpointKind::NoteOfKind("task"),
};

struct FirstPack(Arc<AtomicBool>);
struct SecondPack;

impl Pack for FirstPack {
    const NAME: &'static str = "first";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[];
    const EDGE_RULES: &'static [EdgeEndpointRule] = &[RULE, RULE];
}

impl Pack for SecondPack {
    const NAME: &'static str = "second";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[];
    const EDGE_RULES: &'static [EdgeEndpointRule] = &[RULE];
    const REQUIRES: &'static [&'static str] = &["first"];
}

#[async_trait]
impl PackRuntime for FirstPack {
    fn name(&self) -> &str {
        Self::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        Self::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        Self::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        Self::HANDLERS
    }
    // Deliberately violate the immutable metadata contract after construction:
    // catalog queries must use the rules actually captured for installation.
    fn edge_rules(&self) -> &'static [EdgeEndpointRule] {
        if self.0.load(Ordering::SeqCst) {
            &[]
        } else {
            Self::EDGE_RULES
        }
    }
    async fn dispatch(
        &self,
        _: &str,
        _: Value,
        _: &VerbRegistry,
        _: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        unreachable!("metadata fixture")
    }
}

#[async_trait]
impl PackRuntime for SecondPack {
    khive_runtime::pack_runtime_metadata!();
    async fn dispatch(
        &self,
        _: &str,
        _: Value,
        _: &VerbRegistry,
        _: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        unreachable!("metadata fixture")
    }
}

#[test]
fn rules_preserve_owner_duplicates_topological_order_and_build_snapshot() {
    let changed = Arc::new(AtomicBool::new(false));
    let mut builder = VerbRegistryBuilder::new();
    builder
        .register(SecondPack)
        .register(FirstPack(changed.clone()));
    let registry = builder.build().expect("valid registry");
    changed.store(true, Ordering::SeqCst);
    let expected = vec![("first", RULE), ("first", RULE), ("second", RULE)];
    assert_eq!(registry.all_edge_rules_with_packs(), expected);
    assert_eq!(registry.all_edge_rules(), vec![RULE; 3]);
    assert_eq!(registry.clone().all_edge_rules_with_packs(), expected);
}

#[test]
fn metadata_registry_and_empty_registry_expose_the_same_contract() {
    let mut builder = VerbRegistryBuilder::new();
    builder
        .register(SecondPack)
        .register(FirstPack(Arc::new(AtomicBool::new(false))));
    let metadata = builder.build_metadata().expect("metadata registry");
    assert_eq!(
        metadata.all_edge_rules_with_packs(),
        vec![("first", RULE), ("first", RULE), ("second", RULE)]
    );
    assert_eq!(metadata.all_edge_rules(), vec![RULE; 3]);
    let empty = VerbRegistryBuilder::new().build().expect("empty registry");
    assert!(empty.all_edge_rules_with_packs().is_empty());
    assert!(empty.all_edge_rules().is_empty());
}
