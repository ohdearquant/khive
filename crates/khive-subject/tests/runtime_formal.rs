use std::collections::BTreeMap;

use khive_pack_formal::FormalPack;
use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use khive_subject::{
    DependencyKind, Extractor, FormalExtractor, LeanJsonlScanner, LinkIntent, OntologySpec,
    OperationPlan, RawDecl, Scanner,
};
use khive_types::{EdgeEndpointRule, EdgeRelation, EndpointKind, EntityKind, EntityTypeRegistry};
use serde_json::{json, Value};

fn runtime(with_formal: bool) -> (KhiveRuntime, VerbRegistry) {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    if with_formal {
        builder.register(FormalPack::new(runtime.clone()));
    }
    let registry = builder.build().expect("valid KG/Formal registry");
    runtime.install_edge_rules(registry.all_edge_rules());
    (runtime, registry)
}

fn projection_records() -> Vec<RawDecl> {
    vec![
        RawDecl {
            name: "Point2".into(),
            kind: "inductive".into(),
            type_expr: "Type".into(),
            refs: vec![],
            doc: None,
            module: "Mathlib.Geometry.Point.Basic".into(),
        },
        RawDecl {
            name: "Point3".into(),
            kind: "inductive".into(),
            type_expr: "Type".into(),
            refs: vec![],
            doc: Some(String::new()),
            module: "Mathlib.Geometry.Point.Basic".into(),
        },
        RawDecl {
            name: "Point3.toPoint2".into(),
            kind: "def".into(),
            type_expr: "(self : Point3) → Point2".into(),
            refs: vec!["Point3".into(), "Point2".into(), "External.Missing".into()],
            doc: Some("  projection α\n".into()),
            module: "Mathlib.Geometry.Point.Basic".into(),
        },
    ]
}

fn extract(records: &[RawDecl]) -> OperationPlan {
    let spec = OntologySpec::formal_math().expect("complete formal ontology");
    let result = FormalExtractor.extract(records, &spec);
    assert!(
        result.is_ok(),
        "valid formal declarations must produce the whole operation plan: {result:?}"
    );
    result.unwrap()
}

async fn create_entities(
    registry: &VerbRegistry,
    plan: &OperationPlan,
) -> BTreeMap<String, String> {
    let mut ids = BTreeMap::new();
    for entity in &plan.entities {
        assert_eq!(entity.base_kind, EntityKind::Concept);
        let mut args = json!({
            "kind": entity.entity_type,
            "name": entity.name,
            "properties": { "type_expr": entity.type_expr, "module": entity.module },
        });
        if let Some(doc) = &entity.doc {
            args["description"] = json!(doc);
        }
        let result = registry.dispatch("create", args).await;
        assert!(
            result.is_ok(),
            "extracted canonical subtype create must be admitted: {}: {result:?}",
            entity.name
        );
        let receipt = result.unwrap();
        let id = receipt["id"].as_str().expect("actual create receipt ID");
        assert!(ids.insert(entity.name.clone(), id.to_owned()).is_none());
    }
    ids
}

fn link_args(edge: &LinkIntent, ids: &BTreeMap<String, String>) -> Value {
    let mut args = json!({
        "source_id": ids[&edge.source],
        "target_id": ids[&edge.target],
        "relation": edge.relation.as_str(),
    });
    if let Some(DependencyKind::Runtime) = edge.dependency_kind {
        args["dependency_kind"] = json!("runtime");
    }
    args
}

async fn entity_snapshot(runtime: &KhiveRuntime, ids: &BTreeMap<String, String>) -> Vec<Value> {
    let token = runtime
        .authorize(Namespace::local())
        .expect("authorize local read");
    let store = runtime.entities(&token).expect("entity store");
    let mut rows = Vec::new();
    for id in ids.values() {
        let row = store
            .get_entity(id.parse().expect("receipt UUID"))
            .await
            .expect("read stored entity")
            .expect("created entity remains present");
        rows.push(serde_json::to_value(row).expect("entity serialization"));
    }
    rows
}

#[tokio::test]
async fn ontology_matches_composed_formal_registry_and_literal_rules() {
    use EdgeRelation::{DependsOn, Extends, InstanceOf, VariantOf};
    let (_runtime, registry) = runtime(true);
    let ontology = OntologySpec::formal_math().expect("formal ontology");
    let literal_types: [(&str, &[&str]); 6] = [
        ("theorem", &["lemma", "proposition", "corollary"]),
        ("definition", &["def"]),
        ("structure", &["inductive", "struct", "class"]),
        ("instance", &[]),
        ("axiom", &[]),
        ("goal", &["proof_goal"]),
    ];
    let composed = EntityTypeRegistry::with_extra(registry.all_entity_types());
    let actual_types: Vec<_> = composed
        .definitions()
        .iter()
        .filter(|entry| {
            literal_types
                .iter()
                .any(|(name, _)| *name == entry.type_name)
        })
        .map(|entry| (entry.kind, entry.type_name, entry.aliases))
        .collect();
    let expected_types: Vec<_> = literal_types
        .iter()
        .map(|(name, aliases)| (EntityKind::Concept, *name, *aliases))
        .collect();
    assert_eq!(
        actual_types, expected_types,
        "composed registry must retain all six literal formal subtype and alias definitions"
    );
    let actual_vocabulary: Vec<_> = ontology
        .vocabulary()
        .iter()
        .map(|entry| (entry.kind, entry.type_name, entry.aliases))
        .collect();
    assert_eq!(
        actual_vocabulary,
        expected_types[..5],
        "subject ingestion vocabulary must be exactly the five canonical concept subtypes"
    );
    for (name, aliases) in literal_types {
        for token in std::iter::once(name).chain(aliases.iter().copied()) {
            let resolved = composed.resolve(EntityKind::Concept, Some(token)).unwrap();
            assert_eq!(resolved.kind, EntityKind::Concept);
            assert_eq!(resolved.entity_type.as_deref(), Some(name));
        }
    }
    let expected_rules: Vec<_> = [
        (DependsOn, "theorem", "theorem"),
        (DependsOn, "theorem", "definition"),
        (DependsOn, "theorem", "structure"),
        (DependsOn, "theorem", "axiom"),
        (DependsOn, "definition", "definition"),
        (DependsOn, "definition", "structure"),
        (DependsOn, "definition", "theorem"),
        (DependsOn, "definition", "axiom"),
        (DependsOn, "instance", "structure"),
        (DependsOn, "instance", "definition"),
        (DependsOn, "goal", "theorem"),
        (DependsOn, "goal", "definition"),
        (DependsOn, "goal", "structure"),
        (DependsOn, "goal", "axiom"),
        (InstanceOf, "instance", "structure"),
        (Extends, "structure", "structure"),
        (Extends, "definition", "definition"),
        (VariantOf, "theorem", "theorem"),
        (VariantOf, "definition", "definition"),
        (VariantOf, "goal", "theorem"),
        (VariantOf, "goal", "definition"),
    ]
    .into_iter()
    .map(|(relation, source, target)| EdgeEndpointRule {
        relation,
        source: EndpointKind::EntityOfType {
            kind: "concept",
            entity_type: source,
        },
        target: EndpointKind::EntityOfType {
            kind: "concept",
            entity_type: target,
        },
    })
    .collect();
    assert_eq!(
        ontology.relations().edge_rules,
        expected_rules,
        "subject compatibility rules must preserve the complete literal 21-rule surface"
    );
    let registered_rules: Vec<_> = registry
        .all_edge_rules_with_packs()
        .into_iter()
        .filter_map(|(owner, rule)| (owner == "formal").then_some(rule))
        .collect();
    assert_eq!(
        registered_rules, expected_rules,
        "actual FormalPack registration must match all literal subtype endpoints and relations"
    );
}

#[tokio::test]
async fn extracted_formal_operations_persist_exact_entities_and_edges() {
    let jsonl = concat!(
        r#"{"name":"Point2","kind":"inductive","type":"Type","refs":[],"module":"Mathlib.Geometry.Point.Basic"}"#,
        "\n",
        r#"{"name":"Point3","kind":"inductive","type":"Type","refs":[],"doc":"","module":"Mathlib.Geometry.Point.Basic"}"#,
        "\n",
        r#"{"name":"Point3.toPoint2","kind":"def","type":"(self : Point3) → Point2","refs_in_corpus":["Point3","Point2","External.Missing"],"doc":"  projection α\n","module":"Mathlib.Geometry.Point.Basic"}"#,
        "\n",
    );
    let scanned = LeanJsonlScanner
        .scan(jsonl.as_bytes())
        .expect("valid literal JSONL");
    assert_eq!(scanned, projection_records());
    for records in [projection_records(), scanned] {
        let plan = extract(&records);
        let entity_projection: Vec<_> = plan
            .entities
            .iter()
            .map(|entity| {
                json!([
                    entity.name,
                    entity.base_kind.name(),
                    entity.entity_type,
                    entity.type_expr,
                    entity.doc,
                    entity.module
                ])
            })
            .collect();
        let expected_entities = json!([
            [
                "Point2",
                "concept",
                "structure",
                "Type",
                null,
                "Mathlib.Geometry.Point.Basic"
            ],
            [
                "Point3",
                "concept",
                "structure",
                "Type",
                "",
                "Mathlib.Geometry.Point.Basic"
            ],
            [
                "Point3.toPoint2",
                "concept",
                "definition",
                "(self : Point3) → Point2",
                "  projection α\n",
                "Mathlib.Geometry.Point.Basic"
            ]
        ]);
        assert_eq!(
            json!(entity_projection), expected_entities,
            "extraction must preserve exactly three literal typed declarations, without a formula entity"
        );
        let expected_intents = vec![
            LinkIntent {
                source: "Point3.toPoint2".into(),
                target: "Point3".into(),
                relation: EdgeRelation::DependsOn,
                dependency_kind: Some(DependencyKind::Runtime),
            },
            LinkIntent {
                source: "Point3.toPoint2".into(),
                target: "Point2".into(),
                relation: EdgeRelation::DependsOn,
                dependency_kind: Some(DependencyKind::Runtime),
            },
            LinkIntent {
                source: "Point3".into(),
                target: "Point2".into(),
                relation: EdgeRelation::Extends,
                dependency_kind: None,
            },
        ];
        assert_eq!(
            plan.edges, expected_intents,
            "extraction must emit literal consumer dependencies and parent projection, excluding the external reference"
        );
        let (runtime, registry) = runtime(true);
        let ids = create_entities(&registry, &plan).await;
        for edge in &plan.edges {
            let result = registry.dispatch("link", link_args(edge, &ids)).await;
            assert!(
                result.is_ok(),
                "formal-loaded runtime must admit every emitted allowed link: {edge:?}: {result:?}"
            );
        }
        let rows = entity_snapshot(&runtime, &ids).await;
        let stored_projection: Vec<_> = rows
            .iter()
            .map(|row| {
                json!([
                    row["name"],
                    row["kind"],
                    row["entity_type"],
                    row["properties"]["type_expr"],
                    row["description"],
                    row["properties"]["module"]
                ])
            })
            .collect();
        assert_eq!(
            json!(stored_projection),
            expected_entities,
            "stored declarations must retain literal subtype, type expression, doc and module"
        );
        let token = runtime.authorize(Namespace::local()).unwrap();
        let entities = runtime.entities(&token).unwrap();
        assert_eq!(
            entities
                .count_entities("local", Default::default())
                .await
                .unwrap(),
            3
        );
        let graph = runtime.graph(&token).unwrap();
        assert_eq!(graph.count_edges(Default::default()).await.unwrap(), 3);
        let edges = graph
            .query_edges(Default::default(), vec![], Default::default())
            .await
            .unwrap();
        let names: BTreeMap<_, _> = ids
            .iter()
            .map(|(name, id)| (id.as_str(), name.as_str()))
            .collect();
        let mut actual_edges: Vec<_> = edges
            .items
            .iter()
            .map(|edge| {
                json!([
                    names[edge.source_id.to_string().as_str()],
                    edge.relation.as_str(),
                    names[edge.target_id.to_string().as_str()],
                    edge.metadata
                ])
            })
            .collect();
        let mut expected_edges = vec![
            json!(["Point3.toPoint2", "depends_on", "Point3", {"dependency_kind":"runtime"}]),
            json!(["Point3.toPoint2", "depends_on", "Point2", {"dependency_kind":"runtime"}]),
            json!(["Point3", "extends", "Point2", null]),
        ];
        actual_edges.sort_by_key(Value::to_string);
        expected_edges.sort_by_key(Value::to_string);
        assert_eq!(
            actual_edges, expected_edges,
            "stored edges must match exact direction, relation and explicit runtime metadata"
        );
    }
}

#[tokio::test]
async fn extracted_depends_on_requires_formal_rules_without_writes() {
    let plan = extract(&projection_records());
    let edge = &plan.edges[0];
    assert_eq!(edge.relation, EdgeRelation::DependsOn);
    assert_eq!(edge.source, "Point3.toPoint2");
    assert_eq!(edge.target, "Point3");
    let (runtime, registry) = runtime(false);
    let ids = create_entities(&registry, &plan).await;
    let before = entity_snapshot(&runtime, &ids).await;
    let result = registry.dispatch("link", link_args(edge, &ids)).await;
    assert!(
        matches!(&result, Err(RuntimeError::InvalidInput(_))),
        "extracted formal dependency must be refused without FormalPack: {result:?}"
    );
    let token = runtime.authorize(Namespace::local()).unwrap();
    assert_eq!(
        runtime
            .graph(&token)
            .unwrap()
            .count_edges(Default::default())
            .await
            .unwrap(),
        0
    );
    assert_eq!(entity_snapshot(&runtime, &ids).await, before);
    assert_eq!(
        runtime
            .entities(&token)
            .unwrap()
            .count_entities("local", Default::default())
            .await
            .unwrap(),
        3
    );
}

#[tokio::test]
async fn extracted_forbidden_pair_is_not_silently_dropped_and_runtime_refuses() {
    let records = vec![
        RawDecl {
            name: "Parent".into(),
            kind: "inductive".into(),
            type_expr: "Type".into(),
            refs: vec!["Dependency".into()],
            doc: None,
            module: "Mathlib.Geometry.Point.Basic".into(),
        },
        RawDecl {
            name: "Dependency".into(),
            kind: "inductive".into(),
            type_expr: "Type".into(),
            refs: vec![],
            doc: None,
            module: "Mathlib.Geometry.Point.Basic".into(),
        },
    ];
    let plan = extract(&records);
    assert_eq!(
        plan.edges,
        [LinkIntent { source: "Parent".into(), target: "Dependency".into(), relation: EdgeRelation::DependsOn, dependency_kind: Some(DependencyKind::Runtime) }],
        "in-corpus references must remain emitted intents even when runtime endpoint rules forbid them"
    );
    let (runtime, registry) = runtime(true);
    let ids = create_entities(&registry, &plan).await;
    let before = entity_snapshot(&runtime, &ids).await;
    let result = registry
        .dispatch("link", link_args(&plan.edges[0], &ids))
        .await;
    assert!(
        matches!(&result, Err(RuntimeError::InvalidInput(_))),
        "FormalPack must refuse emitted structure-to-structure depends_on: {result:?}"
    );
    let token = runtime.authorize(Namespace::local()).unwrap();
    assert_eq!(
        runtime
            .graph(&token)
            .unwrap()
            .count_edges(Default::default())
            .await
            .unwrap(),
        0
    );
    assert_eq!(entity_snapshot(&runtime, &ids).await, before);
    assert_eq!(
        runtime
            .entities(&token)
            .unwrap()
            .count_entities("local", Default::default())
            .await
            .unwrap(),
        2
    );
}
