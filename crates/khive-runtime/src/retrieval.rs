//! Retrieval operations: local embedding generation and hybrid search with RRF fusion.
//!
//! See ADR-012 — Retrieval Architecture.

use std::collections::{HashMap, HashSet};

use uuid::Uuid;

use crate::error::RuntimeResult;
use crate::runtime::KhiveRuntime;
use khive_score::{rrf_score, DeterministicScore};
use khive_storage::types::{
    PageRequest, TextFilter, TextQueryMode, TextSearchHit, TextSearchRequest, VectorSearchHit,
    VectorSearchRequest,
};
use khive_storage::EntityFilter;
use khive_types::SubstrateKind;

/// A unified search result combining vector and text signals.
#[derive(Clone, Debug)]
pub struct SearchHit {
    pub entity_id: Uuid,
    pub score: DeterministicScore,
    pub source: SearchSource,
    pub title: Option<String>,
    pub snippet: Option<String>,
}

/// Which retrieval path(s) contributed to a hit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchSource {
    Vector,
    Text,
    Both,
}

/// RRF constant from the original paper. Controls how strongly top ranks dominate.
const RRF_K: usize = 60;

/// Candidates pulled per path before fusion. Higher = better recall, more work.
const CANDIDATE_MULTIPLIER: u32 = 4;

impl KhiveRuntime {
    /// Generate an embedding vector for `text` using the configured local model.
    ///
    /// First call lazily loads model weights (cold start cost). Subsequent calls reuse them.
    /// Returns `Unconfigured("embedding_model")` if no model is configured.
    pub async fn embed(&self, text: &str) -> RuntimeResult<Vec<f32>> {
        let service = self.embedder().await?;
        let model = self
            .config()
            .embedding_model
            .expect("embedder() returns Unconfigured when model is None");
        Ok(service.embed_one(text, model).await?)
    }

    /// Generate embeddings for multiple texts in one call.
    ///
    /// Delegates to the cached `EmbeddingService::embed`, so repeated texts within
    /// and across calls benefit from the runtime-level LRU cache.
    ///
    /// Returns an empty vec for empty input without hitting the embedding service.
    /// Returns `Unconfigured("embedding_model")` if no model is configured.
    pub async fn embed_batch(&self, texts: &[String]) -> RuntimeResult<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        let service = self.embedder().await?;
        let model = self
            .config()
            .embedding_model
            .expect("embedder() returns Unconfigured when model is None");
        Ok(service.embed(texts, model).await?)
    }

    /// Hybrid search: text (FTS5) + vector retrieval fused via Reciprocal Rank Fusion.
    ///
    /// - Always performs text search over `query_text`.
    /// - If `query_vector` is `Some`, also performs vector search and fuses both lists.
    /// - If `None`, returns text-only results — no vector store needed.
    ///
    /// `limit` caps the final returned list; internally pulls `limit * 4` candidates per path.
    pub async fn hybrid_search(
        &self,
        namespace: Option<&str>,
        query_text: &str,
        query_vector: Option<Vec<f32>>,
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

        let mut fused = rrf_fuse(text_hits, vector_hits, limit as usize);

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

    /// Exact KNN over the full namespace's vector store.
    ///
    /// sqlite-vec uses brute-force cosine — results are exact, not approximate.
    /// Cost is O(N · D) per query. For small-to-medium namespaces (~hundreds of
    /// thousands of vectors) this is well within latency budgets.
    pub async fn knn(
        &self,
        namespace: Option<&str>,
        query_vector: Vec<f32>,
        top_k: u32,
    ) -> RuntimeResult<Vec<VectorSearchHit>> {
        let ns = self.ns(namespace).to_string();
        Ok(self
            .vectors(namespace)?
            .search(VectorSearchRequest {
                query_embedding: query_vector,
                top_k,
                namespace: Some(ns),
                kind: Some(SubstrateKind::Entity),
            })
            .await?)
    }

    /// Exact KNN restricted to a candidate set.
    ///
    /// Useful for reranking the top-N results from `hybrid_search` (or any other
    /// retrieval path) with exact cosine similarity against a query vector.
    /// Returns hits sorted by similarity (highest first), truncated to `top_k`.
    pub async fn rerank(
        &self,
        namespace: Option<&str>,
        query_vector: &[f32],
        candidate_ids: &[Uuid],
        top_k: u32,
    ) -> RuntimeResult<Vec<VectorSearchHit>> {
        let candidate_set: HashSet<Uuid> = candidate_ids.iter().copied().collect();
        let ns = self.ns(namespace).to_string();
        let all_hits = self
            .vectors(namespace)?
            .search(VectorSearchRequest {
                query_embedding: query_vector.to_vec(),
                top_k: candidate_ids.len() as u32,
                namespace: Some(ns),
                kind: Some(SubstrateKind::Entity),
            })
            .await?;
        let mut hits: Vec<VectorSearchHit> = all_hits
            .into_iter()
            .filter(|h| candidate_set.contains(&h.subject_id))
            .collect();
        hits.sort_by(|a, b| b.score.cmp(&a.score));
        hits.truncate(top_k as usize);
        Ok(hits)
    }
}

/// Fuse text + vector hits with Reciprocal Rank Fusion (k=60).
///
/// Hits in both lists get RRF scores summed. Sort by fused score, take top-`limit`.
fn rrf_fuse(
    text_hits: Vec<TextSearchHit>,
    vector_hits: Vec<VectorSearchHit>,
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
        let rank = i + 1; // RRF is 1-indexed
        let entry = buckets.entry(hit.subject_id).or_default();
        entry.score = entry.score + rrf_score(rank, RRF_K);
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
        let rank = i + 1;
        let entry = buckets.entry(hit.subject_id).or_default();
        entry.score = entry.score + rrf_score(rank, RRF_K);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{KhiveRuntime, RuntimeConfig};
    use khive_storage::types::{TextSearchHit, VectorSearchHit};
    use lattice_embed::EmbeddingModel;

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
    fn rrf_fuse_text_only() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let text = vec![text_hit(a, 1, "A"), text_hit(b, 2, "B")];
        let hits = rrf_fuse(text, vec![], 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].entity_id, a);
        assert_eq!(hits[0].source, SearchSource::Text);
        assert_eq!(hits[0].title.as_deref(), Some("A"));
    }

    #[test]
    fn rrf_fuse_vector_only() {
        let a = Uuid::new_v4();
        let hits = rrf_fuse(vec![], vec![vector_hit(a, 1)], 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source, SearchSource::Vector);
        assert!(hits[0].title.is_none());
    }

    #[test]
    fn rrf_fuse_marks_both_when_in_both_lists() {
        let id = Uuid::new_v4();
        let text = vec![text_hit(id, 1, "A")];
        let vec = vec![vector_hit(id, 1)];
        let hits = rrf_fuse(text, vec, 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source, SearchSource::Both);
    }

    #[test]
    fn rrf_fuse_respects_limit() {
        let hits: Vec<TextSearchHit> = (0..20)
            .map(|i| text_hit(Uuid::new_v4(), i + 1, "x"))
            .collect();
        let fused = rrf_fuse(hits, vec![], 5);
        assert_eq!(fused.len(), 5);
    }

    #[test]
    fn rrf_fuse_orders_higher_score_first() {
        // Same UUID in both lists at rank 1 → score 2/(60+1). Different UUIDs → 1/(60+1) each.
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let text = vec![text_hit(a, 1, "A")];
        let vec = vec![vector_hit(a, 1), vector_hit(b, 2)];
        let hits = rrf_fuse(text, vec, 10);
        assert_eq!(hits[0].entity_id, a);
        assert_eq!(hits[0].source, SearchSource::Both);
        assert!(hits[0].score > hits[1].score);
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
            default_namespace: "test".to_string(),
            embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
            packs: vec!["kg".to_string()],
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
    #[ignore = "loads ~80 MB model; run with --include-ignored"]
    fn embed_batch_vectors_have_expected_dimensions() {
        let model = EmbeddingModel::AllMiniLmL6V2;
        let config = RuntimeConfig {
            db_path: None,
            default_namespace: "test".to_string(),
            embedding_model: Some(model),
            packs: vec!["kg".to_string()],
        };
        let rt = KhiveRuntime::new(config).unwrap();
        let texts = vec!["hello world".to_string()];
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(rt.embed_batch(&texts));
        let embeddings = result.unwrap();
        assert_eq!(embeddings[0].len(), model.dimensions());
    }
}
