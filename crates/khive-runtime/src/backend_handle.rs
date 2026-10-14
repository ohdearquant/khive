//! Required core capabilities and optional, per-binding retrieval factories (ADR-071).

use std::fmt;
use std::sync::Arc;

use khive_db::StorageBackend;
use khive_storage::{
    EntityStore, EventStore, GraphStore, NoteStore, SparseStore, SparseStoreFactory, SqlAccess,
    StorageError, TextSearch, TextSearchFactory, VectorStore, VectorStoreFactory,
};
use thiserror::Error;

/// An optional retrieval capability required by an operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetrievalTier {
    Vector,
    Sparse,
    Text,
}

impl fmt::Display for RetrievalTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Vector => "vector",
            Self::Sparse => "sparse",
            Self::Text => "text",
        })
    }
}

/// Failure to acquire one bound retrieval capability.
#[derive(Debug, Error)]
pub enum BackendHandleError {
    #[error("backend does not provide the {tier} capability")]
    MissingCapability { tier: RetrievalTier },
    #[error(transparent)]
    Storage(#[from] StorageError),
}

/// Backend-neutral handles with lazy SQLite readiness and per-operation retrieval bindings.
#[derive(Clone)]
pub struct BackendHandle {
    entity: Arc<dyn EntityStore>,
    note: Arc<dyn NoteStore>,
    graph: Arc<dyn GraphStore>,
    event: Arc<dyn EventStore>,
    sql: Arc<dyn SqlAccess>,
    vector: Option<Arc<dyn VectorStoreFactory>>,
    sparse: Option<Arc<dyn SparseStoreFactory>>,
    text: Option<Arc<dyn TextSearchFactory>>,
}

impl BackendHandle {
    /// Construction performs no storage operation; readiness errors occur at first use.
    pub fn from_sqlite(backend: Arc<StorageBackend>) -> Self {
        Self {
            entity: backend.clone(),
            note: backend.clone(),
            graph: backend.clone(),
            event: backend.clone(),
            sql: backend.clone(),
            vector: Some(backend.clone()),
            sparse: Some(backend.clone()),
            text: Some(backend),
        }
    }

    /// Retain the supplied core handles and independently optional retrieval factories.
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        entity: Arc<dyn EntityStore>,
        note: Arc<dyn NoteStore>,
        graph: Arc<dyn GraphStore>,
        event: Arc<dyn EventStore>,
        sql: Arc<dyn SqlAccess>,
        vector: Option<Arc<dyn VectorStoreFactory>>,
        sparse: Option<Arc<dyn SparseStoreFactory>>,
        text: Option<Arc<dyn TextSearchFactory>>,
    ) -> Self {
        Self {
            entity,
            note,
            graph,
            event,
            sql,
            vector,
            sparse,
            text,
        }
    }

    pub fn entity(&self) -> &Arc<dyn EntityStore> {
        &self.entity
    }

    pub fn note(&self) -> &Arc<dyn NoteStore> {
        &self.note
    }

    pub fn graph(&self) -> &Arc<dyn GraphStore> {
        &self.graph
    }

    pub fn event(&self) -> &Arc<dyn EventStore> {
        &self.event
    }

    pub fn sql(&self) -> &Arc<dyn SqlAccess> {
        &self.sql
    }

    pub fn vector(
        &self,
        model_key: &str,
        embedding_model: &str,
        dimensions: usize,
        namespace: &str,
    ) -> Result<Arc<dyn VectorStore>, BackendHandleError> {
        self.vector
            .as_ref()
            .ok_or(BackendHandleError::MissingCapability {
                tier: RetrievalTier::Vector,
            })?
            .open(model_key, embedding_model, dimensions, namespace)
            .map_err(BackendHandleError::Storage)
    }

    pub fn sparse(
        &self,
        model_key: &str,
        namespace: &str,
    ) -> Result<Arc<dyn SparseStore>, BackendHandleError> {
        self.sparse
            .as_ref()
            .ok_or(BackendHandleError::MissingCapability {
                tier: RetrievalTier::Sparse,
            })?
            .open(model_key, namespace)
            .map_err(BackendHandleError::Storage)
    }

    pub fn text(
        &self,
        table_key: &str,
        tokenizer: &str,
    ) -> Result<Arc<dyn TextSearch>, BackendHandleError> {
        self.text
            .as_ref()
            .ok_or(BackendHandleError::MissingCapability {
                tier: RetrievalTier::Text,
            })?
            .open(table_key, tokenizer)
            .map_err(BackendHandleError::Storage)
    }
}
