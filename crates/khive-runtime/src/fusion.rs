//! Fusion strategies for combining ranked result lists.

use std::collections::{hash_map::Entry, HashMap, HashSet};

use uuid::Uuid;

use khive_score::DeterministicScore;
use khive_storage::types::{
    PageRequest, TextFilter, TextQueryMode, TextSearchHit, TextSearchRequest, VectorSearchHit,
};
use khive_storage::EntityFilter;
use khive_types::SubstrateKind;

use crate::error::{RuntimeError, RuntimeResult};
use crate::retrieval::{SearchHit, SearchSource};
use crate::runtime::{KhiveRuntime, NamespaceToken};

pub use khive_fusion::FusionStrategy;

/// A single ranked candidate stream fed into a [`FusionExecutor`] — the
/// entity/note ID keyed shape used throughout hybrid search (ADR-012
/// `FusionStrategy::Custom` §"strategy executor").
pub type CandidateStream = Vec<(Uuid, DeterministicScore)>;

/// One fused, ranked `(id, score)` pair returned by a [`FusionExecutor`].
pub type RankedHit = (Uuid, DeterministicScore);

/// Runtime-registered custom fusion strategy (ADR-012).
///
/// Packs implement this to plug a strategy into `FusionStrategy::Custom { name,
/// .. }` via [`KhiveRuntime::register_fusion_strategy`] — the seam a
/// learned-sparse (SPLADE) retrieval leg plugs into. Async so an executor can
/// perform I/O (e.g. a decay/posterior lookup) while fusing, and fallible so
/// it can reject malformed `params` instead of degrading silently.
#[async_trait::async_trait]
pub trait FusionExecutor: Send + Sync + 'static {
    /// Combine `streams` into a single ranked list, honoring `limit` as a
    /// hint (the dispatch boundary re-sorts and truncates the result with the
    /// crate's canonical comparator regardless, so an executor need not sort
    /// or truncate defensively itself).
    async fn fuse(
        &self,
        streams: Vec<CandidateStream>,
        params: &serde_json::Value,
        limit: usize,
    ) -> RuntimeResult<Vec<RankedHit>>;
}

const CANDIDATE_MULTIPLIER: u32 = 4;

/// RRF convenience wrapper used by operations.rs (k=60 note search path).
pub(crate) async fn rrf_fuse_k(
    rt: &KhiveRuntime,
    text_hits: Vec<TextSearchHit>,
    vector_hits: Vec<VectorSearchHit>,
    k: usize,
    limit: usize,
) -> RuntimeResult<Vec<SearchHit>> {
    rt.fuse_with_strategy(text_hits, vector_hits, &FusionStrategy::Rrf { k }, limit)
        .await
}

impl KhiveRuntime {
    /// Fuse text and vector hits using the given strategy, returning at most
    /// `limit` results. Positional weighted strategies use `[vector, keyword]`
    /// order.
    ///
    /// `FusionStrategy::Custom { name, .. }` is resolved against this
    /// runtime's registered executors (see
    /// [`register_fusion_strategy`](KhiveRuntime::register_fusion_strategy)).
    /// An unregistered name fails closed with
    /// `RuntimeError::UnknownFusionStrategy` rather than silently falling
    /// back to RRF.
    pub(crate) async fn fuse_with_strategy(
        &self,
        text_hits: Vec<TextSearchHit>,
        vector_hits: Vec<VectorSearchHit>,
        strategy: &FusionStrategy,
        limit: usize,
    ) -> RuntimeResult<Vec<SearchHit>> {
        match strategy {
            FusionStrategy::VectorOnly => {
                self.fuse_sources(Vec::new(), vector_hits, strategy, limit)
                    .await
            }
            FusionStrategy::KeywordOnly => {
                self.fuse_sources(text_hits, Vec::new(), strategy, limit)
                    .await
            }
            FusionStrategy::Rrf { .. }
            | FusionStrategy::Weighted { .. }
            | FusionStrategy::Union
            | FusionStrategy::Custom { .. } => {
                self.fuse_sources(text_hits, vector_hits, strategy, limit)
                    .await
            }
        }
    }

    async fn fuse_sources(
        &self,
        text_hits: Vec<TextSearchHit>,
        vector_hits: Vec<VectorSearchHit>,
        strategy: &FusionStrategy,
        limit: usize,
    ) -> RuntimeResult<Vec<SearchHit>> {
        let mut metadata: HashMap<Uuid, SearchHit> =
            HashMap::with_capacity(text_hits.len() + vector_hits.len());

        let text_source: Vec<(Uuid, DeterministicScore)> = text_hits
            .into_iter()
            .map(|h| {
                let hit = SearchHit {
                    entity_id: h.subject_id,
                    score: h.score,
                    source: SearchSource::Text,
                    title: h.title,
                    snippet: h.snippet,
                };
                let id = hit.entity_id;
                let score = hit.score;
                merge_metadata(&mut metadata, hit);
                (id, score)
            })
            .collect();

        let vector_source: Vec<(Uuid, DeterministicScore)> = vector_hits
            .into_iter()
            .map(|h| {
                let hit = SearchHit {
                    entity_id: h.subject_id,
                    score: h.score,
                    source: SearchSource::Vector,
                    title: None,
                    snippet: None,
                };
                let id = hit.entity_id;
                let score = hit.score;
                merge_metadata(&mut metadata, hit);
                (id, score)
            })
            .collect();

        // Canonical positional order is [vector, keyword]. Empty arms remain in
        // place: removing one would shift the surviving arm onto the wrong weight.
        let sources: Vec<Vec<(Uuid, DeterministicScore)>> = vec![vector_source, text_source];

        let fused = self.dispatch_fusion(sources, strategy, limit).await?;

        Ok(fused
            .into_iter()
            .filter_map(|(id, score)| {
                let mut hit = metadata.remove(&id)?;
                hit.score = score;
                Some(hit)
            })
            .collect())
    }

    /// Resolve `strategy` against either the built-in `khive-fusion`
    /// dispatcher or a registered [`FusionExecutor`], applying the crate's
    /// canonical score-desc/id-asc ordering at the boundary either way.
    ///
    /// `Custom` names are resolved *before* the empty-input/zero-limit short
    /// circuit, so a misconfigured name errors on every call -- including
    /// zero-result ones -- rather than being indistinguishable from a valid
    /// empty result.
    async fn dispatch_fusion(
        &self,
        sources: Vec<Vec<(Uuid, DeterministicScore)>>,
        strategy: &FusionStrategy,
        limit: usize,
    ) -> RuntimeResult<Vec<(Uuid, DeterministicScore)>> {
        let FusionStrategy::Custom { name, params } = strategy else {
            return Ok(khive_fusion::fuse(sources, strategy, limit)?);
        };

        let executor = self.fusion_executor(name)?;

        if limit == 0 || sources.iter().all(Vec::is_empty) {
            return Ok(Vec::new());
        }

        let mut hits = executor.fuse(sources, params, limit).await?;
        hits.sort_by(khive_fusion::cmp_desc_then_id);
        hits.truncate(limit);
        Ok(hits)
    }
}

fn merge_metadata(metadata: &mut HashMap<Uuid, SearchHit>, hit: SearchHit) {
    match metadata.entry(hit.entity_id) {
        Entry::Occupied(mut entry) => {
            let existing = entry.get_mut();
            existing.source = merge_sources(existing.source, hit.source);
            if existing.title.is_none() {
                existing.title = hit.title;
            }
            if existing.snippet.is_none() {
                existing.snippet = hit.snippet;
            }
        }
        Entry::Vacant(entry) => {
            entry.insert(hit);
        }
    }
}

fn merge_sources(left: SearchSource, right: SearchSource) -> SearchSource {
    match (left, right) {
        (SearchSource::Both, _) | (_, SearchSource::Both) => SearchSource::Both,
        (SearchSource::Text, SearchSource::Vector) | (SearchSource::Vector, SearchSource::Text) => {
            SearchSource::Both
        }
        (SearchSource::Text, SearchSource::Text) => SearchSource::Text,
        (SearchSource::Vector, SearchSource::Vector) => SearchSource::Vector,
    }
}

impl KhiveRuntime {
    async fn retain_alive_search_hits(
        &self,
        token: &NamespaceToken,
        mut fused: Vec<SearchHit>,
        limit: usize,
    ) -> RuntimeResult<Vec<SearchHit>> {
        // Filter out soft-deleted entities. A single query fetches all alive IDs from the
        // fused candidate pool; any ID absent from the result has been soft-deleted.
        if !fused.is_empty() {
            let candidate_ids: Vec<Uuid> = fused.iter().map(|h| h.entity_id).collect();
            let alive_page = self
                .entities(token)?
                .query_entities(
                    token.namespace().as_str(),
                    EntityFilter {
                        ids: candidate_ids,
                        ..EntityFilter::default()
                    },
                    PageRequest {
                        offset: 0,
                        limit: u32::try_from(fused.len()).unwrap_or(u32::MAX),
                    },
                )
                .await?;
            let alive: HashSet<Uuid> = alive_page.items.into_iter().map(|e| e.id).collect();
            fused.retain(|h| alive.contains(&h.entity_id));
        }

        fused.truncate(limit);
        Ok(fused)
    }

    /// Hybrid search with a caller-supplied fusion strategy.
    ///
    /// `FusionStrategy::Custom { name, .. }` is resolved against this
    /// runtime's registered executors (see
    /// [`register_fusion_strategy`](KhiveRuntime::register_fusion_strategy));
    /// an unregistered name fails closed with
    /// `RuntimeError::UnknownFusionStrategy`.
    pub async fn hybrid_search_with_strategy(
        &self,
        token: &NamespaceToken,
        query_text: &str,
        query_vector: Option<Vec<f32>>,
        strategy: FusionStrategy,
        limit: u32,
    ) -> RuntimeResult<Vec<SearchHit>> {
        let candidates = limit.saturating_mul(CANDIDATE_MULTIPLIER).max(limit);

        let ns = token.namespace().as_str().to_owned();
        // sanitize_fts5_query strips known-unsafe metacharacters, but residual
        // punctuation can still trip the FTS5 parser at runtime; that error must
        // fail loud rather than silently degrade to vector-only fusion. Errors
        // from other legs (vector search) still propagate normally.
        let text_search_result = self
            .text(token)?
            .search(TextSearchRequest {
                query: query_text.to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(TextFilter {
                    namespaces: vec![ns.clone()],
                    ..TextFilter::default()
                }),
                top_k: candidates,
                snippet_chars: 200,
            })
            .await;
        let text_hits = crate::error::fts_text_leg_or_err(
            text_search_result.map_err(RuntimeError::from),
            "hybrid_search_with_strategy",
            query_text,
        )?;

        let vector_hits = if query_vector.is_some() || self.config().embedding_model.is_some() {
            self.vector_search(
                token,
                query_vector,
                Some(query_text),
                candidates,
                Some(SubstrateKind::Entity),
            )
            .await?
        } else {
            Vec::new()
        };

        // Each arm fetched `candidates` independently, so their union can contain
        // twice that many distinct IDs. Keep the complete fetched pool through
        // ranking and the alive check; truncating it first lets stale hits hide
        // live candidates from the other arm.
        let fusion_limit = text_hits.len().saturating_add(vector_hits.len());
        let fused = self
            .fuse_with_strategy(text_hits, vector_hits, &strategy, fusion_limit)
            .await?;
        self.retain_alive_search_hits(token, fused, limit as usize)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use khive_storage::types::{TextDocument, TextSearchHit, VectorSearchHit, VectorSearchRequest};
    use khive_storage::Entity;
    use lattice_embed::EmbeddingModel;
    use std::sync::Arc;

    use crate::RuntimeConfig;

    fn text_hit(id: Uuid, score: f64, title: &str) -> TextSearchHit {
        TextSearchHit {
            subject_id: id,
            score: DeterministicScore::from_f64(score),
            rank: 1,
            title: Some(title.to_string()),
            snippet: Some("...".to_string()),
        }
    }

    fn vector_hit(id: Uuid, score: f64) -> VectorSearchHit {
        VectorSearchHit {
            subject_id: id,
            score: DeterministicScore::from_f64(score),
            rank: 1,
        }
    }

    fn cosine_fixture_vector(dimensions: usize, x: f32, y: f32) -> Vec<f32> {
        let mut vector = vec![0.0; dimensions];
        vector[0] = x;
        vector[1] = y;
        vector
    }

    async fn stale_full_prefix_fixture() -> (
        KhiveRuntime,
        NamespaceToken,
        &'static str,
        Vec<f32>,
        Vec<TextSearchHit>,
        Vec<VectorSearchHit>,
        HashSet<Uuid>,
    ) {
        let model = EmbeddingModel::AllMiniLmL6V2;
        let dimensions = model.dimensions();
        let rt = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            embedding_model: Some(model),
            additional_embedding_models: vec![],
            ..RuntimeConfig::default()
        })
        .unwrap();
        let tok = NamespaceToken::local();
        let query_text = "fusionrefillterm";
        let query_vector = cosine_fixture_vector(dimensions, 1.0, 0.0);

        let common_stale_a = Uuid::from_u128(1);
        let common_stale_b = Uuid::from_u128(2);
        let text_only_stale = Uuid::from_u128(3);
        let vector_only_stale = Uuid::from_u128(4);

        let live_text = Entity::new("local", "concept", "live text candidate");
        let live_vector = Entity::new("local", "concept", "live vector candidate");
        rt.entities(&tok)
            .unwrap()
            .upsert_entities(vec![live_text.clone(), live_vector.clone()])
            .await
            .unwrap();

        let document = |subject_id, repetitions: usize| TextDocument {
            subject_id,
            kind: SubstrateKind::Entity,
            namespace: "local".to_string(),
            title: None,
            body: std::iter::repeat_n(query_text, repetitions)
                .collect::<Vec<_>>()
                .join(" "),
            tags: vec![],
            metadata: None,
            updated_at: Utc::now(),
        };
        rt.text(&tok)
            .unwrap()
            .upsert_documents(vec![
                document(common_stale_a, 12),
                document(common_stale_b, 8),
                document(text_only_stale, 4),
                document(live_text.id, 1),
            ])
            .await
            .unwrap();

        let vectors = rt.vectors(&tok).unwrap();
        for (id, vector) in [
            (common_stale_a, cosine_fixture_vector(dimensions, 1.0, 0.0)),
            (common_stale_b, cosine_fixture_vector(dimensions, 0.8, 0.6)),
            (
                vector_only_stale,
                cosine_fixture_vector(dimensions, 0.5, 0.866_025_4),
            ),
            (live_vector.id, cosine_fixture_vector(dimensions, -1.0, 0.0)),
        ] {
            vectors
                .insert(
                    id,
                    SubstrateKind::Entity,
                    "local",
                    "entity.body",
                    vec![vector],
                )
                .await
                .unwrap();
        }

        let text_hits = rt
            .text(&tok)
            .unwrap()
            .search(TextSearchRequest {
                query: query_text.to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(TextFilter {
                    namespaces: vec!["local".to_string()],
                    ..TextFilter::default()
                }),
                top_k: CANDIDATE_MULTIPLIER,
                snippet_chars: 0,
            })
            .await
            .unwrap();
        let vector_hits = vectors
            .search(VectorSearchRequest {
                query_vectors: vec![query_vector.clone()],
                top_k: CANDIDATE_MULTIPLIER,
                namespace: Some("local".to_string()),
                kind: Some(SubstrateKind::Entity),
                embedding_model: None,
                filter: None,
                backend_hints: None,
            })
            .await
            .unwrap();

        assert_eq!(text_hits.len(), CANDIDATE_MULTIPLIER as usize);
        assert_eq!(vector_hits.len(), CANDIDATE_MULTIPLIER as usize);
        let live = HashSet::from([live_text.id, live_vector.id]);
        (
            rt,
            tok,
            query_text,
            query_vector,
            text_hits,
            vector_hits,
            live,
        )
    }

    // 1. RRF with custom k produces different ordering than k=60
    #[tokio::test]
    async fn rrf_custom_k_differs_from_k60() {
        let rt = KhiveRuntime::memory().unwrap();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        // Single-source input makes a and b tie in relative order at both k values,
        // so assert on raw score magnitude (smaller k widens the rank-1-vs-rank-2 gap)
        // rather than ordering.
        let text = vec![text_hit(a, 0.9, "a"), text_hit(b, 0.1, "b")];
        let hits_k1 = rt
            .fuse_with_strategy(text.clone(), vec![], &FusionStrategy::Rrf { k: 1 }, 10)
            .await
            .unwrap();
        let hits_k60 = rt
            .fuse_with_strategy(text, vec![], &FusionStrategy::Rrf { k: 60 }, 10)
            .await
            .unwrap();
        // Both should have a first (rank 1 always wins in single-source)
        assert_eq!(hits_k1[0].entity_id, a);
        assert_eq!(hits_k60[0].entity_id, a);
        // k=1 produces higher raw score for rank 1 than k=60
        assert!(hits_k1[0].score > hits_k60[0].score);
    }

    // 2. Canonical [vector, keyword] weights change ordering as documented.
    #[tokio::test]
    async fn weighted_ordering_depends_on_weights() {
        let rt = KhiveRuntime::memory().unwrap();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        // a scores high in text, b scores high in vector
        let text = vec![text_hit(a, 0.9, "a"), text_hit(b, 0.1, "b")];
        let vec_hits = vec![vector_hit(b, 0.9), vector_hit(a, 0.1)];

        let heavy_vector = rt
            .fuse_with_strategy(
                text.clone(),
                vec_hits.clone(),
                &FusionStrategy::Weighted {
                    weights: vec![0.7, 0.3],
                },
                10,
            )
            .await
            .unwrap();
        let heavy_keyword = rt
            .fuse_with_strategy(
                text,
                vec_hits,
                &FusionStrategy::Weighted {
                    weights: vec![0.3, 0.7],
                },
                10,
            )
            .await
            .unwrap();

        assert_eq!(heavy_vector[0].entity_id, b);
        assert_eq!(heavy_keyword[0].entity_id, a);
    }

    // 3. Weighted [7.0, 3.0] = Weighted [0.7, 0.3] (normalization)
    #[tokio::test]
    async fn weighted_scale_invariant() {
        let rt = KhiveRuntime::memory().unwrap();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let text = vec![text_hit(a, 0.9, "a"), text_hit(b, 0.1, "b")];
        let vec_hits = vec![vector_hit(b, 0.9), vector_hit(a, 0.1)];

        let w1 = rt
            .fuse_with_strategy(
                text.clone(),
                vec_hits.clone(),
                &FusionStrategy::Weighted {
                    weights: vec![0.7, 0.3],
                },
                10,
            )
            .await
            .unwrap();
        let w2 = rt
            .fuse_with_strategy(
                text,
                vec_hits,
                &FusionStrategy::Weighted {
                    weights: vec![7.0, 3.0],
                },
                10,
            )
            .await
            .unwrap();

        assert_eq!(w1[0].entity_id, w2[0].entity_id);
        assert_eq!(w1[1].entity_id, w2[1].entity_id);
        let diff = (w1[0].score.to_f64() - w2[0].score.to_f64()).abs();
        assert!(diff < 1e-9, "scores differ by {diff}");
    }

    // 4. Weighted [0.0, 0.0] falls back to equal weights
    #[tokio::test]
    async fn weighted_zero_weights_equal_fallback() {
        let rt = KhiveRuntime::memory().unwrap();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        // Both sources agree: a > b
        let text = vec![text_hit(a, 0.9, "a"), text_hit(b, 0.1, "b")];
        let vec_hits = vec![vector_hit(a, 0.9), vector_hit(b, 0.1)];

        let hits = rt
            .fuse_with_strategy(
                text,
                vec_hits,
                &FusionStrategy::Weighted {
                    weights: vec![0.0, 0.0],
                },
                10,
            )
            .await
            .unwrap();
        assert_eq!(hits[0].entity_id, a);
    }

    // 5. Weighted with negative weight clamps to 0
    #[tokio::test]
    async fn weighted_negative_weight_clamped() {
        let rt = KhiveRuntime::memory().unwrap();
        let a = Uuid::new_v4();
        let text = vec![text_hit(a, 0.9, "a")];
        // Negative vector weight → only keyword/text contributes.
        let hits = rt
            .fuse_with_strategy(
                text,
                vec![],
                &FusionStrategy::Weighted {
                    weights: vec![-0.5, 1.0],
                },
                10,
            )
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entity_id, a);
    }

    #[tokio::test]
    async fn weighted_empty_arm_keeps_canonical_position() {
        let rt = KhiveRuntime::memory().unwrap();
        let text_only = Uuid::new_v4();
        let hits = rt
            .fuse_with_strategy(
                vec![text_hit(text_only, 0.9, "text")],
                vec![],
                &FusionStrategy::Weighted {
                    // Canonical [vector, keyword]: the only non-empty arm has zero weight.
                    weights: vec![1.0, 0.0],
                },
                10,
            )
            .await
            .unwrap();
        assert!(
            hits.is_empty(),
            "dropping the empty vector arm would incorrectly rebind text to its weight"
        );
    }

    // 6. Union returns max score per entity when same id appears in both lists
    #[tokio::test]
    async fn union_max_score_per_entity() {
        let rt = KhiveRuntime::memory().unwrap();
        let a = Uuid::new_v4();
        let text = vec![text_hit(a, 0.3, "a")];
        let vec_hits = vec![vector_hit(a, 0.9)];

        let hits = rt
            .fuse_with_strategy(text, vec_hits, &FusionStrategy::Union, 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert!((hits[0].score.to_f64() - 0.9).abs() < 1e-6);
        assert_eq!(hits[0].source, SearchSource::Both);
    }

    // 7. VectorOnly returns vector hits only (text hits dropped)
    #[tokio::test]
    async fn vector_only_drops_text() {
        let rt = KhiveRuntime::memory().unwrap();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let text = vec![text_hit(b, 0.9, "b")];
        let vec_hits = vec![vector_hit(a, 0.8)];

        let hits = rt
            .fuse_with_strategy(text, vec_hits, &FusionStrategy::VectorOnly, 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entity_id, a);
        assert_eq!(hits[0].source, SearchSource::Vector);
        assert!(hits[0].title.is_none());
    }

    #[tokio::test]
    async fn keyword_only_drops_vector() {
        let rt = KhiveRuntime::memory().unwrap();
        let text_id = Uuid::new_v4();
        let vector_id = Uuid::new_v4();
        let hits = rt
            .fuse_with_strategy(
                vec![text_hit(text_id, 0.8, "text")],
                vec![vector_hit(vector_id, 0.9)],
                &FusionStrategy::KeywordOnly,
                10,
            )
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entity_id, text_id);
        assert_eq!(hits[0].source, SearchSource::Text);
    }

    /// Test-only executor: flattens all streams and reverses their order,
    /// keeping each candidate's original score.
    struct ReverseOrderExecutor;

    #[async_trait::async_trait]
    impl FusionExecutor for ReverseOrderExecutor {
        async fn fuse(
            &self,
            streams: Vec<CandidateStream>,
            _params: &serde_json::Value,
            _limit: usize,
        ) -> RuntimeResult<Vec<RankedHit>> {
            let mut flat: Vec<_> = streams.into_iter().flatten().collect();
            flat.reverse();
            Ok(flat)
        }
    }

    /// Test-only executor: inverts each candidate's score (`1.0 - score`) so
    /// the fused ranking is the reverse of what score-descending built-ins
    /// (RRF, Union, Weighted) would produce on the same fixture -- unlike a
    /// mere insertion-order reversal, this survives the dispatch boundary's
    /// canonical re-sort, since the *scores* (not just the order) differ.
    struct InvertScoreExecutor;

    #[async_trait::async_trait]
    impl FusionExecutor for InvertScoreExecutor {
        async fn fuse(
            &self,
            streams: Vec<CandidateStream>,
            _params: &serde_json::Value,
            _limit: usize,
        ) -> RuntimeResult<Vec<RankedHit>> {
            Ok(streams
                .into_iter()
                .flatten()
                .map(|(id, score)| (id, DeterministicScore::from_f64(1.0 - score.to_f64())))
                .collect())
        }
    }

    /// Test-only executor: returns every candidate at an identical score, in
    /// the arbitrary order the input streams happened to flatten to -- used
    /// to prove the dispatch boundary re-sorts by the canonical comparator
    /// rather than trusting executor output order.
    struct EqualScoreExecutor;

    #[async_trait::async_trait]
    impl FusionExecutor for EqualScoreExecutor {
        async fn fuse(
            &self,
            streams: Vec<CandidateStream>,
            _params: &serde_json::Value,
            _limit: usize,
        ) -> RuntimeResult<Vec<RankedHit>> {
            Ok(streams
                .into_iter()
                .flatten()
                .map(|(id, _)| (id, DeterministicScore::from_f64(1.0)))
                .collect())
        }
    }

    // 7b. A registered custom executor dispatches and yields a different
    // ranking than RRF on the same fixture.
    #[tokio::test]
    async fn custom_strategy_dispatches_through_executor_and_differs_from_rrf() {
        let rt = KhiveRuntime::memory().unwrap();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let text = vec![text_hit(a, 0.9, "a"), text_hit(b, 0.5, "b")];

        rt.register_fusion_strategy("invert", Arc::new(InvertScoreExecutor));
        let strategy =
            FusionStrategy::try_custom("invert".to_string(), serde_json::Value::Null).unwrap();

        let custom = rt
            .fuse_with_strategy(text.clone(), vec![], &strategy, 10)
            .await
            .unwrap();
        let rrf = rt
            .fuse_with_strategy(text, vec![], &FusionStrategy::rrf(), 10)
            .await
            .unwrap();

        let custom_ids: Vec<_> = custom.iter().map(|h| h.entity_id).collect();
        let rrf_ids: Vec<_> = rrf.iter().map(|h| h.entity_id).collect();
        assert_ne!(
            custom_ids, rrf_ids,
            "custom and RRF must yield different orderings on this fixture"
        );
    }

    // 7c. An unregistered Custom name fails closed rather than falling back to RRF.
    #[tokio::test]
    async fn custom_strategy_unknown_name_fails_closed() {
        let rt = KhiveRuntime::memory().unwrap();
        let a = Uuid::new_v4();
        let text = vec![text_hit(a, 0.9, "a")];
        let strategy =
            FusionStrategy::try_custom("nonexistent".to_string(), serde_json::Value::Null).unwrap();

        let result = rt.fuse_with_strategy(text, vec![], &strategy, 10).await;
        assert!(matches!(
            result,
            Err(RuntimeError::UnknownFusionStrategy(name)) if name == "nonexistent"
        ));
    }

    // 7d. Unknown name errors even with empty sources -- it must not be
    // indistinguishable from a valid empty result.
    #[tokio::test]
    async fn custom_strategy_unknown_name_fails_closed_even_on_empty_input() {
        let rt = KhiveRuntime::memory().unwrap();
        let strategy =
            FusionStrategy::try_custom("nonexistent".to_string(), serde_json::Value::Null).unwrap();

        let result = rt.fuse_with_strategy(vec![], vec![], &strategy, 10).await;
        assert!(matches!(
            result,
            Err(RuntimeError::UnknownFusionStrategy(name)) if name == "nonexistent"
        ));
    }

    // 7e. Empty sources with a *registered* name is a valid empty result, not
    // an error -- distinguishing "misconfigured" from "genuinely nothing".
    #[tokio::test]
    async fn custom_strategy_registered_name_empty_input_returns_ok_empty() {
        let rt = KhiveRuntime::memory().unwrap();
        rt.register_fusion_strategy("reverse", Arc::new(ReverseOrderExecutor));
        let strategy =
            FusionStrategy::try_custom("reverse".to_string(), serde_json::Value::Null).unwrap();

        let result = rt
            .fuse_with_strategy(vec![], vec![], &strategy, 10)
            .await
            .unwrap();
        assert!(result.is_empty());
    }

    // 7f. Registering a custom strategy never perturbs the default (non-Custom) path.
    #[tokio::test]
    async fn registered_custom_strategy_leaves_default_path_unaffected() {
        let rt = KhiveRuntime::memory().unwrap();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let text = vec![text_hit(a, 0.9, "a"), text_hit(b, 0.5, "b")];

        rt.register_fusion_strategy("reverse", Arc::new(ReverseOrderExecutor));

        let via_rt_with_registration = rt
            .fuse_with_strategy(text.clone(), vec![], &FusionStrategy::rrf(), 10)
            .await
            .unwrap();
        let rt2 = KhiveRuntime::memory().unwrap();
        let via_rt_without_registration = rt2
            .fuse_with_strategy(text, vec![], &FusionStrategy::rrf(), 10)
            .await
            .unwrap();

        let ids_with: Vec<_> = via_rt_with_registration
            .iter()
            .map(|h| h.entity_id)
            .collect();
        let ids_without: Vec<_> = via_rt_without_registration
            .iter()
            .map(|h| h.entity_id)
            .collect();
        assert_eq!(ids_with, ids_without);
    }

    // 7g. A custom executor returning equal-score IDs in arbitrary/reversed
    // order still yields the crate's canonical score-desc/id-asc order --
    // the dispatch boundary re-sorts rather than trusting executor output.
    #[tokio::test]
    async fn custom_executor_output_is_sorted_by_canonical_comparator() {
        let rt = KhiveRuntime::memory().unwrap();
        // Deliberately not in ID order, so a passthrough bug would be visible.
        let ids: Vec<Uuid> = vec![Uuid::from_u128(3), Uuid::from_u128(1), Uuid::from_u128(2)];
        let text: Vec<TextSearchHit> = ids.iter().map(|&id| text_hit(id, 0.5, "tied")).collect();

        rt.register_fusion_strategy("equal_score", Arc::new(EqualScoreExecutor));
        let strategy =
            FusionStrategy::try_custom("equal_score".to_string(), serde_json::Value::Null).unwrap();

        let hits = rt
            .fuse_with_strategy(text, vec![], &strategy, 10)
            .await
            .unwrap();

        let mut expected = ids.clone();
        expected.sort();
        let actual: Vec<_> = hits.iter().map(|h| h.entity_id).collect();
        assert_eq!(
            actual, expected,
            "equal-score executor output must be tie-broken by ascending ID"
        );
    }

    // 8. Default strategy is Rrf{k:60}
    #[test]
    fn default_strategy_is_rrf_k60() {
        assert_eq!(FusionStrategy::default(), FusionStrategy::Rrf { k: 60 });
    }

    #[tokio::test]
    async fn hybrid_union_alive_filter_refills_below_complete_four_x_prefix() {
        let (rt, tok, query_text, query_vector, text_hits, vector_hits, live) =
            stale_full_prefix_fixture().await;
        let truncated = rt
            .fuse_with_strategy(
                text_hits,
                vector_hits,
                &FusionStrategy::Union,
                CANDIDATE_MULTIPLIER as usize,
            )
            .await
            .unwrap();
        assert!(truncated.iter().all(|hit| !live.contains(&hit.entity_id)));

        let hits = rt
            .hybrid_search_with_strategy(
                &tok,
                query_text,
                Some(query_vector),
                FusionStrategy::Union,
                1,
            )
            .await
            .unwrap();

        assert_eq!(hits.len(), 1);
        assert!(live.contains(&hits[0].entity_id));
    }

    #[tokio::test]
    async fn hybrid_rrf_alive_filter_refills_below_complete_four_x_prefix() {
        let (rt, tok, query_text, query_vector, text_hits, vector_hits, live) =
            stale_full_prefix_fixture().await;
        let strategy = FusionStrategy::Rrf { k: 60 };
        let truncated = rt
            .fuse_with_strategy(
                text_hits,
                vector_hits,
                &strategy,
                CANDIDATE_MULTIPLIER as usize,
            )
            .await
            .unwrap();
        assert!(truncated.iter().all(|hit| !live.contains(&hit.entity_id)));

        let hits = rt
            .hybrid_search_with_strategy(&tok, query_text, Some(query_vector), strategy, 1)
            .await
            .unwrap();

        assert_eq!(hits.len(), 1);
        assert!(live.contains(&hits[0].entity_id));
    }

    #[tokio::test]
    async fn hybrid_default_rrf_alive_filter_refills_below_complete_four_x_prefix() {
        let (rt, tok, query_text, query_vector, _text_hits, _vector_hits, live) =
            stale_full_prefix_fixture().await;

        let hits = rt
            .hybrid_search(
                &tok,
                query_text,
                Some(query_vector),
                1,
                None,
                None,
                &[],
                None,
            )
            .await
            .unwrap();

        assert_eq!(hits.len(), 1);
        assert!(live.contains(&hits[0].entity_id));
    }

    // 9. Roundtrip serde preserves variant
    #[test]
    fn serde_roundtrip() {
        let cases = vec![
            FusionStrategy::Rrf { k: 60 },
            FusionStrategy::Rrf { k: 20 },
            FusionStrategy::Weighted {
                weights: vec![0.7, 0.3],
            },
            FusionStrategy::Union,
            FusionStrategy::VectorOnly,
            FusionStrategy::KeywordOnly,
        ];
        for strategy in cases {
            let json = serde_json::to_string(&strategy).expect("serialize");
            let back: FusionStrategy = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(strategy, back, "roundtrip failed for {json}");
        }
    }

    // 10. hybrid_search_with_strategy must not hard-fail on a query containing FTS5
    // metacharacters like `$`, since sanitize_fts5_query strips them before the query
    // reaches SQLite. This covers the sanitizer path; test 11 covers the fail-loud
    // path for characters the sanitizer does not strip.
    #[tokio::test]
    async fn hybrid_search_with_strategy_dollar_sign_query_does_not_error() {
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
            .hybrid_search_with_strategy(&tok, "$prev.id", None, FusionStrategy::default(), 10)
            .await;

        assert!(
            result.is_ok(),
            "#388 hybrid_search_with_strategy must not hard-fail on a '$'-bearing query, got: {:?}",
            result.err()
        );
    }

    // 11. #916: `@` used to reach SQLite FTS5's bareword parser raw and error,
    // surfacing as RuntimeError::InvalidInput per #569's fail-loud policy.
    // sanitize_fts5_token_group's bareword-safety gate now routes it through the
    // quoted-phrase alternative instead, so the query succeeds and the fail-loud
    // arm is no longer reached for ordinary punctuation.
    #[tokio::test]
    async fn hybrid_search_with_strategy_residual_fts5_char_now_sanitized() {
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
            .hybrid_search_with_strategy(&tok, "foo@bar", None, FusionStrategy::default(), 10)
            .await;

        let hits = result.unwrap_or_else(|e| {
            panic!(
                "#916 hybrid_search_with_strategy must not fail on an '@'-bearing query, got: {e:?}"
            )
        });
        assert!(
            !hits.is_empty(),
            "#916 '@'-bearing query must still find the seeded 'foo@bar' content via the \
             quoted-phrase alternative"
        );
    }
}
