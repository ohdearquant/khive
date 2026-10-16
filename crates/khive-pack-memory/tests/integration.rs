#[path = "support/const_vec.rs"]
mod const_vec;
#[path = "../../khive-runtime/tests/support/receipt_credentials.rs"]
mod receipt_credentials;
use const_vec::ConstVecProvider;

use async_trait::async_trait;
use khive_brain_core::PackTunable;
use khive_pack_gtd::GtdPack;
use khive_pack_kg::KgPack;
use khive_pack_memory::MemoryPack;
use khive_runtime::{
    EmbedderProvider, FusionStrategy, KhiveRuntime, Namespace, NamespaceToken, PackRuntime,
    RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::types::{EdgeFilter, PageRequest};
use khive_storage::{EdgeRelation, SqlStatement, SqlValue};
use khive_types::Pack;
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;

fn make_runtime() -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: vec![],
        ..RuntimeConfig::default()
    })
    .expect("in-memory runtime")
}

fn make_registry(rt: KhiveRuntime) -> khive_runtime::VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(MemoryPack::new(
        receipt_credentials::with_receipt_credentials(rt),
    ));
    builder.build().expect("registry builds")
}

/// Minimal readable brain profile surface whose snapshot is intentionally null.
/// The profile record exists; null state means bootstrap from configured defaults.
struct NullSnapshotBrainPack;

impl Pack for NullSnapshotBrainPack {
    const NAME: &'static str = "brain";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [khive_types::HandlerDef] = &[khive_types::HandlerDef {
        name: "brain.profile",
        description: "test readable null-snapshot profile",
        visibility: khive_types::Visibility::Subhandler,
        category: khive_types::VerbCategory::Assertive,
        params: &[],
    }];
}

#[async_trait]
impl PackRuntime for NullSnapshotBrainPack {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        Self::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        Self::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [khive_types::HandlerDef] {
        Self::HANDLERS
    }

    async fn dispatch(
        &self,
        verb: &str,
        params: serde_json::Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<serde_json::Value, RuntimeError> {
        if verb != "brain.profile" {
            return Err(RuntimeError::InvalidInput(format!(
                "unexpected test verb {verb}"
            )));
        }
        Ok(json!({
            "id": params["profile_id"],
            "state_snapshot": null,
            "lifecycle": "active"
        }))
    }
}

include!("integration_tests/remember_smoke_tests.rs");

include!("integration_tests/decay_and_salience_tests.rs");

include!("integration_tests/mmr_numeric_ranking_tests.rs");

include!("integration_tests/remember_metadata_tests.rs");

include!("integration_tests/recall_subhandler_tests.rs");

include!("integration_tests/recall_eligibility_and_tuning_tests.rs");

include!("integration_tests/recall_knob_tests.rs");

include!("integration_tests/reranker_integration_tests.rs");

include!("integration_tests/handler_and_breakdown_contract_tests.rs");

include!("integration_tests/multi_model_and_cjk_tests.rs");

include!("integration_tests/memory_type_and_breakdown_tests.rs");

include!("integration_tests/tag_filter_tests.rs");

include!("integration_tests/embedding_opt_in_tests.rs");

include!("integration_tests/memory_type_default_tests.rs");

include!("integration_tests/token_budget_tests.rs");

include!("integration_tests/supersedes_bound_tests.rs");

include!("integration_tests/supersedes_batch_tests.rs");

include!("integration_tests/namespace_visibility_tests.rs");

include!("integration_tests/actor_namespace_recall_tests.rs");

// ── B2 — multi-namespace recall regression (ADR-062 §2 over-fetch + filter) ────

/// B2: ANN over-fetch + namespace post-filter.
///
/// Stores memories in two namespaces (`local` and `other`).  The global ANN index
/// spans both.  Asserts:
///  1. Default recall (visible = [`local`]) excludes `other`-namespace memories.
///  2. A wide token (visible = [`local`, `other`]) includes both namespaces.
///  3. Recall@k is satisfied when ≥k local candidates exist (over-fetch is enough).
///
/// Uses a deterministic custom embedder (no lattice weights required).
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_multi_namespace_recall_overfetch_filter() {
    use khive_runtime::PackRuntime;

    const MODEL: &str = "ns-filter-enc";
    const DIMS: usize = 8;

    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: vec![],
        ..RuntimeConfig::default()
    })
    .expect("runtime");
    rt.register_embedder(ConstVecProvider::new(MODEL, DIMS, 0.42));

    let registry = make_registry(rt.clone());

    // Store 3 memories in `local` namespace.
    let mut local_ids = Vec::new();
    for i in 0..3 {
        let r = registry
            .dispatch(
                "memory.remember",
                json!({
                    "content": format!("local memory entry number {i} about graph databases"),
                    "salience": 0.7,
                    "namespace": "local"
                }),
            )
            .await
            .unwrap_or_else(|e| panic!("local remember {i} failed: {e}"));
        local_ids.push(r["id"].as_str().expect("note_id").to_string());
    }

    // Store 2 memories in `other` namespace.
    let mut other_ids = Vec::new();
    for i in 0..2 {
        let r = registry
            .dispatch(
                "memory.remember",
                json!({
                    "content": format!("other namespace memory {i} about graph databases"),
                    "salience": 0.7,
                    "namespace": "other"
                }),
            )
            .await
            .unwrap_or_else(|e| panic!("other remember {i} failed: {e}"));
        other_ids.push(r["id"].as_str().expect("note_id").to_string());
    }

    // ── Assertion 1: default recall (local-only) excludes `other` memories ────
    let default_recall = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "graph databases", "limit": 10 }),
        )
        .await
        .expect("default recall");
    let default_hits: Vec<&str> = default_recall
        .as_array()
        .expect("array")
        .iter()
        .map(|h| h["id"].as_str().unwrap())
        .collect();

    for oid in &other_ids {
        assert!(
            !default_hits.contains(&oid.as_str()),
            "default recall must exclude other-namespace memory {oid}; got: {default_hits:?}"
        );
    }
    for lid in &local_ids {
        assert!(
            default_hits.contains(&lid.as_str()),
            "default recall must include local memory {lid}; got: {default_hits:?}"
        );
    }

    // ── Assertion 2: wide token includes both namespaces ──────────────────────
    let pack = MemoryPack::new(receipt_credentials::with_receipt_credentials(rt.clone()));
    // Warm the ANN so vectors are indexed.
    pack.warm().await;

    let wide_token = rt
        .authorize_with_visibility(
            Namespace::parse("local").expect("local ns"),
            vec![Namespace::parse("other").expect("other ns")],
        )
        .expect("wide token");

    let wide_recall = pack
        .dispatch(
            "memory.recall",
            json!({ "query": "graph databases", "limit": 10 }),
            &registry,
            &wide_token,
        )
        .await
        .expect("wide-token recall");

    let wide_hits: Vec<&str> = wide_recall
        .as_array()
        .expect("array")
        .iter()
        .map(|h| h["id"].as_str().unwrap())
        .collect();

    for lid in &local_ids {
        assert!(
            wide_hits.contains(&lid.as_str()),
            "wide-token recall must include local memory {lid}; got: {wide_hits:?}"
        );
    }
    for oid in &other_ids {
        assert!(
            wide_hits.contains(&oid.as_str()),
            "wide-token recall must include other-namespace memory {oid}; got: {wide_hits:?}"
        );
    }

    // ── Assertion 3: recall@3 satisfied with 3 local candidates even when ────
    // the global ANN top results may include foreign-namespace hits.
    let k3_recall = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "graph databases", "limit": 3 }),
        )
        .await
        .expect("recall@3");
    let k3_hits: Vec<&str> = k3_recall
        .as_array()
        .expect("array")
        .iter()
        .map(|h| h["id"].as_str().unwrap())
        .collect();

    // All 3 local memories should be present — over-fetch ensures enough
    // local candidates survive the post-filter even if the global NN top-k
    // includes foreign-namespace hits.
    assert_eq!(
        k3_hits.len(),
        3,
        "recall@3 must return exactly 3 results when 3 local candidates exist; got: {k3_hits:?}"
    );
    for lid in &local_ids {
        assert!(
            k3_hits.contains(&lid.as_str()),
            "recall@3 must include local memory {lid}; got: {k3_hits:?}"
        );
    }
}

// ── C1 regression (rewritten) ─────────────────────────────────────────────────
//
// ANN bounded retry — local memories must be found even when the first over-fetch
// round is dominated by foreign-namespace vectors that cluster NEAR the query.
//
// Setup (deterministic, hand-constructed geometry):
//   - N_FOREIGN=50 "foreign" vectors embedded near the query direction [1,0,0,0].
//   - N_LOCAL=3  "local"   vectors embedded far from the query direction [0,1,0,0].
//   - Query text maps to [1,0,0,0] (same cluster as foreign).
//   - candidate_limit = Some(8) → ann_fetch_limit = max(8×4, 8+32) = 40.
//   - Round 1: ANN returns 40 nearest → all foreign (locals are far). 0 local survivors.
//   - The retry gate must fire (index spans "foreign" which is NOT in visible {"local"}).
//   - Round 2: doubles fetch to 80 > 53 total → exhausts corpus, finds all 3 locals.
//
// Correctness:
//   - Default (ANN_OVERFETCH_MAX_ROUNDS env unset → 3 rounds): all locals FOUND.
//   - ANN_OVERFETCH_MAX_ROUNDS=1 (single round only): locals MISSING.
//     Run as: ANN_OVERFETCH_MAX_ROUNDS=1 cargo test -p khive-pack-memory \
//                test_ann_overfetch_retry_stalls_without_widening
//
// Token: uses a DEFAULT local-only token — the production default-recall scenario.

/// Content-keyed embedding service.
///
/// Foreign cluster (query direction):
///   texts containing "xforeign", or the query "xforeign dominant cluster" → [1,0,0,0]
///   cosine with query = 1.0 — closest to query, fill top-40 ANN results.
///
/// Local cluster (far from query but NOT orthogonal):
///   texts containing "xlocal" → [0.15, 0.9887, 0.0, 0.0] (pre-normalised unit vector)
///   cosine with query [1,0,0,0] = 0.15 — above the default min_raw_relevance=0.10 floor,
///   but cosine < 1.0 so ANN ranks locals behind all 50 foreign vectors.
///
/// Geometry guarantee:
///   ann_fetch_limit=40 < 50 foreign vectors → round 1 returns only foreign hits;
///   locals are unreachable until the retry widens the window to >53.
struct ClusteredVecService;

#[async_trait]
impl EmbeddingService for ClusteredVecService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts
            .iter()
            .map(|t| {
                if t.contains("xlocal") {
                    // Far cluster: cosine=0.15 with query direction → above score floor,
                    // but farther than all 50 foreign vectors (cosine=1.0).
                    // Pre-normalised: 0.15² + 0.9887² ≈ 0.0225 + 0.9775 = 1.0000.
                    vec![0.15_f32, 0.9887, 0.0, 0.0]
                } else {
                    // "xforeign" content and the query both map to the foreign cluster.
                    vec![1.0_f32, 0.0, 0.0, 0.0]
                }
            })
            .collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "clustered-vec"
    }
}

struct ClusteredVecProvider {
    provider_name: String,
}

impl ClusteredVecProvider {
    fn new(name: &str) -> Self {
        Self {
            provider_name: name.to_owned(),
        }
    }
}

#[async_trait]
impl EmbedderProvider for ClusteredVecProvider {
    fn name(&self) -> &str {
        &self.provider_name
    }

    fn dimensions(&self) -> usize {
        4
    }

    async fn build(&self) -> Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
        Ok(Arc::new(ClusteredVecService))
    }
}

/// Shared setup for the C1 regression tests. Returns `(rt, registry, pack, local_ids)`.
///
/// Corpus layout:
///   - 50 foreign memories with vector [1,0,0,0]        (cosine=1.0, nearest to query)
///   - 3  local  memories with vector [0.15,0.9887,0,0] (cosine=0.15, farther from query)
///   - Query "xforeign dominant cluster" → vector [1,0,0,0]
///   - candidate_limit = Some(8) → ann_fetch_limit = 40 < total 53
///
/// With only 40 candidates fetched in round 1, all 40 are foreign (nearest to query).
/// Locals are ranked 51-53 (behind all 50 foreign) and NOT retrieved until the fetch
/// window is widened to ≥53 (round 2 doubles to 80, exhausting the corpus).
///
/// Local cosine=0.15 is above the default min_raw_relevance=0.10 score floor so locals
/// survive the final scoring step once the retry loop surfaces them.
async fn c1_setup() -> (
    KhiveRuntime,
    khive_runtime::VerbRegistry,
    MemoryPack,
    Vec<String>,
) {
    const MODEL: &str = "c1-clustered-enc";
    const N_FOREIGN: usize = 50;
    const N_LOCAL: usize = 3;

    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: vec![],
        ..RuntimeConfig::default()
    })
    .expect("runtime");
    rt.register_embedder(ClusteredVecProvider::new(MODEL));

    let registry = make_registry(rt.clone());

    // Insert N_FOREIGN memories in "foreign" namespace. Content contains "xforeign"
    // so the embedder returns [1,0,0,0] — same direction as the query vector.
    for i in 0..N_FOREIGN {
        registry
            .dispatch(
                "memory.remember",
                json!({
                    "content": format!("xforeign memory slot {i} cluster-a"),
                    "salience": 0.6,
                    "namespace": "foreign"
                }),
            )
            .await
            .unwrap_or_else(|e| panic!("foreign remember {i} failed: {e}"));
    }

    // Insert N_LOCAL memories in "local" namespace. Content contains "xlocal"
    // so the embedder returns [0.15,0.9887,0,0] — far from query (cosine=0.15),
    // ranked behind all 50 foreign vectors but above the min_raw_relevance floor.
    let mut local_ids = Vec::new();
    for i in 0..N_LOCAL {
        let r = registry
            .dispatch(
                "memory.remember",
                json!({
                    "content": format!("xlocal memory {i} must be recalled"),
                    "salience": 0.8,
                    "namespace": "local"
                }),
            )
            .await
            .unwrap_or_else(|e| panic!("local remember {i} failed: {e}"));
        local_ids.push(r["id"].as_str().expect("note_id").to_string());
    }

    let pack = MemoryPack::new(receipt_credentials::with_receipt_credentials(rt.clone()));
    {
        use khive_runtime::PackRuntime;
        pack.warm().await;
    }

    (rt, registry, pack, local_ids)
}

// C1 deterministic: asserts BOTH branches of the ANN over-fetch retry loop in a
// single test with no env mutation. Both branches run every CI invocation.
//
// Corpus geometry (see c1_setup):
//   - 50 foreign vectors [1,0,0,0] closest to query direction
//   - 3 local  vectors [0.15,0.9887,0,0] farther from query (cosine=0.15)
//   - candidate_limit=8 → ann_fetch_limit=40 < 53 total
//
// Branch (a): ann_overfetch_max_rounds=3 (≥2 rounds needed to widen to 80>53)
//   → retry fires, corpus exhausted, all 3 locals surface.
//
// Branch (b): ann_overfetch_max_rounds=1 (single round only)
//   → round 1 returns 40 foreign hits; no retry; locals silently absent.
//
// Both branches are exercised in-process via RecallConfig.ann_overfetch_max_rounds.
// No env var mutation; no cross-test interference; no silent skips.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_ann_overfetch_retry_both_branches_deterministic() {
    use khive_runtime::PackRuntime;

    let (rt, registry, pack, local_ids) = c1_setup().await;

    // Local-only token: the production default. visible = {"local"}.
    // The global ANN index spans {"local", "foreign"} — retry gate must fire.
    let local_token = rt
        .authorize(Namespace::parse("local").expect("local ns"))
        .expect("authorize local");

    // Branch (a): with adequate rounds (3), the retry widens to 80>53 and
    // exhausts the corpus, surfacing all 3 local memories.
    let result_with_retry = pack
        .dispatch(
            "memory.recall",
            json!({
                "query": "xforeign dominant cluster",
                "limit": 3,
                "config": {
                    "candidate_limit": 8,
                    "ann_overfetch_max_rounds": 3
                }
            }),
            &registry,
            &local_token,
        )
        .await
        .expect("recall with retry");

    let hits_with_retry: Vec<&str> = result_with_retry
        .as_array()
        .expect("array")
        .iter()
        .map(|h| h["id"].as_str().unwrap())
        .collect();

    for lid in &local_ids {
        assert!(
            hits_with_retry.contains(&lid.as_str()),
            "C1 branch-a: local memory {lid} MUST appear when retry rounds=3; \
             got: {hits_with_retry:?}"
        );
    }

    // Branch (b): with a single round, round 1 fetches 40 of 53 vectors — all
    // foreign (nearest to query direction). No retry fires so local memories
    // (ranked 51-53 behind all 50 foreign) are absent from the result.
    let result_no_retry = pack
        .dispatch(
            "memory.recall",
            json!({
                "query": "xforeign dominant cluster",
                "limit": 3,
                "config": {
                    "candidate_limit": 8,
                    "ann_overfetch_max_rounds": 1
                }
            }),
            &registry,
            &local_token,
        )
        .await
        .expect("recall without retry");

    let hits_no_retry: Vec<&str> = result_no_retry
        .as_array()
        .expect("array")
        .iter()
        .map(|h| h["id"].as_str().unwrap())
        .collect();

    for lid in &local_ids {
        assert!(
            !hits_no_retry.contains(&lid.as_str()),
            "C1 branch-b: local memory {lid} must NOT appear when retry rounds=1 \
             (corpus geometry broken if it does); got: {hits_no_retry:?}"
        );
    }
}

include!("integration_tests/episodic_actor_routing_tests.rs");

include!("integration_tests/expiry_and_maintenance_tests.rs");

include!("integration_tests/profile_visibility_tests.rs");

include!("integration_tests/source_and_excluded_tag_tests.rs");

include!("integration_tests/memory_update_contract_tests.rs");

include!("integration_tests/generic_create_admission_tests.rs");

include!("integration_tests/stream_and_owning_writer_tests.rs");

include!("integration_tests/proposal_admission_tests.rs");

include!("integration_tests/historical_proposal_apply_refusal_tests.rs");

include!("integration_tests/bulk_and_embedding_budget_tests.rs");

/// A keyed memory over the embedder input budget discloses the truncation on
/// the fresh write and its replay. Replay retains the stored identity and
/// fences, reports its recomputed truncation, and adds no rows.
// The in-memory backend serves reads from its one connection. A current-thread runtime
// cannot run the background ANN task that holds it across an await while the write path
// makes its synchronous readiness read, so this test needs a second worker thread.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(config_ledger)]
async fn keyed_remember_discloses_truncation_on_fresh_write_and_on_identical_replay() {
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: vec![],
        ..RuntimeConfig::default()
    })
    .expect("runtime");
    rt.register_embedder(ConstVecProvider::new("truncation-enc", 4, 0.9));
    let registry = make_registry(rt);

    let args = json!({
        "content": "x".repeat(lattice_embed::MAX_TEXT_BYTES + 1),
        "key": "keyed-truncation",
    });
    let created = registry
        .dispatch("memory.remember", args.clone())
        .await
        .expect("an over-budget keyed memory must not fail after commit");
    assert!(
        created.get("replayed").is_none(),
        "first call is a fresh write: {created}"
    );
    assert_eq!(
        created["warnings"],
        json!([khive_runtime::retrieval::EMBEDDING_INPUT_TRUNCATED_WARNING]),
        "a fresh keyed remember must disclose the truncated embedding input: {created}"
    );

    let replayed = registry
        .dispatch("memory.remember", args)
        .await
        .expect("replay must succeed");
    assert_eq!(
        replayed["replayed"],
        json!(true),
        "second call replays: {replayed}"
    );
    assert_eq!(replayed["id"], created["id"]);
    assert_eq!(
        replayed["warnings"],
        json!([khive_runtime::retrieval::EMBEDDING_INPUT_TRUNCATED_WARNING]),
        "a replay must disclose its computed truncated embedding input: {replayed}"
    );
}

/// Both remember branches retain a visibility fence while disclosing bounded
/// embedding input. Keyed replay keeps that fence even after log compaction.
// The in-memory backend serves reads from its one connection. A current-thread runtime
// cannot run the background ANN task that holds it across an await while the write path
// makes its synchronous readiness read, so this test needs a second worker thread.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(config_ledger)]
async fn remember_truncation_and_visibility_survive_fresh_write_and_keyed_replay() {
    const MODEL: &str = "all-minilm-l6-v2";
    for keyed in [false, true] {
        let rt = make_runtime();
        rt.register_embedder(ConstVecProvider::new(
            MODEL,
            EmbeddingModel::AllMiniLmL6V2.dimensions(),
            0.9,
        ));
        let token = rt.authorize(Namespace::local()).expect("local token");
        let registry = make_registry(rt.clone());
        let content = "x".repeat(lattice_embed::MAX_TEXT_BYTES + 1);
        let mut args = json!({
            "content": content,
            "memory_type": "semantic",
            "embedding_model": MODEL,
        });
        if keyed {
            args["key"] = json!("joint-receipt-key");
        }
        let created = registry
            .dispatch("memory.remember", args.clone())
            .await
            .expect("bounded embedding input must not fail after commit");
        assert_eq!(
            created["warnings"],
            json!([khive_runtime::retrieval::EMBEDDING_INPUT_TRUNCATED_WARNING]),
            "keyed={keyed}: truncation and its fence must be returned together"
        );
        let receipt = created["visibility_token"]
            .as_str()
            .expect("sealed receipt is an opaque string");
        let receipt_runtime = receipt_credentials::with_receipt_credentials(rt.clone());
        let opened = receipt_runtime
            .open_visibility_receipt(receipt, &["local"], &[MODEL.to_owned()])
            .expect("authenticate the actual returned receipt");
        assert_eq!(opened.namespace(), "local");
        let seq = opened
            .sequence_for_model(MODEL)
            .expect("positive committed vector fence");
        assert!(seq > 0);
        let id: Uuid = created["id"].as_str().expect("note id").parse().unwrap();
        let stored = rt
            .notes(&token)
            .unwrap()
            .get_note(id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.content, content,
            "full source text must remain stored"
        );

        let mut reader = rt.sql().reader().await.unwrap();
        let high_sequence = reader
            .query_scalar(SqlStatement {
                sql: "SELECT seq FROM sqlite_sequence WHERE name = 'ann_write_log'".into(),
                params: vec![],
                label: Some("joint-remember-committed-sequence".into()),
            })
            .await
            .unwrap();
        assert!(matches!(high_sequence, Some(SqlValue::Integer(value)) if value == seq as i64));
        drop(reader);

        if keyed {
            let durable = khive_runtime::keyed_memory::memory_visibility_receipt(&rt, &token, id)
                .await
                .unwrap()
                .expect("keyed memory must retain its original receipt");
            assert_eq!(durable, vec![(MODEL.to_owned(), seq)]);
            let mut writer = rt.sql().writer().await.unwrap();
            writer
                .execute(SqlStatement {
                    sql: "DELETE FROM ann_write_log WHERE namespace = ?1 AND subject_id = ?2"
                        .into(),
                    params: vec![
                        SqlValue::Text("local".into()),
                        SqlValue::Text(id.to_string()),
                    ],
                    label: Some("joint-remember-compact-original-log".into()),
                })
                .await
                .unwrap();
            drop(writer);
            let replayed = registry.dispatch("memory.remember", args).await.unwrap();
            assert_eq!(replayed["id"], created["id"]);
            assert_eq!(replayed["replayed"], json!(true));
            let resealed = replayed["visibility_token"]
                .as_str()
                .expect("replay returns an opaque receipt");
            assert_ne!(resealed, receipt, "replay uses a fresh nonce");
            let replay_fence = receipt_runtime
                .open_visibility_receipt(resealed, &["local"], &[MODEL.to_owned()])
                .expect("authenticate the replay receipt after log compaction");
            assert_eq!(replay_fence.namespace(), "local");
            assert_eq!(replay_fence.sequence_for_model(MODEL), Some(seq));
            assert_eq!(
                replayed["warnings"], created["warnings"],
                "replay must retain its computed truncation warning with the original fence"
            );
            let mut reader = rt.sql().reader().await.unwrap();
            let replay_rows = reader
                .query_scalar(SqlStatement {
                    sql: "SELECT COUNT(*) FROM ann_write_log WHERE namespace = ?1 AND subject_id = ?2".into(),
                    params: vec![SqlValue::Text("local".into()), SqlValue::Text(id.to_string())],
                    label: Some("joint-remember-replay-no-new-log".into()),
                })
                .await
                .unwrap();
            assert!(matches!(replay_rows, Some(SqlValue::Integer(0))));
            drop(reader);
            let holders = rt
                .notes(&token)
                .unwrap()
                .get_live_notes_by_key("local", "joint-receipt-key", Some("memory"))
                .await
                .unwrap();
            assert_eq!(holders.len(), 1);
            assert_eq!(holders[0].id, id);
        }
    }
}

include!("integration_tests/final_mmr_score_floor_tests.rs");
