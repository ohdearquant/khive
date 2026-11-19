use khive_pack_kg::handlers::ValidatedSearchRequest;
use khive_pack_kg::KgPack;
use khive_runtime::curation::{entity_fts_document, note_fts_document};
use khive_runtime::{
    runtime_error_value, DomainDisposition, KhiveRuntime, Namespace, RuntimeError, VerbRegistry,
    VerbRegistryBuilder,
};
use khive_storage::{Entity, Note};
use serde_json::{json, Value};
use uuid::Uuid;

const QUERY: &str = "refusalalpha refusalbeta";

fn fixture() -> (KhiveRuntime, VerbRegistry) {
    let runtime = KhiveRuntime::memory().expect("private memory runtime");
    assert!(runtime.backend().pool().canonical_path().is_none());
    assert!(runtime.default_embedder_name().is_empty());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    (runtime, builder.build().expect("KG registry"))
}

async fn seed_pair(runtime: &KhiveRuntime, kind: &str) -> [String; 2] {
    let token = runtime.authorize(Namespace::local()).unwrap();
    let first = Uuid::from_u128(1);
    let second = Uuid::from_u128(2);
    for (id, content, created, updated) in [
        (first, QUERY, 1_000_000, 3_000_000),
        (second, "refusalalpha", 2_000_000, 2_000_000),
    ] {
        if kind == "entity" {
            let mut entity = Entity::new("local", "concept", content);
            entity.id = id;
            entity.created_at = created;
            entity.updated_at = updated;
            runtime
                .entities(&token)
                .unwrap()
                .upsert_entity(entity.clone())
                .await
                .unwrap();
            runtime
                .text(&token)
                .unwrap()
                .upsert_document(entity_fts_document(&entity))
                .await
                .unwrap();
        } else {
            let mut note = Note::new("local", "observation", content);
            note.id = id;
            note.created_at = created;
            note.updated_at = updated;
            runtime
                .notes(&token)
                .unwrap()
                .upsert_note(note.clone())
                .await
                .unwrap();
            runtime
                .text_for_notes(&token)
                .unwrap()
                .upsert_document(note_fts_document(&note))
                .await
                .unwrap();
        }
    }
    [first.to_string(), second.to_string()]
}

async fn hits(registry: &VerbRegistry, args: Value) -> Vec<Value> {
    registry
        .dispatch("search", args)
        .await
        .expect("valid search")
        .as_array()
        .expect("canonical hit array")
        .clone()
}

fn ids(hits: &[Value]) -> Vec<&str> {
    hits.iter()
        .map(|hit| hit["id"].as_str().expect("hit id"))
        .collect()
}

fn assert_error(error: RuntimeError, expected: &str, secret: Option<&str>) {
    let RuntimeError::InvalidInput(message) = &error else {
        panic!("search changed its refusal variant: {error:?}");
    };
    if let Some(secret) = secret {
        assert!(
            !message.contains(secret),
            "search refusal exposed the synthetic credential"
        );
        assert!(message.contains("***MASKED***"), "mask absent: {message}");
    }
    assert_eq!(message, expected, "search diagnostic");
    assert_eq!(
        runtime_error_value(error, DomainDisposition::NotCommitted),
        json!({
            "kind": "runtime_error",
            "message": format!("invalid input: {expected}"),
            "domain_disposition": "not_committed"
        })
    );
}

async fn assert_bad_values(registry: &VerbRegistry, kind: &str, field: &str, choices: &str) {
    let secret = format!("sk-proj-{}", "A".repeat(80));
    for (value, shown, masked) in [
        ("bogus", "bogus", false),
        ("", "", false),
        (secret.as_str(), "***MASKED***", true),
    ] {
        let mut args = json!({"kind": kind, "query": QUERY});
        args[field] = json!(value);
        let expected = format!("{field} must be one of: {choices}; got {shown:?}");
        let hidden = masked.then_some(secret.as_str());
        assert_error(
            registry
                .dispatch("search", args.clone())
                .await
                .expect_err("invalid enum"),
            &expected,
            hidden,
        );
        // Coordinated search uses this same public parser before backend dispatch.
        assert_error(
            ValidatedSearchRequest::from_value(args, registry).expect_err("invalid shared request"),
            &expected,
            hidden,
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn text_mode_refusals_mask_values_and_preserve_term_matching() {
    for kind in ["entity", "note"] {
        let (runtime, registry) = fixture();
        let pair = seed_pair(&runtime, kind).await;
        assert_bad_values(&registry, kind, "text_mode", "all_terms, any_term").await;
        let default = hits(&registry, json!({"kind": kind, "query": QUERY})).await;
        let all = hits(
            &registry,
            json!({"kind": kind, "query": QUERY, "text_mode": "all_terms"}),
        )
        .await;
        assert_eq!(ids(&all), [pair[0].as_str()]);
        assert_eq!(all, default);
        let any = hits(
            &registry,
            json!({"kind": kind, "query": QUERY, "text_mode": "any_term"}),
        )
        .await;
        assert_eq!(any.len(), 2);
        for id in &pair {
            assert!(ids(&any).contains(&id.as_str()));
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn source_refusals_mask_values_and_preserve_source_filters() {
    for kind in ["entity", "note"] {
        let (runtime, registry) = fixture();
        let pair = seed_pair(&runtime, kind).await;
        assert_bad_values(&registry, kind, "source", "text, vector, both").await;
        let default = hits(&registry, json!({"kind": kind, "query": "refusalalpha"})).await;
        let text = hits(
            &registry,
            json!({"kind": kind, "query": "refusalalpha", "source": "text"}),
        )
        .await;
        assert_eq!(text.len(), 2);
        for id in &pair {
            assert!(ids(&text).contains(&id.as_str()));
        }
        assert!(text.iter().all(|hit| hit["source"] == "text"));
        assert_eq!(text, default);
        for source in ["vector", "both"] {
            let filtered = hits(
                &registry,
                json!({"kind": kind, "query": "refusalalpha", "source": source}),
            )
            .await;
            assert!(
                filtered.is_empty(),
                "text-only hits cannot satisfy source={source}"
            );
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn order_by_refusals_mask_values_and_preserve_time_orders() {
    for kind in ["entity", "note"] {
        let (runtime, registry) = fixture();
        let pair = seed_pair(&runtime, kind).await;
        assert_bad_values(&registry, kind, "order_by", "score, updated_at, created_at").await;
        let default = hits(&registry, json!({"kind": kind, "query": "refusalalpha"})).await;
        let score = hits(
            &registry,
            json!({"kind": kind, "query": "refusalalpha", "order_by": "score"}),
        )
        .await;
        assert_eq!(score.len(), 2);
        assert_eq!(score, default);
        for (order, expected) in [("created_at", &pair[1]), ("updated_at", &pair[0])] {
            let ordered = hits(
                &registry,
                json!({"kind": kind, "query": "refusalalpha", "order_by": order, "limit": 1}),
            )
            .await;
            assert_eq!(ids(&ordered), [expected.as_str()]);
        }
    }
}

#[tokio::test]
async fn search_enum_refusals_preserve_validation_precedence() {
    let (_runtime, registry) = fixture();
    for (args, expected) in [
        (
            json!({"kind": "entity", "query": QUERY, "min_score": 0, "min_rank_score": 0, "text_mode": "bad"}),
            "supply only min_rank_score; min_score is its deprecated alias",
        ),
        (
            json!({"kind": "entity", "query": QUERY, "properties": [], "text_mode": "bad", "source": "bad", "order_by": "bad"}),
            "text_mode must be one of: all_terms, any_term; got \"bad\"",
        ),
        (
            json!({"kind": "entity", "query": QUERY, "min_score": 2, "source": "bad", "order_by": "bad"}),
            "min_score must be between 0.0 and 1.0; got 2",
        ),
        (
            json!({"kind": "entity", "query": QUERY, "source": "bad", "order_by": "bad"}),
            "source must be one of: text, vector, both; got \"bad\"",
        ),
    ] {
        assert_error(
            registry
                .dispatch("search", args)
                .await
                .expect_err("earlier validation wins"),
            expected,
            None,
        );
    }
}
