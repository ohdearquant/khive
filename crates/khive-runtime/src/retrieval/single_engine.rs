use khive_storage::{StorageCapability, StorageError, VectorSearchHit, VectorSearchRequest};
use khive_types::SubstrateKind;
use uuid::Uuid;

use crate::{KhiveRuntime, NamespaceToken, RuntimeError, RuntimeResult};

impl KhiveRuntime {
    /// Search one configured embedding space using a raw vector or query text.
    ///
    /// This compatibility API selects the configured default (the first configured
    /// peer). It never infers a space from vector dimensions or falls through to
    /// another engine. Use [`Self::vector_search_in`] to name the space explicitly.
    pub async fn vector_search(
        &self,
        token: &NamespaceToken,
        query_embedding: Option<Vec<f32>>,
        query_text: Option<&str>,
        top_k: u32,
        kind: Option<SubstrateKind>,
    ) -> RuntimeResult<Vec<VectorSearchHit>> {
        validate_query_input(query_embedding.as_deref(), query_text)?;
        let engine = self.require_default_embedder()?;
        self.vector_search_in(token, engine, query_embedding, query_text, top_k, kind)
            .await
    }

    /// Search exactly the named engine's space, even when another engine has the
    /// same dimensions. Built-in aliases resolve to their canonical model name.
    ///
    /// A supplied vector avoids embedding. Otherwise the named query embedder is
    /// invoked with the caller's token. Unknown/excluded engines, wrong dimensions
    /// and nonfinite coordinates refuse even when `top_k` is zero.
    // REASON: preserve vector_search's arguments while adding an explicit engine identity.
    #[allow(clippy::too_many_arguments)]
    pub async fn vector_search_in(
        &self,
        token: &NamespaceToken,
        engine: &str,
        query_embedding: Option<Vec<f32>>,
        query_text: Option<&str>,
        top_k: u32,
        kind: Option<SubstrateKind>,
    ) -> RuntimeResult<Vec<VectorSearchHit>> {
        validate_query_input(query_embedding.as_deref(), query_text)?;
        let (engine, dimensions) = self.vector_model_metadata(engine)?;
        let embedding = match query_embedding {
            Some(vector) => vector,
            None => {
                let text = query_text.ok_or_else(missing_query)?;
                self.embed_query_with_model_for_token(token, &engine, text)
                    .await?
            }
        };
        validate_vector(&engine, dimensions, &embedding, "vec_search")?;
        let hits = self
            .vectors_for_model(token, &engine)?
            .search(VectorSearchRequest {
                query_vectors: vec![embedding],
                top_k,
                namespace: Some(token.namespace().as_str().to_owned()),
                kind,
                embedding_model: None,
                filter: None,
                backend_hints: None,
            })
            .await;
        crate::usage::count(crate::usage::UsageUnit::VectorPasses, 1);
        hits.map_err(RuntimeError::from)
    }

    /// Exact entity KNN in the configured default (first configured peer) space.
    /// Use [`Self::knn_in`] when the query vector belongs to a named engine.
    pub async fn knn(
        &self,
        token: &NamespaceToken,
        query_vector: Vec<f32>,
        top_k: u32,
    ) -> RuntimeResult<Vec<VectorSearchHit>> {
        let engine = self.require_default_embedder()?;
        self.knn_in(token, engine, query_vector, top_k).await
    }

    /// Exact entity KNN in the named engine's space and token namespace.
    /// Unknown/excluded names and invalid vectors refuse before search, including
    /// zero-result requests. No embedding service is instantiated for this API.
    pub async fn knn_in(
        &self,
        token: &NamespaceToken,
        engine: &str,
        query_vector: Vec<f32>,
        top_k: u32,
    ) -> RuntimeResult<Vec<VectorSearchHit>> {
        let (engine, dimensions) = self.vector_model_metadata(engine)?;
        validate_vector(&engine, dimensions, &query_vector, "vec_search")?;
        Ok(self
            .vectors_for_model(token, &engine)?
            .search(VectorSearchRequest {
                query_vectors: vec![query_vector],
                top_k,
                namespace: Some(token.namespace().as_str().to_owned()),
                kind: Some(SubstrateKind::Entity),
                embedding_model: None,
                filter: None,
                backend_hints: None,
            })
            .await?)
    }

    /// Score only the supplied entity candidates in the configured default
    /// (first configured peer) space. Use [`Self::rerank_in`] for a named space.
    pub async fn rerank(
        &self,
        token: &NamespaceToken,
        query_vector: &[f32],
        candidate_ids: &[Uuid],
        top_k: u32,
    ) -> RuntimeResult<Vec<VectorSearchHit>> {
        let engine = self.require_default_embedder()?;
        self.rerank_in(token, engine, query_vector, candidate_ids, top_k)
            .await
    }

    /// Score only the supplied entity candidates in the named engine's space.
    ///
    /// Missing or out-of-scope vectors are omitted; equal scores use ascending
    /// IDs and duplicate IDs yield one hit. The result limit applies after all
    /// candidates are scored. Unsupported storage propagates its capability error
    /// without falling back to namespace-wide search. Engine identity, dimensions
    /// and finite coordinates are checked even for empty candidates or zero limit.
    pub async fn rerank_in(
        &self,
        token: &NamespaceToken,
        engine: &str,
        query_vector: &[f32],
        candidate_ids: &[Uuid],
        top_k: u32,
    ) -> RuntimeResult<Vec<VectorSearchHit>> {
        let (engine, dimensions) = self.vector_model_metadata(engine)?;
        validate_vector(&engine, dimensions, query_vector, "score_candidates")?;
        let mut hits = self
            .vectors_for_model(token, &engine)?
            .score_candidates(query_vector, candidate_ids, Some(SubstrateKind::Entity))
            .await?;
        hits.truncate(top_k as usize);
        Ok(hits)
    }
}

fn missing_query() -> RuntimeError {
    RuntimeError::InvalidInput("vector search requires query_embedding or query_text".into())
}

fn validate_query_input(vector: Option<&[f32]>, text: Option<&str>) -> RuntimeResult<()> {
    if vector.is_none() {
        let text = text.ok_or_else(missing_query)?;
        if text.trim().is_empty() {
            return Err(RuntimeError::InvalidInput(
                "query_text must not be empty".into(),
            ));
        }
    }
    Ok(())
}

fn validate_vector(
    engine: &str,
    dimensions: usize,
    vector: &[f32],
    operation: &'static str,
) -> RuntimeResult<()> {
    let message = if vector.len() != dimensions {
        Some(format!(
            "engine '{engine}' query has {} dims, expected {dimensions}",
            vector.len()
        ))
    } else {
        vector
            .iter()
            .position(|coordinate| !coordinate.is_finite())
            .map(|index| {
                format!("engine '{engine}' query has a nonfinite coordinate at index {index}")
            })
    };
    if let Some(message) = message {
        return Err(StorageError::InvalidInput {
            capability: StorageCapability::Vectors,
            operation: operation.into(),
            message,
        }
        .into());
    }
    Ok(())
}
