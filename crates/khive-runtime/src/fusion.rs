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
/// Positional weighted strategies use `[vector, keyword]` order.
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

    // Canonical positional order is [vector, keyword]. Empty arms remain in
    // place: removing one would shift the surviving arm onto the wrong weight.
    let sources: Vec<Vec<(Uuid, DeterministicScore)>> = vec![vector_source, text_source];

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
        let fused = fuse_with_strategy(text_hits, vector_hits, &strategy, fusion_limit)?;
        self.retain_alive_search_hits(token, fused, limit as usize)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use khive_storage::types::{TextDocument, TextSearchHit, VectorSearchHit, VectorSearchRequest};
    use khive_types::Entity;
    use lattice_embed::EmbeddingModel;

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

    // 2. Canonical [vector, keyword] weights change ordering as documented.
    #[test]
    fn weighted_ordering_depends_on_weights() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        // a scores high in text, b scores high in vector
        let text = vec![text_hit(a, 0.9, "a"), text_hit(b, 0.1, "b")];
        let vec_hits = vec![vector_hit(b, 0.9), vector_hit(a, 0.1)];

        let heavy_vector = fuse_with_strategy(
            text.clone(),
            vec_hits.clone(),
            &FusionStrategy::Weighted {
                weights: vec![0.7, 0.3],
            },
            10,
        )
        .unwrap();
        let heavy_keyword = fuse_with_strategy(
            text,
            vec_hits,
            &FusionStrategy::Weighted {
                weights: vec![0.3, 0.7],
            },
            10,
        )
        .unwrap();

        assert_eq!(heavy_vector[0].entity_id, b);
        assert_eq!(heavy_keyword[0].entity_id, a);
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
        // Negative vector weight → only keyword/text contributes.
        let hits = fuse_with_strategy(
            text,
            vec![],
            &FusionStrategy::Weighted {
                weights: vec![-0.5, 1.0],
            },
            10,
        )
        .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entity_id, a);
    }

    #[test]
    fn weighted_empty_arm_keeps_canonical_position() {
        let text_only = Uuid::new_v4();
        let hits = fuse_with_strategy(
            vec![text_hit(text_only, 0.9, "text")],
            vec![],
            &FusionStrategy::Weighted {
                // Canonical [vector, keyword]: the only non-empty arm has zero weight.
                weights: vec![1.0, 0.0],
            },
            10,
        )
        .unwrap();
        assert!(
            hits.is_empty(),
            "dropping the empty vector arm would incorrectly rebind text to its weight"
        );
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

    #[test]
    fn keyword_only_drops_vector() {
        let text_id = Uuid::new_v4();
        let vector_id = Uuid::new_v4();
        let hits = fuse_with_strategy(
            vec![text_hit(text_id, 0.8, "text")],
            vec![vector_hit(vector_id, 0.9)],
            &FusionStrategy::KeywordOnly,
            10,
        )
        .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entity_id, text_id);
        assert_eq!(hits[0].source, SearchSource::Text);
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
        let truncated = fuse_with_strategy(
            text_hits,
            vector_hits,
            &FusionStrategy::Union,
            CANDIDATE_MULTIPLIER as usize,
        )
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
        let truncated = fuse_with_strategy(
            text_hits,
            vector_hits,
            &strategy,
            CANDIDATE_MULTIPLIER as usize,
        )
        .unwrap();
        assert!(truncated.iter().all(|hit| !live.contains(&hit.entity_id)));

        let hits = rt
            .hybrid_search_with_strategy(&tok, query_text, Some(query_vector), strategy, 1)
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
