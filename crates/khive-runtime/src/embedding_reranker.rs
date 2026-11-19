//! Token-scoped cosine reranking of entity candidates in one named engine.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use khive_retrieval::error::RetrievalError;
use khive_retrieval::hybrid::Reranker;
use khive_score::{cmp_desc_then_id, DeterministicScore};
use uuid::Uuid;

use crate::{KhiveRuntime, NamespaceToken, RuntimeError, RuntimeResult};

/// One input entry after cosine scoring or missing-vector fallback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EmbeddingRerankHit {
    pub id: Uuid,
    pub score: DeterministicScore,
    /// No entity vector was found in the selected engine and token namespace.
    /// This says nothing about vectors in other namespaces, engines or kinds.
    pub missing_vector: bool,
}

/// Cosine reranker bound to one runtime, caller token and named engine.
///
/// Query embedding uses the runtime's token-aware query-role path. Candidate
/// scoring uses [`KhiveRuntime::rerank_in`], without a namespace-wide KNN cut.
/// The signed canonical cosine score replaces each stored candidate's score;
/// missing vectors retain the exact incoming score instead.
///
/// [`Self::rerank_detailed`] exposes the fallback flag and typed runtime errors.
/// The [`Reranker`] implementation projects hits to tuples and maps failures to
/// [`RetrievalError::Rerank`], losing their typed causes and retry classification.
#[derive(Clone)]
pub struct EmbeddingCosineReranker {
    runtime: Arc<KhiveRuntime>,
    token: NamespaceToken,
    engine: String,
}

impl EmbeddingCosineReranker {
    /// Bind a registered engine, canonicalizing built-in aliases without loading
    /// its embedding service. Unknown or request-excluded engines are refused.
    pub fn new(
        runtime: Arc<KhiveRuntime>,
        token: NamespaceToken,
        engine: impl Into<String>,
    ) -> RuntimeResult<Self> {
        let engine = engine.into();
        let (engine, _) = runtime.vector_model_metadata(&engine)?;
        Ok(Self {
            runtime,
            token,
            engine,
        })
    }

    /// Score every supplied entry, retaining missing vectors and duplicate IDs.
    ///
    /// Vector reads deduplicate IDs; output does not. Missing duplicates each
    /// retain their own incoming score. Results sort by descending final score,
    /// ascending UUID, then original position for identical ID/score ties. The
    /// limit applies after scored and missing entries are merged.
    ///
    /// Engine and generated-vector validation still run for empty candidates or
    /// a zero limit. Embedding and storage failures fail the whole call, rather
    /// than marking candidates missing. More than `u32::MAX` distinct IDs refuse
    /// because the underlying scorer's result bound cannot represent that set.
    pub async fn rerank_detailed(
        &self,
        query: &str,
        results: Vec<(Uuid, DeterministicScore)>,
        top_k: usize,
    ) -> RuntimeResult<Vec<EmbeddingRerankHit>> {
        let ids: Vec<_> = results
            .iter()
            .map(|(id, _)| *id)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let count = u32::try_from(ids.len()).map_err(|_| {
            RuntimeError::InvalidInput("rerank candidate count exceeds u32::MAX".into())
        })?;
        let query_vector = self
            .runtime
            .embed_query_with_model_for_token(&self.token, &self.engine, query)
            .await?;
        let scores: BTreeMap<_, _> = self
            .runtime
            .rerank_in(&self.token, &self.engine, &query_vector, &ids, count)
            .await?
            .into_iter()
            .map(|hit| (hit.subject_id, hit.score))
            .collect();
        let mut hits: Vec<_> = results
            .into_iter()
            .enumerate()
            .map(|(position, (id, incoming_score))| {
                let score = scores.get(&id).copied();
                (
                    position,
                    EmbeddingRerankHit {
                        id,
                        score: score.unwrap_or(incoming_score),
                        missing_vector: score.is_none(),
                    },
                )
            })
            .collect();
        hits.sort_by(|(a_position, a), (b_position, b)| {
            cmp_desc_then_id(a.score, &a.id, b.score, &b.id)
                .then_with(|| a_position.cmp(b_position))
        });
        Ok(hits.into_iter().take(top_k).map(|(_, hit)| hit).collect())
    }
}

#[async_trait]
impl Reranker<Uuid> for EmbeddingCosineReranker {
    async fn rerank(
        &self,
        query: &str,
        results: Vec<(Uuid, DeterministicScore)>,
        top_k: usize,
    ) -> khive_retrieval::error::Result<Vec<(Uuid, DeterministicScore)>> {
        self.rerank_detailed(query, results, top_k)
            .await
            .map(|hits| hits.into_iter().map(|hit| (hit.id, hit.score)).collect())
            .map_err(|error| RetrievalError::Rerank(error.to_string()))
    }
}
