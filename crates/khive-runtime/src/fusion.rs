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

const CANDIDATE_MULTIPLIER: u32 = 4;

/// Fuse text and vector hits using the given strategy, returning at most `limit` results.
pub fn fuse_with_strategy(
    text_hits: Vec<TextSearchHit>,
    vector_hits: Vec<VectorSearchHit>,
    strategy: &FusionStrategy,
    limit: usize,
) -> RuntimeResult<Vec<SearchHit>> {
    match strategy {
        FusionStrategy::VectorOnly => fuse_sources(Vec::new(), vector_hits, strategy, limit),
        FusionStrategy::KeywordOnly => fuse_sources(text_hits, Vec::new(), strategy, limit),
        FusionStrategy::Rrf { .. } | FusionStrategy::Weighted { .. } | FusionStrategy::Union => {
            fuse_sources(text_hits, vector_hits, strategy, limit)
        }
        FusionStrategy::Custom { ref name, .. } => {
            Err(khive_fusion::FuseError::CustomRequiresRuntime(name.clone()).into())
        }
    }
}

/// RRF convenience wrapper used by operations.rs (k=60 note search path).
pub(crate) fn rrf_fuse_k(
    text_hits: Vec<TextSearchHit>,
    vector_hits: Vec<VectorSearchHit>,
    k: usize,
    limit: usize,
) -> RuntimeResult<Vec<SearchHit>> {
    fuse_with_strategy(text_hits, vector_hits, &FusionStrategy::Rrf { k }, limit)
}

fn fuse_sources(
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

    let sources: Vec<Vec<(Uuid, DeterministicScore)>> = vec![text_source, vector_source]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();

    Ok(khive_fusion::fuse(sources, strategy, limit)?
        .into_iter()
        .filter_map(|(id, score)| {
            let mut hit = metadata.remove(&id)?;
            hit.score = score;
            Some(hit)
        })
        .collect())
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
    /// Hybrid search with a caller-supplied fusion strategy.
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

        let mut fused = fuse_with_strategy(text_hits, vector_hits, &strategy, limit as usize)?;

        // Filter out soft-deleted entities. A single query fetches all alive IDs from the
        // fused set; any ID absent from the result has been soft-deleted (deleted_at IS NOT NULL).
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
                        limit: fused.len() as u32,
                    },
                )
                .await?;
            let alive: HashSet<Uuid> = alive_page.items.into_iter().map(|e| e.id).collect();
            fused.retain(|h| alive.contains(&h.entity_id));
        }

        Ok(fused)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use khive_storage::types::{TextSearchHit, VectorSearchHit};

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

    // 1. RRF with custom k produces different ordering than k=60
    #[test]
    fn rrf_custom_k_differs_from_k60() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        // Single-source input makes a and b tie in relative order at both k values,
        // so assert on raw score magnitude (smaller k widens the rank-1-vs-rank-2 gap)
        // rather than ordering.
        let text = vec![text_hit(a, 0.9, "a"), text_hit(b, 0.1, "b")];
        let hits_k1 =
            fuse_with_strategy(text.clone(), vec![], &FusionStrategy::Rrf { k: 1 }, 10).unwrap();
        let hits_k60 =
            fuse_with_strategy(text, vec![], &FusionStrategy::Rrf { k: 60 }, 10).unwrap();
        // Both should have a first (rank 1 always wins in single-source)
        assert_eq!(hits_k1[0].entity_id, a);
        assert_eq!(hits_k60[0].entity_id, a);
        // k=1 produces higher raw score for rank 1 than k=60
        assert!(hits_k1[0].score > hits_k60[0].score);
    }

    // 2. Weighted [0.7, 0.3] gives different ordering than [0.3, 0.7]
    #[test]
    fn weighted_ordering_depends_on_weights() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        // a scores high in text, b scores high in vector
        let text = vec![text_hit(a, 0.9, "a"), text_hit(b, 0.1, "b")];
        let vec_hits = vec![vector_hit(b, 0.9), vector_hit(a, 0.1)];

        let heavy_text = fuse_with_strategy(
            text.clone(),
            vec_hits.clone(),
            &FusionStrategy::Weighted {
                weights: vec![0.7, 0.3],
            },
            10,
        )
        .unwrap();
        let heavy_vec = fuse_with_strategy(
            text,
            vec_hits,
            &FusionStrategy::Weighted {
                weights: vec![0.3, 0.7],
            },
            10,
        )
        .unwrap();

        assert_eq!(heavy_text[0].entity_id, a);
        assert_eq!(heavy_vec[0].entity_id, b);
    }

    // 3. Weighted [7.0, 3.0] = Weighted [0.7, 0.3] (normalization)
    #[test]
    fn weighted_scale_invariant() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let text = vec![text_hit(a, 0.9, "a"), text_hit(b, 0.1, "b")];
        let vec_hits = vec![vector_hit(b, 0.9), vector_hit(a, 0.1)];

        let w1 = fuse_with_strategy(
            text.clone(),
            vec_hits.clone(),
            &FusionStrategy::Weighted {
                weights: vec![0.7, 0.3],
            },
            10,
        )
        .unwrap();
        let w2 = fuse_with_strategy(
            text,
            vec_hits,
            &FusionStrategy::Weighted {
                weights: vec![7.0, 3.0],
            },
            10,
        )
        .unwrap();

        assert_eq!(w1[0].entity_id, w2[0].entity_id);
        assert_eq!(w1[1].entity_id, w2[1].entity_id);
        let diff = (w1[0].score.to_f64() - w2[0].score.to_f64()).abs();
        assert!(diff < 1e-9, "scores differ by {diff}");
    }

    // 4. Weighted [0.0, 0.0] falls back to equal weights
    #[test]
    fn weighted_zero_weights_equal_fallback() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        // Both sources agree: a > b
        let text = vec![text_hit(a, 0.9, "a"), text_hit(b, 0.1, "b")];
        let vec_hits = vec![vector_hit(a, 0.9), vector_hit(b, 0.1)];

        let hits = fuse_with_strategy(
            text,
            vec_hits,
            &FusionStrategy::Weighted {
                weights: vec![0.0, 0.0],
            },
            10,
        )
        .unwrap();
        assert_eq!(hits[0].entity_id, a);
    }

    // 5. Weighted with negative weight clamps to 0
    #[test]
    fn weighted_negative_weight_clamped() {
        let a = Uuid::new_v4();
        let text = vec![text_hit(a, 0.9, "a")];
        // Negative vector weight → only text contributes
        let hits = fuse_with_strategy(
            text,
            vec![],
            &FusionStrategy::Weighted {
                weights: vec![1.0, -0.5],
            },
            10,
        )
        .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entity_id, a);
    }

    // 6. Union returns max score per entity when same id appears in both lists
    #[test]
    fn union_max_score_per_entity() {
        let a = Uuid::new_v4();
        let text = vec![text_hit(a, 0.3, "a")];
        let vec_hits = vec![vector_hit(a, 0.9)];

        let hits = fuse_with_strategy(text, vec_hits, &FusionStrategy::Union, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!((hits[0].score.to_f64() - 0.9).abs() < 1e-6);
        assert_eq!(hits[0].source, SearchSource::Both);
    }

    // 7. VectorOnly returns vector hits only (text hits dropped)
    #[test]
    fn vector_only_drops_text() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let text = vec![text_hit(b, 0.9, "b")];
        let vec_hits = vec![vector_hit(a, 0.8)];

        let hits = fuse_with_strategy(text, vec_hits, &FusionStrategy::VectorOnly, 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entity_id, a);
        assert_eq!(hits[0].source, SearchSource::Vector);
        assert!(hits[0].title.is_none());
    }

    // 8. Default strategy is Rrf{k:60}
    #[test]
    fn default_strategy_is_rrf_k60() {
        assert_eq!(FusionStrategy::default(), FusionStrategy::Rrf { k: 60 });
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

    // 11. Unlike `$`, `@` is not stripped by sanitize_fts5_query (kept minimal by
    // design), and SQLite FTS5's bareword parser rejects it unconditionally. That
    // parser error must surface as RuntimeError::InvalidInput rather than silently
    // degrading to vector-only fusion.
    #[tokio::test]
    async fn hybrid_search_with_strategy_residual_fts5_char_fails_loud() {
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

        assert!(
            result.is_err(),
            "#569 hybrid_search_with_strategy must fail loud when the FTS leg errors \
             on a residual FTS5 char ('@'), not silently degrade to vector-only fusion, \
             got: {:?}",
            result.ok()
        );
        assert!(
            matches!(result.unwrap_err(), RuntimeError::InvalidInput(_)),
            "residual FTS5 parser failure must surface as RuntimeError::InvalidInput"
        );
    }
}
