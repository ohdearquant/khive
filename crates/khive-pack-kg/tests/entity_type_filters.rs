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

async fn create_property_typed_entity(
    registry: &VerbRegistry,
    kind: &str,
    stored_type: &str,
    column_type: Option<&str>,
) -> String {
    let created = registry
        .dispatch(
            "create",
            json!({
                "kind": kind, "name": format!("EntityFilterWitness legacy {kind} {stored_type}"),
                "entity_type": column_type, "properties": {"type": stored_type},
                "tags": ["alias-witness"], "skip_dedup_check": true,
            }),
        )
        .await
        .expect("create historical property fixture");
    created["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn list_entity_type_aliases_match_legacy_rows_before_every_pagination_mode() {
    let registry = registry(false);
    let canonical =
        create_property_typed_entity(&registry, "document", "paper", Some("paper")).await;
    let preprint = create_property_typed_entity(&registry, "document", "preprint", None).await;
    let article = create_property_typed_entity(&registry, "document", "article", None).await;
    let patched = create_property_typed_entity(&registry, "document", "report", None).await;
    registry
        .dispatch(
            "update",
            json!({"id": patched, "properties": {"type": "preprint"}}),
        )
        .await
        .expect("raw property update preserves legacy spelling");
    create_property_typed_entity(&registry, "document", "preprint", Some("report")).await;
    create_property_typed_entity(&registry, "concept", "preprint", None).await;
    let mut expected = vec![canonical.clone(), preprint.clone(), article, patched];
    expected.sort_unstable();

    for kind in [None, Some("document")] {
        for raw in ["paper", "preprint", "article", " --PREPRINT__ "] {
            assert_eq!(filtered_ids(&registry, "list", kind, raw).await, expected);
            // Search intentionally retains its exact typed-column contract.
            assert_eq!(
                filtered_ids(&registry, "search", kind, raw).await,
                vec![canonical.clone()]
            );
            for tags in [None, Some(json!([])), Some(json!(["alias-witness"]))] {
                let mut params = filter_params("list", kind, raw);
                params["limit"] = json!(1);
                if let Some(tags) = tags {
                    params["tags"] = tags;
                }
                let mut offset_ids = Vec::new();
                for offset in 0..expected.len() {
                    let mut page_params = params.clone();
                    page_params["offset"] = json!(offset);
                    let page = registry.dispatch("list", page_params).await.unwrap();
                    let items = page["items"].as_array().unwrap();
                    assert_eq!(items.len(), 1, "{page}");
                    offset_ids.push(items[0]["id"].as_str().unwrap().to_string());
                }
                offset_ids.sort_unstable();
                assert_eq!(offset_ids, expected);

                let mut cursor_ids = Vec::new();
                let mut after = json!("");
                for index in 0..expected.len() {
                    let mut page_params = params.clone();
                    page_params["after"] = after;
                    let page = registry.dispatch("list", page_params).await.unwrap();
                    let items = page["entities"].as_array().unwrap();
                    assert_eq!(items.len(), 1, "{page}");
                    cursor_ids.push(items[0]["id"].as_str().unwrap().to_string());
                    after = page["next_after"].clone();
                    assert_eq!(after.is_null(), index + 1 == expected.len(), "{page}");
                }
                cursor_ids.sort_unstable();
                assert_eq!(cursor_ids, expected);
            }
        }
    }
    let row = registry
        .dispatch("get", json!({"id":preprint}))
        .await
        .unwrap();
    assert!(row["entity_type"].is_null());
    assert_eq!(row["properties"]["type"], "preprint");
}

#[tokio::test]
async fn list_aliasless_kebab_property_type_matches_before_every_pagination_mode() {
    let registry = registry(false);
    let report = create_property_typed_entity(&registry, "document", "research-report", None).await;
    // Must-FAIL control: remove canonical kebab expansion while retaining this
    // legacy row; both canonical selectors then return zero instead of one.
    for kind in [None, Some("document")] {
        for raw in ["research-report", "research_report"] {
            assert_eq!(
                filtered_ids(&registry, "list", kind, raw).await,
                vec![report.clone()]
            );
            for tags in [None, Some(json!([])), Some(json!(["alias-witness"]))] {
                for cursor in [false, true] {
                    let mut params = filter_params("list", kind, raw);
                    params["limit"] = json!(1);
                    if let Some(tags) = &tags {
                        params["tags"] = tags.clone();
                    }
                    if cursor {
                        params["after"] = json!("");
                    } else {
                        params["offset"] = json!(0);
                    }
                    let page = registry.dispatch("list", params).await.unwrap();
                    let items = page[if cursor { "entities" } else { "items" }]
                        .as_array()
                        .unwrap();
                    assert_eq!(items.len(), 1, "{page}");
                    assert_eq!(items[0]["id"], report);
                    if cursor {
                        assert!(page["next_after"].is_null(), "{page}");
                    }
                }
            }
            // This repair retains search's exact typed-column contract.
            assert!(filtered_ids(&registry, "search", kind, raw)
                .await
                .is_empty());
        }
    }
    let row = registry
        .dispatch("get", json!({"id": report}))
        .await
        .unwrap();
    assert!(row["entity_type"].is_null());
    assert_eq!(row["properties"]["type"], "research-report");
}

#[tokio::test]
async fn list_alias_groups_do_not_cross_kind_when_legacy_spellings_overlap() {
    let registry = registry(true);
    let document =
        create_property_typed_entity(&registry, "document", "filter_shared_alias", None).await;
    let concept =
        create_property_typed_entity(&registry, "concept", "filter_shared_alias", None).await;
    for (kind, canonical, id) in [
        ("document", "filter_report", &document),
        ("concept", "filter_concept", &concept),
    ] {
        for pinned in [None, Some(kind)] {
            assert_eq!(
                filtered_ids(&registry, "list", pinned, canonical).await,
                vec![id.clone()]
            );
        }
        assert_eq!(
            filtered_ids(&registry, "list", Some(kind), "filter_shared_alias").await,
            vec![id.clone()]
        );
    }
    assert!(registry
        .dispatch("list", filter_params("list", None, "filter_shared_alias"))
        .await
        .unwrap_err()
        .to_string()
        .contains("ambiguous entity_type"));

    let shared_document =
        create_property_typed_entity(&registry, "document", "filter_shared_type_alias", None).await;
    let shared_concept =
        create_property_typed_entity(&registry, "concept", "filter_shared_type_alias", None).await;
    let mut expected = vec![shared_document, shared_concept];
    expected.sort_unstable();
    for raw in ["filter_shared_type", "filter_shared_type_alias"] {
        assert_eq!(filtered_ids(&registry, "list", None, raw).await, expected);
    }
}
