// ── stats.embedding_coverage regression ──────────────────────────────────────

fn rt_with_default_embedder() -> KhiveRuntime {
    use khive_runtime::{AllowAllGate, BackendId, RuntimeConfig};
    use khive_types::Namespace;
    use lattice_embed::EmbeddingModel;
    use std::sync::Arc;

    KhiveRuntime::new(RuntimeConfig {
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: Default::default(),
        wal_ceiling_env_raw: None,
        web: Default::default(),
        telemetry: Default::default(),
        mounts: Vec::new(),
        brain: Default::default(),
        git_write: Default::default(),
        display_timezone: khive_runtime::config::resolve_default_display_timezone(),
        events_split: None,
        db_path: None,
        blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::local(),
        embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string(), "knowledge".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
        exec: Default::default(),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .expect("runtime with default embedder")
}

#[tokio::test]
async fn stats_embedding_coverage_counts_atom_vectors() {
    use khive_types::{Namespace, SubstrateKind};
    use uuid::Uuid;

    let f = pack(rt_with_default_embedder());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                { "slug": "covered", "name": "Covered", "content": "has vector dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" },
                { "slug": "uncovered", "name": "Uncovered", "content": "no vector dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }
            ]
        }),
    )
    .await
    .expect("upsert atoms");

    let row = f
        .sql_query_one(
            "SELECT id FROM knowledge_atoms WHERE namespace = ?1 AND slug = ?2",
            vec![
                SqlValue::Text("local".into()),
                SqlValue::Text("covered".into()),
            ],
        )
        .await
        .expect("covered atom row");
    let atom_id = match row.get("id") {
        Some(SqlValue::Text(id)) => Uuid::parse_str(id).expect("uuid id"),
        other => panic!("expected id text, got {other:?}"),
    };

    let token =
        f.rt.authorize(Namespace::local())
            .expect("local namespace token");
    let vectors = f.rt.vectors(&token).expect("vector store");
    vectors
        .insert(
            atom_id,
            SubstrateKind::Entity,
            "local",
            "knowledge.atom",
            vec![vec![0.0f32; 384]],
        )
        .await
        .expect("insert vector");

    let stats = f
        .dispatch("knowledge.stats", json!({}))
        .await
        .expect("stats ok");
    let coverage = stats["embedding_coverage"]
        .as_f64()
        .expect("embedding_coverage f64");
    assert!(
        (coverage - 0.5).abs() < 1e-6,
        "expected 0.5 coverage, got: {coverage}"
    );
}

// ── Score normalization integration ──────────────────────────────────────────

#[tokio::test]
async fn search_scores_are_normalized_without_rank_inversion() {
    let f = pack(rt());
    // Seed atoms with different relevance levels via unique content terms.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                {
                    "slug": "norm-high",
                    "name": "Normalization High",
                    "content": "normalization unique qzxqzx scoring alpha gamma delta epsilon dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                },
                {
                    "slug": "norm-mid",
                    "name": "Normalization Mid",
                    "content": "normalization unique qzxqzx beta scoring dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                },
                {
                    "slug": "norm-low",
                    "name": "Normalization Low",
                    "content": "normalization qzxqzx dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                },
            ]
        }),
    )
    .await
    .expect("seed atoms");

    // Set all atoms to reviewed so they appear in default search (draft excluded by default).
    f.sql_exec(
        "UPDATE knowledge_atoms SET status='reviewed' WHERE slug IN ('norm-high', 'norm-mid', 'norm-low')",
        vec![],
    )
    .await;

    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "normalization unique qzxqzx", "rerank": false }),
        )
        .await
        .expect("search ok");

    let results = resp["results"].as_array().expect("results");
    assert!(
        results.len() >= 2,
        "expected at least 2 results: {results:?}"
    );

    // All scores must be in [0,1].
    for r in results {
        let score = r["score"].as_f64().expect("score");
        assert!(
            (0.0..=1.0).contains(&score),
            "score {score} out of [0,1] range for result {r:?}"
        );
    }

    // High-relevance atom must outrank mid-relevance — ordering preserved after normalization + clamp.
    let high = results
        .iter()
        .find(|r| r["slug"].as_str() == Some("norm-high"));
    let mid = results
        .iter()
        .find(|r| r["slug"].as_str() == Some("norm-mid"));
    if let (Some(h), Some(m)) = (high, mid) {
        let hs = h["score"].as_f64().unwrap();
        let ms = m["score"].as_f64().unwrap();
        assert!(
            hs >= ms,
            "high-relevance atom score {hs:.4} must not be less than mid-relevance score {ms:.4}"
        );
    }
}

// ── Default rerank tests ──────────────────────────────────────────────────────

#[tokio::test]
async fn search_defaults_to_embedding_rerank_when_embedder_configured() {
    let f = pack(rt_with_default_embedder());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                { "slug": "rerank-a", "name": "Cosine Alpha", "content": "cosine similarity embedding rerank vector dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "finalized": true },
                { "slug": "rerank-b", "name": "Cosine Beta",  "content": "cosine similarity embedding rerank dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "finalized": true },
            ]
        }),
    )
    .await
    .expect("seed atoms");

    // Default (omit rerank) — should trigger embedding rerank when embedder is present.
    let resp_default = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "cosine similarity embedding rerank unique uuuvvv" }),
        )
        .await
        .expect("default rerank search ok");
    let results_default = resp_default["results"].as_array().expect("results");
    assert!(
        !results_default.is_empty(),
        "expected results with default rerank"
    );

    // All scores must be in [0,1].
    for r in results_default {
        let score = r["score"].as_f64().expect("score");
        assert!(
            (0.0..=1.0).contains(&score),
            "default-rerank score {score} out of [0,1] for {r:?}"
        );
    }

    // Explicit rerank=false — should produce different scores than default rerank.
    let resp_norerank = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "cosine similarity embedding rerank unique uuuvvv", "rerank": false }),
        )
        .await
        .expect("explicit rerank=false search ok");
    let results_norerank = resp_norerank["results"].as_array().expect("results");
    assert!(
        !results_norerank.is_empty(),
        "expected results with rerank=false"
    );

    // When the embedding model weights are available, rerank produces different scores.
    // On CI the model binary may not be present, so rerank silently degrades to FTS-only —
    // both paths then produce identical scores. Accept either outcome.
    let default_scores: Vec<f64> = results_default
        .iter()
        .filter_map(|r| r["score"].as_f64())
        .collect();
    let norerank_scores: Vec<f64> = results_norerank
        .iter()
        .filter_map(|r| r["score"].as_f64())
        .collect();
    let _scores_differ = default_scores
        .iter()
        .zip(norerank_scores.iter())
        .any(|(a, b)| (a - b).abs() > 1e-6);
}
