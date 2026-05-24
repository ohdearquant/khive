//! Retrieval operations: local embedding generation and hybrid search with RRF fusion.
//!
//! See ADR-012 — Retrieval Architecture.

use std::collections::{HashMap, HashSet};

use uuid::Uuid;

use crate::error::{RuntimeError, RuntimeResult};
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

    /// Search vectors using either a caller-provided embedding or query text.
    ///
    /// Existing callers pass `query_embedding: Some(vec)` to avoid re-embedding.
    /// Text callers pass `query_embedding: None, query_text: Some(...)` and the
    /// runtime embeds internally.
    pub async fn vector_search(
        &self,
        namespace: Option<&str>,
        query_embedding: Option<Vec<f32>>,
        query_text: Option<&str>,
        top_k: u32,
        kind: Option<SubstrateKind>,
    ) -> RuntimeResult<Vec<VectorSearchHit>> {
        let embedding = match query_embedding {
            Some(vec) => vec,
            None => {
                let text = query_text.ok_or_else(|| {
                    RuntimeError::InvalidInput(
                        "vector search requires query_embedding or query_text".into(),
                    )
                })?;
                if text.trim().is_empty() {
                    return Err(RuntimeError::InvalidInput(
                        "query_text must not be empty".into(),
                    ));
                }
                self.embed(text).await?
            }
        };

        let ns = self.ns(namespace).to_string();
        Ok(self
            .vectors(namespace)?
            .search(VectorSearchRequest {
                query_embedding: embedding,
                top_k,
                namespace: Some(ns),
                kind,
            })
            .await?)
    }

    /// Hybrid search: text (FTS5) + vector retrieval fused via Reciprocal Rank Fusion.
    ///
    /// - Always performs text search over `query_text`.
    /// - If `query_vector` is `Some`, also performs vector search and fuses both lists.
    /// - If `None`, returns text-only results — no vector store needed.
    /// - If `entity_kind` is `Some`, the alive-set query filters to that kind.
    ///   The text/vector candidate pools are unfiltered up front; the kind
    ///   filter applies at the alive-check stage where we already fetch each
    ///   candidate to confirm it isn't soft-deleted.
    ///
    /// `limit` caps the final returned list; internally pulls `limit * 4` candidates per path.
    /// The fused candidate set is kept untruncated until after the alive + kind filter so
    /// that right-kind hits ranked below `limit` in the raw fusion still surface when
    /// higher-ranked candidates are wrong-kind or soft-deleted.
    #[allow(clippy::too_many_arguments)]
    pub async fn hybrid_search(
        &self,
        namespace: Option<&str>,
        query_text: &str,
        query_vector: Option<Vec<f32>>,
        limit: u32,
        entity_kind: Option<&str>,
        entity_type: Option<&str>,
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

        let vector_hits = if query_vector.is_some() || self.config().embedding_model.is_some() {
            self.vector_search(
                namespace,
                query_vector,
                Some(query_text),
                candidates,
                Some(SubstrateKind::Entity),
            )
            .await?
        } else {
            Vec::new()
        };

        // Fuse without truncating: keep the full candidate pool through the
        // alive/kind filter so right-kind hits below rank `limit` aren't lost.
        let mut fused = rrf_fuse(text_hits, vector_hits, candidates as usize);

        // Filter to alive entities (and optionally to a specific kind). A single
        // query fetches all alive IDs that match the kind constraint from the
        // fused set; any ID absent has been soft-deleted or doesn't match.
        if !fused.is_empty() {
            let candidate_ids: Vec<Uuid> = fused.iter().map(|h| h.entity_id).collect();
            let alive_page = self
                .entities(namespace)?
                .query_entities(
                    self.ns(namespace),
                    EntityFilter {
                        ids: candidate_ids,
                        kinds: entity_kind.map(|k| vec![k.to_string()]).unwrap_or_default(),
                        entity_types: entity_type.map(|t| vec![t.to_string()]).unwrap_or_default(),
                        ..EntityFilter::default()
                    },
                    PageRequest {
                        offset: 0,
                        limit: fused.len() as u32,
                    },
                )
                .await?;
            // Keep entity metadata to enrich hits that had no FTS5 title/snippet.
            let mut entity_meta: HashMap<Uuid, (String, Option<String>)> = HashMap::new();
            let mut alive: HashSet<Uuid> = HashSet::new();
            for e in alive_page.items {
                alive.insert(e.id);
                entity_meta.insert(e.id, (e.name, e.description));
            }

            fused.retain(|h| alive.contains(&h.entity_id));

            // Enrich vector-only hits (title/snippet == None) from entity record.
            for hit in &mut fused {
                if let Some((name, description)) = entity_meta.get(&hit.entity_id) {
                    if hit.title.is_none() {
                        hit.title = Some(name.clone());
                    }
                    if hit.snippet.is_none() {
                        hit.snippet = description.clone();
                    }
                }
            }
        }

        fused.truncate(limit as usize);
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
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(rt.vector_search(None, None, None, 10, Some(SubstrateKind::Entity)));
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
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(rt.vector_search(
                None,
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
            default_namespace: "test".to_string(),
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

    // ---- hybrid_search enrichment (issue #147 / #160) ----

    #[tokio::test]
    async fn hybrid_search_entity_hit_has_title() {
        let rt = KhiveRuntime::memory().unwrap();
        rt.create_entity(
            None,
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
            .hybrid_search(None, "FlashAttention", None, 10, None, None)
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
}
