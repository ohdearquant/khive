//! Independent Rust-dispatch diagnostics and supported FTS-only score configurations.
use std::time::{Duration, Instant};

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use khive_storage::EventFilter;
use khive_types::{EventKind, Namespace};
use serde_json::{json, Value};
use uuid::Uuid;

mod lifecycle_fixture;
use lifecycle_fixture::{check_recall, executor, report, rows, Fixture, DIMS, QUERY, SAMPLES};

#[derive(Clone, Copy)]
enum Scene {
    Embed,
    Candidates,
    Fuse,
    Rerank,
    Score,
    WeightsOff,
    WeightsOn,
    Full,
    Preview,
}

impl Scene {
    fn name(self) -> &'static str {
        match self {
            Self::Embed => "embed_deterministic",
            Self::Candidates => "candidates_fts_500",
            Self::Fuse => "fuse_regathers_fts_500",
            Self::Rerank => "rerank_150_fused_candidates",
            Self::Score => "score_scalar",
            Self::WeightsOff => "recall_weights_off_fts_500",
            Self::WeightsOn => "recall_weights_on_fts_500",
            Self::Full => "recall_full_content_fts_500",
            Self::Preview => "recall_preview_fts_500",
        }
    }
    fn verb(self) -> &'static str {
        match self {
            Self::Embed => "memory.recall_embed",
            Self::Candidates => "memory.recall_candidates",
            Self::Fuse => "memory.recall_fuse",
            Self::Rerank => "memory.recall_rerank",
            Self::Score => "memory.recall_score",
            _ => "memory.recall",
        }
    }
}

fn query() -> Value {
    json!({"query":QUERY,"limit":10,"full_content":true,"config":{"candidate_limit":150}})
}

async fn iteration(scene: Scene) -> Duration {
    let embeddings = matches!(scene, Scene::Embed);
    let count = if matches!(scene, Scene::Embed | Scene::Score) {
        0
    } else {
        500
    };
    let fixture = Fixture::new(count, false, embeddings).await;
    assert_eq!(
        fixture.runtime.registered_embedding_model_names().len(),
        usize::from(embeddings)
    );
    let mut expected_ids = Vec::new();
    let params = match scene {
        Scene::Embed => json!({"query":QUERY,"include_embeddings":true}),
        Scene::Score => json!({"rrf":0.6,"salience":0.8,"decay_factor":0.0,"age_days":1.0}),
        Scene::Rerank => {
            let fused = fixture
                .registry
                .dispatch("memory.recall_fuse", query())
                .await
                .expect("prepare real fused candidates");
            let mut candidates = fused["fused_candidates"]
                .as_array()
                .expect("fused candidates")
                .clone();
            assert_eq!(candidates.len(), 150);
            for candidate in &mut candidates {
                let id = candidate["id"].as_str().expect("candidate ID").to_owned();
                let ordinal = (Uuid::parse_str(&id).expect("UUID").as_u128() as u64) - 1;
                expected_ids.push(id);
                candidate["salience"] = json!(if ordinal.is_multiple_of(2) { 0.1 } else { 0.8 });
                candidate["decay_factor"] = json!(0.0);
                candidate["age_days"] = json!(0.0);
                candidate["temporal"] = json!(1.0);
            }
            json!({"candidates":candidates,"config":{"reranker_weights":{"relevance":0.7,"salience":0.3}}})
        }
        _ => {
            let mut params = query();
            if matches!(scene, Scene::WeightsOn) {
                params["config"]["reranker_weights"] = json!({"relevance":0.7,"salience":0.3});
            } else if matches!(scene, Scene::WeightsOff) {
                params["config"]["reranker_weights"] = json!({});
            }
            if matches!(scene, Scene::Preview) {
                params["full_content"] = json!(false);
            }
            params
        }
    };
    if matches!(scene, Scene::Rerank) {
        check_rerank_events(&fixture, 0).await;
    }
    // Warm provider/query/statement paths on this independent fixture. Both
    // rerank appends are verified outside the timer; candidate-build cost is untimed.
    let warm = fixture
        .registry
        .dispatch(scene.verb(), params.clone())
        .await
        .expect("warm diagnostic");
    check_response(scene, &warm, &expected_ids);
    if matches!(scene, Scene::Rerank) {
        check_rerank_events(&fixture, 1).await;
    }
    let start = Instant::now();
    let response = fixture
        .registry
        .dispatch(scene.verb(), black_box(params))
        .await
        .expect("diagnostic dispatch");
    let elapsed = start.elapsed();
    check_response(scene, &response, &expected_ids);
    if matches!(scene, Scene::Rerank) {
        check_rerank_events(&fixture, 2).await;
    }
    let _ = fixture.close().await;
    elapsed
}

async fn check_rerank_events(fixture: &Fixture, expected: u64) {
    let token = fixture
        .runtime
        .authorize(Namespace::local())
        .expect("local event token");
    let count = fixture
        .runtime
        .events(&token)
        .expect("scoped event store")
        .count_events(EventFilter {
            kinds: vec![EventKind::RerankExecuted],
            verbs: vec!["memory.recall_rerank".into()],
            ..Default::default()
        })
        .await
        .expect("observe persisted rerank events");
    assert_eq!(
        count, expected,
        "best-effort append must succeed in this fixture"
    );
}

fn check_response(scene: Scene, response: &Value, expected_ids: &[String]) {
    match scene {
        Scene::Embed => {
            assert_eq!(response["dimensions"], DIMS);
            let engines = response["engines"].as_array().expect("engines");
            assert_eq!(engines.len(), 1);
            assert!(engines[0].get("error").is_none());
            let embedding = response["embedding"].as_array().expect("embedding");
            assert_eq!(embedding.len(), DIMS);
            assert!(embedding
                .iter()
                .all(|v| v.as_f64().is_some_and(|v| v.is_finite() && v > 0.0)));
        }
        Scene::Candidates => {
            assert_eq!(
                response["text_candidates"]
                    .as_array()
                    .expect("text candidates")
                    .len(),
                150
            );
            assert!(response["vector_candidates"]
                .as_array()
                .expect("vector candidates")
                .is_empty());
        }
        Scene::Fuse => {
            assert_eq!(
                response["fused_candidates"]
                    .as_array()
                    .expect("fused candidates")
                    .len(),
                150
            );
        }
        Scene::Rerank => {
            let ranked = response["reranked"].as_array().expect("reranked");
            assert_eq!(ranked.len(), expected_ids.len());
            for (item, expected) in ranked.iter().zip(expected_ids) {
                assert_eq!(item["id"].as_str().expect("ID"), expected);
                let score = item["rerank_score"].as_f64().expect("rerank score");
                assert!(score.is_finite());
                assert_eq!(
                    item["rerank_scores"]
                        .as_object()
                        .expect("feature scores")
                        .len(),
                    2
                );
            }
            assert_eq!(
                response["active_rerankers"]
                    .as_array()
                    .expect("active weights")
                    .len(),
                2
            );
        }
        Scene::Score => {
            assert!(response["total"].as_f64().expect("score").is_finite());
            assert!(response["breakdown"].is_object());
        }
        _ => {
            check_recall(response);
            assert_eq!(rows(response).len(), 10);
            for row in rows(response) {
                let content = row["content"].as_str().expect("content");
                if matches!(scene, Scene::Preview) {
                    assert_eq!(content.chars().count(), 201);
                    assert!(content.ends_with('…'));
                } else {
                    assert!(content.chars().count() > 200);
                    assert!(!content.ends_with('…'));
                }
            }
        }
    }
}

async fn actual_stage_report() {
    let fixture = Fixture::new(500, false, false).await;
    check_recall(
        &fixture
            .registry
            .dispatch("memory.recall", query())
            .await
            .expect("warm recall"),
    );
    let mut params = query();
    params["include_timings"] = json!(true);
    let response = fixture
        .registry
        .dispatch("memory.recall", params)
        .await
        .expect("actual timing report");
    check_recall(&response);
    let timing = &response["timings"];
    let total = timing["total_us"].as_u64().expect("handler total");
    let mut ratios = serde_json::Map::new();
    for stage in [
        "embed_us",
        "fts_us",
        "ann_us",
        "fresh_tail_us",
        "hydrate_us",
    ] {
        let ratio = timing[stage]
            .as_u64()
            .filter(|_| total > 0)
            .map(|micros| micros as f64 / total as f64);
        ratios.insert(stage.into(), json!(ratio));
    }
    assert!(timing["fts_us"].is_u64());
    assert!(timing["hydrate_us"].is_u64());
    println!(
        "{}",
        json!({"scenario":"actual_fts_recall_stage_report","timings":timing,"stage_over_handler_total":ratios,"interpretation":"one call; stages can overlap and are incomplete, so ratios are not summed"})
    );
    let _ = fixture.close().await;
}

fn diagnostics(c: &mut Criterion) {
    let rt = executor();
    rt.block_on(actual_stage_report());
    let scenes = [
        Scene::Embed,
        Scene::Candidates,
        Scene::Fuse,
        Scene::Rerank,
        Scene::Score,
        Scene::WeightsOff,
        Scene::WeightsOn,
        Scene::Full,
        Scene::Preview,
    ];
    let mut off = None;
    let mut on = None;
    let mut group = c.benchmark_group("memory_diagnostics");
    group.sample_size(10);
    for scene in scenes {
        let samples = (0..SAMPLES)
            .map(|_| rt.block_on(iteration(scene)))
            .collect();
        let median = report(&format!("memory_diagnostics/{}", scene.name()), samples);
        match scene {
            Scene::WeightsOff => off = Some(median),
            Scene::WeightsOn => on = Some(median),
            _ => {}
        }
        group.bench_function(scene.name(), |b| {
            b.iter_custom(|iterations| {
                (0..iterations)
                    .map(|_| rt.block_on(iteration(scene)))
                    .sum::<Duration>()
            })
        });
    }
    group.finish();
    let off = off.expect("unweighted median");
    assert!(off > 0.0, "nonzero dispatch duration");
    println!(
        "{}",
        json!({"comparison":"FTS weighted-score / default-score median","factor":on.expect("weighted median")/off,"samples_per_arm":SAMPLES,"scope":"generated FTS-only, not warm vector/hybrid inference"})
    );
}

criterion_group!(benches, diagnostics);
criterion_main!(benches);
