use async_trait::async_trait;
use khive_pack_kg::KgPack;
use khive_runtime::{
    KhiveRuntime, NamespaceToken, PackRuntime, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_types::{EntityKind, EntityTypeDef, HandlerDef, Pack};
use serde_json::{json, Value};

struct FilterTypesPack;

impl Pack for FilterTypesPack {
    const NAME: &'static str = "filter_fixture";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[];
    const REQUIRES: &'static [&'static str] = &["kg"];
    const ENTITY_TYPES: &'static [EntityTypeDef] = &[
        EntityTypeDef {
            kind: EntityKind::Document,
            type_name: "filter_report",
            aliases: &["filter_rep", "filter_shared_alias"],
        },
        EntityTypeDef {
            kind: EntityKind::Concept,
            type_name: "filter_concept",
            aliases: &["filter_shared_alias"],
        },
        EntityTypeDef {
            kind: EntityKind::Document,
            type_name: "filter_shared_type",
            aliases: &["filter_shared_type_alias"],
        },
        EntityTypeDef {
            kind: EntityKind::Concept,
            type_name: "filter_shared_type",
            aliases: &["filter_shared_type_alias"],
        },
    ];
}

#[async_trait]
impl PackRuntime for FilterTypesPack {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        Self::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        Self::ENTITY_KINDS
    }

    fn entity_types(&self) -> &'static [EntityTypeDef] {
        Self::ENTITY_TYPES
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        Self::HANDLERS
    }

    fn requires(&self) -> &'static [&'static str] {
        Self::REQUIRES
    }

    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Err(RuntimeError::InvalidInput(format!(
            "FilterTypesPack does not handle verb {verb:?}"
        )))
    }
}

fn registry(with_extra_types: bool) -> VerbRegistry {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime));
    if with_extra_types {
        builder.register(FilterTypesPack);
    }
    builder.build().expect("registry builds")
}

async fn create_typed_entity(registry: &VerbRegistry, kind: &str, entity_type: &str) -> String {
    let created = registry
        .dispatch(
            "create",
            json!({
                "kind": kind,
                "name": format!("EntityFilterWitness {kind} {entity_type}"),
                "entity_type": entity_type,
                "skip_dedup_check": true,
            }),
        )
        .await
        .expect("create typed entity");
    created["id"].as_str().expect("entity id").to_string()
}

fn filter_params(verb: &str, kind: Option<&str>, entity_type: &str) -> Value {
    let mut params = json!({"kind": "entity", "entity_type": entity_type, "limit": 50});
    if let Some(kind) = kind {
        params["entity_kind"] = json!(kind);
    }
    if verb == "search" {
        params["query"] = json!("EntityFilterWitness");
        params["source"] = json!("text");
    }
    params
}

async fn filtered_ids(
    registry: &VerbRegistry,
    verb: &str,
    kind: Option<&str>,
    entity_type: &str,
) -> Vec<String> {
    let found = registry
        .dispatch(verb, filter_params(verb, kind, entity_type))
        .await
        .unwrap_or_else(|error| panic!("{verb} {kind:?} {entity_type:?}: {error}"));
    let items = if verb == "list" {
        &found["items"]
    } else {
        &found
    };
    let mut ids = items
        .as_array()
        .expect("result items")
        .iter()
        .map(|item| item["id"].as_str().expect("entity id").to_string())
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids
}

#[tokio::test]
async fn entity_type_filters_refuse_unknown_values_with_and_without_kind() {
    let registry = registry(false);
    for verb in ["list", "search"] {
        for kind in [None, Some("document")] {
            let error = registry
                .dispatch(verb, filter_params(verb, kind, "zz_no_such_type_zz"))
                .await
                .expect_err("unknown type must fail before querying an empty population");
            assert!(matches!(error, RuntimeError::InvalidInput(_)));
            let message = error.to_string();
            assert!(message.contains("unknown entity_type"), "{message}");
            assert!(message.contains("zz_no_such_type_zz"), "{message}");
            assert!(message.contains("valid:"), "{message}");
            assert!(message.contains("paper"), "{message}");
        }
        let mut granular = filter_params(verb, None, "zz_no_such_type_zz");
        granular["kind"] = json!("document");
        let error = registry
            .dispatch(verb, granular)
            .await
            .expect_err("granular kind must validate the same filter");
        assert!(error.to_string().contains("unknown entity_type"));
    }
}

#[tokio::test]
async fn entity_type_filters_normalize_builtin_aliases_case_and_separators() {
    let registry = registry(false);
    let paper = create_typed_entity(&registry, "document", "paper").await;
    let blog = create_typed_entity(&registry, "document", "blog_post").await;
    create_typed_entity(&registry, "concept", "algorithm").await;

    for verb in ["list", "search"] {
        for kind in [None, Some("document")] {
            for (raw, expected) in [
                ("paper", &paper),
                (" --PREPRINT__ ", &paper),
                (" --BLOG Post__ ", &blog),
            ] {
                assert_eq!(
                    filtered_ids(&registry, verb, kind, raw).await,
                    vec![expected.clone()],
                    "{verb} {kind:?} {raw:?}"
                );
            }
        }
    }
}

#[tokio::test]
async fn entity_type_filters_use_loaded_pack_types_and_aliases() {
    let loaded = registry(true);
    let expected = create_typed_entity(&loaded, "document", "filter_report").await;
    create_typed_entity(&loaded, "document", "paper").await;
    let unloaded = registry(false);

    for verb in ["list", "search"] {
        for kind in [None, Some("document")] {
            for raw in ["filter_report", " --FILTER Rep__ "] {
                assert_eq!(
                    filtered_ids(&loaded, verb, kind, raw).await,
                    vec![expected.clone()]
                );
                let error = unloaded
                    .dispatch(verb, filter_params(verb, kind, raw))
                    .await
                    .expect_err("an unloaded pack's type must not look like an empty population");
                assert!(error.to_string().contains("unknown entity_type"));
            }
        }
    }
}

#[tokio::test]
async fn entity_type_filters_require_kind_for_ambiguous_cross_kind_alias() {
    let registry = registry(true);
    let document = create_typed_entity(&registry, "document", "filter_report").await;
    let concept = create_typed_entity(&registry, "concept", "filter_concept").await;

    for verb in ["list", "search"] {
        let error = registry
            .dispatch(verb, filter_params(verb, None, "filter_shared_alias"))
            .await
            .expect_err("different canonical types must not select a registration-order winner");
        let message = error.to_string();
        assert!(message.contains("ambiguous entity_type"), "{message}");
        assert!(message.contains("specify entity_kind"), "{message}");
        assert!(message.contains("document:filter_report"), "{message}");
        assert!(message.contains("concept:filter_concept"), "{message}");
        for (kind, expected) in [("document", &document), ("concept", &concept)] {
            assert_eq!(
                filtered_ids(&registry, verb, Some(kind), "filter_shared_alias").await,
                vec![expected.clone()]
            );
        }
    }
}

#[tokio::test]
async fn entity_type_filters_accept_one_canonical_type_across_multiple_kinds() {
    let registry = registry(true);
    let document = create_typed_entity(&registry, "document", "filter_shared_type").await;
    let concept = create_typed_entity(&registry, "concept", "filter_shared_type").await;
    let mut both = vec![document.clone(), concept.clone()];
    both.sort_unstable();

    for verb in ["list", "search"] {
        for raw in ["filter_shared_type", " --FILTER Shared Type Alias__ "] {
            assert_eq!(filtered_ids(&registry, verb, None, raw).await, both);
            for (kind, expected) in [("document", &document), ("concept", &concept)] {
                assert_eq!(
                    filtered_ids(&registry, verb, Some(kind), raw).await,
                    vec![expected.clone()]
                );
            }
        }
    }
}
