//! Native cross-encoder composition over live entity text.

use std::sync::Arc;

use async_trait::async_trait;
use khive_retrieval::error::{Result, RetrievalError};
use khive_retrieval::{
    NativeCrossEncoderModel, NativeCrossEncoderReranker, RerankDocumentResolver,
};
use uuid::Uuid;

use crate::{KhiveRuntime, NamespaceToken};

/// Resolves live entities to the canonical name-plus-description retrieval text.
///
/// Reads use the bound runtime and caller token and preserve the store's by-ID
/// namespace contract. Missing/deleted entities yield `None`; storage errors
/// fail resolution. Redirects and attachments are not followed. Input order and
/// duplicate IDs are preserved.
#[derive(Clone)]
pub struct RuntimeRerankDocumentResolver {
    runtime: Arc<KhiveRuntime>,
    token: NamespaceToken,
}

impl RuntimeRerankDocumentResolver {
    /// Bind entity resolution to a runtime and caller token.
    pub fn new(runtime: Arc<KhiveRuntime>, token: NamespaceToken) -> Self {
        Self { runtime, token }
    }
}

#[async_trait]
impl RerankDocumentResolver<Uuid> for RuntimeRerankDocumentResolver {
    async fn resolve_documents(&self, ids: &[Uuid]) -> Result<Vec<Option<String>>> {
        let store = self
            .runtime
            .entities(&self.token)
            .map_err(|error| RetrievalError::rerank(error.to_string()))?;
        let mut documents = Vec::with_capacity(ids.len());
        for id in ids {
            let entity = store
                .get_entity(*id)
                .await
                .map_err(|error| RetrievalError::rerank(error.to_string()))?;
            documents.push(entity.as_ref().map(crate::curation::entity_embedding_text));
        }
        Ok(documents)
    }
}

impl KhiveRuntime {
    /// Bind a loaded native model to this runtime's live entity resolver.
    ///
    /// Scores are the pinned model's sigmoid probabilities, not raw logits;
    /// finite precision and saturation can produce ties. Missing entities are
    /// dropped before the requested result limit. This does not enable native
    /// reranking in any pack's search or recall pipeline.
    pub fn native_cross_encoder_reranker(
        self: &Arc<Self>,
        token: NamespaceToken,
        model: Arc<NativeCrossEncoderModel>,
    ) -> NativeCrossEncoderReranker<Uuid, RuntimeRerankDocumentResolver, NativeCrossEncoderModel>
    {
        NativeCrossEncoderReranker::new(
            model,
            Arc::new(RuntimeRerankDocumentResolver::new(self.clone(), token)),
        )
    }
}
