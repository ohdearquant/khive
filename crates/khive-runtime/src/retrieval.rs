//! Retrieval operations: local embedding generation and hybrid search with RRF fusion.

use std::collections::{HashMap, HashSet};

use uuid::Uuid;

use crate::config::{parse_embedding_model_alias, sanitize_key};
use crate::curation::note_fts_document;
use crate::error::{RuntimeError, RuntimeResult};
use crate::runtime::{KhiveRuntime, NamespaceToken};
use khive_score::{rrf_score, DeterministicScore};
use khive_storage::types::{
    PageRequest, TextFilter, TextQueryMode, TextSearchHit, TextSearchRequest, VectorSearchHit,
    VectorSearchRequest,
};
use khive_storage::EntityFilter;
use khive_types::SubstrateKind;

// Fault-injection point for backfill reader errors. When set, the next
// `backfill_missing_embeddings` call substitutes a `StorageError::Pool` for the
// result of `sql.reader().await`, then resets to false. Available in test builds
// and the `fault-injection` feature for integration testing.
#[cfg(any(test, feature = "fault-injection"))]
std::thread_local! {
    static BACKFILL_READER_FAIL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Arm the backfill reader fault injection. When set, the next call to
/// `backfill_missing_embeddings` will substitute a `StorageError::Pool` for the
/// result of `sql.reader().await`, then reset the flag. The injected error
/// passes through `map_err(RuntimeError::Storage)?` — the same path as a real
/// reader failure — so it exercises the fail-closed guard rather than bypassing it.
/// Available when compiled with `cfg(test)` or `feature = "fault-injection"`.
#[cfg(any(test, feature = "fault-injection"))]
pub fn arm_backfill_reader_fail() {
    BACKFILL_READER_FAIL.with(|c| c.set(true));
}

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

/// RRF constant. Controls how strongly top ranks dominate.
///
/// The original paper uses k=60 for large-scale document retrieval. For a knowledge
/// graph with tens to thousands of entities, k=60 over-compresses scores into a
/// narrow band (rank 1 ≈ 0.016, rank 10 ≈ 0.014, spread ≈ 0.002). k=10 produces
/// rank 1 ≈ 0.091, rank 10 ≈ 0.050, spread ≈ 0.041 — 20× better discrimination,
/// making dedup-before-create reliable at graph sizes of 50–2700 entities.
const RRF_K: usize = 10;

/// Candidates pulled per path before fusion. Higher = better recall, more work.
const CANDIDATE_MULTIPLIER: u32 = 4;

impl KhiveRuntime {
    /// Generate an embedding vector for `text` using the configured default model.
    ///
    /// First call lazily loads model weights (cold start cost). Subsequent calls reuse them.
    /// Returns `Unconfigured("embedding_model")` if no model is configured.
    pub async fn embed(&self, text: &str) -> RuntimeResult<Vec<f32>> {
        let model_name = self.default_embedder_name();
        if model_name.is_empty() {
            return Err(RuntimeError::Unconfigured("embedding_model".into()));
        }
        self.embed_with_model(model_name, text).await
    }

    /// Generate an embedding vector for `text` using the named model.
    ///
    /// Accepts both built-in lattice model names/aliases and custom provider
    /// names registered via [`KhiveRuntime::register_embedder`]. For lattice
    /// models the resolved `EmbeddingModel` enum is forwarded to `embed_one`
    /// so the service can select the correct model variant. For custom
    /// providers, `embed_one` is called with `EmbeddingModel::default()`
    /// because custom services are expected to ignore the enum argument (they
    /// own a single model implicitly).
    ///
    /// Applies no instruction prefix (generic role). Use
    /// [`Self::embed_document_with_model`] / [`Self::embed_query_with_model`] for
    /// instruction-tuned models where the asymmetric prefix matters.
    ///
    /// Returns `UnknownModel` if `model_name` is not in the embedder registry.
    pub async fn embed_with_model(&self, model_name: &str, text: &str) -> RuntimeResult<Vec<f32>> {
        // Try to resolve as a lattice alias. If that succeeds, use the enum to
        // inform the service which model to run. If not, fall through to the
        // custom-provider path — custom services ignore the EmbeddingModel arg.
        let model = parse_embedding_model_alias(model_name);
        let service = self.embedder(model_name).await?;
        let emb_model = model.unwrap_or_default();
        Ok(service.embed_one(text, emb_model).await?)
    }

    /// Embed a document/passage for indexing using the named model.
    ///
    /// Applies `EmbeddingService::embed_passage`, which prepends the model's
    /// `document_instruction()` prefix when defined (e.g. `"passage: "` for
    /// multilingual-e5). For models with no document prefix (MiniLM, BGE) this
    /// is identical to [`Self::embed_with_model`].
    ///
    /// Use this for all index/store/backfill paths so that instruction-tuned
    /// models produce passage-side vectors.
    ///
    /// **Reindex caveat**: switching from an unprefixed model (or a model with no
    /// `document_instruction`) to an instruction-tuned model changes the vector
    /// representation. Vectors stored under the old scheme are not comparable to
    /// newly prefixed vectors. Operators must trigger a full reindex
    /// (`knowledge.index(rebuild_ann=true)` / `kkernel reindex`) after changing
    /// the embedding model config.
    ///
    /// Returns `UnknownModel` if `model_name` is not registered.
    pub async fn embed_document_with_model(
        &self,
        model_name: &str,
        text: &str,
    ) -> RuntimeResult<Vec<f32>> {
        let model = parse_embedding_model_alias(model_name);
        let service = self.embedder(model_name).await?;
        let emb_model = model.unwrap_or_default();
        service
            .embed_passage(&[text.to_string()], emb_model)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| RuntimeError::Internal("embed_passage returned empty vec".into()))
    }

    /// Embed a query string for retrieval using the named model.
    ///
    /// Applies `EmbeddingService::embed_query`, which prepends the model's
    /// `query_instruction()` prefix when defined (e.g. `"query: "` for
    /// multilingual-e5). For models with no query prefix (MiniLM, BGE) this
    /// is identical to [`Self::embed_with_model`].
    ///
    /// Use this for all search/recall/suggest query embedding paths so that
    /// instruction-tuned models land in the correct side of their retrieval
    /// space.
    ///
    /// Returns `UnknownModel` if `model_name` is not registered.
    pub async fn embed_query_with_model(
        &self,
        model_name: &str,
        text: &str,
    ) -> RuntimeResult<Vec<f32>> {
        let model = parse_embedding_model_alias(model_name);
        let service = self.embedder(model_name).await?;
        let emb_model = model.unwrap_or_default();
        service
            .embed_query(&[text.to_string()], emb_model)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| RuntimeError::Internal("embed_query returned empty vec".into()))
    }

    /// Embed a document for indexing using the configured default model.
    ///
    /// Delegates to [`Self::embed_document_with_model`]. Use for entity/note
    /// create and reindex paths.
    ///
    /// Returns `Unconfigured("embedding_model")` if no model is configured.
    pub async fn embed_document(&self, text: &str) -> RuntimeResult<Vec<f32>> {
        let model_name = self.default_embedder_name();
        if model_name.is_empty() {
            return Err(RuntimeError::Unconfigured("embedding_model".into()));
        }
        self.embed_document_with_model(model_name, text).await
    }

    /// Embed a query for retrieval using the configured default model.
    ///
    /// Delegates to [`Self::embed_query_with_model`]. Use for vector search and
    /// hybrid search query paths.
    ///
    /// Returns `Unconfigured("embedding_model")` if no model is configured.
    pub async fn embed_query(&self, text: &str) -> RuntimeResult<Vec<f32>> {
        let model_name = self.default_embedder_name();
        if model_name.is_empty() {
            return Err(RuntimeError::Unconfigured("embedding_model".into()));
        }
        self.embed_query_with_model(model_name, text).await
    }

    /// Generate embeddings for multiple texts in one call using the configured default model.
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
        let model_name = self.default_embedder_name();
        if model_name.is_empty() {
            return Err(RuntimeError::Unconfigured("embedding_model".into()));
        }
        self.embed_batch_with_model(model_name, texts).await
    }

    /// Generate embeddings for multiple texts using the named model.
    ///
    /// Accepts lattice model names/aliases and custom provider names.
    /// Returns `UnknownModel` if `model_name` is not in the embedder registry.
    pub async fn embed_batch_with_model(
        &self,
        model_name: &str,
        texts: &[String],
    ) -> RuntimeResult<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        let model = parse_embedding_model_alias(model_name);
        let service = self.embedder(model_name).await?;
        let emb_model = model.unwrap_or_default();
        Ok(service.embed(texts, emb_model).await?)
    }

    /// Embed a batch of documents for indexing using the named model.
    ///
    /// Applies `EmbeddingService::embed_passage`. Use for all bulk
    /// index/backfill/reindex operations to apply the passage-side prefix.
    ///
    /// **Reindex caveat**: see [`Self::embed_document_with_model`] — the same
    /// incomparability applies to batch-indexed vectors when switching models.
    ///
    /// Returns `UnknownModel` if `model_name` is not registered.
    pub async fn embed_document_batch_with_model(
        &self,
        model_name: &str,
        texts: &[String],
    ) -> RuntimeResult<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        let model = parse_embedding_model_alias(model_name);
        let service = self.embedder(model_name).await?;
        let emb_model = model.unwrap_or_default();
        Ok(service.embed_passage(texts, emb_model).await?)
    }

    /// Embed a batch of documents for indexing using the configured default model.
    ///
    /// Convenience delegate to [`Self::embed_document_batch_with_model`]. Use for
    /// bulk knowledge-atom and section indexing paths.
    ///
    /// Returns `Unconfigured("embedding_model")` if no model is configured.
    pub async fn embed_document_batch(&self, texts: &[String]) -> RuntimeResult<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        let model_name = self.default_embedder_name();
        if model_name.is_empty() {
            return Err(RuntimeError::Unconfigured("embedding_model".into()));
        }
        self.embed_document_batch_with_model(model_name, texts)
            .await
    }

    /// Embed a batch of queries for retrieval using the named model.
    ///
    /// Applies `EmbeddingService::embed_query`. Use for bulk query-side
    /// operations where multiple queries need instruction-tuned prefixing.
    ///
    /// Returns `UnknownModel` if `model_name` is not registered.
    pub async fn embed_query_batch_with_model(
        &self,
        model_name: &str,
        texts: &[String],
    ) -> RuntimeResult<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        let model = parse_embedding_model_alias(model_name);
        let service = self.embedder(model_name).await?;
        let emb_model = model.unwrap_or_default();
        Ok(service.embed_query(texts, emb_model).await?)
    }

    /// Search vectors using either a caller-provided embedding or query text.
    ///
    /// Existing callers pass `query_embedding: Some(vec)` to avoid re-embedding.
    /// Text callers pass `query_embedding: None, query_text: Some(...)` and the
    /// runtime embeds internally.
    pub async fn vector_search(
        &self,
        token: &NamespaceToken,
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
                self.embed_query(text).await?
            }
        };

        let ns = token.namespace().as_str().to_owned();
        Ok(self
            .vectors(token)?
            .search(VectorSearchRequest {
                query_vectors: vec![embedding],
                top_k,
                namespace: Some(ns),
                kind,
                embedding_model: None,
                filter: None,
                backend_hints: None,
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
    /// - `tags_any`: when non-empty, only entities that have at least one of these
    ///   tags (case-insensitive) survive the alive-set intersection. Applied BEFORE
    ///   truncation so matches ranked beyond `limit` in the raw fusion are not lost.
    /// - `properties_filter`: when `Some`, only entities whose properties are a
    ///   superset of the given JSON object survive. Applied BEFORE truncation.
    ///
    /// `limit` caps the final returned list; internally pulls `limit * 4` candidates per path.
    /// The fused candidate set is kept untruncated until after the alive + kind + tag +
    /// properties filter so that matching hits ranked below `limit` in the raw fusion
    /// still surface when higher-ranked candidates are excluded by any filter.
    ///
    /// # Cross-namespace visibility (entity search — primary namespace only; deferred)
    ///
    /// Both the **FTS leg** and the **vector/ANN leg** of entity search (`hybrid_search`)
    /// are restricted to the **primary namespace only**.
    ///
    /// Rationale: each namespace owns a separate FTS table (`fts_entities_{ns}`)
    /// and a separate ANN index instance. Cross-namespace entity-search fanout
    /// requires iterating over every visible namespace's store, issuing parallel
    /// search requests, and fusing the results — this is deferred (entity-search
    /// cross-namespace fanout is the outstanding follow-up).
    ///
    /// Note: this is distinct from memory recall's cross-namespace fanout, which
    /// ships in ADR-007 Rev 4 (`memory.recall` iterates `visible_namespaces` across
    /// both the FTS and vector legs). Entity search fanout is the remaining deferred
    /// piece; memory recall fanout is not deferred.
    ///
    /// The `visible_ns` list is forwarded in the `TextFilter.namespaces` field,
    /// which limits results to those namespaces within the primary store. Because
    /// entities from extra namespaces live in their own FTS tables, this filter has
    /// no cross-namespace effect today.
    ///
    /// Callers with a multi-namespace visible set can READ cross-namespace entities
    /// directly via `get_entity` / `resolve`, but `hybrid_search` returns only
    /// primary-namespace hits until entity-search cross-namespace fanout ships.
    #[allow(clippy::too_many_arguments)]
    pub async fn hybrid_search(
        &self,
        token: &NamespaceToken,
        query_text: &str,
        query_vector: Option<Vec<f32>>,
        limit: u32,
        entity_kind: Option<&str>,
        entity_type: Option<&str>,
        tags_any: &[String],
        properties_filter: Option<&serde_json::Value>,
    ) -> RuntimeResult<Vec<SearchHit>> {
        let candidates = limit.saturating_mul(CANDIDATE_MULTIPLIER).max(limit);

        let visible_ns: Vec<String> = token
            .visible_namespaces()
            .iter()
            .map(|ns| ns.as_str().to_owned())
            .collect();
        let text_hits = self
            .text(token)?
            .search(TextSearchRequest {
                query: query_text.to_string(),
                mode: TextQueryMode::Plain,
                filter: Some(TextFilter {
                    namespaces: visible_ns.clone(),
                    ..TextFilter::default()
                }),
                top_k: candidates,
                snippet_chars: 200,
            })
            .await?;

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

        // Fuse without truncating: keep the full candidate pool through the
        // alive/kind/tag/property filter so matching hits below rank `limit`
        // aren't lost when higher-ranked candidates are excluded.
        let mut fused = rrf_fuse(text_hits, vector_hits, candidates as usize, query_text);

        // Filter to alive entities (and optionally to a specific kind, tags, or
        // properties). A single query fetches all alive IDs that match the kind
        // and tag constraints from the fused set; any ID absent has been
        // soft-deleted or doesn't match. The SQL-level `tags_any` filter is
        // pushed into `query_entities`; properties filtering (no SQL column)
        // is applied at the Rust level using the entity records already fetched.
        if !fused.is_empty() {
            let candidate_ids: Vec<Uuid> = fused.iter().map(|h| h.entity_id).collect();
            let alive_page = self
                .entities(token)?
                .query_entities(
                    token.namespace().as_str(),
                    EntityFilter {
                        ids: candidate_ids,
                        kinds: entity_kind.map(|k| vec![k.to_string()]).unwrap_or_default(),
                        entity_types: entity_type.map(|t| vec![t.to_string()]).unwrap_or_default(),
                        namespaces: visible_ns,
                        tags_any: tags_any.to_vec(),
                        ..EntityFilter::default()
                    },
                    PageRequest {
                        offset: 0,
                        limit: fused.len() as u32,
                    },
                )
                .await?;
            // Keep entity metadata to enrich hits that had no FTS5 title/snippet,
            // and to apply the properties filter before truncation.
            let mut entity_meta: HashMap<Uuid, (String, Option<String>)> = HashMap::new();
            let mut alive: HashSet<Uuid> = HashSet::new();
            for e in alive_page.items {
                // Apply properties predicate here — before adding to the alive set —
                // so that non-matching candidates are dropped before truncation.
                if let Some(pf) = properties_filter {
                    if !entity_props_match(e.properties.as_ref(), pf) {
                        continue;
                    }
                }
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
        token: &NamespaceToken,
        query_vector: Vec<f32>,
        top_k: u32,
    ) -> RuntimeResult<Vec<VectorSearchHit>> {
        let ns = token.namespace().as_str().to_owned();
        Ok(self
            .vectors(token)?
            .search(VectorSearchRequest {
                query_vectors: vec![query_vector],
                top_k,
                namespace: Some(ns),
                kind: Some(SubstrateKind::Entity),
                embedding_model: None,
                filter: None,
                backend_hints: None,
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
        token: &NamespaceToken,
        query_vector: &[f32],
        candidate_ids: &[Uuid],
        top_k: u32,
    ) -> RuntimeResult<Vec<VectorSearchHit>> {
        let candidate_set: HashSet<Uuid> = candidate_ids.iter().copied().collect();
        let ns = token.namespace().as_str().to_owned();
        let all_hits = self
            .vectors(token)?
            .search(VectorSearchRequest {
                query_vectors: vec![query_vector.to_vec()],
                top_k: candidate_ids.len() as u32,
                namespace: Some(ns),
                kind: Some(SubstrateKind::Entity),
                embedding_model: None,
                filter: None,
                backend_hints: None,
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

    /// Backfill vector and FTS index entries for entities and notes that are missing them.
    ///
    /// Intended to run once at startup as a background task (warm-up sequence steps 2–4).
    /// Queries the SQL substrate for entity descriptions and note contents that have no
    /// corresponding entry in the vector store for any registered embedding model, then
    /// embeds and inserts them. FTS entries missing for notes are also repopulated.
    ///
    /// The operation is best-effort: individual embed/insert failures are logged and
    /// skipped rather than aborting the whole backfill. If no embedding models are
    /// registered, returns immediately with 0.
    ///
    /// Returns the total number of records backfilled across all models.
    pub async fn backfill_missing_embeddings(&self, token: &NamespaceToken) -> RuntimeResult<u64> {
        use khive_storage::types::{SqlRow, SqlStatement, SqlValue};

        let model_names = self.registered_embedding_model_names();
        if model_names.is_empty() {
            tracing::debug!(
                "backfill_missing_embeddings: no embedding models registered, skipping"
            );
            return Ok(0);
        }

        let ns = token.namespace().as_str().to_string();
        let mut total_backfilled = 0u64;

        for model_name in &model_names {
            // Derive the vec table name from the model name (must match vec_model_key logic).
            let vec_table = format!("vec_{}", sanitize_key(model_name));

            // --- Entities: embed description where no vector entry exists ---
            // Loop until a batch returns fewer than PAGE_SIZE rows. Because the query uses
            // NOT IN (SELECT subject_id FROM vec_table ...), each successfully inserted row is
            // excluded from subsequent pages — no OFFSET needed.
            const PAGE_SIZE: usize = 500;
            let mut entity_total = 0usize;
            loop {
                let entity_sql = SqlStatement {
                    sql: format!(
                        "SELECT id, name, description FROM entities \
                         WHERE namespace = ?1 AND deleted_at IS NULL \
                         AND id NOT IN (\
                             SELECT subject_id FROM {vec_table} \
                             WHERE namespace = ?1 AND embedding_model = ?2 \
                         ) LIMIT {PAGE_SIZE}"
                    ),
                    params: vec![
                        SqlValue::Text(ns.clone()),
                        SqlValue::Text(model_name.clone()),
                    ],
                    label: Some("backfill_entities".into()),
                };

                let entity_rows: Vec<SqlRow> = {
                    let sql = self.sql();
                    let reader_result = sql.reader().await;
                    #[cfg(any(test, feature = "fault-injection"))]
                    let reader_result = if BACKFILL_READER_FAIL.with(|c| c.get()) {
                        BACKFILL_READER_FAIL.with(|c| c.set(false));
                        Err(khive_storage::StorageError::Pool {
                            operation: "reader".into(),
                            message: "injected failure".into(),
                        })
                    } else {
                        reader_result
                    };
                    let mut reader = reader_result.map_err(RuntimeError::Storage)?;
                    reader
                        .query_all(entity_sql)
                        .await
                        .map_err(RuntimeError::Storage)?
                };

                let batch_len = entity_rows.len();
                entity_total += batch_len;

                for row in &entity_rows {
                    let id_str = row.columns.first().and_then(|c| {
                        if let SqlValue::Text(s) = &c.value {
                            Some(s.clone())
                        } else {
                            None
                        }
                    });
                    let description = row.columns.get(2).and_then(|c| {
                        if let SqlValue::Text(s) = &c.value {
                            Some(s.clone())
                        } else if let SqlValue::Null = &c.value {
                            None
                        } else {
                            None
                        }
                    });

                    let (Some(id_str), Some(desc)) = (id_str, description) else {
                        continue;
                    };
                    let Ok(id) = id_str.parse::<Uuid>() else {
                        continue;
                    };
                    if desc.trim().is_empty() {
                        continue;
                    }

                    match self.embed_document_with_model(model_name, &desc).await {
                        Ok(vector) => {
                            if let Ok(vs) = self.vectors_for_model(token, model_name) {
                                match vs
                                    .insert(
                                        id,
                                        SubstrateKind::Entity,
                                        &ns,
                                        "entity.description",
                                        vec![vector],
                                    )
                                    .await
                                {
                                    Ok(()) => {
                                        total_backfilled += 1;
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            id = %id, model = %model_name,
                                            error = %e,
                                            "backfill_missing_embeddings: entity vector insert failed"
                                        );
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                id = %id, model = %model_name,
                                error = %e,
                                "backfill_missing_embeddings: entity embed failed"
                            );
                        }
                    }
                }

                if batch_len < PAGE_SIZE {
                    break;
                }
            }

            // --- Notes: embed content where no vector entry exists ---
            let text_store = self.text_for_notes(token).ok();
            let note_store = self.notes(token).ok();
            let mut note_total = 0usize;
            loop {
                // Select only the id here; the full Note is fetched below so that
                // note_fts_document receives all fields (name, properties, updated_at)
                // and produces a parity-correct document rather than a stripped one.
                let note_sql = SqlStatement {
                    sql: format!(
                        "SELECT id FROM notes \
                         WHERE namespace = ?1 AND deleted_at IS NULL \
                         AND id NOT IN (\
                             SELECT subject_id FROM {vec_table} \
                             WHERE namespace = ?1 AND embedding_model = ?2 \
                         ) LIMIT {PAGE_SIZE}"
                    ),
                    params: vec![
                        SqlValue::Text(ns.clone()),
                        SqlValue::Text(model_name.clone()),
                    ],
                    label: Some("backfill_notes".into()),
                };

                let note_rows: Vec<SqlRow> = {
                    let sql = self.sql();
                    let reader_result = sql.reader().await;
                    #[cfg(any(test, feature = "fault-injection"))]
                    let reader_result = if BACKFILL_READER_FAIL.with(|c| c.get()) {
                        BACKFILL_READER_FAIL.with(|c| c.set(false));
                        Err(khive_storage::StorageError::Pool {
                            operation: "reader".into(),
                            message: "injected failure".into(),
                        })
                    } else {
                        reader_result
                    };
                    let mut reader = reader_result.map_err(RuntimeError::Storage)?;
                    reader
                        .query_all(note_sql)
                        .await
                        .map_err(RuntimeError::Storage)?
                };

                let batch_len = note_rows.len();
                note_total += batch_len;

                for row in &note_rows {
                    let id_str = row.columns.first().and_then(|c| {
                        if let SqlValue::Text(s) = &c.value {
                            Some(s.clone())
                        } else {
                            None
                        }
                    });

                    let Some(id_str) = id_str else {
                        continue;
                    };
                    let Ok(id) = id_str.parse::<Uuid>() else {
                        continue;
                    };

                    // Fetch the full Note so that note_fts_document has all fields
                    // (name, properties, updated_at) — prevents overwriting a correct
                    // FTS row with a stripped content-only document.
                    let note = match &note_store {
                        Some(store) => match store.get_note(id).await {
                            Ok(Some(n)) => n,
                            _ => continue,
                        },
                        None => continue,
                    };

                    if note.content.trim().is_empty() {
                        continue;
                    }

                    // Repopulate FTS entry using the shared constructor (first model only
                    // to avoid N identical overwrites per note).
                    if model_names.first().map(|n| n.as_str()) == Some(model_name.as_str()) {
                        if let Some(ref ts) = text_store {
                            if let Err(e) = ts.upsert_document(note_fts_document(&note)).await {
                                tracing::warn!(id = %id, error = %e,
                                    "backfill_missing_embeddings: note FTS upsert failed");
                            }
                        }
                    }

                    let content = note.content.clone();
                    match self.embed_document_with_model(model_name, &content).await {
                        Ok(vector) => {
                            if let Ok(vs) = self.vectors_for_model(token, model_name) {
                                match vs
                                    .insert(
                                        id,
                                        SubstrateKind::Note,
                                        &ns,
                                        "note.content",
                                        vec![vector],
                                    )
                                    .await
                                {
                                    Ok(()) => {
                                        total_backfilled += 1;
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            id = %id, model = %model_name,
                                            error = %e,
                                            "backfill_missing_embeddings: note vector insert failed"
                                        );
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                id = %id, model = %model_name,
                                error = %e,
                                "backfill_missing_embeddings: note embed failed"
                            );
                        }
                    }
                }

                if batch_len < PAGE_SIZE {
                    break;
                }
            }

            tracing::info!(
                model = %model_name,
                namespace = %ns,
                entities = entity_total,
                notes = note_total,
                "backfill_missing_embeddings: model pass complete"
            );
        }

        tracing::info!(
            namespace = %ns,
            total_backfilled = total_backfilled,
            "backfill_missing_embeddings: finished"
        );

        Ok(total_backfilled)
    }

    /// Sweep orphaned vector entries for all registered embedding models.
    ///
    /// A vector entry is orphaned when its `subject_id` no longer exists as a
    /// live row in the entity or note tables (i.e. either the row is absent or
    /// has `deleted_at IS NOT NULL`). Orphaned entries accumulate after
    /// hard-deletes because the vector store and SQL substrate are decoupled.
    ///
    /// Iterates over every registered embedding model and calls
    /// [`khive_storage::VectorStore::orphan_sweep`] for the token's namespace. Models whose
    /// backend returns [`khive_storage::StorageError::Unsupported`] are skipped without error —
    /// this preserves forward-compat when a newly registered model does not yet
    /// implement sweep.
    ///
    /// Returns the total number of vector rows deleted across all models.
    pub async fn sweep_orphan_vectors(
        &self,
        token: &NamespaceToken,
        max_delete_per_model: u32,
        dry_run: bool,
    ) -> RuntimeResult<u64> {
        use khive_storage::types::OrphanSweepConfig;
        use khive_storage::StorageError;

        let model_names = self.registered_embedding_model_names();
        if model_names.is_empty() {
            tracing::debug!("sweep_orphan_vectors: no embedding models registered, skipping");
            return Ok(0);
        }

        let ns = token.namespace().as_str().to_string();
        let mut total_deleted = 0u64;

        for model_name in &model_names {
            let store = match self.vectors_for_model(token, model_name) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        model = %model_name,
                        error = %e,
                        "sweep_orphan_vectors: failed to get vector store, skipping model"
                    );
                    continue;
                }
            };

            let caps = store.capabilities();
            if !caps.supports_orphan_sweep {
                tracing::debug!(
                    model = %model_name,
                    "sweep_orphan_vectors: backend does not support orphan sweep, skipping"
                );
                continue;
            }

            let config = OrphanSweepConfig {
                subject_id_allowlist: None,
                namespaces: vec![ns.clone()],
                substrate_kinds: vec![],
                max_delete: max_delete_per_model,
                dry_run,
            };

            match store.orphan_sweep(&config).await {
                Ok(result) => {
                    tracing::info!(
                        model = %model_name,
                        namespace = %ns,
                        scanned = result.scanned,
                        deleted = result.deleted,
                        would_delete = result.would_delete,
                        dry_run = dry_run,
                        "sweep_orphan_vectors: sweep complete"
                    );
                    total_deleted += result.deleted;
                }
                Err(StorageError::Unsupported { .. }) => {
                    tracing::debug!(
                        model = %model_name,
                        "sweep_orphan_vectors: backend returned Unsupported, skipping"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        model = %model_name,
                        error = %e,
                        "sweep_orphan_vectors: sweep failed, continuing with other models"
                    );
                }
            }
        }

        tracing::info!(
            namespace = %ns,
            total_deleted = total_deleted,
            dry_run = dry_run,
            "sweep_orphan_vectors: finished"
        );

        Ok(total_deleted)
    }
}

/// Returns `true` when `entity_props` is a superset of all key-value pairs in `filter`.
///
/// Mirrors the semantics of `khive_pack_kg::handlers::common::props_match` so that the
/// storage-leg predicate is identical to the handler-side post-filter.
fn entity_props_match(
    entity_props: Option<&serde_json::Value>,
    filter: &serde_json::Value,
) -> bool {
    let required = match filter.as_object() {
        Some(obj) if !obj.is_empty() => obj,
        _ => return true,
    };
    let actual = match entity_props.and_then(serde_json::Value::as_object) {
        Some(obj) => obj,
        None => return false,
    };
    required
        .iter()
        .all(|(k, v)| actual.get(k).is_some_and(|av| av == v))
}

/// Score bonus applied when an entity's title is an exact case-insensitive match for
/// the query. Dominates RRF scores (~0.09–0.18 range with k=10) so that an exact
/// name match always ranks above any partial or semantic match.
const EXACT_MATCH_BOOST: f64 = 0.5;

/// Fuse text + vector hits with Reciprocal Rank Fusion (k=10).
///
/// Entity search stays local because it uses k=10 plus exact-match boosting.
/// Hits in both lists get RRF scores summed. If `query_text` exactly matches
/// (case-insensitive) an entity's title from the text hits, a bonus of
/// `EXACT_MATCH_BOOST` is added to ensure exact-name matches dominate.
/// Sort by fused score, take top-`limit`.
fn rrf_fuse(
    text_hits: Vec<TextSearchHit>,
    vector_hits: Vec<VectorSearchHit>,
    limit: usize,
    query_text: &str,
) -> Vec<SearchHit> {
    #[derive(Default)]
    struct Bucket {
        score: DeterministicScore,
        source: Option<SearchSource>,
        title: Option<String>,
        snippet: Option<String>,
    }

    let mut buckets: HashMap<Uuid, Bucket> = HashMap::new();

    let query_lower = query_text.to_lowercase();
    for (i, hit) in text_hits.into_iter().enumerate() {
        let rank = i + 1; // RRF is 1-indexed
        let entry = buckets.entry(hit.subject_id).or_default();
        entry.score = entry.score + rrf_score(rank, RRF_K);
        entry.source = Some(match entry.source {
            Some(SearchSource::Vector) => SearchSource::Both,
            _ => SearchSource::Text,
        });
        if entry.title.is_none() {
            // Apply exact-match boost before storing the title so we only check once.
            if let Some(ref title) = hit.title {
                if title.to_lowercase() == query_lower {
                    entry.score = entry.score + DeterministicScore::from_f64(EXACT_MATCH_BOOST);
                }
            }
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
    use crate::runtime::{KhiveRuntime, NamespaceToken, RuntimeConfig};
    use khive_storage::types::{TextSearchHit, VectorSearchHit};
    use khive_types::namespace::Namespace;
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

    // ---- hybrid_search enrichment (issue #147 / #160) ----

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

    // ---- issue #225 regression: predicate pushdown before truncation ----

    /// Regression test for issue #225 (entity branch).
    ///
    /// Scenario: `limit=1`, tag_filter=["target-tag"]. Two entities are inserted:
    ///   - "decoy_alpha_beta_gamma": many query tokens → ranks 1 in FTS (dominates).
    ///     Does NOT have "target-tag".
    ///   - "alpha_beta_gamma target": fewer query tokens → ranks 2 in FTS.
    ///     HAS "target-tag".
    ///
    /// Without predicate pushdown: `fused.truncate(1)` keeps only the decoy. The
    /// tag-matching entity is invisible. The test asserts the matching entity IS
    /// returned — this assertion fails on the unfixed code (where tags_any is not
    /// passed into `query_entities`) and passes after the fix.
    ///
    /// Isomorphism: reverting `tags_any: tags_any.to_vec()` in the `EntityFilter`
    /// inside `hybrid_search` re-breaks this test (the decoy survives `retain` and
    /// occupies the single slot, dropping the target).
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

    /// Regression test for issue #225 (entity branch, properties predicate).
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

    // ---- embed intent tests (issue #93) ----

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
                .embed_document_with_model(&model.to_string(), &text)
                .await
                .unwrap();
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
                .embed_document_with_model(&model.to_string(), &text)
                .await
                .unwrap();
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

    // ---- M-07 regression: backfill reader error must be propagated, not swallowed ----

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

    /// Regression test for M-07: `backfill_missing_embeddings` must propagate a
    /// reader error rather than treating it as "zero rows to embed" (silent swallow).
    ///
    /// Before the fix: `Err(_) => vec![]` caused the caller to receive `Ok(0)`,
    /// silently skipping all embeddings. After the fix: the error is returned via `?`.
    ///
    /// The fault injection substitutes a `StorageError::Pool` for the result of
    /// `sql.reader().await` (i.e., the error originates AT the reader boundary, not
    /// before it), so the test exercises the exact `map_err(RuntimeError::Storage)?`
    /// lines that the fix introduced. Reverting those lines to `unwrap_or_default()`
    /// would swallow the injected error and cause this test to fail.
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
}
