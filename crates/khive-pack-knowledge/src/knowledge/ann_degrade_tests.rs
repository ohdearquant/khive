//! Handler-level degrade regression tests for the ANN warm-wait path.
//!
//! These tests must live inside the crate because [`vamana::simulate_warming_in_flight`]
//! and [`vamana::set_warm_wait_timeout_override_ms`] are `pub(crate)` — inaccessible
//! from the external `tests/` directory.
//!
//! ## P1 — `suggest` and `compose` degrade path
//!
//! When the ANN is warming but not yet loaded and the bounded wait times out,
//! `suggest` must set `ann_unavailable: true` rather than silently returning zero
//! results.  `compose` in auto-mode calls `suggest` internally and must propagate
//! the flag in `data["ann_unavailable"]`.
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

// ── helpers ───────────────────────────────────────────────────────────────────

fn rt_with_fake_embedder() -> KhiveRuntime {
    let rt = KhiveRuntime::new(RuntimeConfig {
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
    let result = KnowledgeHandlers::compose(
        &rt,
        &token,
        json!({
            "query": "machine learning neural network transformer attention architecture multi head self attention"
        }),
        &ann,
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
    let rt = rt_with_fake_embedder();
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
