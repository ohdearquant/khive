use super::*;

// ─── embedding rerank ────────────────────────────────────────────────────────

/// Request-local cache for the query's embedding vector(s), threaded through
/// search/suggest/compose so a query is embedded at most once per role it is
/// actually needed in.
///
/// `EmbeddingService::embed_query` and `::embed` are different provider
/// methods: instruction-tuned models (E5, Qwen) prepend a query prompt for
/// the former, so the two calls land in different sides of the retrieval
/// space for the identical text. `role_specific` holds the outcome of the
/// request's one attempt at `runtime.embed_query` (the ANN and KG-blend
/// dense-search path); `generic` holds the first slot of a combined
/// `runtime.embed_batch` rerank call. A `generic` vector is a valid stand-in
/// for `role_specific` in rerank cosine math (both are just "the query's
/// embedding" for a same-space comparison against candidates embedded the
/// same generic way), but it must never satisfy a caller that specifically
/// requires `role_specific` — passing it to the KG blend's `hybrid_search`
/// masks a real `embed_query` failure behind a same-shaped, wrong-space
/// vector (#2307).
#[derive(Debug, Default, Clone)]
pub(super) struct QueryEmbeddingCache {
    pub(super) role_specific: RoleSpecificEmbedding,
    pub(super) generic: Option<Vec<f32>>,
}

/// Outcome of the request's (at most one) attempt to obtain the query's
/// role-specific (`embed_query`) vector.
///
/// A plain `Option<Vec<f32>>` cannot tell "never tried" from "tried and the
/// provider failed" — both read as `None`. That collapse is exactly what let
/// a failed attempt in one stage (e.g. `suggest`) get retried by a later
/// stage in the same request (e.g. `compose`'s KG-blend gate): the later
/// stage saw `None` and had no way to know an attempt, and a failure, had
/// already happened (#2307). `Failed` records that the attempt happened so
/// nothing downstream pays for a second failing provider call; only
/// `NotAttempted` authorizes a stage to try.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) enum RoleSpecificEmbedding {
    #[default]
    NotAttempted,
    Failed,
    Vector(Vec<f32>),
}

impl RoleSpecificEmbedding {
    pub(super) fn as_deref(&self) -> Option<&[f32]> {
        match self {
            Self::Vector(v) => Some(v.as_slice()),
            Self::NotAttempted | Self::Failed => None,
        }
    }

    pub(super) fn is_not_attempted(&self) -> bool {
        matches!(self, Self::NotAttempted)
    }
}

impl QueryEmbeddingCache {
    pub(super) fn any(&self) -> Option<&[f32]> {
        self.role_specific.as_deref().or(self.generic.as_deref())
    }
}

pub(super) async fn embed_cosine_scores(
    runtime: &KhiveRuntime,
    query: &str,
    query_embedding: &mut QueryEmbeddingCache,
    candidate_texts: &[String],
) -> Result<Option<Vec<f32>>, RuntimeError> {
    if runtime.default_embedder_name().is_empty() || candidate_texts.is_empty() {
        return Ok(None);
    }

    // When nothing is cached yet, include the query in this one batch and
    // retain its vector as `generic` for downstream cosine math. Otherwise
    // embed only candidates; the query vector is request-local immutable data.
    let query_was_cached = query_embedding.any().is_some();
    let texts = if query_was_cached {
        candidate_texts.to_vec()
    } else {
        let mut texts = Vec::with_capacity(candidate_texts.len() + 1);
        texts.push(query.to_string());
        texts.extend_from_slice(candidate_texts);
        texts
    };
    // A request read-deadline timeout here degrades to "rerank did not run"
    // (`Ok(None)`) rather than propagating, same contract as every other
    // read in this module (issue #1930 Amendment 2): before the lexical
    // stage owned its own budget, this call was only ever reached when the
    // shared request deadline had *not* already expired, so a mid-flight
    // expiry racing this specific await was unreachable in practice. With
    // the stages decoupled, a lexical-only degradation with real time left
    // to spare now reaches this call normally, and the ordinary
    // async-scheduling race — the deadline elapsing while this embed_batch
    // is in flight — is reachable and must degrade, not hard-error.
    let embeddings = match khive_storage::await_request_read_phase(
        "knowledge.embedding_rerank",
        runtime.embed_batch(&texts),
    )
    .await
    {
        Ok(Ok(embeddings)) => embeddings,
        Ok(Err(_)) => return Ok(None),
        Err(e) if is_timeout(&e) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if embeddings.len() != texts.len() {
        return Ok(None);
    }
    let candidate_embeddings = if query_was_cached {
        embeddings.as_slice()
    } else {
        query_embedding.generic = Some(embeddings[0].clone());
        &embeddings[1..]
    };
    let query_emb = query_embedding
        .any()
        .expect("query embedding was cached or populated from non-empty batch");
    Ok(Some(
        candidate_embeddings
            .iter()
            .map(|emb| cosine_similarity(query_emb, emb, false))
            .collect(),
    ))
}

fn blend_and_rank(hits: &mut [ScoredHit], cosines: impl IntoIterator<Item = f32>, alpha: f32) {
    let max_score = hits
        .iter()
        .map(|hit| hit.score)
        .fold(0.0f32, f32::max)
        .max(1e-6);
    for (hit, cosine) in hits.iter_mut().zip(cosines) {
        let normalized_score = hit.score / max_score;
        hit.score = alpha * normalized_score + (1.0 - alpha) * cosine.max(0.0);
        hit.provenance.embedding_rerank = true;
    }
    hits.sort_by(compare_scored_hits);
}

pub(super) async fn rerank_with_embeddings(
    runtime: &KhiveRuntime,
    query: &str,
    query_embedding: &mut QueryEmbeddingCache,
    hits: &mut [ScoredHit],
    alpha: f32,
) -> Result<bool, RuntimeError> {
    if hits.is_empty() {
        return Ok(false);
    }
    let texts: Vec<String> = hits
        .iter()
        .map(|h| format!("{} {}", h.name, h.content.as_deref().unwrap_or("")))
        .collect();
    if let Some(cosines) = embed_cosine_scores(runtime, query, query_embedding, &texts).await? {
        blend_and_rank(hits, cosines, alpha);
        return Ok(true);
    }
    Ok(false)
}

/// Search uses the same document-intent vectors as knowledge indexing. The
/// older generic reranker remains for suggest, whose candidates are domains.
pub(super) async fn rerank_search_with_stored_vectors(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    namespace: &str,
    query: &str,
    query_embedding: &mut QueryEmbeddingCache,
    hits: &mut [ScoredHit],
    alpha: f32,
) -> Result<Option<Value>, RuntimeError> {
    if hits.is_empty() {
        return Ok(None);
    }
    if query_embedding.role_specific.is_not_attempted() {
        query_embedding.role_specific = match khive_storage::await_request_read_phase(
            "knowledge.embedding_rerank",
            runtime.embed_query(query),
        )
        .await
        {
            Ok(Ok(vector)) => RoleSpecificEmbedding::Vector(vector),
            Ok(Err(_)) => RoleSpecificEmbedding::Failed,
            Err(error) if is_timeout(&error) => RoleSpecificEmbedding::Failed,
            Err(error) => return Err(error.into()),
        };
    }
    let Some(query_vector) = query_embedding.role_specific.as_deref() else {
        return Ok(None);
    };
    let store = match runtime.vectors_for_model(token, runtime.default_embedder_name()) {
        Ok(store) => Some(store),
        Err(RuntimeError::Storage(khive_storage::StorageError::Unsupported { .. })) => None,
        Err(error) => return Err(error),
    };

    rerank_search_from_store(
        runtime,
        store.as_deref(),
        namespace,
        query_vector,
        hits,
        alpha,
    )
    .await
}

/// Kept separate from store resolution so a backend without by-ID reads can
/// exercise the fallback and its response provenance in focused tests.
pub(super) async fn rerank_search_from_store(
    runtime: &KhiveRuntime,
    store: Option<&dyn khive_storage::VectorStore>,
    namespace: &str,
    query_vector: &[f32],
    hits: &mut [ScoredHit],
    alpha: f32,
) -> Result<Option<Value>, RuntimeError> {
    let candidate_ids: Vec<Uuid> = hits
        .iter()
        .filter_map(|hit| Uuid::parse_str(&hit.id).ok())
        .collect();
    let mut stored_vectors = HashMap::new();
    let mut stored_vector_lookup = "unsupported";
    if let Some(store) = store {
        if store.capabilities().supports_vector_read {
            stored_vector_lookup = "supported";
            if !candidate_ids.is_empty() {
                stored_vectors = match khive_storage::await_request_read_phase(
                    "knowledge.embedding_rerank.vector_read",
                    store.get_vectors(&candidate_ids, namespace, "knowledge.atom"),
                )
                .await
                {
                    Ok(Ok(vectors)) => vectors,
                    Ok(Err(khive_storage::StorageError::Unsupported { .. })) => {
                        stored_vector_lookup = "unsupported";
                        HashMap::new()
                    }
                    Ok(Err(error)) => return Err(error.into()),
                    Err(error) if is_timeout(&error) => return Ok(None),
                    Err(error) => return Err(error.into()),
                };
            }
        }
    }

    let mut fallback_indices = Vec::new();
    let mut fallback_texts = Vec::new();
    let mut candidate_vectors = vec![None; hits.len()];
    for (index, hit) in hits.iter().enumerate() {
        let stored = Uuid::parse_str(&hit.id)
            .ok()
            .and_then(|id| stored_vectors.get(&id));
        if let Some(vector) = stored {
            candidate_vectors[index] = Some(vector.clone());
            continue;
        }
        fallback_indices.push(index);
        fallback_texts.push(hit.atom_embed_text.clone().unwrap_or_else(|| {
            atom_embed_text_fields(
                &hit.name,
                hit.content.as_deref().unwrap_or(""),
                hit.tags.as_deref().unwrap_or("[]"),
            )
        }));
    }

    if !fallback_texts.is_empty() {
        let embedded = match khive_storage::await_request_read_phase(
            "knowledge.embedding_rerank.fallback",
            runtime.embed_document_batch_outcomes(&fallback_texts),
        )
        .await
        {
            Ok(Ok(outcomes)) => outcomes
                .into_iter()
                .map(|outcome| outcome.vector)
                .collect::<Vec<_>>(),
            Ok(Err(_)) => return Ok(None),
            Err(error) if is_timeout(&error) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if embedded.len() != fallback_indices.len() {
            return Ok(None);
        }
        for (index, vector) in fallback_indices.iter().copied().zip(embedded) {
            candidate_vectors[index] = Some(vector);
        }
    }

    let from_stored = hits.len() - fallback_indices.len();
    let cosines = candidate_vectors.into_iter().map(|vector| {
        let vector = vector.expect("every rerank candidate has a stored or fallback vector");
        cosine_similarity(query_vector, &vector, false)
    });
    blend_and_rank(hits, cosines, alpha);
    Ok(Some(json!({
        "stored_vector_lookup": stored_vector_lookup,
        "candidates": hits.len(),
        "from_stored": from_stored,
        "embedded_fallback": fallback_indices.len(),
    })))
}
