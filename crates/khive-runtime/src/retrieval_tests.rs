use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use crate::runtime::{KhiveRuntime, NamespaceToken, RuntimeConfig};
use khive_score::rrf_score;
use khive_storage::types::{TextSearchHit, VectorSearchHit};
use khive_types::namespace::Namespace;
use lattice_embed::{EmbedError, EmbeddingModel};

/// Arm a provider that changes from healthy to failing after setup so a
/// test can exercise an error on a provider already serving requests.
fn break_vector_arm(fail: &AtomicBool) {
    fail.store(true, Ordering::SeqCst);
}

/// An `EmbeddingService` that always succeeds with a fixed vector — used
/// to exercise a genuinely healthy vector leg without loading real model
/// weights.
struct ConstantEmbeddingService {
    dimensions: usize,
    fail: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl EmbeddingService for ConstantEmbeddingService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(EmbedError::ModelInitialization(
                "injected vector-arm failure".to_string(),
            ));
        }
        Ok(texts.iter().map(|_| vec![1.0; self.dimensions]).collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "hybrid-search-test-constant-embedding"
    }
}

struct ConstantEmbedderProvider {
    name: String,
    dimensions: usize,
    fail: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl EmbedderProvider for ConstantEmbedderProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(Arc::new(ConstantEmbeddingService {
            dimensions: self.dimensions,
            fail: Arc::clone(&self.fail),
        }))
    }
}

/// A runtime configured with a healthy (constant, non-failing) embedder —
/// the vector leg genuinely runs and succeeds.
fn runtime_with_constant_embeddings() -> KhiveRuntime {
    runtime_with_toggleable_embeddings().0
}

fn runtime_with_toggleable_embeddings() -> (KhiveRuntime, Arc<AtomicBool>) {
    let model = EmbeddingModel::AllMiniLmL6V2;
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: Some(model),
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime");
    let fail = Arc::new(AtomicBool::new(false));
    runtime.register_embedder(ConstantEmbedderProvider {
        name: model.to_string(),
        dimensions: model.dimensions(),
        fail: Arc::clone(&fail),
    });
    (runtime, fail)
}

#[test]
fn bounded_embedding_input_reserves_prefix_and_preserves_utf8() {
    assert_eq!(
        document_embedding_budget("multilingual-e5-base"),
        MAX_TEXT_BYTES - "passage: ".len()
    );

    let input = format!("{}\u{1f980}tail", "a".repeat(MAX_TEXT_BYTES - 1));
    let (bounded, truncated) = bounded_embedding_input(&input, MAX_TEXT_BYTES);
    assert!(truncated);
    assert_eq!(bounded.len(), MAX_TEXT_BYTES - 1);
    assert!(bounded.is_char_boundary(bounded.len()));
    assert!(!bounded.contains('\u{1f980}'));
}

#[test]
fn bounded_embedding_input_leaves_normal_text_unchanged() {
    let input = "normal byte-identical embedding input";
    let (bounded, truncated) = bounded_embedding_input(input, MAX_TEXT_BYTES);
    assert!(!truncated);
    assert_eq!(bounded, input);
    assert_eq!(bounded.as_ptr(), input.as_ptr());
}

fn text_hit(id: Uuid, rank: u32, title: &str) -> TextSearchHit {
    TextSearchHit {
        subject_id: id,
        score: DeterministicScore::from_f64(1.0),
        rank,
        title: Some(title.to_string()),
        snippet: Some("...".to_string()),
    }
}

fn vector_hit(id: Uuid, rank: u32) -> VectorSearchHit {
    VectorSearchHit {
        subject_id: id,
        score: DeterministicScore::from_f64(0.9),
        rank,
    }
}

#[test]
fn rrf_evidence_keeps_absence_distinct_from_measured_zero() {
    let text_id = Uuid::from_u128(1);
    let vector_id = Uuid::from_u128(2);
    let both_id = Uuid::from_u128(3);
    let quarter = DeterministicScore::from_raw(1_i64 << 30);
    let half = DeterministicScore::from_raw(1_i64 << 31);
    let text = vec![
        TextSearchHit {
            score: DeterministicScore::ZERO,
            ..text_hit(text_id, 1, "text")
        },
        TextSearchHit {
            score: quarter,
            ..text_hit(both_id, 2, "both")
        },
        text_hit(both_id, 3, "duplicate"),
    ];
    let vector = vec![
        VectorSearchHit {
            score: DeterministicScore::ZERO,
            ..vector_hit(vector_id, 1)
        },
        VectorSearchHit {
            score: half,
            ..vector_hit(both_id, 2)
        },
        vector_hit(both_id, 3),
    ];
    let hits = rrf_fuse(text, vector, 10, "unmatched");
    assert_eq!(hits.len(), 3);
    for (id, source, signals) in [
        (
            text_id,
            SearchSource::Text,
            SearchSignals {
                vector_similarity: None,
                keyword_score: Some(DeterministicScore::ZERO),
            },
        ),
        (
            vector_id,
            SearchSource::Vector,
            SearchSignals {
                vector_similarity: Some(DeterministicScore::ZERO),
                keyword_score: None,
            },
        ),
        (
            both_id,
            SearchSource::Both,
            SearchSignals {
                vector_similarity: Some(half),
                keyword_score: Some(quarter),
            },
        ),
    ] {
        let hit = hits.iter().find(|hit| hit.entity_id == id).unwrap();
        assert_eq!(hit.rank_score_kind, RankScoreKind::Rrf);
        assert_eq!(hit.source, source);
        assert_eq!(hit.signals, signals);
    }
}

#[test]
fn rrf_evidence_golden_preserves_true_ties_across_permutations() {
    let a = Uuid::from_u128(1);
    let b = Uuid::from_u128(2);
    let expected = vec![
        (
            a,
            748_365_513,
            RankScoreKind::Rrf,
            SearchSignals {
                vector_similarity: Some(DeterministicScore::from_raw(1_i64 << 31)),
                keyword_score: Some(DeterministicScore::from_raw(1_i64 << 30)),
            },
        ),
        (
            b,
            748_365_513,
            RankScoreKind::Rrf,
            SearchSignals {
                vector_similarity: Some(DeterministicScore::from_raw(1_i64 << 32)),
                keyword_score: Some(DeterministicScore::from_raw(3_i64 << 30)),
            },
        ),
    ];
    // Swapping both arms preserves each ID's rank-1 plus rank-2 total.
    for (text_ids, vector_ids) in [([a, b], [b, a]), ([b, a], [a, b])] {
        for _ in 0..4 {
            let text = text_ids
                .into_iter()
                .enumerate()
                .map(|(rank, id)| TextSearchHit {
                    score: DeterministicScore::from_raw(if id == a { 1_i64 } else { 3_i64 } << 30),
                    ..text_hit(id, rank as u32 + 1, "candidate")
                })
                .collect();
            let vector = vector_ids
                .into_iter()
                .enumerate()
                .map(|(rank, id)| VectorSearchHit {
                    score: DeterministicScore::from_raw(if id == a { 1_i64 } else { 2_i64 } << 31),
                    ..vector_hit(id, rank as u32 + 1)
                })
                .collect();
            let hits = rrf_fuse(text, vector, 10, "unmatched");
            assert_eq!(hits.len(), 2);
            assert_eq!(hits[0].score, hits[1].score);
            assert!(hits.iter().all(|hit| hit.source == SearchSource::Both));
            let snapshot: Vec<_> = hits
                .iter()
                .map(|hit| {
                    (
                        hit.entity_id,
                        hit.score.to_raw(),
                        hit.rank_score_kind,
                        hit.signals,
                    )
                })
                .collect();
            assert_eq!(snapshot, expected);
        }
    }
}

#[test]
fn rrf_fuse_text_only() {
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let text = vec![text_hit(a, 1, "A"), text_hit(b, 2, "B")];
    let hits = rrf_fuse(text, vec![], 10, "query");
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].entity_id, a);
    assert_eq!(hits[0].source, SearchSource::Text);
    assert_eq!(hits[0].title.as_deref(), Some("A"));
}

#[test]
fn rrf_fuse_vector_only() {
    let a = Uuid::new_v4();
    let hits = rrf_fuse(vec![], vec![vector_hit(a, 1)], 10, "query");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].source, SearchSource::Vector);
    assert!(hits[0].title.is_none());
}

#[test]
fn rrf_fuse_marks_both_when_in_both_lists() {
    let id = Uuid::new_v4();
    let text = vec![text_hit(id, 1, "A")];
    let vec = vec![vector_hit(id, 1)];
    let hits = rrf_fuse(text, vec, 10, "query");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].source, SearchSource::Both);
}

#[test]
fn rrf_fuse_preserves_unique_leg_scores_exactly() {
    let text_only = Uuid::new_v4();
    let both = Uuid::new_v4();
    let vector_only = Uuid::new_v4();
    let text = vec![text_hit(text_only, 1, "A"), text_hit(both, 2, "B")];
    let vector = vec![vector_hit(both, 1), vector_hit(vector_only, 2)];

    let hits = rrf_fuse(text, vector, 10, "query");
    let score_for = |id| {
        hits.iter()
            .find(|hit| hit.entity_id == id)
            .expect("expected fused hit")
            .score
    };

    assert_eq!(score_for(text_only), rrf_score(1, RRF_K));
    assert_eq!(score_for(both), rrf_score(2, RRF_K) + rrf_score(1, RRF_K));
    assert_eq!(score_for(vector_only), rrf_score(2, RRF_K));
}

#[test]
fn rrf_fuse_counts_duplicate_once_per_leg() {
    let id = Uuid::new_v4();
    let text = vec![text_hit(id, 1, "A"), text_hit(id, 2, "A duplicate")];
    let vector = vec![vector_hit(id, 1), vector_hit(id, 2)];

    let hits = rrf_fuse(text, vector, 10, "query");

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].source, SearchSource::Both);
    assert_eq!(hits[0].score, rrf_score(1, RRF_K) + rrf_score(1, RRF_K));
}

#[test]
fn rrf_fuse_respects_limit() {
    let hits: Vec<TextSearchHit> = (0..20)
        .map(|i| text_hit(Uuid::new_v4(), i + 1, "x"))
        .collect();
    let fused = rrf_fuse(hits, vec![], 5, "query");
    assert_eq!(fused.len(), 5);
}

#[test]
fn rrf_fuse_orders_higher_score_first() {
    // Same UUID in both lists at rank 1 → score 2/(10+1). Different UUIDs → 1/(10+1) each.
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let text = vec![text_hit(a, 1, "A")];
    let vec = vec![vector_hit(a, 1), vector_hit(b, 2)];
    let hits = rrf_fuse(text, vec, 10, "query");
    assert_eq!(hits[0].entity_id, a);
    assert_eq!(hits[0].source, SearchSource::Both);
    assert!(hits[0].score > hits[1].score);
}

#[test]
fn rrf_fuse_k10_score_spread_exceeds_threshold() {
    // With k=10: rank 1 → 1/11 ≈ 0.0909, rank 10 → 1/20 = 0.0500.
    // Spread ≈ 0.041, well above the 0.03 minimum required for reliable dedup.
    let ids: Vec<Uuid> = (0..10).map(|_| Uuid::new_v4()).collect();
    let text: Vec<TextSearchHit> = ids
        .iter()
        .enumerate()
        .map(|(i, &id)| text_hit(id, (i + 1) as u32, "x"))
        .collect();
    let hits = rrf_fuse(text, vec![], 10, "query");
    assert_eq!(hits.len(), 10);
    let top_score = hits[0].score.to_f64();
    let bottom_score = hits[9].score.to_f64();
    let spread = top_score - bottom_score;
    assert!(
        spread >= 0.03,
        "score spread {spread:.4} between rank 1 and rank 10 must be ≥ 0.03 (was {spread:.4})"
    );
}

#[test]
fn rrf_fuse_exact_match_boost_elevates_score() {
    // An entity whose title exactly matches the query should receive a score
    // significantly above a non-matching entity ranked first by text search.
    let exact_id = Uuid::new_v4();
    let other_id = Uuid::new_v4();
    // other_id ranks 1 in text, exact_id ranks 2 — but exact_id matches query.
    let text = vec![
        text_hit(other_id, 1, "something else"),
        text_hit(exact_id, 2, "FlashAttention"),
    ];
    let hits = rrf_fuse(text, vec![], 10, "flashattention");
    assert_eq!(hits.len(), 2);
    assert_eq!(
        hits[0].entity_id, exact_id,
        "exact match must rank first despite being rank-2 in raw text search"
    );
}

// ---- embed_batch tests ----

#[test]
fn embed_batch_unconfigured_on_memory_runtime() {
    // KhiveRuntime::memory() has no embedding model — embed_batch returns Unconfigured.
    let rt = KhiveRuntime::memory().unwrap();
    let result = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(rt.embed_batch(&[]));
    // Empty slice short-circuits before hitting the model check.
    assert!(result.is_ok());
    assert!(result.unwrap().is_empty());
}

#[test]
fn embed_batch_empty_input_returns_empty_vec() {
    // No model needed — empty slice is handled before the embedder is touched.
    let rt = KhiveRuntime::memory().unwrap();
    let result = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(rt.embed_batch(&[]));
    assert_eq!(result.unwrap(), Vec::<Vec<f32>>::new());
}

#[test]
fn embed_batch_no_model_non_empty_returns_unconfigured() {
    let rt = KhiveRuntime::memory().unwrap();
    let texts = vec!["hello".to_string()];
    let result = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(rt.embed_batch(&texts));
    match result {
        Err(crate::RuntimeError::Unconfigured(s)) => assert_eq!(s, "embedding_model"),
        Err(other) => panic!("expected Unconfigured, got {:?}", other),
        Ok(_) => panic!("expected Err, got Ok"),
    }
}

#[test]
#[ignore = "loads ~80 MB model; run with --include-ignored"]
fn embed_batch_count_matches_input() {
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("test").unwrap(),
        embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::default()
    };
    let rt = KhiveRuntime::new(config).unwrap();
    let texts: Vec<String> = vec!["foo".to_string(), "bar".to_string(), "baz".to_string()];
    let result = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(rt.embed_batch(&texts));
    let embeddings = result.unwrap();
    assert_eq!(embeddings.len(), texts.len());
}

#[test]
fn vector_search_requires_embedding_or_text() {
    let rt = KhiveRuntime::memory().unwrap();
    let tok = NamespaceToken::local();
    let result = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(rt.vector_search(&tok, None, None, 10, Some(SubstrateKind::Entity)));
    match result {
        Err(crate::RuntimeError::InvalidInput(msg)) => {
            assert!(msg.contains("query_embedding or query_text"), "msg: {msg}");
        }
        other => panic!("expected InvalidInput, got {other:?}"),
    }
}

#[test]
fn vector_search_text_without_model_returns_unconfigured() {
    let rt = KhiveRuntime::memory().unwrap();
    let tok = NamespaceToken::local();
    let result = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(rt.vector_search(
            &tok,
            None,
            Some("attention"),
            10,
            Some(SubstrateKind::Entity),
        ));
    match result {
        Err(crate::RuntimeError::Unconfigured(s)) => assert_eq!(s, "embedding_model"),
        other => panic!("expected Unconfigured, got {other:?}"),
    }
}

#[test]
#[ignore = "loads ~80 MB model; run with --include-ignored"]
fn embed_batch_vectors_have_expected_dimensions() {
    let model = EmbeddingModel::AllMiniLmL6V2;
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("test").unwrap(),
        embedding_model: Some(model),
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::default()
    };
    let rt = KhiveRuntime::new(config).unwrap();
    let texts = vec!["hello world".to_string()];
    let result = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(rt.embed_batch(&texts));
    let embeddings = result.unwrap();
    assert_eq!(embeddings[0].len(), model.dimensions());
}

// ---- hybrid_search_outcome: vector-arm failure must not discard text hits ----

/// Baseline (red without the fix): the fail-loud `hybrid_search` must
/// still propagate a vector-arm failure as a whole-call error, even
/// though the text leg found a match — proving `break_vector_arm`
/// genuinely exercises the vector leg and that `hybrid_search_outcome`'s
/// tolerance below is a real behavioral difference, not a no-op.
#[tokio::test]
async fn hybrid_search_still_fails_loud_on_vector_arm_error() {
    let (rt, fail) = runtime_with_toggleable_embeddings();
    let tok = NamespaceToken::local();
    rt.create_entity(
        &tok,
        "concept",
        None,
        "FlashAttention",
        Some("IO-aware exact attention using tiling"),
        None,
        vec![],
    )
    .await
    .unwrap();
    break_vector_arm(&fail);

    let result = rt
        .hybrid_search(&tok, "FlashAttention", None, 10, None, None, &[], None)
        .await;

    assert!(
        result.is_err(),
        "the fail-loud entry point must still propagate a vector-arm failure, got {result:?}"
    );
}

/// Green: the coordinator-only outcome variant preserves the text hit and
/// reports the vector failure separately instead of discarding the whole
/// call.
#[tokio::test]
async fn hybrid_search_outcome_preserves_text_hits_on_vector_arm_error() {
    let (rt, fail) = runtime_with_toggleable_embeddings();
    let tok = NamespaceToken::local();
    rt.create_entity(
        &tok,
        "concept",
        None,
        "FlashAttention",
        Some("IO-aware exact attention using tiling"),
        None,
        vec![],
    )
    .await
    .unwrap();
    break_vector_arm(&fail);

    let outcome = rt
        .hybrid_search_outcome(&tok, "FlashAttention", 10, None, None, &[], None)
        .await
        .expect("text leg must still succeed");

    assert!(
        !outcome.hits.is_empty(),
        "text arm's hit must survive a vector-arm failure"
    );
    assert!(
        outcome.hits[0]
            .title
            .as_deref()
            .unwrap_or_default()
            .contains("FlashAttention"),
        "surviving hit must be the text match"
    );
    let vector_error = outcome
        .vector_error
        .expect("vector arm failure must be reported");
    assert!(
        vector_error.contains("injected vector-arm failure"),
        "vector_error must carry the underlying cause, got {vector_error:?}"
    );
}

/// A healthy vector arm still leaves `vector_error` `None` — the outcome
/// variant does not manufacture a failure where there is none.
#[tokio::test]
async fn hybrid_search_outcome_has_no_vector_error_when_vector_arm_healthy() {
    let rt = runtime_with_constant_embeddings();
    let tok = NamespaceToken::local();
    rt.create_entity(
        &tok,
        "concept",
        None,
        "FlashAttention",
        Some("IO-aware exact attention using tiling"),
        None,
        vec![],
    )
    .await
    .unwrap();

    let outcome = rt
        .hybrid_search_outcome(&tok, "FlashAttention", 10, None, None, &[], None)
        .await
        .expect("hybrid search must succeed");

    assert!(!outcome.hits.is_empty(), "should find the entity");
    assert!(
        outcome.vector_error.is_none(),
        "a healthy vector arm must not report an error"
    );
}

// ---- hybrid_search enrichment ----

#[tokio::test]
async fn hybrid_search_entity_hit_has_title() {
    let rt = KhiveRuntime::memory().unwrap();
    let tok = NamespaceToken::local();
    rt.create_entity(
        &tok,
        "concept",
        None,
        "FlashAttention",
        Some("IO-aware exact attention using tiling"),
        None,
        vec![],
    )
    .await
    .unwrap();

    let hits = rt
        .hybrid_search(&tok, "FlashAttention", None, 10, None, None, &[], None)
        .await
        .unwrap();

    assert!(!hits.is_empty(), "should find the entity");
    let hit = &hits[0];
    assert!(hit.title.is_some(), "title must be populated");
    assert!(
        hit.title.as_deref().unwrap().contains("FlashAttention"),
        "title must contain entity name"
    );
}

/// `hybrid_search` must not hard-fail on a query containing FTS5 metacharacters
/// like `$` (e.g. the DSL doc query `$prev.id`). `sanitize_fts5_query` (khive-db)
/// strips `$`, so this exercises the sanitizer path and takes the `Ok` arm; see
/// `hybrid_search_with_residual_fts5_char_now_sanitized` below for the character
/// class #916 closed (previously the fail-loud arm, prior to #916).
#[tokio::test]
async fn hybrid_search_with_dollar_sign_query_does_not_error() {
    let rt = KhiveRuntime::memory().unwrap();
    let tok = NamespaceToken::local();
    rt.create_entity(
        &tok,
        "concept",
        None,
        "DSL docs",
        Some("use $prev.id to chain calls"),
        None,
        vec![],
    )
    .await
    .unwrap();

    let result = rt
        .hybrid_search(&tok, "$prev.id", None, 10, None, None, &[], None)
        .await;

    assert!(
        result.is_ok(),
        "#388 hybrid_search must not hard-fail on a '$'-bearing query, got: {:?}",
        result.err()
    );
}

/// #916: `@` was previously NOT stripped by `sanitize_fts5_query` and SQLite
/// FTS5's bareword parser rejected it unconditionally, so this query used to
/// reach the runtime-level fail-loud arm (`RuntimeError::InvalidInput`, #569).
/// `sanitize_fts5_token_group`'s bareword-safety gate now recognizes `@` (and
/// every other ASCII punctuation character not already handled) as unsafe for
/// an unquoted bareword position and routes it through the quoted-phrase
/// alternative instead, which FTS5 accepts literally, so the query
/// now succeeds and finds the seeded content rather than erroring.
#[tokio::test]
async fn hybrid_search_with_residual_fts5_char_now_sanitized() {
    let rt = KhiveRuntime::memory().unwrap();
    let tok = NamespaceToken::local();
    rt.create_entity(
        &tok,
        "concept",
        None,
        "DSL docs",
        Some("use foo@bar to chain calls"),
        None,
        vec![],
    )
    .await
    .unwrap();

    let result = rt
        .hybrid_search(&tok, "foo@bar", None, 10, None, None, &[], None)
        .await;

    let hits = result.unwrap_or_else(|e| {
        panic!("#916 hybrid_search must not fail on an '@'-bearing query, got: {e:?}")
    });
    assert!(
        !hits.is_empty(),
        "#916 '@'-bearing query must still find the seeded 'foo@bar' content via the \
             quoted-phrase alternative"
    );
}

/// #916 end-to-end regression, using the exact character classes from the
/// issue's live-log evidence (`#682 Stage 2`, `Min-K%Prob`, `B=128`):
/// `hybrid_search`'s FTS leg must not lose its lexical signal to a parser
/// syntax error on `#`, `%`, or `=`. Each query below must both succeed
/// and actually surface a `Text`/`Both`-sourced hit, proving the FTS leg
/// contributed, not just that the vector leg papered over a degraded
/// text leg.
#[tokio::test]
async fn hybrid_search_with_916_issue_characters_finds_text_leg_hits() {
    let rt = KhiveRuntime::memory().unwrap();
    let tok = NamespaceToken::local();

    rt.create_entity(
        &tok,
        "concept",
        None,
        "issue tracker",
        Some("tracking #682 Stage 2: MoE expert-cache prefetch work"),
        None,
        vec![],
    )
    .await
    .unwrap();
    rt.create_entity(
        &tok,
        "concept",
        None,
        "benchmark notes",
        Some("chunkwise B=128 traffic arithmetic simdgroup_matrix DPLR"),
        None,
        vec![],
    )
    .await
    .unwrap();
    rt.create_entity(
        &tok,
        "concept",
        None,
        "sampling notes",
        Some("evaluated with the Min-K%Prob membership inference method"),
        None,
        vec![],
    )
    .await
    .unwrap();

    for query in ["#682 Stage 2", "B=128", "Min-K%Prob"] {
        let result = rt
            .hybrid_search(&tok, query, None, 10, None, None, &[], None)
            .await;
        let hits = result.unwrap_or_else(|e| {
            panic!("#916 hybrid_search must not fail on query {query:?}, got: {e:?}")
        });
        assert!(
            hits.iter()
                .any(|h| matches!(h.source, SearchSource::Text | SearchSource::Both)),
            "#916 query {query:?} must surface a Text/Both-sourced hit \
                 (the FTS leg must contribute, not just the vector leg); got {hits:?}"
        );
    }
}

// ---- predicate pushdown before truncation ----

/// Entity-branch tag-filter regression.
///
/// Scenario: `limit=1`, tag_filter=["target-tag"]. Two entities are inserted:
///   - "decoy_alpha_beta_gamma": many query tokens → ranks 1 in FTS (dominates).
///     Does NOT have "target-tag".
///   - "alpha_beta_gamma target": fewer query tokens → ranks 2 in FTS.
///     HAS "target-tag".
///
/// Without predicate pushdown, `fused.truncate(1)` keeps only the decoy and the
/// tag-matching entity is invisible: this requires `tags_any` to be passed into
/// `query_entities`'s `EntityFilter` so the decoy is excluded before truncation.
#[tokio::test]
async fn hybrid_search_tag_filter_pushed_before_truncation() {
    let rt = KhiveRuntime::memory().unwrap();
    let tok = NamespaceToken::local();

    // Decoy: high-ranking FTS hit (content repeats query words), no target tag.
    rt.create_entity(
        &tok,
        "concept",
        None,
        "alpha beta gamma decoy alpha beta gamma",
        Some("alpha beta gamma decoy description alpha beta gamma"),
        None,
        vec!["other-tag".to_string()],
    )
    .await
    .unwrap();

    // Target: lower-ranking FTS hit, has the tag we filter on.
    let target = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "alpha beta gamma target",
            Some("alpha beta gamma target description"),
            None,
            vec!["target-tag".to_string()],
        )
        .await
        .unwrap();

    // With limit=1 and tag_filter, the fix must return the target entity despite
    // the decoy ranking higher. Without pushdown, the decoy occupies the single
    // slot and the target is silently dropped.
    let hits = rt
        .hybrid_search(
            &tok,
            "alpha beta gamma",
            None,
            1,
            None,
            None,
            &["target-tag".to_string()],
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        hits.len(),
        1,
        "exactly one hit expected (the tag-matching entity)"
    );
    assert_eq!(
        hits[0].entity_id, target.id,
        "the tag-filtered entity must be returned even when ranked below limit in raw fusion"
    );
}

/// Entity-branch properties-filter regression (analogous to the tag-filter test above).
///
/// Scenario: `limit=1`, properties_filter={{"domain": "target"}}. Two entities:
///   - decoy: high FTS rank, properties {{"domain": "other"}}.
///   - target: lower FTS rank, properties {{"domain": "target"}}.
///
/// Without pushdown: decoy fills the slot, target is dropped. With pushdown:
/// only the target survives the properties filter before truncation.
#[tokio::test]
async fn hybrid_search_props_filter_pushed_before_truncation() {
    let rt = KhiveRuntime::memory().unwrap();
    let tok = NamespaceToken::local();

    rt.create_entity(
        &tok,
        "concept",
        None,
        "delta epsilon zeta decoy delta epsilon zeta",
        Some("delta epsilon zeta decoy description delta epsilon zeta"),
        Some(serde_json::json!({"domain": "other"})),
        vec![],
    )
    .await
    .unwrap();

    let target = rt
        .create_entity(
            &tok,
            "concept",
            None,
            "delta epsilon zeta target",
            Some("delta epsilon zeta target description"),
            Some(serde_json::json!({"domain": "target"})),
            vec![],
        )
        .await
        .unwrap();

    let filter = serde_json::json!({"domain": "target"});
    let hits = rt
        .hybrid_search(
            &tok,
            "delta epsilon zeta",
            None,
            1,
            None,
            None,
            &[],
            Some(&filter),
        )
        .await
        .unwrap();

    assert_eq!(hits.len(), 1, "exactly one hit expected (properties match)");
    assert_eq!(
        hits[0].entity_id, target.id,
        "the properties-filtered entity must be returned even when ranked below limit"
    );
}

/// Entity-branch kind-filter regression.
///
/// Scenario: `limit=1`, so the text arm fetches 4 candidates, and
/// `entity_kind="document"`. Twelve `concept` entities repeat the query term and
/// outrank the one `document` entity that mentions it once in a long description.
///
/// When the kind is applied only after the text arm has picked its candidates,
/// the filter sees four concepts and the document is never returned. The kind
/// has to reach the text query so the candidate budget is spent on documents.
#[tokio::test]
async fn hybrid_search_entity_kind_filter_pushed_into_text_arm() {
    let rt = KhiveRuntime::memory().unwrap();
    let tok = NamespaceToken::local();

    for i in 0..12 {
        rt.create_entity(
            &tok,
            "concept",
            None,
            &format!("quillfeather decoy {i}"),
            Some("quillfeather quillfeather quillfeather"),
            None,
            vec![],
        )
        .await
        .unwrap();
    }

    let description = format!(
        "A long administrative description that mentions quillfeather once. {}",
        "Unrelated filing, scheduling and review words. ".repeat(20)
    );
    let target = rt
        .create_entity(
            &tok,
            "document",
            None,
            "Archive filing report",
            Some(description.as_str()),
            None,
            vec![],
        )
        .await
        .unwrap();

    // Premise: without a kind filter the target ranks behind more entities than
    // the 4 candidates the filtered `limit=1` search requests from the text arm.
    let unfiltered = rt
        .hybrid_search(&tok, "quillfeather", None, 13, None, None, &[], None)
        .await
        .unwrap();
    let target_rank = unfiltered
        .iter()
        .position(|hit| hit.entity_id == target.id)
        .expect("the document is found without a kind filter");
    assert!(
        target_rank >= 4,
        "the document must rank behind the candidate window, got rank {target_rank}"
    );

    let hits = rt
        .hybrid_search(
            &tok,
            "quillfeather",
            None,
            1,
            Some("document"),
            None,
            &[],
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        hits.len(),
        1,
        "the kind-filtered search must return the matching document"
    );
    assert_eq!(
        hits[0].entity_id, target.id,
        "the returned hit must be the document, not a concept"
    );
}

/// `hybrid_search_each_kind` gives each kind the list `hybrid_search` returns for it.
///
/// Scenario: twenty `concept` entities repeat the query term and outrank three
/// `document` entities that mention it once, with `limit=2`. The text arm fetches 8
/// candidates per kind, so the concepts alone overflow it and the cut to `limit`
/// binds. A text budget shared by the two kinds would leave the documents none; each
/// kind has to keep its own, so both lists equal the per-kind search.
#[tokio::test]
async fn hybrid_search_each_kind_matches_per_kind_search_when_one_kind_dominates() {
    let rt = KhiveRuntime::memory().unwrap();
    let tok = NamespaceToken::local();

    for i in 0..20 {
        rt.create_entity(
            &tok,
            "concept",
            None,
            &format!("quillfeather decoy {i}"),
            Some("quillfeather quillfeather quillfeather"),
            None,
            vec![],
        )
        .await
        .unwrap();
    }
    for i in 0..3 {
        let description = format!(
            "A long administrative description {i} that mentions quillfeather once. {}",
            "Unrelated filing, scheduling and review words. ".repeat(20)
        );
        rt.create_entity(
            &tok,
            "document",
            None,
            &format!("Archive filing report {i}"),
            Some(description.as_str()),
            None,
            vec![],
        )
        .await
        .unwrap();
    }

    let kinds = ["concept", "document"];
    let mut expected: Vec<Vec<Uuid>> = Vec::new();
    for kind in kinds {
        let hits = rt
            .hybrid_search(&tok, "quillfeather", None, 2, Some(kind), None, &[], None)
            .await
            .unwrap();
        expected.push(hits.iter().map(|hit| hit.entity_id).collect());
    }
    assert_eq!(
        expected[0].len(),
        2,
        "premise: the concept list is cut to limit"
    );
    assert_eq!(
        expected[1].len(),
        2,
        "premise: the document list is not starved"
    );

    let per_kind = rt
        .hybrid_search_each_kind(&tok, "quillfeather", None, 2, &kinds)
        .await
        .unwrap();
    let mut actual: Vec<Vec<Uuid>> = Vec::new();
    for hits in &per_kind {
        actual.push(hits.iter().map(|hit| hit.entity_id).collect());
    }
    assert_eq!(
        actual, expected,
        "each kind must get the ids and order its own per-kind search returns"
    );
}

// ---- embed intent tests ----

struct CapturingEmbeddingService {
    dimensions: usize,
    captured: std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>>,
}

#[async_trait::async_trait]
impl EmbeddingService for CapturingEmbeddingService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> std::result::Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
        self.captured.lock().unwrap().push(texts.to_vec());
        Ok(texts.iter().map(|_| vec![1.0; self.dimensions]).collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "capturing-embedding-service"
    }
}

struct CapturingEmbedderProvider {
    name: String,
    captured: std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>>,
}

struct RewritingPassageService {
    dimensions: usize,
    captured: std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>>,
}

#[async_trait::async_trait]
impl EmbeddingService for RewritingPassageService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> std::result::Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
        self.captured.lock().unwrap().push(texts.to_vec());
        Ok(texts.iter().map(|_| vec![1.0; self.dimensions]).collect())
    }

    async fn embed_passage(
        &self,
        texts: &[String],
        model: EmbeddingModel,
    ) -> std::result::Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
        let prepared: Vec<String> = texts.iter().map(|text| format!("custom:{text}")).collect();
        self.embed(&prepared, model).await
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "rewriting-passage-service"
    }
}

struct RewritingPassageProvider {
    name: String,
    captured: std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>>,
}

#[async_trait::async_trait]
impl EmbedderProvider for RewritingPassageProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn dimensions(&self) -> usize {
        crate::config::parse_embedding_model_alias(&self.name)
            .map(|model| model.dimensions())
            .unwrap_or(1)
    }

    async fn build(&self) -> crate::error::RuntimeResult<std::sync::Arc<dyn EmbeddingService>> {
        Ok(std::sync::Arc::new(RewritingPassageService {
            dimensions: self.dimensions(),
            captured: std::sync::Arc::clone(&self.captured),
        }))
    }
}

struct WrongCardinalityEmbeddingService;

#[async_trait::async_trait]
impl EmbeddingService for WrongCardinalityEmbeddingService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> std::result::Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
        Ok(texts
            .iter()
            .take(texts.len().saturating_sub(1))
            .map(|_| vec![1.0])
            .collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "wrong-cardinality-embedding-service"
    }
}

struct WrongCardinalityEmbedderProvider;

#[async_trait::async_trait]
impl EmbedderProvider for WrongCardinalityEmbedderProvider {
    fn name(&self) -> &str {
        "wrong-cardinality-embedding-service"
    }

    fn dimensions(&self) -> usize {
        1
    }

    async fn build(&self) -> crate::error::RuntimeResult<std::sync::Arc<dyn EmbeddingService>> {
        Ok(std::sync::Arc::new(WrongCardinalityEmbeddingService))
    }
}

struct SurplusCardinalityEmbeddingService;

#[async_trait::async_trait]
impl EmbeddingService for SurplusCardinalityEmbeddingService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> std::result::Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
        let mut vectors: Vec<Vec<f32>> = texts.iter().map(|_| vec![1.0]).collect();
        vectors.push(vec![1.0]);
        Ok(vectors)
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "surplus-cardinality-embedding-service"
    }
}

struct SurplusCardinalityEmbedderProvider;

#[async_trait::async_trait]
impl EmbedderProvider for SurplusCardinalityEmbedderProvider {
    fn name(&self) -> &str {
        "surplus-cardinality-embedding-service"
    }

    fn dimensions(&self) -> usize {
        1
    }

    async fn build(&self) -> crate::error::RuntimeResult<std::sync::Arc<dyn EmbeddingService>> {
        Ok(std::sync::Arc::new(SurplusCardinalityEmbeddingService))
    }
}

#[async_trait::async_trait]
impl EmbedderProvider for CapturingEmbedderProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn dimensions(&self) -> usize {
        crate::config::parse_embedding_model_alias(&self.name)
            .map(|model| model.dimensions())
            .unwrap_or(1)
    }

    async fn build(&self) -> crate::error::RuntimeResult<std::sync::Arc<dyn EmbeddingService>> {
        Ok(std::sync::Arc::new(CapturingEmbeddingService {
            dimensions: self.dimensions(),
            captured: std::sync::Arc::clone(&self.captured),
        }))
    }
}

fn runtime_with_capturing_embedder(
    model: EmbeddingModel,
) -> (
    KhiveRuntime,
    std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>>,
) {
    let runtime = KhiveRuntime::memory().unwrap();
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    runtime.register_embedder(CapturingEmbedderProvider {
        name: model.to_string(),
        captured: std::sync::Arc::clone(&captured),
    });
    (runtime, captured)
}

#[tokio::test]
async fn bge_query_paths_pass_raw_unprefixed_text() {
    const BGE_QUERY_INSTRUCTION: &str = "Represent this sentence for searching relevant passages: ";
    let single = "single raw query";
    let batch = vec![
        "first raw query".to_string(),
        "second raw query".to_string(),
    ];

    for model in [
        EmbeddingModel::BgeSmallEnV15,
        EmbeddingModel::BgeBaseEnV15,
        EmbeddingModel::BgeLargeEnV15,
    ] {
        let (runtime, captured) = runtime_with_capturing_embedder(model);
        runtime
            .embed_query_with_model(&model.to_string(), single)
            .await
            .unwrap();
        runtime
            .embed_query_batch_with_model(&model.to_string(), &batch)
            .await
            .unwrap();

        let calls = captured.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec![vec![single.to_string()], batch.clone()],
            "{model} must receive raw query text through single and batch paths"
        );
        assert!(
            calls
                .iter()
                .flatten()
                .all(|text| !text.contains(BGE_QUERY_INSTRUCTION)),
            "{model} must not receive the BGE retrieval instruction"
        );
    }
}

#[tokio::test]
async fn e5_query_paths_apply_query_prefix() {
    let model = EmbeddingModel::MultilingualE5Small;
    let single = "single raw query";
    let batch = vec![
        "first raw query".to_string(),
        "second raw query".to_string(),
    ];
    let (runtime, captured) = runtime_with_capturing_embedder(model);

    runtime
        .embed_query_with_model(&model.to_string(), single)
        .await
        .unwrap();
    runtime
        .embed_query_batch_with_model(&model.to_string(), &batch)
        .await
        .unwrap();

    assert_eq!(
        captured.lock().unwrap().as_slice(),
        [
            vec!["query: single raw query".to_string()],
            vec![
                "query: first raw query".to_string(),
                "query: second raw query".to_string(),
            ],
        ],
        "E5 must receive its query prefix through single and batch paths"
    );
}

#[tokio::test]
async fn custom_provider_query_paths_receive_raw_text_without_lattice_instructions() {
    const CUSTOM: &str = "team-custom-query-embedder";
    assert!(
        crate::config::parse_embedding_model_alias(CUSTOM).is_none(),
        "fixture precondition: {CUSTOM} must not name a built-in model"
    );
    assert_eq!(CUSTOM_QUERY_ROLE_MODEL.query_instruction(), None);
    assert_eq!(CUSTOM_QUERY_ROLE_MODEL.document_instruction(), None);

    let single = "single raw query";
    let batch = vec![
        "first raw query".to_string(),
        "second raw query".to_string(),
    ];
    let runtime = KhiveRuntime::memory().unwrap();
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    runtime.register_embedder(CapturingEmbedderProvider {
        name: CUSTOM.to_string(),
        captured: std::sync::Arc::clone(&captured),
    });

    runtime
        .embed_query_with_model(CUSTOM, single)
        .await
        .unwrap();
    runtime
        .embed_query_batch_with_model(CUSTOM, &batch)
        .await
        .unwrap();

    assert_eq!(
        captured.lock().unwrap().as_slice(),
        [vec![single.to_string()], batch],
        "a custom provider that implements only embed must receive the original query text \
         through single and batch paths"
    );
}

#[tokio::test]
async fn mixed_document_batch_stays_one_ordered_provider_call() {
    let model = EmbeddingModel::AllMiniLmL6V2;
    let (runtime, captured) = runtime_with_capturing_embedder(model);
    let texts = vec![
        "first normal document".to_string(),
        "x".repeat(MAX_TEXT_BYTES + 1),
        "second normal document".to_string(),
    ];

    let outcomes = runtime
        .embed_document_batch_with_model_outcomes(&model.to_string(), &texts)
        .await
        .expect("mixed batch must embed");
    assert_eq!(outcomes.len(), texts.len());
    assert!(!outcomes[0].truncated);
    assert_eq!(outcomes[0].source_bytes, outcomes[0].embedded_bytes);
    assert!(outcomes[1].truncated);
    assert_eq!(outcomes[1].source_bytes, MAX_TEXT_BYTES + 1);
    assert_eq!(outcomes[1].embedded_bytes, MAX_TEXT_BYTES);
    assert!(!outcomes[2].truncated);
    assert_eq!(
        captured.lock().unwrap().as_slice(),
        [vec![
            texts[0].clone(),
            "x".repeat(MAX_TEXT_BYTES),
            texts[2].clone(),
        ]]
    );
}

#[tokio::test]
async fn canonical_name_override_has_no_exact_input_fingerprint() {
    let model = EmbeddingModel::MultilingualE5Small;
    let model_name = model.to_string();
    let (runtime, captured) = runtime_with_capturing_embedder(model);
    let input = "x".repeat(document_embedding_budget(&model_name) + 3);
    let outcomes = runtime
        .embed_document_batch_with_model_outcomes(&model_name, std::slice::from_ref(&input))
        .await
        .unwrap();
    let calls = captured.lock().unwrap();
    let actual_input = &calls[0][0];
    assert!(actual_input.starts_with("passage: "));
    assert_eq!(actual_input.len(), MAX_TEXT_BYTES);
    assert!(outcomes[0].truncated);
    assert_eq!(outcomes[0].prepared_text_fingerprint, None);
    assert_eq!(
        prepared_document_fingerprint(
            bounded_embedding_input(&input, document_embedding_budget(&model_name)).0,
            model,
        ),
        VectorRecord::fingerprint_text(actual_input),
        "the audited lattice path hashes the bounded text plus passage prefix"
    );
}

#[tokio::test]
async fn ordinary_custom_document_provider_has_no_exact_input_fingerprint() {
    let runtime = KhiveRuntime::memory().unwrap();
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    runtime.register_embedder(CapturingEmbedderProvider {
        name: "custom-document-provider".to_string(),
        captured: std::sync::Arc::clone(&captured),
    });

    let outcome = runtime
        .embed_document_with_model_outcome("custom-document-provider", "document")
        .await
        .expect("custom provider must embed");
    assert_eq!(outcome.prepared_text_fingerprint, None);
    assert_eq!(captured.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn custom_passage_override_under_builtin_name_cannot_claim_exact_input() {
    let runtime = KhiveRuntime::memory().unwrap();
    let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let model = EmbeddingModel::MultilingualE5Small;
    let model_name = model.to_string();
    runtime.register_embedder(RewritingPassageProvider {
        name: model_name.clone(),
        captured: std::sync::Arc::clone(&captured),
    });

    let outcome = runtime
        .embed_document_with_model_outcome(&model_name, "document")
        .await
        .expect("custom override must embed");
    assert_eq!(outcome.prepared_text_fingerprint, None);
    assert_eq!(
        captured.lock().unwrap()[0],
        vec!["custom:document".to_string()]
    );
    assert_ne!(
        prepared_document_fingerprint("document", model),
        VectorRecord::fingerprint_text("custom:document")
    );
}

#[tokio::test]
async fn truncated_document_batch_rejects_provider_cardinality_mismatch() {
    let runtime = KhiveRuntime::memory().unwrap();
    runtime.register_embedder(WrongCardinalityEmbedderProvider);
    let model_name = "wrong-cardinality-embedding-service";
    let texts = vec!["x".repeat(MAX_TEXT_BYTES + 1), "normal".to_string()];

    let error = runtime
        .embed_document_batch_with_model_outcomes(model_name, &texts)
        .await
        .expect_err("provider cardinality mismatch must fail the whole batch");

    assert!(
        error
            .to_string()
            .contains("embed_passage returned 1 vectors for 2 inputs"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn singleton_document_embed_rejects_zero_vectors() {
    let runtime = KhiveRuntime::memory().unwrap();
    runtime.register_embedder(WrongCardinalityEmbedderProvider);
    let model_name = "wrong-cardinality-embedding-service";

    let error = runtime
        .embed_document_with_model_outcome(model_name, "single document")
        .await
        .expect_err("provider returning zero vectors must fail closed");

    assert!(
        error
            .to_string()
            .contains("embed_passage returned 0 vectors for 1 input"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn singleton_document_embed_rejects_surplus_vectors() {
    let runtime = KhiveRuntime::memory().unwrap();
    runtime.register_embedder(SurplusCardinalityEmbedderProvider);
    let model_name = "surplus-cardinality-embedding-service";

    let error = runtime
        .embed_document_with_model_outcome(model_name, "single document")
        .await
        .expect_err("provider returning surplus vectors must fail closed");

    assert!(
        error
            .to_string()
            .contains("embed_passage returned 2 vectors for 1 input"),
        "unexpected error: {error}"
    );
}

#[test]
#[ignore = "loads ~80 MB model; run with --include-ignored"]
fn minilm_document_and_query_embed_are_identical_no_prefix_model() {
    // MiniLM has no instruction prefixes; document and query paths must
    // produce byte-identical vectors so that existing stored vectors remain
    // comparable after this change.
    let model = EmbeddingModel::AllMiniLmL6V2;
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("test").unwrap(),
        embedding_model: Some(model),
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::default()
    };
    let rt = KhiveRuntime::new(config).unwrap();
    let text = "attention is all you need".to_string();
    let rt_ref = &rt;
    let (doc_emb, query_emb) = tokio::runtime::Runtime::new().unwrap().block_on(async {
        let d = rt_ref
            .embed_document_with_model_outcome(&model.to_string(), &text)
            .await
            .unwrap()
            .vector;
        let q = rt_ref
            .embed_query_with_model(&model.to_string(), &text)
            .await
            .unwrap();
        (d, q)
    });
    assert_eq!(
        doc_emb, query_emb,
        "MiniLM has no instruction prefix: document and query embeds must be identical"
    );
}

#[test]
#[ignore = "loads multilingual-e5-small (~90 MB); run with --include-ignored"]
fn e5_document_and_query_embed_differ_instruction_tuned_model() {
    // multilingual-e5 prepends "passage: " for documents and "query: " for
    // queries. The same raw text must produce different embeddings when the
    // correct prefixes are applied, confirming the asymmetric-retrieval
    // capability is now exercised.
    let model = EmbeddingModel::MultilingualE5Small;
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("test").unwrap(),
        embedding_model: Some(model),
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::default()
    };
    let rt = KhiveRuntime::new(config).unwrap();
    let text = "attention is all you need".to_string();
    let rt_ref = &rt;
    let (doc_emb, query_emb) = tokio::runtime::Runtime::new().unwrap().block_on(async {
        let d = rt_ref
            .embed_document_with_model_outcome(&model.to_string(), &text)
            .await
            .unwrap()
            .vector;
        let q = rt_ref
            .embed_query_with_model(&model.to_string(), &text)
            .await
            .unwrap();
        (d, q)
    });
    assert_ne!(
        doc_emb, query_emb,
        "multilingual-e5-small uses asymmetric prefixes: document ('passage: ') \
             and query ('query: ') embeds of the same text must differ"
    );
}

// ---- backfill reader error must be propagated, not swallowed ----

use crate::embedder_registry::EmbedderProvider;
use lattice_embed::EmbeddingService;

/// A stub embedder that never actually loads weights — used to satisfy the
/// `registered_embedding_model_names` check inside `backfill_missing_embeddings`
/// without triggering a real model load. The test fault-injects a reader error
/// before any embedding call is made, so `embed()` is never reached.
struct StubEmbedderProvider;

#[async_trait::async_trait]
impl EmbedderProvider for StubEmbedderProvider {
    fn name(&self) -> &str {
        "stub-model-m07"
    }

    fn dimensions(&self) -> usize {
        4
    }

    async fn build(&self) -> crate::error::RuntimeResult<std::sync::Arc<dyn EmbeddingService>> {
        struct StubSvc;
        #[async_trait::async_trait]
        impl EmbeddingService for StubSvc {
            async fn embed(
                &self,
                _texts: &[String],
                _model: lattice_embed::EmbeddingModel,
            ) -> std::result::Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
                Ok(vec![])
            }

            fn supports_model(&self, _model: lattice_embed::EmbeddingModel) -> bool {
                true
            }

            fn name(&self) -> &'static str {
                "stub-svc-m07"
            }
        }
        Ok(std::sync::Arc::new(StubSvc))
    }
}

/// `backfill_missing_embeddings` must propagate a reader error rather than
/// treating it as "zero rows to embed" (a silent `Err(_) => vec![]` would
/// return `Ok(0)` and skip all embeddings without any signal).
///
/// The fault injection substitutes a `StorageError::Pool` for the result of
/// `sql.reader().await`, exercising the `map_err(RuntimeError::Storage)?` path;
/// falling back to `unwrap_or_default()` there would swallow the injected error.
#[tokio::test]
async fn backfill_reader_error_is_propagated_not_swallowed() {
    let rt = KhiveRuntime::memory().unwrap();
    rt.register_embedder(StubEmbedderProvider);
    let tok = NamespaceToken::local();

    // Arm the fault injection: the next backfill call will substitute a
    // StorageError at the sql.reader().await boundary, then reset.
    super::arm_backfill_reader_fail();

    let result = rt.backfill_missing_embeddings(&tok).await;
    assert!(
        result.is_err(),
        "backfill_missing_embeddings must propagate the reader error (got Ok instead)"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("injected failure"),
        "error must originate from the injected reader failure, got: {err_msg}"
    );
}

#[tokio::test]
async fn backfill_skips_a_full_page_of_excluded_messages_and_reaches_eligible_tail() {
    use crate::{NoteEmbeddingPolicy, NoteEmbeddingPolicySpec};
    use khive_storage::note::Note;

    const PAGE: u128 = EMBEDDING_BATCH_PAGE_SIZE as u128;
    let primary = EmbeddingModel::AllMiniLmL6V2;
    let primary_name = primary.to_string();
    let secondary_name = "zz-backfill-secondary";
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: Some(primary),
        packs: vec![],
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    rt.register_embedder(ConstantEmbedderProvider {
        name: primary_name.clone(),
        dimensions: primary.dimensions(),
        fail: Arc::new(AtomicBool::new(false)),
    });
    rt.register_embedder(ConstantEmbedderProvider {
        name: secondary_name.into(),
        dimensions: 4,
        fail: Arc::new(AtomicBool::new(false)),
    });
    rt.install_note_embedding_policies(&[NoteEmbeddingPolicySpec {
        kind: "message",
        policy: NoteEmbeddingPolicy::DefaultModel,
    }]);
    let tok = NamespaceToken::local();
    let primary_store = rt.vectors_for_model(&tok, &primary_name).unwrap();
    let secondary_store = rt.vectors_for_model(&tok, secondary_name).unwrap();
    let notes = rt.notes(&tok).unwrap();
    let mut seeded = Vec::new();
    for ordinal in 1..=PAGE + 1 {
        let mut message = Note::new("local", "message", "excluded from secondary");
        message.id = Uuid::from_u128(ordinal);
        seeded.push(message);
    }
    let mut ordinary = Note::new("local", "observation", "eligible after full page");
    ordinary.id = Uuid::from_u128(u128::MAX);
    seeded.push(ordinary.clone());
    notes.upsert_notes(seeded).await.unwrap();

    let backfilled = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        rt.backfill_missing_embeddings(&tok),
    )
    .await
    .expect("backfill must advance past a full excluded page")
    .unwrap();
    assert_eq!(backfilled, (PAGE + 2) as u64 + 1);
    assert_eq!(primary_store.count().await.unwrap(), (PAGE + 2) as u64);
    assert_eq!(secondary_store.count().await.unwrap(), 1);
    let excluded_document = rt
        .text_for_notes(&tok)
        .unwrap()
        .get_document("local", Uuid::from_u128(1))
        .await
        .unwrap()
        .expect("first-model pass must index an excluded message");
    assert_eq!(excluded_document.body, "excluded from secondary");
    assert!(
        rt.text_for_notes(&tok)
            .unwrap()
            .get_document("local", ordinary.id)
            .await
            .unwrap()
            .is_some(),
        "the first-model pass must still repopulate FTS"
    );
}

const BACKFILL_BATCH_MODEL: &str = "backfill-batch-model";

struct CountingBackfillService {
    calls: Arc<AtomicUsize>,
    reject_poison: bool,
}

#[async_trait::async_trait]
impl EmbeddingService for CountingBackfillService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.reject_poison
            && (texts.len() > 1 || texts.iter().any(|text| text.contains("poison")))
        {
            return Err(EmbedError::InferenceFailed("poison input".into()));
        }
        Ok(texts
            .iter()
            .map(|text| {
                vec![
                    text.len() as f32,
                    text.bytes().map(u32::from).sum::<u32>() as f32,
                    text.as_bytes().first().copied().unwrap_or_default() as f32,
                    text.as_bytes().last().copied().unwrap_or_default() as f32,
                ]
            })
            .collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "backfill-batch-counting-service"
    }
}

struct CountingBackfillProvider {
    calls: Arc<AtomicUsize>,
    reject_poison: bool,
}

#[async_trait::async_trait]
impl EmbedderProvider for CountingBackfillProvider {
    fn name(&self) -> &str {
        BACKFILL_BATCH_MODEL
    }

    fn dimensions(&self) -> usize {
        4
    }

    async fn build(&self) -> crate::error::RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(Arc::new(CountingBackfillService {
            calls: Arc::clone(&self.calls),
            reject_poison: self.reject_poison,
        }))
    }
}

#[tokio::test]
async fn backfill_batches_provider_calls_and_matches_single_record_vectors() {
    let token = NamespaceToken::local();
    let runtime = KhiveRuntime::memory().unwrap();
    let mut entity_ids = Vec::new();
    for index in 0..257 {
        let entity = runtime
            .create_entity(
                &token,
                "concept",
                None,
                &format!("Backfill entity {index}"),
                (index != 0).then_some("body"),
                None,
                vec![],
            )
            .await
            .unwrap();
        entity_ids.push(entity.id);
    }
    let mut note_ids = Vec::new();
    for index in 0..3 {
        let note = runtime
            .create_note(
                &token,
                "observation",
                None,
                &format!("Backfill note {index}"),
                None,
                None,
                vec![],
            )
            .await
            .unwrap();
        note_ids.push(note.id);
    }
    let calls = Arc::new(AtomicUsize::new(0));
    runtime.register_embedder(CountingBackfillProvider {
        calls: Arc::clone(&calls),
        reject_poison: false,
    });
    let usage = crate::usage::UsageContext::new();
    let backfilled =
        crate::usage::scope(usage.clone(), runtime.backfill_missing_embeddings(&token))
            .await
            .unwrap();
    assert_eq!(backfilled, 260);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "two entity pages and one note page"
    );
    assert_eq!(
        usage.snapshot()["embed_calls"],
        260,
        "ADR-103 counts texts, not provider calls"
    );

    let singles = KhiveRuntime::memory().unwrap();
    let single_calls = Arc::new(AtomicUsize::new(0));
    singles.register_embedder(CountingBackfillProvider {
        calls: Arc::clone(&single_calls),
        reject_poison: false,
    });
    for id in &entity_ids {
        let entity = runtime.get_entity(&token, *id).await.unwrap();
        singles
            .entities(&token)
            .unwrap()
            .upsert_entity(entity.clone())
            .await
            .unwrap();
        singles.reindex_entity(&token, &entity).await.unwrap();
    }
    for id in &note_ids {
        let note = runtime
            .notes(&token)
            .unwrap()
            .get_note(*id)
            .await
            .unwrap()
            .unwrap();
        singles
            .notes(&token)
            .unwrap()
            .upsert_note(note.clone())
            .await
            .unwrap();
        singles.reindex_note(&token, &note).await.unwrap();
    }
    assert_eq!(single_calls.load(Ordering::SeqCst), 260);
    let batched_entities = runtime
        .vectors_for_model(&token, BACKFILL_BATCH_MODEL)
        .unwrap()
        .get_vectors(&entity_ids, "local", "entity.body")
        .await
        .unwrap();
    let single_entities = singles
        .vectors_for_model(&token, BACKFILL_BATCH_MODEL)
        .unwrap()
        .get_vectors(&entity_ids, "local", "entity.body")
        .await
        .unwrap();
    assert_eq!(batched_entities.len(), 257);
    assert_eq!(batched_entities, single_entities);
    let batched_notes = runtime
        .vectors_for_model(&token, BACKFILL_BATCH_MODEL)
        .unwrap()
        .get_vectors(&note_ids, "local", "note.content")
        .await
        .unwrap();
    let single_notes = singles
        .vectors_for_model(&token, BACKFILL_BATCH_MODEL)
        .unwrap()
        .get_vectors(&note_ids, "local", "note.content")
        .await
        .unwrap();
    assert_eq!(batched_notes.len(), 3);
    assert_eq!(batched_notes, single_notes);
}

#[tokio::test]
async fn backfill_failed_pages_retry_singly_without_duplicate_index_rows() {
    use khive_storage::types::{SqlStatement, SqlValue};

    let token = NamespaceToken::local();
    let runtime = KhiveRuntime::memory().unwrap();
    for name in ["good one", "poison", "good two"] {
        runtime
            .create_entity(&token, "concept", None, name, Some("body"), None, vec![])
            .await
            .unwrap();
        runtime
            .create_note(&token, "observation", None, name, None, None, vec![])
            .await
            .unwrap();
    }
    let calls = Arc::new(AtomicUsize::new(0));
    runtime.register_embedder(CountingBackfillProvider {
        calls: Arc::clone(&calls),
        reject_poison: true,
    });
    let backfilled = runtime.backfill_missing_embeddings(&token).await.unwrap();
    assert_eq!(backfilled, 4);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        8,
        "two failed pages plus six singleton retries"
    );
    let mut reader = runtime.sql().reader().await.unwrap();
    for (table, expected) in [("ann_write_log", 4), ("vector_provenance", 0)] {
        let count = reader
            .query_scalar(SqlStatement {
                sql: format!("SELECT COUNT(*) FROM {table} WHERE namespace = ?1"),
                params: vec![SqlValue::Text("local".into())],
                label: Some("backfill-batch-no-duplicate-rows".into()),
            })
            .await
            .unwrap();
        assert!(
            matches!(count, Some(SqlValue::Integer(value)) if value == expected),
            "{table} must match the guarded per-record writer"
        );
    }
    assert_eq!(
        runtime.backfill_missing_embeddings(&token).await.unwrap(),
        0
    );
}

// ---- Note-search route attribution (#6075) ----

struct RouteTestAnnProvider {
    owner: KhiveRuntime,
    eligible: bool,
    fail: bool,
}

#[async_trait::async_trait]
impl crate::note_search_ann::NoteSearchAnnProvider for RouteTestAnnProvider {
    fn backend_id(&self) -> &str {
        self.owner.backend_id().as_str()
    }

    fn serves_backend(&self, runtime: &KhiveRuntime) -> bool {
        self.owner.shares_backend_storage_with(runtime)
    }

    async fn search(
        &self,
        _token: &NamespaceToken,
        _model: &str,
        _query_embedding: &[f32],
        _top_k: u32,
    ) -> RuntimeResult<Option<Vec<VectorSearchHit>>> {
        if self.fail {
            return Err(RuntimeError::Internal("injected ANN provider error".into()));
        }
        Ok(self.eligible.then(Vec::new))
    }
}

fn install_route_test_provider(runtime: &KhiveRuntime, eligible: bool, fail: bool) {
    runtime.install_note_search_ann_provider(Arc::new(RouteTestAnnProvider {
        owner: runtime.detached_for_note_search_ann_provider(),
        eligible,
        fail,
    }));
}

async fn run_note_search_vector_route(
    runtime: &KhiveRuntime,
) -> RuntimeResult<Vec<VectorSearchHit>> {
    runtime
        .note_search_vector_search(
            &NamespaceToken::local(),
            Some(vec![1.0; EmbeddingModel::AllMiniLmL6V2.dimensions()]),
            "route test",
            1,
        )
        .await
}

#[tokio::test]
async fn note_search_routes_and_diagnostics_are_scoped_to_the_opened_backend() {
    use khive_db::pool::NoteSearchRouteSnapshot;

    let with_provider = runtime_with_constant_embeddings();
    let without_provider = runtime_with_constant_embeddings();
    assert_eq!(
        with_provider.backend_id().as_str(),
        without_provider.backend_id().as_str()
    );
    install_route_test_provider(&with_provider, true, false);
    let process_before = crate::note_search_ann::route_totals();
    for _ in 0..2 {
        assert!(run_note_search_vector_route(&with_provider)
            .await
            .unwrap()
            .is_empty());
    }
    assert!(run_note_search_vector_route(&without_provider)
        .await
        .unwrap()
        .is_empty());

    let ann_expected = NoteSearchRouteSnapshot {
        ann_route_total: 2,
        ..Default::default()
    };
    let fallback_expected = NoteSearchRouteSnapshot {
        fallback_route_total: 1,
        fallback_no_provider_total: 1,
        ..Default::default()
    };
    assert_eq!(
        with_provider.backend().pool().note_search_route_snapshot(),
        ann_expected
    );
    assert_eq!(
        without_provider
            .backend()
            .pool()
            .note_search_route_snapshot(),
        fallback_expected
    );

    let opened = crate::runtime::OpenedDiagnosticBackend {
        backend_names: vec!["without_provider".into()],
        canonical_path: None,
        pool: without_provider.backend().pool_arc(),
    };
    let fallback_report = with_provider
        .db_diagnostics_for_opened_backend_with_audit_metrics(&opened, None, false)
        .await
        .unwrap();
    assert_eq!(fallback_report.note_search_routes, fallback_expected);
    assert_eq!(
        with_provider
            .db_diagnostics()
            .await
            .unwrap()
            .note_search_routes,
        ann_expected
    );
    assert_eq!(
        without_provider
            .db_diagnostics()
            .await
            .unwrap()
            .note_search_routes,
        fallback_expected
    );
    // Other tests may search concurrently, so process totals have a lower bound.
    let process_after = crate::note_search_ann::route_totals();
    assert!(process_after.0 >= process_before.0.saturating_add(2));
    assert!(process_after.1 >= process_before.1.saturating_add(1));
    assert!(fallback_report.note_search_ann_route_total >= process_before.0.saturating_add(2));
    assert!(fallback_report.note_search_fallback_route_total >= process_before.1.saturating_add(1));
    let value = serde_json::to_value(&fallback_report).unwrap();
    assert_eq!(value["note_search_routes"]["fallback_no_provider_total"], 1);
    assert_eq!(
        without_provider
            .backend()
            .pool()
            .note_search_route_snapshot(),
        fallback_expected,
        "diagnostics must not reset route counters"
    );
}

#[tokio::test]
async fn note_search_fallback_causes_are_distinct_and_provider_errors_do_not_fallback() {
    use khive_db::pool::NoteSearchRouteSnapshot;

    let no_model = KhiveRuntime::memory().unwrap();
    // Supplied vectors reach fallback selection; exact search still requires a model.
    assert!(matches!(
        run_note_search_vector_route(&no_model).await,
        Err(RuntimeError::Unconfigured(_))
    ));
    assert_eq!(
        no_model.backend().pool().note_search_route_snapshot(),
        NoteSearchRouteSnapshot {
            fallback_route_total: 1,
            fallback_no_model_total: 1,
            ..Default::default()
        }
    );

    let no_provider = runtime_with_constant_embeddings();
    run_note_search_vector_route(&no_provider).await.unwrap();
    assert_eq!(
        no_provider.backend().pool().note_search_route_snapshot(),
        NoteSearchRouteSnapshot {
            fallback_route_total: 1,
            fallback_no_provider_total: 1,
            ..Default::default()
        }
    );

    let no_graph = runtime_with_constant_embeddings();
    install_route_test_provider(&no_graph, false, false);
    run_note_search_vector_route(&no_graph).await.unwrap();
    assert_eq!(
        no_graph.backend().pool().note_search_route_snapshot(),
        NoteSearchRouteSnapshot {
            fallback_route_total: 1,
            fallback_no_eligible_graph_total: 1,
            ..Default::default()
        }
    );

    let provider_error = runtime_with_constant_embeddings();
    install_route_test_provider(&provider_error, false, true);
    assert!(matches!(
        run_note_search_vector_route(&provider_error).await,
        Err(RuntimeError::Internal(_))
    ));
    assert_eq!(
        provider_error.backend().pool().note_search_route_snapshot(),
        NoteSearchRouteSnapshot::default()
    );
    let embedding_error = KhiveRuntime::memory().unwrap();
    assert!(embedding_error
        .note_search_vector_search(&NamespaceToken::local(), None, "no model", 1)
        .await
        .is_err());
    assert_eq!(
        embedding_error
            .backend()
            .pool()
            .note_search_route_snapshot(),
        NoteSearchRouteSnapshot::default(),
        "embedding errors precede route selection"
    );
}

#[tokio::test]
async fn note_search_routes_follow_shared_pool_handles_and_end_with_the_pool() {
    use khive_db::pool::NoteSearchRouteSnapshot;

    let backend = Arc::new(khive_db::StorageBackend::memory().unwrap());
    backend.prepare_core_schema().unwrap();
    let pool = backend.pool_arc();
    let weak_pool = Arc::downgrade(&pool);
    drop(pool);
    let config = RuntimeConfig {
        embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
        packs: vec!["kg".into()],
        ..RuntimeConfig::no_embeddings()
    };
    let first = KhiveRuntime::from_backend(Arc::clone(&backend), config.clone());
    let separately_assembled = KhiveRuntime::from_backend(Arc::clone(&backend), config);
    let no_model = KhiveRuntime::from_backend(Arc::clone(&backend), RuntimeConfig::no_embeddings());
    install_route_test_provider(&first, true, false);
    let cloned = first.clone();
    let detached = first.detached_for_note_search_ann_provider();
    run_note_search_vector_route(&cloned).await.unwrap();
    run_note_search_vector_route(&separately_assembled)
        .await
        .unwrap();
    run_note_search_vector_route(&detached).await.unwrap();
    assert!(matches!(
        run_note_search_vector_route(&no_model).await,
        Err(RuntimeError::Unconfigured(_))
    ));
    let expected = NoteSearchRouteSnapshot {
        ann_route_total: 1,
        fallback_route_total: 3,
        fallback_no_model_total: 1,
        fallback_no_provider_total: 2,
        ..Default::default()
    };
    for runtime in [&first, &cloned, &detached, &separately_assembled, &no_model] {
        assert_eq!(
            runtime.backend().pool().note_search_route_snapshot(),
            expected
        );
    }
    let independent = KhiveRuntime::memory().unwrap();
    assert_eq!(
        first.backend_id().as_str(),
        independent.backend_id().as_str()
    );
    assert_eq!(
        independent.backend().pool().note_search_route_snapshot(),
        NoteSearchRouteSnapshot::default()
    );
    drop((
        first,
        cloned,
        detached,
        separately_assembled,
        no_model,
        backend,
    ));
    assert!(
        weak_pool.upgrade().is_none(),
        "counters must not retain their pool"
    );
}
