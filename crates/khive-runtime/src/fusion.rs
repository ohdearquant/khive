//! Fusion strategies for combining ranked result lists.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use khive_score::{rrf_score, DeterministicScore};
use khive_storage::types::{
    PageRequest, TextFilter, TextQueryMode, TextSearchHit, TextSearchRequest, VectorSearchHit,
    VectorSearchRequest,
};
use khive_storage::EntityFilter;
use khive_types::SubstrateKind;

use crate::error::RuntimeResult;
use crate::retrieval::{SearchHit, SearchSource};
use crate::runtime::KhiveRuntime;

const CANDIDATE_MULTIPLIER: u32 = 4;

/// Strategy for fusing ranked result lists from multiple retrieval sources.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FusionStrategy {
    /// Reciprocal Rank Fusion. Uses only ranks; robust to different score scales.
    Rrf { k: usize },
    /// Weighted linear combination. Min-max normalizes each source to [0,1] first.
    /// Weights are normalized to sum to 1.0; negatives clamped to 0; all-zero falls back to equal.
    Weighted { weights: Vec<f64> },
    /// Take all hits; keep the max score per entity_id.
    Union,
    /// Drop text hits; return vector hits only.
    VectorOnly,
}

impl Default for FusionStrategy {
    fn default() -> Self {
        Self::Rrf { k: 60 }
    }
}

/// Fuse text and vector hits using the given strategy, returning at most `limit` results.
pub fn fuse_with_strategy(
    text_hits: Vec<TextSearchHit>,
    vector_hits: Vec<VectorSearchHit>,
    strategy: &FusionStrategy,
    limit: usize,
) -> Vec<SearchHit> {
    match strategy {
        FusionStrategy::Rrf { k } => rrf_fuse_k(text_hits, vector_hits, *k, limit),
        FusionStrategy::Weighted { weights } => {
            weighted_fuse(text_hits, vector_hits, weights, limit)
        }
        FusionStrategy::Union => union_fuse(text_hits, vector_hits, limit),
        FusionStrategy::VectorOnly => vector_only(vector_hits, limit),
    }
}

impl KhiveRuntime {
    /// Hybrid search with a caller-supplied fusion strategy.
    pub async fn hybrid_search_with_strategy(
        &self,
        namespace: Option<&str>,
        query_text: &str,
        query_vector: Option<Vec<f32>>,
        strategy: FusionStrategy,
        limit: u32,
    ) -> RuntimeResult<Vec<SearchHit>> {
        let candidates = limit.saturating_mul(CANDIDATE_MULTIPLIER).max(limit);

        let ns = self.ns(namespace).to_string();
        let text_hits = self
            .text(namespace)?
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
            .await?;

        let vector_hits = if let Some(vec) = query_vector {
            self.vectors(namespace)?
                .search(VectorSearchRequest {
                    query_embedding: vec,
                    top_k: candidates,
                    namespace: Some(ns.clone()),
                    kind: Some(SubstrateKind::Entity),
                })
                .await?
        } else {
            Vec::new()
        };

        let mut fused = fuse_with_strategy(text_hits, vector_hits, &strategy, limit as usize);

        // Filter out soft-deleted entities. A single query fetches all alive IDs from the
        // fused set; any ID absent from the result has been soft-deleted (deleted_at IS NOT NULL).
        if !fused.is_empty() {
            let candidate_ids: Vec<Uuid> = fused.iter().map(|h| h.entity_id).collect();
            let alive_page = self
                .entities(namespace)?
                .query_entities(
                    self.ns(namespace),
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

fn rrf_fuse_k(
    text_hits: Vec<TextSearchHit>,
    vector_hits: Vec<VectorSearchHit>,
    k: usize,
    limit: usize,
) -> Vec<SearchHit> {
    #[derive(Default)]
    struct Bucket {
        score: DeterministicScore,
        source: Option<SearchSource>,
        title: Option<String>,
        snippet: Option<String>,
    }

    let mut buckets: HashMap<Uuid, Bucket> = HashMap::new();

    for (i, hit) in text_hits.into_iter().enumerate() {
        let entry = buckets.entry(hit.subject_id).or_default();
        entry.score = entry.score + rrf_score(i + 1, k);
        entry.source = Some(match entry.source {
            Some(SearchSource::Vector) => SearchSource::Both,
            _ => SearchSource::Text,
        });
        if entry.title.is_none() {
            entry.title = hit.title;
        }
        if entry.snippet.is_none() {
            entry.snippet = hit.snippet;
        }
    }

    for (i, hit) in vector_hits.into_iter().enumerate() {
        let entry = buckets.entry(hit.subject_id).or_default();
        entry.score = entry.score + rrf_score(i + 1, k);
        entry.source = Some(match entry.source {
            Some(SearchSource::Text) => SearchSource::Both,
            _ => SearchSource::Vector,
        });
    }

    let mut hits: Vec<SearchHit> = buckets
        .into_iter()
        .map(|(id, b)| SearchHit {
            entity_id: id,
            score: b.score,
            source: b.source.expect("each bucket gets a source"),
            title: b.title,
            snippet: b.snippet,
        })
        .collect();

    hits.sort_by(|a, b| b.score.cmp(&a.score).then(a.entity_id.cmp(&b.entity_id)));
    hits.truncate(limit);
    hits
}

fn weighted_fuse(
    text_hits: Vec<TextSearchHit>,
    vector_hits: Vec<VectorSearchHit>,
    weights: &[f64],
    limit: usize,
) -> Vec<SearchHit> {
    // Normalize: clamp negatives to 0, fall back to equal if all zero.
    let w0 = weights.first().copied().unwrap_or(0.0).max(0.0);
    let w1 = weights.get(1).copied().unwrap_or(0.0).max(0.0);
    let total = w0 + w1;
    let (nw0, nw1) = if total <= 0.0 {
        (0.5, 0.5)
    } else {
        (w0 / total, w1 / total)
    };

    // Collect metadata from text hits before consuming them for scores.
    let mut meta: HashMap<Uuid, (Option<String>, Option<String>)> = HashMap::new();
    let text_scores: Vec<(Uuid, f64)> = text_hits
        .into_iter()
        .map(|h| {
            meta.entry(h.subject_id)
                .or_insert_with(|| (h.title, h.snippet));
            (h.subject_id, h.score.to_f64())
        })
        .collect();

    let vector_scores: Vec<(Uuid, f64)> = vector_hits
        .into_iter()
        .map(|h| (h.subject_id, h.score.to_f64()))
        .collect();

    // Per-source min-max normalize to [0, 1].
    let text_norm = min_max_normalize(&text_scores);
    let vector_norm = min_max_normalize(&vector_scores);

    let mut combined: HashMap<Uuid, f64> = HashMap::new();
    for (id, s) in &text_norm {
        *combined.entry(*id).or_insert(0.0) += s * nw0;
    }
    for (id, s) in &vector_norm {
        *combined.entry(*id).or_insert(0.0) += s * nw1;
    }

    let mut hits: Vec<SearchHit> = combined
        .into_iter()
        .map(|(id, score)| {
            let (title, snippet) = meta.get(&id).cloned().unwrap_or_default();
            let source = match (
                text_norm.iter().any(|(i, _)| *i == id),
                vector_norm.iter().any(|(i, _)| *i == id),
            ) {
                (true, true) => SearchSource::Both,
                (true, false) => SearchSource::Text,
                _ => SearchSource::Vector,
            };
            SearchHit {
                entity_id: id,
                score: DeterministicScore::from_f64(score),
                source,
                title,
                snippet,
            }
        })
        .collect();

    hits.sort_by(|a, b| b.score.cmp(&a.score).then(a.entity_id.cmp(&b.entity_id)));
    hits.truncate(limit);
    hits
}

fn min_max_normalize(scores: &[(Uuid, f64)]) -> Vec<(Uuid, f64)> {
    if scores.is_empty() {
        return Vec::new();
    }
    let min = scores.iter().map(|(_, s)| *s).fold(f64::INFINITY, f64::min);
    let max = scores
        .iter()
        .map(|(_, s)| *s)
        .fold(f64::NEG_INFINITY, f64::max);
    let span = max - min;
    if span <= f64::EPSILON {
        return scores.iter().map(|(id, _)| (*id, 1.0)).collect();
    }
    scores
        .iter()
        .map(|(id, s)| (*id, (s - min) / span))
        .collect()
}

fn union_fuse(
    text_hits: Vec<TextSearchHit>,
    vector_hits: Vec<VectorSearchHit>,
    limit: usize,
) -> Vec<SearchHit> {
    struct Bucket {
        score: DeterministicScore,
        source: SearchSource,
        title: Option<String>,
        snippet: Option<String>,
    }

    let mut buckets: HashMap<Uuid, Bucket> = HashMap::new();

    for hit in text_hits {
        let entry = buckets.entry(hit.subject_id).or_insert_with(|| Bucket {
            score: DeterministicScore::ZERO,
            source: SearchSource::Text,
            title: None,
            snippet: None,
        });
        if hit.score > entry.score {
            entry.score = hit.score;
        }
        if entry.title.is_none() {
            entry.title = hit.title;
        }
        if entry.snippet.is_none() {
            entry.snippet = hit.snippet;
        }
        if entry.source == SearchSource::Vector {
            entry.source = SearchSource::Both;
        }
    }

    for hit in vector_hits {
        let entry = buckets.entry(hit.subject_id).or_insert_with(|| Bucket {
            score: DeterministicScore::ZERO,
            source: SearchSource::Vector,
            title: None,
            snippet: None,
        });
        if hit.score > entry.score {
            entry.score = hit.score;
        }
        if entry.source == SearchSource::Text {
            entry.source = SearchSource::Both;
        }
    }

    let mut hits: Vec<SearchHit> = buckets
        .into_iter()
        .map(|(id, b)| SearchHit {
            entity_id: id,
            score: b.score,
            source: b.source,
            title: b.title,
            snippet: b.snippet,
        })
        .collect();

    hits.sort_by(|a, b| b.score.cmp(&a.score).then(a.entity_id.cmp(&b.entity_id)));
    hits.truncate(limit);
    hits
}

fn vector_only(vector_hits: Vec<VectorSearchHit>, limit: usize) -> Vec<SearchHit> {
    let mut hits: Vec<SearchHit> = vector_hits
        .into_iter()
        .map(|h| SearchHit {
            entity_id: h.subject_id,
            score: h.score,
            source: SearchSource::Vector,
            title: None,
            snippet: None,
        })
        .collect();
    hits.sort_by(|a, b| b.score.cmp(&a.score).then(a.entity_id.cmp(&b.entity_id)));
    hits.truncate(limit);
    hits
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
        // With k=1, top rank contributes 1/(1+1)=0.5 vs rank-2 1/(1+2)=0.333 — bigger gap
        // With k=60, top rank contributes 1/61 vs 1/62 — much smaller gap
        // Use a case where combining one source forces a=rank1, b=rank2 in text, reversed in vector
        // k=1: a from text rank1 + vector rank2 = 1/2 + 1/3 = 5/6
        //       b from text rank2 + vector rank1 = 1/3 + 1/2 = 5/6 (tie, broken by UUID)
        // k=60: same math, but: 1/61 + 1/62 ≈ 0.0326 each — same tie
        // Instead verify k=1 produces larger absolute score differences for rank differences
        let text = vec![text_hit(a, 0.9, "a"), text_hit(b, 0.1, "b")];
        let hits_k1 = fuse_with_strategy(text.clone(), vec![], &FusionStrategy::Rrf { k: 1 }, 10);
        let hits_k60 = fuse_with_strategy(text, vec![], &FusionStrategy::Rrf { k: 60 }, 10);
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
        );
        let heavy_vec = fuse_with_strategy(
            text,
            vec_hits,
            &FusionStrategy::Weighted {
                weights: vec![0.3, 0.7],
            },
            10,
        );

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
        );
        let w2 = fuse_with_strategy(
            text,
            vec_hits,
            &FusionStrategy::Weighted {
                weights: vec![7.0, 3.0],
            },
            10,
        );

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
        );
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
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entity_id, a);
    }

    // 6. Union returns max score per entity when same id appears in both lists
    #[test]
    fn union_max_score_per_entity() {
        let a = Uuid::new_v4();
        let text = vec![text_hit(a, 0.3, "a")];
        let vec_hits = vec![vector_hit(a, 0.9)];

        let hits = fuse_with_strategy(text, vec_hits, &FusionStrategy::Union, 10);
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

        let hits = fuse_with_strategy(text, vec_hits, &FusionStrategy::VectorOnly, 10);
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
}
