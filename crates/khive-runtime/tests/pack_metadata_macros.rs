//! External-consumer coverage for the metadata macros and their optional consts.

use async_trait::async_trait;
use khive_runtime as runtime;
use khive_types::{
    EdgeEndpointRule, EdgeRelation, EndpointKind, EntityKind, EntityTypeDef, HandlerDef,
    NoteEmbeddingPolicy, NoteEmbeddingPolicySpec, NoteKindSpec, NoteLifecycleSpec, Pack,
    PackColumnAddition, PackColumnAffinity, PackSchemaPlan, VerbCategory, Visibility,
};
use runtime::{PackFactory, PackRuntime};
use serde_json::Value;

struct DeclaredPack;

impl Pack for DeclaredPack {
    const NAME: &'static str = "declared";
    const NOTE_KINDS: &'static [&'static str] = &["declared_note"];
    const ENTITY_KINDS: &'static [&'static str] = &["declared_entity"];
    const BRAIN_CONSUMER_KINDS: &'static [&'static str] = &["declared_consumer"];
    const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
        name: "declared.read",
        description: "Read the declared fixture",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[],
    }];
    const EDGE_RULES: &'static [EdgeEndpointRule] = &[EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source: EndpointKind::NoteOfKind("declared_note"),
        target: EndpointKind::NoteOfKind("declared_note"),
    }];
    const ENTITY_TYPES: &'static [EntityTypeDef] = &[EntityTypeDef {
        kind: EntityKind::Concept,
        type_name: "declared_type",
        aliases: &["declared_alias"],
    }];
    const REQUIRES: &'static [&'static str] = &["kg"];
    const NOTE_KIND_SPECS: &'static [NoteKindSpec] = &[NoteKindSpec {
        kind: "declared_note",
        aliases: &[],
        lifecycle: NoteLifecycleSpec {
            field: "kind_status",
            initial: "active",
            terminal: &["done"],
            transitions: &[("active", "done")],
        },
    }];
    const NOTE_EMBEDDING_POLICIES: &'static [NoteEmbeddingPolicySpec] =
        &[NoteEmbeddingPolicySpec {
            kind: "declared_note",
            policy: NoteEmbeddingPolicy::DefaultModel,
        }];
    const SCHEMA_PLAN: Option<PackSchemaPlan> = Some(PackSchemaPlan {
        pack: "declared",
        statements: &["fixture schema statement"],
    });
    const SCHEMA_COLUMN_ADDITIONS: &'static [PackColumnAddition] = &[PackColumnAddition {
        table: "declared_aux",
        column: "extra",
        affinity: PackColumnAffinity::Text,
    }];
}

#[async_trait]
impl PackRuntime for DeclaredPack {
    runtime::pack_runtime_metadata!();

    async fn dispatch(
        &self,
        _verb: &str,
        _params: Value,
        _registry: &runtime::VerbRegistry,
        _token: &runtime::NamespaceToken,
    ) -> Result<Value, runtime::RuntimeError> {
        Ok(Value::Null)
    }
}

struct DefaultPack;

impl Pack for DefaultPack {
    const NAME: &'static str = "defaults";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[];
}

#[async_trait]
impl PackRuntime for DefaultPack {
    runtime::pack_runtime_metadata!();

    async fn dispatch(
        &self,
        _verb: &str,
        _params: Value,
        _registry: &runtime::VerbRegistry,
        _token: &runtime::NamespaceToken,
    ) -> Result<Value, runtime::RuntimeError> {
        Ok(Value::Null)
    }
}

struct DeclaredFactory;

impl PackFactory for DeclaredFactory {
    runtime::pack_factory_metadata!(DeclaredPack);

    fn create(&self, _runtime: runtime::KhiveRuntime) -> Box<dyn PackRuntime> {
        Box::new(DeclaredPack)
    }
}

#[test]
fn object_safe_metadata_preserves_every_declared_constant() {
    let pack: &dyn PackRuntime = &DeclaredPack;
    assert_eq!(pack.name(), DeclaredPack::NAME);
    assert_eq!(pack.note_kinds(), DeclaredPack::NOTE_KINDS);
    assert_eq!(pack.entity_kinds(), DeclaredPack::ENTITY_KINDS);
    assert_eq!(
        pack.brain_consumer_kinds(),
        DeclaredPack::BRAIN_CONSUMER_KINDS
    );
    assert_eq!(pack.handlers(), DeclaredPack::HANDLERS);
    assert_eq!(pack.edge_rules(), DeclaredPack::EDGE_RULES);
    let actual_types = pack.entity_types();
    let expected_types = DeclaredPack::ENTITY_TYPES;
    assert_eq!(actual_types.len(), expected_types.len());
    for (actual, expected) in actual_types.iter().zip(expected_types) {
        assert_eq!(actual.kind, expected.kind);
        assert_eq!(actual.type_name, expected.type_name);
        assert_eq!(actual.aliases, expected.aliases);
    }
    assert_eq!(pack.requires(), DeclaredPack::REQUIRES);
    assert_eq!(pack.note_kind_specs(), DeclaredPack::NOTE_KIND_SPECS);
    assert_eq!(
        pack.note_embedding_policies(),
        DeclaredPack::NOTE_EMBEDDING_POLICIES
    );
    assert_eq!(
        pack.schema_column_additions(),
        DeclaredPack::SCHEMA_COLUMN_ADDITIONS
    );
    let expected = DeclaredPack::SCHEMA_PLAN.unwrap();
    assert_eq!(pack.schema_plan().pack, expected.pack);
    assert_eq!(pack.schema_plan().statements, expected.statements);
}

#[test]
fn absent_optional_metadata_keeps_empty_runtime_defaults() {
    let pack: &dyn PackRuntime = &DefaultPack;
    assert_eq!(pack.name(), "defaults");
    assert!(pack.brain_consumer_kinds().is_empty());
    assert!(pack.edge_rules().is_empty());
    assert!(pack.entity_types().is_empty());
    assert!(pack.requires().is_empty());
    assert!(pack.note_kind_specs().is_empty());
    assert!(pack.note_embedding_policies().is_empty());
    assert!(pack.schema_column_additions().is_empty());
    assert_eq!(pack.schema_plan().pack, runtime::SchemaPlan::empty().pack);
    assert!(pack.schema_plan().is_empty());
}

#[test]
fn factory_uses_the_constructed_pack_type_metadata() {
    let factory: &dyn PackFactory = &DeclaredFactory;
    assert_eq!(factory.name(), DeclaredPack::NAME);
    assert_eq!(factory.requires(), DeclaredPack::REQUIRES);
}
