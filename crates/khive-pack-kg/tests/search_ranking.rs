use khive_pack_kg::handlers::{search_rank_fields, ValidatedSearchRequest};
use khive_pack_kg::KgPack;
use khive_runtime::curation::{entity_fts_document, note_fts_document};
use khive_runtime::{
    KhiveRuntime, Namespace, RankScoreKind, RuntimeConfig, RuntimeError, SearchSignals,
    VerbRegistry, VerbRegistryBuilder,
};
use khive_score::DeterministicScore;
use khive_storage::{Entity, Note};
use serde_json::{json, Value};
use uuid::Uuid;

const QUERY: &str = "rankingfixture";
const QUANTUM: f64 = 1.0 / 4_294_967_296.0;

fn fixture() -> (KhiveRuntime, VerbRegistry) {
    let actor = "search-ranking-test";
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        packs: vec!["kg".to_string()],
        actor_id: Some(actor.to_string()),
        ..RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime without embeddings");
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some(actor.to_string()));
    builder.register(KgPack::new(runtime.clone()));
    (runtime, builder.build().expect("KG registry"))
}

async fn seed_ranked_pair(runtime: &KhiveRuntime, kind: &str) -> (Uuid, Uuid) {
    let token = runtime.authorize(Namespace::local()).unwrap();
    let high = Uuid::from_u128(1);
    let low = Uuid::from_u128(2);
    for (id, timestamp) in [(high, 1_000_000), (low, 2_000_000)] {
        if kind == "entity" {
            let name = if id == high {
                QUERY.to_string()
            } else {
                format!("{QUERY} secondary")
            };
            let mut entity = Entity::new("local", "concept", name);
            entity.id = id;
            entity.created_at = timestamp;
            entity.updated_at = timestamp;
            entity.properties = Some(json!({"bucket": "rank"}));
            entity.tags = vec!["rank".to_string()];
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
            let mut note = Note::new("local", "observation", QUERY);
            note.id = id;
            note.created_at = timestamp;
            note.updated_at = timestamp;
            note.salience = Some(if id == high { 1.0 } else { 0.0 });
            note.properties = Some(json!({"bucket": "rank", "tags": ["rank"]}));
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
    (high, low)
}

async fn hits(registry: &VerbRegistry, args: Value) -> Vec<Value> {
    registry
        .dispatch("search", args)
        .await
        .expect("search dispatch")
        .as_array()
        .expect("canonical hit array")
        .clone()
}

#[test]
fn ranking_wire_preserves_all_five_kinds_alias_bits_and_optional_evidence() {
    let rank = DeterministicScore::from_raw(123_456_789);
    for (kind, label) in [
        (RankScoreKind::Rrf, "rrf"),
        (RankScoreKind::Vector, "vector"),
        (RankScoreKind::Keyword, "keyword"),
        (RankScoreKind::Weighted, "weighted"),
        (RankScoreKind::Union, "union"),
    ] {
        for (signals, expected) in [
            (SearchSignals::default(), json!({})),
            (
                SearchSignals {
                    vector_similarity: Some(DeterministicScore::ZERO),
                    keyword_score: None,
                },
                json!({"vector_similarity": 0.0}),
            ),
            (
                SearchSignals {
                    vector_similarity: None,
                    keyword_score: Some(DeterministicScore::from_f64(0.25)),
                },
                json!({"keyword_score": 0.25}),
            ),
            (
                SearchSignals {
                    vector_similarity: Some(DeterministicScore::from_f64(0.5)),
                    keyword_score: Some(DeterministicScore::ZERO),
                },
                json!({"vector_similarity": 0.5, "keyword_score": 0.0}),
            ),
        ] {
            let wire = search_rank_fields(rank, kind, signals);
            let decoded: Value = serde_json::from_str(&wire.to_string()).unwrap();
            assert_eq!(decoded.as_object().unwrap().len(), 4);
            assert_eq!(decoded["rank_score_kind"], label);
            assert_eq!(decoded["signals"], expected);
            assert_eq!(
                decoded["rank_score"].as_f64().unwrap().to_bits(),
                decoded["score"].as_f64().unwrap().to_bits()
            );
            assert_eq!(decoded["rank_score"], json!(rank.to_f64()));
        }
    }
}

#[test]
fn ranking_request_validates_alias_presence_range_and_one_time_quantization() {
    let (_runtime, registry) = fixture();
    for kind in ["entity", "note"] {
        for name in ["min_rank_score", "min_score"] {
            for raw in [0.0, 0.5 + QUANTUM / 4.0, 1.0] {
                let mut args = json!({"kind": kind, "query": QUERY});
                args[name] = json!(raw);
                let request = ValidatedSearchRequest::from_value(args, &registry).unwrap();
                assert_eq!(request.min_score().to_bits(), raw.to_bits());
                assert_eq!(request.min_rank_score(), DeterministicScore::from_f64(raw));
            }
            for invalid in [json!(-0.1), json!(1.1), json!("0.5")] {
                let mut args = json!({"kind": kind, "query": QUERY});
                let numeric = invalid.is_number();
                args[name] = invalid;
                let error = ValidatedSearchRequest::from_value(args, &registry)
                    .expect_err("invalid rank floor must be rejected");
                assert!(matches!(&error, RuntimeError::InvalidInput(_)));
                if numeric {
                    assert!(error.to_string().contains(name));
                }
            }
            let mut args = json!({"kind": kind, "query": QUERY});
            args[name] = Value::Null;
            assert_eq!(
                ValidatedSearchRequest::from_value(args, &registry)
                    .unwrap()
                    .min_rank_score(),
                DeterministicScore::ZERO
            );
        }
        for (canonical, alias) in [
            (json!(0.5), json!(0.5)),
            (json!(0.5), json!(0.75)),
            (Value::Null, Value::Null),
            (Value::Null, json!(0.5)),
            (json!(0.5), Value::Null),
        ] {
            let error = ValidatedSearchRequest::from_value(
                json!({"kind": kind, "query": QUERY, "min_rank_score": canonical, "min_score": alias}),
                &registry,
            )
            .expect_err("both spellings must be invalid even with equal or null values");
            assert!(matches!(&error, RuntimeError::InvalidInput(_)));
            assert!(error.to_string().contains("min_rank_score"));
            assert!(error.to_string().contains("min_score"));
        }
    }
}

#[tokio::test]
async fn ranking_dispatch_emits_evidence_and_compares_the_quantized_final_score() {
    for kind in ["entity", "note"] {
        let (runtime, registry) = fixture();
        let (high, low) = seed_ranked_pair(&runtime, kind).await;
        let baseline = hits(&registry, json!({"kind": kind, "query": QUERY})).await;
        assert_eq!(baseline.len(), 2, "both indexed records must be reachable");
        assert_eq!(baseline[0]["id"], high.to_string());
        assert_eq!(baseline[1]["id"], low.to_string());
        for row in &baseline {
            assert_eq!(row["rank_score_kind"], "rrf");
            assert_eq!(row["source"], "text");
            assert_eq!(row["rank_score"], row["score"]);
            assert!(row["signals"]["keyword_score"].is_number());
            assert!(row["signals"].get("vector_similarity").is_none());
        }
        // Reuse the wire projection only to construct an adversarial input;
        // production decisions compare the original deterministic scores.
        let boundary = baseline[0]["rank_score"].as_f64().unwrap();
        assert!(boundary > baseline[1]["rank_score"].as_f64().unwrap());
        assert!(boundary + QUANTUM < 1.0);
        let sub_step = boundary + QUANTUM / 4.0;
        assert!(sub_step > boundary, "raw floats differ before quantization");
        assert_eq!(
            DeterministicScore::from_f64(sub_step),
            DeterministicScore::from_f64(boundary)
        );
        for name in ["min_rank_score", "min_score"] {
            for floor in [boundary, sub_step] {
                let mut args = json!({"kind": kind, "query": QUERY});
                args[name] = json!(floor);
                assert_eq!(hits(&registry, args).await, vec![baseline[0].clone()]);
            }
            let mut args = json!({"kind": kind, "query": QUERY});
            args[name] = json!(boundary + QUANTUM);
            assert!(hits(&registry, args).await.is_empty());
        }
    }
}

#[tokio::test]
async fn ranking_floor_precedes_time_order_limit_for_entities_and_notes() {
    for kind in ["entity", "note"] {
        let (runtime, registry) = fixture();
        let (high, low) = seed_ranked_pair(&runtime, kind).await;
        let baseline = hits(&registry, json!({"kind": kind, "query": QUERY})).await;
        assert_eq!(baseline.len(), 2);
        let high_score = baseline[0]["rank_score"].as_f64().unwrap();
        let low_score = baseline[1]["rank_score"].as_f64().unwrap();
        assert_eq!(baseline[0]["id"], high.to_string());
        assert!(high_score > low_score);
        let floor = (high_score + low_score) / 2.0;
        for order in ["created_at", "updated_at"] {
            let args = json!({
                "kind": kind, "query": QUERY, "limit": 1, "order_by": order,
                "source": "text", "properties": {"bucket": "rank"}, "tags": ["rank"],
            });
            let unfiltered = hits(&registry, args.clone()).await;
            assert_eq!(unfiltered.len(), 1);
            assert_eq!(
                unfiltered[0]["id"],
                low.to_string(),
                "newer low score wins without floor"
            );
            for name in ["min_rank_score", "min_score"] {
                let mut filtered = args.clone();
                filtered[name] = json!(floor);
                let result = hits(&registry, filtered).await;
                assert_eq!(
                    result.len(),
                    1,
                    "floor must run before the time-order limit"
                );
                assert_eq!(result[0]["id"], high.to_string());
            }
        }
    }
}

async fn seed_created_record(
    runtime: &KhiveRuntime,
    substrate: &str,
    id: u128,
    created: i64,
    fields: (&str, Option<&str>, &str),
) {
    let token = runtime.authorize(Namespace::local()).unwrap();
    if substrate == "entity" {
        let mut entity = Entity::new("local", fields.0, QUERY);
        entity.id = Uuid::from_u128(id);
        entity.created_at = created;
        entity.updated_at = 10_000_000 - created;
        entity.entity_type = fields.1.map(str::to_owned);
        entity.tags = vec![fields.2.into()];
        entity.properties = Some(json!({"bucket":"range"}));
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
        let mut note = Note::new("local", fields.0, QUERY);
        note.id = Uuid::from_u128(id);
        note.created_at = created;
        note.updated_at = 10_000_000 - created;
        note.properties = Some(json!({"bucket":"range", "tags":[fields.2]}));
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

#[tokio::test]
async fn created_range_filters_actual_entity_and_note_search_with_other_filters() {
    for substrate in ["entity", "note"] {
        let (runtime, registry) = fixture();
        let kind = if substrate == "entity" {
            "concept"
        } else {
            "observation"
        };
        for id in 1..=5 {
            seed_created_record(
                &runtime,
                substrate,
                id,
                id as i64 * 1_000_000,
                (kind, Some("theorem"), "range"),
            )
            .await;
        }
        seed_created_record(
            &runtime,
            substrate,
            6,
            3_000_000,
            (kind, Some("theorem"), "other"),
        )
        .await;
        seed_created_record(
            &runtime,
            substrate,
            7,
            3_000_000,
            (
                if substrate == "entity" {
                    "document"
                } else {
                    "insight"
                },
                None,
                "range",
            ),
        )
        .await;
        if substrate == "entity" {
            seed_created_record(
                &runtime,
                substrate,
                8,
                3_000_000,
                (kind, Some("algorithm"), "range"),
            )
            .await;
        }
        for (after, before, expected) in [
            (None, None, vec![1, 2, 3, 4, 5]),
            (Some("1970-01-01T00:00:02Z"), None, vec![2, 3, 4, 5]),
            (None, Some("1970-01-01T00:00:04Z"), vec![1, 2, 3, 4]),
            (
                Some("1970-01-01T00:00:02Z"),
                Some("1970-01-01T00:00:04Z"),
                vec![2, 3, 4],
            ),
            (
                Some("1970-01-01T01:00:03+01:00"),
                Some("1969-12-31T19:00:03-05:00"),
                vec![3],
            ),
            (
                Some("1970-01-01T00:00:03.000000001Z"),
                Some("1970-01-01T00:00:03.999999999Z"),
                vec![],
            ),
        ] {
            for order in ["score", "created_at", "updated_at"] {
                let mut args = json!({"kind":kind, "query":QUERY, "tags":["range"],
                    "properties":{"bucket":"range"}, "order_by":order,
                    "created_after":after, "created_before":before});
                if substrate == "entity" {
                    args["entity_type"] = json!("theorem");
                }
                let mut ids: Vec<_> = hits(&registry, args)
                    .await
                    .iter()
                    .map(|row| {
                        Uuid::parse_str(row["id"].as_str().unwrap())
                            .unwrap()
                            .as_u128()
                    })
                    .collect();
                ids.sort_unstable();
                assert_eq!(ids, expected, "{substrate}, {order}, {after:?}, {before:?}");
            }
        }
    }
}

#[tokio::test]
async fn created_range_widens_candidates_and_filters_before_limit() {
    for kind in ["entity", "note"] {
        let (runtime, registry) = fixture();
        let (high, low) = seed_ranked_pair(&runtime, kind).await;
        let base = json!({"kind":kind, "query":QUERY, "limit":1});
        let baseline = hits(&registry, base.clone()).await;
        assert_eq!(baseline[0]["id"], high.to_string());
        let mut ranged = base.clone();
        ranged["created_after"] = json!("1970-01-01T00:00:01.000000001Z");
        let result = hits(&registry, ranged).await;
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["id"], low.to_string());
        let mut nulls = base;
        nulls["created_after"] = Value::Null;
        nulls["created_before"] = Value::Null;
        assert_eq!(hits(&registry, nulls).await, baseline);
    }
}

#[tokio::test]
async fn created_range_refuses_invalid_or_overprecise_bounds_before_search() {
    let (_, registry) = fixture();
    for fields in [
        json!({"created_after":"invalid"}),
        json!({"created_before":42}),
        json!({"created_after":"1970-01-01T00:00:02Z", "created_before":"1970-01-01T00:00:01Z"}),
        json!({"created_after":"1970-01-01T00:00:01.0000000001Z"}),
        json!({"created_before":"1970-01-01T00:00:01.0000000000Z"}),
        json!({"created_after":"1970-01-01T00:00:01.0000000002Z", "created_before":"1970-01-01T00:00:01.0000000001Z"}),
    ] {
        let mut args = json!({"kind":"entity", "query":QUERY});
        args.as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        let parsed = ValidatedSearchRequest::from_value(args.clone(), &registry).unwrap_err();
        assert!(matches!(parsed, RuntimeError::InvalidInput(_)));
        let dispatched = registry.dispatch("search", args).await.unwrap_err();
        assert_eq!(dispatched.to_string(), parsed.to_string());
    }
}
