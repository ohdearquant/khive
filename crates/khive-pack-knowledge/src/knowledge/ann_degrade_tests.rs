//! Handler-level degrade regression tests for the ANN warm-wait path.
//!
//! These tests must live inside the crate because [`vamana::simulate_warming_in_flight`]
//! and [`vamana::set_warm_wait_timeout_override_ms`] are `pub(crate)` — inaccessible
//! from the external `tests/` directory.
//!
//! ## P1 — `search`, `suggest`, and `compose` degrade path
//!
//! When the ANN is warming but not yet loaded and the bounded wait times out,
//! `search` and `suggest` must set `ann_unavailable: true` rather than silently
//! returning zero results. `search` preserves its existing partial-result policy:
//! a lexical hit remains usable without the advisory. `compose` in auto-mode calls
//! `suggest` internally and must propagate the flag in `data["ann_unavailable"]`.
//!
//! The prerequisite for `ann_unavailable` is a non-empty corpus (vectors in the
//! store); we satisfy this by upsert + `knowledge.index` through the registry
//! before the handler call.  The warming-not-loaded state is forced via
//! `simulate_warming_in_flight` on a *fresh* `SharedAnn` (separate from the
//! registry's own).
//!
//! ## P2 — `warm_known_snapshots` end-to-end
//!
//! After `knowledge.index rebuild_ann=true` the persisted Vamana snapshot lives in
//! `retrieval_snapshots`.  Calling `warm_known_snapshots` on a *fresh* `SharedAnn`
//! must load the snapshot so `search_loaded` returns `Some`.

use crate::knowledge::{vamana, KnowledgeHandlers};
use async_trait::async_trait;
use khive_pack_kg::KgPack;
use khive_runtime::{
    AllowAllGate, BackendId, EmbedderProvider, KhiveRuntime, Namespace, RuntimeConfig,
    VerbRegistry, VerbRegistryBuilder,
};
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use serde_json::json;
use std::sync::Arc;

// ── fake embedder ─────────────────────────────────────────────────────────────
//
// Returns N distinct 384-dim unit vectors (one per text, differentiated by index
// position) so every indexed atom gets a valid embedding and the Vamana builder
// can produce a non-trivial index.

const MODEL_KEY: &str = "all-minilm-l6-v2";
const DIM: usize = 384;

struct FakeDimService;

#[async_trait]
impl EmbeddingService for FakeDimService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts
            .iter()
            .enumerate()
            .map(|(i, _)| {
                let v = (i + 1) as f32;
                let norm = (DIM as f32 * v * v).sqrt();
                vec![v / norm; DIM]
            })
            .collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "fake-dim"
    }
}

struct FakeDimProvider;

#[async_trait]
impl EmbedderProvider for FakeDimProvider {
    fn name(&self) -> &str {
        MODEL_KEY
    }

    fn dimensions(&self) -> usize {
        DIM
    }

    async fn build(&self) -> Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
        Ok(Arc::new(FakeDimService))
    }
}

// Controlled two-topic embedder for the degraded-candidate ranking test.  The
// query and genuinely relevant domain share the first axis; the domain whose
// title only collides lexically shares the second.  The failing variant still
// embeds/indexes the corpus and embeds the ANN query, but rejects the later
// query-plus-candidates batch so the fresh rerank is genuinely unavailable.
struct ControlledRankingService {
    fail_fresh_rerank: bool,
}

#[async_trait]
impl EmbeddingService for ControlledRankingService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        if self.fail_fresh_rerank
            && texts.len() > 1
            && texts[0].contains("speculative decoding inference acceleration")
        {
            return Err(EmbedError::InferenceFailed(
                "controlled fresh-rerank failure".into(),
            ));
        }
        Ok(texts
            .iter()
            .map(|text| {
                let lower = text.to_lowercase();
                if lower.contains("semantic_target")
                    || lower.contains("speculative decoding inference acceleration")
                {
                    let mut vector = vec![0.0; DIM];
                    vector[0] = 1.0;
                    vector
                } else if lower.contains("lexical_collision") {
                    let mut vector = vec![0.0; DIM];
                    vector[1] = 1.0;
                    vector
                } else {
                    vec![1.0 / (DIM as f32).sqrt(); DIM]
                }
            })
            .collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "controlled-ranking"
    }
}

struct ControlledRankingProvider {
    fail_fresh_rerank: bool,
}

#[async_trait]
impl EmbedderProvider for ControlledRankingProvider {
    fn name(&self) -> &str {
        MODEL_KEY
    }

    fn dimensions(&self) -> usize {
        DIM
    }

    async fn build(&self) -> Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
        Ok(Arc::new(ControlledRankingService {
            fail_fresh_rerank: self.fail_fresh_rerank,
        }))
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn rt_with_fake_embedder() -> KhiveRuntime {
    let rt = KhiveRuntime::new(RuntimeConfig {
        git_write: Default::default(),
        db_path: None,
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
    })
    .expect("in-memory runtime");
    rt.register_embedder(FakeDimProvider);
    rt
}

fn rt_with_controlled_ranking(fail_fresh_rerank: bool) -> KhiveRuntime {
    let rt = KhiveRuntime::new(RuntimeConfig {
        git_write: Default::default(),
        db_path: None,
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
    })
    .expect("in-memory runtime");
    rt.register_embedder(ControlledRankingProvider { fail_fresh_rerank });
    rt
}

/// File-backed variant. Required by tests that exercise v2 ANN persistence:
/// `knowledge.index(rebuild_ann=true)` only writes v2 segments when the backend
/// has a `data_dir`. An in-memory runtime has none, and ADR-079 removed the v1
/// `retrieval_snapshots` write path, so an in-memory rebuild persists nothing.
fn file_rt_with_fake_embedder(db_path: std::path::PathBuf) -> KhiveRuntime {
    let rt = KhiveRuntime::new(RuntimeConfig {
        git_write: Default::default(),
        db_path: Some(db_path),
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
    })
    .expect("file-backed runtime");
    rt.register_embedder(FakeDimProvider);
    rt
}

fn build_registry(rt: &KhiveRuntime) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(crate::KnowledgePack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");
    rt.install_edge_rules(registry.all_edge_rules());
    registry
}

/// RAII guard: reset the timeout override when the test exits (even on panic).
struct TimeoutOverrideReset;

impl Drop for TimeoutOverrideReset {
    fn drop(&mut self) {
        vamana::set_warm_wait_timeout_override_ms(0);
    }
}

/// Serializes the timeout-override tests. They mutate the process-global
/// `ANN_WARM_WAIT_TIMEOUT_OVERRIDE_MS`, and `TimeoutOverrideReset` clears it on
/// drop. Under Cargo's parallel test runner, one test's reset could otherwise
/// fire while another is mid-flight, dropping it back to the 5s production
/// timeout (a latency-order-dependent slow run). `tokio::sync::Mutex` is
/// await-safe (no `clippy::await_holding_lock`) and does not poison on panic,
/// so a failing test still releases the lock. Each test declares the guard
/// before `TimeoutOverrideReset`, so on exit the reset (override -> 0) runs
/// first and the lock releases only after, handing a clean state to the next.
static TIMEOUT_OVERRIDE_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// ── P1: search preserves its timeout fallback policy

/// `search` reports ANN unavailability only when warming times out, the corpus
/// is non-empty, and lexical search also found no result. A lexical hit remains
/// a valid partial result without the advisory.
#[tokio::test]
async fn search_preserves_timeout_fallback_policy() {
    let _serial = TIMEOUT_OVERRIDE_SERIAL.lock().await;
    vamana::set_warm_wait_timeout_override_ms(50);
    let _reset = TimeoutOverrideReset;

    let rt = rt_with_fake_embedder();
    let registry = build_registry(&rt);
    registry
        .dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [{
                    "slug": "degrade-search-atom",
                    "name": "Degrade Search Atom",
                    "finalized": true,
                    "content": "lexicalsentinel transformer retrieval corpus benchmark search latency vector index nearest neighbor ranking fusion embedding cosine similarity attention encoder decoder positional normalization residual connection"
                }]
            }),
        )
        .await
        .expect("upsert atom");
    registry
        .dispatch("knowledge.index", json!({ "rebuild_ann": false }))
        .await
        .expect("index");

    let ann = vamana::new_shared();
    let key = vamana::AnnKey::new("local", rt.default_embedder_name());
    vamana::simulate_warming_in_flight(&ann, key);
    let token = rt.authorize(Namespace::local()).expect("authorize");

    // The domain gate drops the seeded non-domain atom, including from the
    // small-corpus full-scan fallback, so lexical search is genuinely empty.
    let empty = KnowledgeHandlers::search(
        &rt,
        &token,
        json!({ "query": "unfindablegossamer", "kind": "domain", "rerank": false }),
        &ann,
    )
    .await
    .expect("empty search must not Err");
    assert_eq!(empty["total"], 0, "result: {empty}");
    assert_eq!(empty["ann_unavailable"], true, "result: {empty}");

    let lexical = KnowledgeHandlers::search(
        &rt,
        &token,
        json!({ "query": "lexicalsentinel", "rerank": false }),
        &ann,
    )
    .await
    .expect("lexical fallback search must not Err");
    assert!(
        lexical["total"].as_u64().unwrap_or_default() > 0,
        "lexical fallback must retain its hit; result: {lexical}"
    );
    assert!(
        lexical.get("ann_unavailable").is_none(),
        "search preserves its existing no-advisory policy when FTS found hits; result: {lexical}"
    );
}

// ── P1a: suggest sets ann_unavailable when warming times out ─────────────────

/// `suggest` must set `ann_unavailable: true` when:
/// 1. The ANN key is in the warming set but the index is not yet loaded
///    (`simulate_warming_in_flight` injects this state into a fresh `SharedAnn`).
/// 2. The bounded wait times out (50 ms override via `set_warm_wait_timeout_override_ms`).
/// 3. FTS hits are empty: `suggest` uses `type_filter = Some("domain")` internally,
///    and our seeded atom has no `type:domain` tag — `load_candidates_from_atoms`
///    drops it, so `hits.is_empty() == true`.
/// 4. The corpus has vectors: `compute_fingerprint().vector_count > 0` (satisfied
///    by running `knowledge.index` before the handler call).
#[tokio::test]
async fn suggest_sets_ann_unavailable_when_warming_times_out() {
    let _serial = TIMEOUT_OVERRIDE_SERIAL.lock().await;
    vamana::set_warm_wait_timeout_override_ms(50);
    let _reset = TimeoutOverrideReset;

    let rt = rt_with_fake_embedder();
    let registry = build_registry(&rt);

    // Seed a regular (non-domain) atom, then index to populate the vector store.
    registry
        .dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [{
                    "slug": "degrade-suggest-atom",
                    "name": "Degrade Suggest Atom",
                    "content": "transformer neural network attention mechanism self-attention encoder decoder positional embedding layer normalization residual connection feed forward dense sparse retrieval vector index"
                }]
            }),
        )
        .await
        .expect("upsert atom");

    registry
        .dispatch("knowledge.index", json!({ "rebuild_ann": false }))
        .await
        .expect("index");

    // A fresh SharedAnn — separate from the registry's own — with the key in
    // warming but no index loaded.  This forces the degrade path in `suggest`.
    let ann = vamana::new_shared();
    let model = rt.default_embedder_name().to_string();
    let key = vamana::AnnKey::new("local", &model);
    vamana::simulate_warming_in_flight(&ann, key);

    let token = rt.authorize(Namespace::local()).expect("authorize");
    let result = KnowledgeHandlers::suggest(
        &rt,
        &token,
        // ≥5 words required by suggest; type_filter="domain" will drop the
        // non-domain atom, leaving FTS hits empty → ann_unavailable condition met.
        json!({ "query": "machine learning neural network transformer attention" }),
        &ann,
    )
    .await
    .expect("suggest must not Err");

    assert_eq!(
        result.get("ann_unavailable").and_then(|v| v.as_bool()),
        Some(true),
        "suggest must carry ann_unavailable=true when ANN warming times out \
         and FTS hits are empty; got: {result}"
    );
}

// ── P1b: compose propagates ann_unavailable from its internal suggest call ────

/// `compose` in auto-mode delegates to `suggest` and must surface
/// `data["ann_unavailable"] = true` when the underlying `suggest` sets the flag.
///
/// Auto-mode is triggered when `domain_ids` and `atom_ids` are absent.  Because
/// `suggest` finds no domain hits, `compose` returns early with the no-domains
/// response, placing `ann_unavailable` in `result["data"]["ann_unavailable"]`.
#[tokio::test]
async fn compose_propagates_ann_unavailable_in_auto_mode() {
    let _serial = TIMEOUT_OVERRIDE_SERIAL.lock().await;
    vamana::set_warm_wait_timeout_override_ms(50);
    let _reset = TimeoutOverrideReset;

    let rt = rt_with_fake_embedder();
    let registry = build_registry(&rt);

    registry
        .dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [{
                    "slug": "degrade-compose-atom",
                    "name": "Degrade Compose Atom",
                    "content": "attention mechanism self-attention transformer encoder decoder positional embedding layer normalization residual connection feed forward dense sparse retrieval vector nearest neighbor"
                }]
            }),
        )
        .await
        .expect("upsert atom");

    registry
        .dispatch("knowledge.index", json!({ "rebuild_ann": false }))
        .await
        .expect("index");

    let ann = vamana::new_shared();
    let model = rt.default_embedder_name().to_string();
    let key = vamana::AnnKey::new("local", &model);
    vamana::simulate_warming_in_flight(&ann, key);

    let token = rt.authorize(Namespace::local()).expect("authorize");
    // Auto-mode requires ≥10 words; no domain_ids/atom_ids.
    // type_weights are not reached on the ANN-degrade path (returns before section scoring).
    let result = KnowledgeHandlers::compose(
        &rt,
        &token,
        json!({
            "query": "machine learning neural network transformer attention architecture multi head self attention"
        }),
        &ann,
        std::collections::HashMap::new(),
    )
    .await
    .expect("compose must not Err");

    assert_eq!(
        result
            .get("data")
            .and_then(|d| d.get("ann_unavailable"))
            .and_then(|v| v.as_bool()),
        Some(true),
        "compose must propagate ann_unavailable=true from its internal suggest call; \
         got: {result}"
    );
}

// ── P2: warm_known_snapshots loads a persisted snapshot into a fresh SharedAnn ─

/// After `knowledge.index rebuild_ann=true` the Vamana snapshot is persisted in
/// `retrieval_snapshots`.  Calling `warm_known_snapshots` on a *fresh* `SharedAnn`
/// must load that snapshot so `search_loaded` returns `Some` (index is in memory).
#[tokio::test]
async fn warm_known_snapshots_loads_persisted_snapshot() {
    // File-backed runtime so knowledge.index(rebuild_ann=true) persists v2 ANN
    // segments to data_dir/ann/<hex>; warm_known_snapshots then enumerates and
    // loads them. (In-memory has no data_dir, and ADR-079 removed the v1 write
    // path, so nothing would be persisted to warm.)
    let dir = tempfile::TempDir::new().expect("tempdir");
    let rt = file_rt_with_fake_embedder(dir.path().join("test.db"));
    let registry = build_registry(&rt);

    // Seed two atoms so Vamana has enough vectors to build a non-trivial index.
    registry
        .dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [
                    {
                        "slug": "warm-snap-atom-a",
                        "name": "Warm Snapshot Atom A",
                        "content": "dense retrieval corpus benchmark search latency gradient descent vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity unique warm78a"
                    },
                    {
                        "slug": "warm-snap-atom-b",
                        "name": "Warm Snapshot Atom B",
                        "content": "ranking fusion pipeline embedding rerank cosine similarity unique warm78b transformer attention mechanism self-attention encoder decoder positional feed forward dense neural network gradient"
                    }
                ]
            }),
        )
        .await
        .expect("upsert atoms");

    // Index with rebuild_ann=true to persist the Vamana snapshot in retrieval_snapshots.
    let index_result = registry
        .dispatch("knowledge.index", json!({ "rebuild_ann": true }))
        .await
        .expect("index with rebuild_ann=true");

    // Guard: the index run must have actually embedded atoms (not just done nothing).
    let indexed = index_result
        .get("indexed")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    assert!(
        indexed >= 2,
        "knowledge.index must embed at least 2 atoms for this test to be meaningful; \
         got indexed={indexed}"
    );

    // A fresh SharedAnn — no snapshot loaded yet.
    let ann = vamana::new_shared();
    let model = rt.default_embedder_name().to_string();
    let key = vamana::AnnKey::new("local", &model);

    // Precondition: the fresh ann has nothing loaded.
    let dummy_query = vec![1.0f32 / (DIM as f32).sqrt(); DIM];
    assert!(
        vamana::search_loaded(&ann, &key, &dummy_query, 1)
            .await
            .is_none(),
        "precondition: fresh SharedAnn must have no index loaded before warm_known_snapshots"
    );

    // warm_known_snapshots reads retrieval_snapshots, finds the persisted key, and
    // calls ensure_ann_for_model which restores the AnnBridge from the snapshot.
    vamana::warm_known_snapshots(&rt, &ann).await;

    assert!(
        vamana::search_loaded(&ann, &key, &dummy_query, 1)
            .await
            .is_some(),
        "search_loaded must return Some after warm_known_snapshots loads the snapshot; \
         model={model}, key={key:?}"
    );
}

// ── P3: issue #91 — degraded-mode escalation + consequence semantics ──────────

/// When ANN candidate retrieval times out, distinguish a successful fresh
/// dense rerank from a genuinely lexical-only result.  Both cases use the same
/// candidates: dense reranking promotes the semantically relevant domain,
/// while an unavailable rerank leaves the lexical title collision first.
#[tokio::test]
async fn suggest_reports_degraded_candidates_or_lexical_only_from_ranking_consequence() {
    let _serial = TIMEOUT_OVERRIDE_SERIAL.lock().await;
    vamana::set_warm_wait_timeout_override_ms(50);
    let _reset = TimeoutOverrideReset;

    for (fail_fresh_rerank, expected_mode, expected_winner) in [
        (false, "ann_candidates_degraded", "Opaque Systems Domain"),
        (
            true,
            "lexical_only",
            "Speculative Decoding Methods for Inference",
        ),
    ] {
        let rt = rt_with_controlled_ranking(fail_fresh_rerank);
        let registry = build_registry(&rt);
        registry
            .dispatch(
                "knowledge.upsert_domains",
                json!({"domains": [
                    {
                        "slug": "degrade-semantic-domain",
                        "name": "Opaque Systems Domain",
                        "description": "SEMANTIC_TARGET serving throughput latency batching cache scheduling gpu utilization request queuing parallelism autoscaling deployment topology load balancing production workloads resource management observability reliability"
                    },
                    {
                        "slug": "degrade-lexical-collision",
                        "name": "Speculative Decoding Methods for Inference",
                        "description": "LEXICAL_COLLISION techniques for large language models emphasize decoding speculation, inference methods, acceleration strategies, model techniques, language processing, and unrelated terminology repeated for lexical matching"
                    }
                ]}),
            )
            .await
            .expect("upsert domains");
        registry
            .dispatch("knowledge.index", json!({ "rebuild_ann": false }))
            .await
            .expect("index");

        let ann = vamana::new_shared();
        let key = vamana::AnnKey::new("local", rt.default_embedder_name());
        vamana::simulate_warming_in_flight(&ann, key);
        let token = rt.authorize(Namespace::local()).expect("authorize");
        let result = KnowledgeHandlers::suggest(
            &rt,
            &token,
            json!({
                "query": "speculative decoding inference acceleration techniques for large language models"
            }),
            &ann,
        )
        .await
        .expect("suggest must not Err");

        assert_eq!(result["ann_unavailable"], true, "result: {result}");
        assert_eq!(
            result["degraded"]["mode"], expected_mode,
            "result: {result}"
        );
        assert_eq!(
            result["results"][0]["name"], expected_winner,
            "result: {result}"
        );
    }
}

/// The zero-hit case (P1a's scenario) must ALSO carry the new `degraded`
/// object with `mode: "no_match"`, distinguishing "couldn't check" from
/// "lexical matching ran and found this particular set" (the P3 case above).
#[tokio::test]
async fn suggest_flags_degraded_no_match_when_hits_empty() {
    let _serial = TIMEOUT_OVERRIDE_SERIAL.lock().await;
    vamana::set_warm_wait_timeout_override_ms(50);
    let _reset = TimeoutOverrideReset;

    let rt = rt_with_fake_embedder();
    let registry = build_registry(&rt);

    // Non-domain atom: `suggest`'s type_filter="domain" drops it, so lexical
    // hits are empty (mirrors P1a).
    registry
        .dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [{
                    "slug": "degrade-suggest-atom-2",
                    "name": "Degrade Suggest Atom 2",
                    "content": "transformer neural network attention mechanism self-attention encoder decoder positional embedding layer normalization residual connection feed forward dense sparse retrieval vector index"
                }]
            }),
        )
        .await
        .expect("upsert atom");

    registry
        .dispatch("knowledge.index", json!({ "rebuild_ann": false }))
        .await
        .expect("index");

    let ann = vamana::new_shared();
    let model = rt.default_embedder_name().to_string();
    let key = vamana::AnnKey::new("local", &model);
    vamana::simulate_warming_in_flight(&ann, key);

    let token = rt.authorize(Namespace::local()).expect("authorize");
    let result = KnowledgeHandlers::suggest(
        &rt,
        &token,
        json!({ "query": "machine learning neural network transformer attention" }),
        &ann,
    )
    .await
    .expect("suggest must not Err");

    assert_eq!(result.get("total").and_then(|v| v.as_u64()), Some(0));
    assert_eq!(
        result.get("ann_unavailable").and_then(|v| v.as_bool()),
        Some(true)
    );
    let degraded = result
        .get("degraded")
        .unwrap_or_else(|| panic!("degraded object must be present; got: {result}"));
    assert_eq!(
        degraded.get("mode").and_then(|v| v.as_str()),
        Some("no_match"),
        "zero-hit degraded suggest must report mode=no_match, distinct from lexical_only; got: {result}"
    );
    assert_eq!(
        degraded.get("cache_safe").and_then(|v| v.as_bool()),
        Some(false)
    );
}
