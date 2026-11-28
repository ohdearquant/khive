use std::io::Cursor;

use khive_pack_formal::FormalPack;
use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, VerbRegistry, VerbRegistryBuilder};
use khive_subject::{
    DependencyKind, Extractor, FormalExtractor, LeanJsonlScanner, OntologySpec, Scanner, SpecError,
    SpecPart, TaxonomyConfig,
};
use khive_types::{
    EdgeEndpointRule, EdgeRelation, EndpointKind, EntityKind, EntityTypeDef, EntityTypeRegistry,
};

fn formal_spec() -> OntologySpec {
    OntologySpec::formal_math().expect("the builtin formal ontology is complete")
}

fn actual_registry() -> VerbRegistry {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(FormalPack::new(runtime));
    builder.build().expect("compose actual kg/formal packs")
}

#[test]
fn missing_ontology_parts_are_named_and_cannot_construct_a_spec() {
    let complete = formal_spec();
    for (vocabulary, relations, taxonomy, expected) in [
        (
            None,
            Some(complete.relations().clone()),
            Some(*complete.taxonomy()),
            SpecPart::Vocabulary,
        ),
        (
            Some(complete.vocabulary().to_vec()),
            None,
            Some(*complete.taxonomy()),
            SpecPart::Relations,
        ),
        (
            Some(complete.vocabulary().to_vec()),
            Some(complete.relations().clone()),
            None,
            SpecPart::Taxonomy,
        ),
    ] {
        let result = OntologySpec::try_new(vocabulary, relations, taxonomy);
        assert!(
            matches!(&result, Err(SpecError::MissingPart { part }) if *part == expected),
            "a missing ontology part must refuse with its own identity: expected {expected:?}, got {result:?}"
        );
    }

    let empty_vocabulary = OntologySpec::try_new(
        Some(Vec::new()),
        Some(complete.relations().clone()),
        Some(*complete.taxonomy()),
    );
    assert!(matches!(
        empty_vocabulary,
        Err(SpecError::MissingPart {
            part: SpecPart::Vocabulary
        })
    ));
    let mut empty_relations = complete.relations().clone();
    empty_relations.edge_rules.clear();
    let empty_relations = OntologySpec::try_new(
        Some(complete.vocabulary().to_vec()),
        Some(empty_relations),
        Some(*complete.taxonomy()),
    );
    assert!(matches!(
        empty_relations,
        Err(SpecError::MissingPart {
            part: SpecPart::Relations
        })
    ));
}

#[test]
fn formal_taxonomy_and_relation_roles_have_literal_values() {
    let spec = formal_spec();
    assert_eq!(
        *spec.taxonomy(),
        TaxonomyConfig {
            separator: '.',
            discipline_segment: 1,
            subdiscipline_segment: 2
        },
        "formal taxonomy must retain the declared separator and distinct zero-based positions"
    );
    let module: Vec<_> = "Mathlib.Algebra.Group.Basic"
        .split(spec.taxonomy().separator)
        .collect();
    assert_eq!(module[spec.taxonomy().discipline_segment], "Algebra");
    assert_eq!(module[spec.taxonomy().subdiscipline_segment], "Group");
    assert_eq!(spec.relations().reference, EdgeRelation::DependsOn);
    assert_eq!(spec.relations().parent_projection, EdgeRelation::Extends);
}

#[test]
fn ontology_rejects_alias_collisions_and_undeclared_mapped_relations() {
    let complete = formal_spec();
    let collision = OntologySpec::try_new(
        Some(vec![
            EntityTypeDef {
                kind: EntityKind::Concept,
                type_name: "first",
                aliases: &["shared name"],
            },
            EntityTypeDef {
                kind: EntityKind::Concept,
                type_name: "second",
                aliases: &["SHARED-NAME"],
            },
        ]),
        Some(complete.relations().clone()),
        Some(*complete.taxonomy()),
    );
    assert!(
        matches!(collision, Err(SpecError::InvalidVocabulary { .. })),
        "normalized alias collisions must not become last-registration-wins"
    );
    let mut relations = complete.relations().clone();
    relations.reference = EdgeRelation::Supports;
    assert!(matches!(
        OntologySpec::try_new(
            Some(complete.vocabulary().to_vec()),
            Some(relations),
            Some(*complete.taxonomy()),
        ),
        Err(SpecError::InvalidRelations { .. })
    ));
}

#[test]
fn formal_alias_resolution_keeps_registry_whitespace_and_available_tokens() {
    let spec = formal_spec();
    for (raw, expected) in [
        ("def", "definition"),
        ("inductive", "structure"),
        ("class", "structure"),
        ("lemma", "theorem"),
        ("\tDEF\n", "definition"),
        ("\tTHEOREM\n", "theorem"),
    ] {
        let result = spec.resolve_kind(raw);
        assert!(
            matches!(&result, Ok(kind) if kind.base_kind == EntityKind::Concept && kind.entity_type == expected),
            "formal aliases must use the existing registry normalization, including outer whitespace: {raw:?} -> {result:?}"
        );
    }
    for raw in ["goal", "concept", "not_a_decl"] {
        let result = spec.resolve_kind(raw);
        assert!(
            matches!(&result, Err(SpecError::UnknownKind { kind, available })
                if kind == raw && available == &["theorem", "definition", "structure", "instance", "axiom"]),
            "unknown ingestion kinds must identify the available five canonical tokens: {result:?}"
        );
    }
}

#[tokio::test]
async fn formal_vocabulary_matches_the_actual_composed_registry() {
    let registry = actual_registry();
    let composed = EntityTypeRegistry::with_extra(registry.all_entity_types());
    let spec = formal_spec();
    assert_eq!(
        spec.vocabulary()
            .iter()
            .map(|d| d.type_name)
            .collect::<Vec<_>>(),
        ["theorem", "definition", "structure", "instance", "axiom"],
        "formal ingestion must expose its five canonical declaration tokens"
    );
    for definition in spec.vocabulary() {
        assert_eq!(definition.kind, EntityKind::Concept);
        let actual = composed
            .definitions()
            .iter()
            .find(|d| d.type_name == definition.type_name)
            .expect("declared subtype must exist in the composed registry");
        assert_eq!(actual.kind, definition.kind);
        assert_eq!(actual.aliases, definition.aliases);
    }
    let expected: [(&str, &[&str]); 6] = [
        ("theorem", &["lemma", "proposition", "corollary"]),
        ("definition", &["def"]),
        ("structure", &["inductive", "struct", "class"]),
        ("instance", &[]),
        ("axiom", &[]),
        ("goal", &["proof_goal"]),
    ];
    for (name, aliases) in expected {
        let actual = composed
            .definitions()
            .iter()
            .find(|d| d.type_name == name)
            .expect("existing formal subtype must remain registered");
        assert_eq!(actual.kind, EntityKind::Concept);
        assert_eq!(
            actual.aliases, aliases,
            "existing aliases changed for {name}"
        );
        for spelling in std::iter::once(name).chain(aliases.iter().copied()) {
            let resolved = composed
                .resolve(EntityKind::Concept, Some(spelling))
                .unwrap();
            assert_eq!(resolved.entity_type.as_deref(), Some(name));
        }
    }
}

fn typed_triples(rules: &[EdgeEndpointRule]) -> Vec<(&str, &str, &str)> {
    rules
        .iter()
        .map(|rule| match (rule.source, rule.target) {
            (
                EndpointKind::EntityOfType {
                    kind: "concept",
                    entity_type: source,
                },
                EndpointKind::EntityOfType {
                    kind: "concept",
                    entity_type: target,
                },
            ) => (rule.relation.as_str(), source, target),
            endpoints => {
                panic!("formal endpoint rules must match the concept subtype field: {endpoints:?}")
            }
        })
        .collect()
}

#[tokio::test]
async fn formal_rules_match_real_pack_registration_and_literal_contract() {
    let expected = vec![
        ("depends_on", "theorem", "theorem"),
        ("depends_on", "theorem", "definition"),
        ("depends_on", "theorem", "structure"),
        ("depends_on", "theorem", "axiom"),
        ("depends_on", "definition", "definition"),
        ("depends_on", "definition", "structure"),
        ("depends_on", "definition", "theorem"),
        ("depends_on", "definition", "axiom"),
        ("depends_on", "instance", "structure"),
        ("depends_on", "instance", "definition"),
        ("depends_on", "goal", "theorem"),
        ("depends_on", "goal", "definition"),
        ("depends_on", "goal", "structure"),
        ("depends_on", "goal", "axiom"),
        ("instance_of", "instance", "structure"),
        ("extends", "structure", "structure"),
        ("extends", "definition", "definition"),
        ("variant_of", "theorem", "theorem"),
        ("variant_of", "definition", "definition"),
        ("variant_of", "goal", "theorem"),
        ("variant_of", "goal", "definition"),
    ];
    let spec = formal_spec();
    assert_eq!(
        typed_triples(&spec.relations().edge_rules),
        expected,
        "the ontology must retain all twenty-one literal formal endpoint rules"
    );
    let registry = actual_registry();
    let actual: Vec<_> = registry
        .all_edge_rules_with_packs()
        .into_iter()
        .filter_map(|(owner, rule)| (owner == "formal").then_some(rule))
        .collect();
    assert_eq!(
        typed_triples(&actual),
        expected,
        "actual formal registration must match the same complete literal contract"
    );
    assert_eq!(actual, spec.relations().edge_rules);
}

#[test]
fn scanner_and_extractor_are_reusable_without_a_server_or_stale_membership() {
    let scanner = LeanJsonlScanner;
    let extractor = FormalExtractor;
    let spec = formal_spec();
    let first = scanner.scan(Cursor::new(
        "{\"name\":\"First.use\",\"kind\":\"def\",\"type\":\"Nat\",\"refs\":[\"First.base\"],\"doc\":\" α \",\"module\":\"Mathlib.Algebra.Group\"}\n\
         {\"name\":\"First.base\",\"kind\":\"def\",\"type\":\"Nat\",\"refs\":[],\"module\":\"Mathlib.Algebra.Group\"}\n"
    )).expect("first whole corpus");
    let first = extractor
        .extract(&first, &spec)
        .expect("first pure extraction");
    assert_eq!(
        first
            .entities
            .iter()
            .map(|e| e.name.as_str())
            .collect::<Vec<_>>(),
        ["First.use", "First.base"]
    );
    assert_eq!(first.entities[0].doc.as_deref(), Some(" α "));
    assert_eq!(first.entities[0].entity_type, "definition");
    assert_eq!(first.entities[0].type_expr, "Nat");
    assert_eq!(first.edges.len(), 1);
    assert_eq!(first.edges[0].source, "First.use");
    assert_eq!(first.edges[0].target, "First.base");
    assert_eq!(first.edges[0].relation, EdgeRelation::DependsOn);
    assert_eq!(
        first.edges[0].dependency_kind,
        Some(DependencyKind::Runtime)
    );

    let second = scanner.scan(Cursor::new(
        "{\"name\":\"Second.only\",\"kind\":\"theorem\",\"type\":\"True\",\"refs\":[\"First.base\"],\"doc\":\"\",\"module\":\"Mathlib.Logic.Basic\"}\n"
    )).expect("second whole corpus");
    let second = extractor
        .extract(&second, &spec)
        .expect("second pure extraction");
    assert_eq!(second.entities.len(), 1);
    assert_eq!(second.entities[0].name, "Second.only");
    assert_eq!(second.entities[0].entity_type, "theorem");
    assert_eq!(second.entities[0].doc.as_deref(), Some(""));
    assert!(
        second.edges.is_empty(),
        "membership from a prior corpus must not leak into a new extraction"
    );
}
