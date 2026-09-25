use khive_pack_gtd::GtdPack;
use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use serde_json::{json, Value};

fn registry() -> VerbRegistry {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(GtdPack::new(runtime));
    builder.build().expect("registry builds")
}

async fn seed_entities(registry: &VerbRegistry) -> Vec<(String, String, bool)> {
    let mut records = Vec::new();
    for kind in ["resource", "document", "concept"] {
        for tagged in [false, true] {
            let created = registry
                .dispatch(
                    "create",
                    json!({
                        "kind": kind,
                        "name": format!("GraphKindWitness {kind} {tagged}"),
                        "properties": {"type": "issue"},
                        "tags": if tagged { vec!["kind-witness"] } else { vec![] },
                        "skip_dedup_check": true,
                    }),
                )
                .await
                .expect("create graph fixture");
            records.push((
                kind.to_string(),
                created["id"].as_str().expect("entity id").to_string(),
                tagged,
            ));
        }
    }
    records
}

fn read_params(verb: &str, raw_kind: &str, legacy: bool) -> Value {
    let mut params = if legacy {
        json!({"kind": "entity", "entity_kind": raw_kind})
    } else {
        json!({"kind": raw_kind})
    };
    if verb == "search" {
        params["query"] = json!("GraphKindWitness");
        params["source"] = json!("text");
    }
    params
}

#[tokio::test]
async fn graph_reads_refuse_corpus_kinds_with_knowledge_verb_guidance() {
    for populated in [false, true] {
        let registry = registry();
        if populated {
            seed_entities(&registry).await;
        }
        for verb in ["search", "list"] {
            for kind in ["atom", "domain"] {
                for raw in [kind.to_string(), format!(" {} ", kind.to_ascii_uppercase())] {
                    for legacy in [false, true] {
                        let error = registry
                            .dispatch(verb, read_params(verb, &raw, legacy))
                            .await
                            .expect_err("corpus kinds cannot select graph records");
                        let RuntimeError::InvalidInput(message) = error else {
                            panic!("expected InvalidInput, got {error:?}");
                        };
                        let field = if legacy { "entity_kind" } else { "kind" };
                        assert!(message.contains(verb), "{message}");
                        assert!(message.contains(&format!("{field}={raw:?}")), "{message}");
                        assert!(message.contains("knowledge corpus"), "{message}");
                        assert!(
                            message.contains(&format!("knowledge.search(kind={kind:?}, ...)")),
                            "{message}"
                        );
                        assert!(
                            message.contains(&format!("knowledge.list(type={kind:?}, ...)")),
                            "{message}"
                        );
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn graph_list_preserves_exact_kinds_across_pagination_and_tags() {
    let registry = registry();
    let records = seed_entities(&registry).await;
    for kind in ["resource", "document", "concept"] {
        for legacy in [false, true] {
            for tags in [None, Some(json!([])), Some(json!(["kind-witness"]))] {
                let tagged_only = tags
                    .as_ref()
                    .is_some_and(|tags| tags == &json!(["kind-witness"]));
                let mut expected: Vec<_> = records
                    .iter()
                    .filter(|(stored_kind, _, tagged)| {
                        stored_kind == kind && (!tagged_only || *tagged)
                    })
                    .map(|(_, id, _)| id.clone())
                    .collect();
                expected.sort_unstable();
                for cursor in [false, true] {
                    let mut actual = Vec::new();
                    let mut after = json!("");
                    for index in 0..expected.len() {
                        let mut params = read_params("list", kind, legacy);
                        params["limit"] = json!(1);
                        if let Some(tags) = &tags {
                            params["tags"] = tags.clone();
                        }
                        if cursor {
                            params["after"] = after.clone();
                        } else {
                            params["offset"] = json!(index);
                        }
                        let page = registry
                            .dispatch("list", params)
                            .await
                            .expect("list graph kind");
                        let rows = page[if cursor { "entities" } else { "items" }]
                            .as_array()
                            .expect("list rows");
                        assert_eq!(rows.len(), 1, "{page}");
                        assert_eq!(rows[0]["kind"], kind, "{page}");
                        actual.push(rows[0]["id"].as_str().expect("entity id").to_string());
                        if cursor {
                            after = page["next_after"].clone();
                            assert_eq!(after.is_null(), index + 1 == expected.len(), "{page}");
                        }
                    }
                    actual.sort_unstable();
                    assert_eq!(actual, expected);
                }
            }
        }
    }
}

#[tokio::test]
async fn graph_reads_preserve_supported_kinds_and_unknown_kind_refusals() {
    let registry = registry();
    let records = seed_entities(&registry).await;
    let task = registry
        .dispatch("gtd.assign", json!({"title": "GraphKindWitness task"}))
        .await
        .expect("create registered note kind");
    for verb in ["list", "search"] {
        let response = registry
            .dispatch(verb, read_params(verb, "document", false))
            .await
            .expect("supported graph kind");
        let rows = if verb == "list" {
            &response["items"]
        } else {
            &response
        }
        .as_array()
        .expect("graph rows");
        let mut actual: Vec<_> = rows
            .iter()
            .map(|row| {
                assert_eq!(row["kind"], "document");
                row["id"].as_str().expect("entity id").to_string()
            })
            .collect();
        actual.sort_unstable();
        let mut expected: Vec<_> = records
            .iter()
            .filter(|(kind, _, _)| kind == "document")
            .map(|(_, id, _)| id.clone())
            .collect();
        expected.sort_unstable();
        assert_eq!(actual, expected);

        let response = registry
            .dispatch(verb, read_params(verb, "task", false))
            .await
            .expect("registered note kind");
        let rows = if verb == "list" {
            &response["items"]
        } else {
            &response
        }
        .as_array()
        .expect("note rows");
        assert_eq!(rows.len(), 1, "{response}");
        assert_eq!(rows[0]["id"], task["full_id"]);
        assert_eq!(rows[0]["kind"], "task");

        for legacy in [false, true] {
            let error = registry
                .dispatch(verb, read_params(verb, "zzbogus", legacy))
                .await
                .expect_err("unknown graph kind");
            let RuntimeError::InvalidInput(message) = error else {
                panic!("expected InvalidInput, got {error:?}");
            };
            assert!(message.contains("zzbogus"), "{message}");
            assert!(message.contains("valid:"), "{message}");
        }
    }
}
