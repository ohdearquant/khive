//! Runtime error types.

use thiserror::Error;

pub type RuntimeResult<T> = Result<T, RuntimeError>;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("storage: {0}")]
    Storage(#[from] khive_storage::StorageError),

    #[error("sqlite: {0}")]
    Sqlite(#[from] khive_db::SqliteError),

    #[error("query: {0}")]
    Query(#[from] khive_query::QueryError),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("unconfigured: {0} is not set")]
    Unconfigured(String),

    #[error("embedding: {0}")]
    Embedding(#[from] lattice_embed::EmbedError),
}
