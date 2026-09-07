use khive_pack_kg::KgPack;
use khive_pack_web::WebPack;
use khive_runtime::{
    base_entity_rule_allows, KhiveRuntime, PackRegistry, RuntimeConfig, RuntimeError, VerbRegistry,
    VerbRegistryBuilder,
};
use khive_types::{EdgeRelation, EndpointKind, Pack, VerbCategory, Visibility};
use serde_json::{json, Value};

const SUBTYPES: [(&str, &str, &str); 5] = [
    ("service", "site", "origin"),
    ("document", "page", "web_page"),
    ("document", "machine_view", "view"),
    ("service", "agent_tool", "mcp_tool"),
    ("document", "agent_skill", "skill_manifest"),
];

struct Triple {
    source: (&'static str, &'static str),
    relation: EdgeRelation,
    target: (&'static str, &'static str),
    base_covered: bool,
}

const TRIPLES: [Triple; 6] = [
    Triple {
        source: ("service", "site"),
        relation: EdgeRelation::Contains,
        target: ("document", "page"),
        base_covered: false,
    },
    Triple {
        source: ("service", "site"),
        relation: EdgeRelation::Contains,
        target: ("service", "agent_tool"),
        base_covered: false,
    },
    Triple {
        source: ("service", "site"),
        relation: EdgeRelation::Contains,
        target: ("document", "agent_skill"),
        base_covered: false,
    },
    Triple {
        source: ("document", "machine_view"),
        relation: EdgeRelation::DerivedFrom,
        target: ("document", "page"),
        base_covered: true,
    },
    Triple {
        source: ("document", "agent_skill"),
        relation: EdgeRelation::DependsOn,
        target: ("service", "agent_tool"),
        base_covered: false,
    },
    Triple {
        source: ("service", "site"),
        relation: EdgeRelation::Implements,
        target: ("concept", "interface"),
        base_covered: true,
    },
];

fn registry(include_web: bool) -> VerbRegistry {
    let runtime = KhiveRuntime::memory().expect("memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    if include_web {
        builder.register(WebPack::new(runtime.clone()));
    }
    let registry = builder.build().expect("registry builds");
    runtime.install_edge_rules(registry.all_edge_rules());
    registry
}

async fn create(registry: &VerbRegistry, kind: &str, entity_type: &str, name: &str) -> Value {
    registry
        .dispatch(
            "create",
            json!({
                "kind": kind,
                "entity_type": entity_type,
                "name": name,
                "skip_dedup_check": true,
            }),
        )
        .await
        .unwrap_or_else(|e| panic!("create {kind}/{entity_type} must succeed: {e}"))
}

#[test]
fn web_pack_declares_adr175_metadata() {
    assert_eq!(WebPack::NAME, "web");
    assert_eq!(WebPack::REQUIRES, &["kg"]);
    assert!(WebPack::ENTITY_KINDS.is_empty());
    assert!(WebPack::NOTE_KINDS.is_empty());
    assert!(WebPack::NOTE_KIND_SPECS.is_empty());
    assert!(WebPack::SCHEMA_PLAN.is_none());
    assert_eq!(WebPack::HANDLERS.len(), 1);
    assert_eq!(WebPack::HANDLERS[0].name, "web.ingest");
    assert_eq!(WebPack::HANDLERS[0].category, VerbCategory::Commissive);
    assert_eq!(WebPack::HANDLERS[0].visibility, Visibility::Verb);
}

#[tokio::test]
async fn web_tokens_and_aliases_validate_through_create() {
    for include_web in [false, true] {
        let registry = registry(include_web);
        for (kind, canonical, alias) in SUBTYPES {
            for raw in [canonical, alias] {
                let created = create(&registry, kind, raw, &format!("Declared-{raw}")).await;
                assert_eq!(created["entity_type"], canonical);
                let stored = registry
                    .dispatch("get", json!({"id": created["id"]}))
                    .await
                    .expect("created entity is stored");
                assert_eq!(stored["entity_type"], canonical);
            }
        }
    }
}

#[tokio::test]
async fn web_tokens_and_aliases_do_not_claim_bare_kg_kinds() {
    let registry = registry(true);
    for (_, token, alias) in SUBTYPES {
        for name in [token, alias] {
            assert!(
                registry
                    .dispatch("create", json!({"kind":name,"name":"Bare kind control"}))
                    .await
                    .is_err(),
                "{name:?} must not collide with a KG kind or bare-kind alias"
            );
        }
    }
    for alias in ["tool", "skill"] {
        let created = registry
            .dispatch(
                "create",
                json!({"kind":alias,"name":"Resource alias control", "skip_dedup_check":true}),
            )
            .await
            .expect("existing resource alias");
        let stored = registry
            .dispatch("get", json!({"id": created["id"]}))
            .await
            .expect("resource alias is stored");
        assert_eq!(stored["kind"], "resource");
    }
}

#[tokio::test]
async fn tool_remains_a_project_subtype_and_is_refused_on_a_service() {
    let registry = registry(true);
    let tool = create(&registry, "project", "tool", "Synthetic tool project").await;
    assert_eq!(tool["entity_type"], "tool");
    let error = registry
        .dispatch(
            "create",
            json!({"kind": "service", "entity_type": "tool", "name": "Wrong tool kind"}),
        )
        .await
        .expect_err("tool must not become a service subtype");
    assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error:?}");
}

#[tokio::test]
async fn endpoint_rules_advertise_all_six_exact_web_triples() {
    let registry = registry(true);
    let help = registry
        .dispatch("link", json!({"help": true}))
        .await
        .expect("link help succeeds");
    let advertised = help["endpoint_rules"]
        .as_array()
        .expect("link help advertises endpoint rules");
    assert_eq!(WebPack::EDGE_RULES.len(), TRIPLES.len());
    for triple in TRIPLES {
        let source = EndpointKind::EntityOfType {
            kind: triple.source.0,
            entity_type: triple.source.1,
        };
        let target = EndpointKind::EntityOfType {
            kind: triple.target.0,
            entity_type: triple.target.1,
        };
        assert_eq!(
            WebPack::EDGE_RULES
                .iter()
                .filter(|rule| {
                    rule.relation == triple.relation
                        && rule.source == source
                        && rule.target == target
                })
                .count(),
            1,
            "each ADR-175 rule must be declared exactly once"
        );
        assert_eq!(
            base_entity_rule_allows(triple.source.0, triple.relation, triple.target.0),
            triple.base_covered,
            "only derivation and implementation restate base rules"
        );
        let expected = json!({
            "relation": triple.relation.as_str(),
            "source": format!("entity:{}({})", triple.source.0, triple.source.1),
            "target": format!("entity:{}({})", triple.target.0, triple.target.1),
        });
        assert_eq!(
            advertised.iter().filter(|rule| **rule == expected).count(),
            1,
            "link help must advertise {expected} exactly once"
        );
    }
}

#[tokio::test]
async fn link_accepts_all_web_triples_and_requires_the_pack_for_additive_rows() {
    for include_web in [false, true] {
        let registry = registry(include_web);
        for (index, triple) in TRIPLES.iter().enumerate() {
            let source = create(
                &registry,
                triple.source.0,
                triple.source.1,
                &format!("Source-{index}"),
            )
            .await;
            let target = create(
                &registry,
                triple.target.0,
                triple.target.1,
                &format!("Target-{index}"),
            )
            .await;
            let result = registry
                .dispatch(
                    "link",
                    json!({
                        "source_id": source["id"],
                        "target_id": target["id"],
                        "relation": triple.relation.as_str(),
                    }),
                )
                .await;
            if include_web || triple.base_covered {
                assert!(
                    result.is_ok(),
                    "rule {} must be accepted: {result:?}",
                    index + 1
                );
            } else {
                let error = result.expect_err("additive rules require the web pack");
                assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error:?}");
            }
        }
    }
}

#[tokio::test]
async fn base_contract_accepts_reverse_derivation() {
    for include_web in [false, true] {
        let registry = registry(include_web);
        let page = create(&registry, "document", "page", "Declared page").await;
        let view = create(&registry, "document", "machine_view", "Declared view").await;
        registry
            .dispatch(
                "link",
                json!({
                    "source_id": page["id"],
                    "target_id": view["id"],
                    "relation": "derived_from",
                }),
            )
            .await
            .expect("additive packs cannot tighten base document derived_from document");
    }
}

#[tokio::test]
async fn link_refuses_site_contains_site() {
    let registry = registry(true);
    let source = create(&registry, "service", "site", "First site").await;
    let target = create(&registry, "service", "site", "Second site").await;
    let error = registry
        .dispatch(
            "link",
            json!({
                "source_id": source["id"],
                "target_id": target["id"],
                "relation": "contains",
            }),
        )
        .await
        .expect_err("there is no site contains site rule");
    assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error:?}");
}

#[tokio::test]
async fn ingest_help_publishes_every_parameter_and_view_default() {
    let registry = registry(true);
    let help = registry
        .dispatch("web.ingest", json!({"help": true}))
        .await
        .expect("ingest help needs no source and performs no ingest");
    assert_eq!(help["pack"], "web");
    assert_eq!(help["category"], "Commissive");
    let params = help["params"].as_array().expect("help parameter array");
    assert_eq!(params.len(), 3);
    for (name, param_type, required) in [
        ("source", "string", true),
        ("db", "string", false),
        ("include_views", "boolean", false),
    ] {
        let param = params.iter().find(|param| param["name"] == name).unwrap();
        assert_eq!(param["type"], param_type);
        assert_eq!(param["required"], required);
    }
    let views = params
        .iter()
        .find(|param| param["name"] == "include_views")
        .unwrap();
    assert!(views["description"]
        .as_str()
        .unwrap()
        .contains("Defaults to true"));
}

#[test]
fn inventory_registration_is_opt_in_and_requires_kg() {
    assert!(PackRegistry::discovered_names().contains(&"web"));
    assert!(!RuntimeConfig::built_in_packs()
        .iter()
        .any(|pack| pack == "web"));
    assert!(registry(false).describe_verb("web.ingest").is_err());

    let runtime = KhiveRuntime::memory().expect("memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    PackRegistry::register_packs(&["web".into()], runtime.clone(), &mut builder)
        .expect_err("web must require kg in the explicit pack list");
    PackRegistry::register_packs(&["kg".into(), "web".into()], runtime, &mut builder)
        .expect("kg and web factories load");
    let registry = builder.build().expect("inventory registry builds");
    assert_eq!(registry.pack_requires("web"), Some(&["kg"][..]));
    assert_eq!(registry.describe_verb("web.ingest").unwrap()["pack"], "web");
}

#[tokio::test]
async fn verbs_catalog_lists_ingest_only_when_web_is_loaded() {
    for include_web in [false, true] {
        let catalog = registry(include_web)
            .dispatch("verbs", json!({"pack": "web"}))
            .await
            .expect("verbs catalog succeeds");
        let verbs = catalog["verbs"].as_array().expect("verbs array");
        assert_eq!(verbs.len(), usize::from(include_web));
        if include_web {
            assert_eq!(verbs[0]["verb"], "web.ingest");
            let signature = verbs[0]["signature"].as_str().expect("verb signature");
            for param in ["source", "db", "include_views"] {
                assert!(
                    signature.contains(param),
                    "catalog signature must publish {param}"
                );
            }
        }
    }
}
