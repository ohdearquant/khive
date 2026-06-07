//! Storage error types shared across all backend implementations.

use std::borrow::Cow;
use std::error::Error as StdError;

use thiserror::Error;

use crate::capability::StorageCapability;

/// Unified error type for all storage operations.
#[derive(Debug, Error)]
pub enum StorageError {
    #[error("{capability:?} resource not found: {resource} ({key})")]
    NotFound {
        capability: StorageCapability,
        resource: &'static str,
        key: String,
    },

    #[error("{capability:?} resource already exists: {resource} ({key})")]
    AlreadyExists {
        capability: StorageCapability,
        resource: &'static str,
        key: String,
    },

    #[error("conflict in {capability:?} during {operation}: {message}")]
    Conflict {
        capability: StorageCapability,
        operation: Cow<'static, str>,
        message: String,
    },

    #[error("invalid input for {capability:?} during {operation}: {message}")]
    InvalidInput {
        capability: StorageCapability,
        operation: Cow<'static, str>,
        message: String,
    },

    #[error("unsupported operation for {capability:?}: {operation} ({message})")]
    Unsupported {
        capability: StorageCapability,
        operation: Cow<'static, str>,
        message: String,
    },

    #[error("pool failure during {operation}: {message}")]
    Pool {
        operation: Cow<'static, str>,
        message: String,
    },

    #[error("timeout during {operation}")]
    Timeout { operation: Cow<'static, str> },

    #[error("sql transaction failure during {operation}: {message}")]
    Transaction {
        operation: Cow<'static, str>,
        message: String,
    },

    #[error("serialization failure in {capability:?}: {message}")]
    Serialization {
        capability: StorageCapability,
        message: String,
    },

    #[error("index maintenance failure in {capability:?}: {message}")]
    IndexMaintenance {
        capability: StorageCapability,
        message: String,
    },

    #[error("backend driver error in {capability:?} during {operation}: {source}")]
    Driver {
        capability: StorageCapability,
        operation: Cow<'static, str>,
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
}

impl StorageError {
    /// Construct a `Driver` error wrapping a backend-specific error source.
    pub fn driver(
        capability: StorageCapability,
        operation: impl Into<Cow<'static, str>>,
        source: impl StdError + Send + Sync + 'static,
    ) -> Self {
        Self::Driver {
            capability,
            operation: operation.into(),
            source: Box::new(source),
        }
    }

    /// Return the storage capability surface that produced this error, if any.
    pub fn capability(&self) -> Option<StorageCapability> {
        match self {
            Self::NotFound { capability, .. }
            | Self::AlreadyExists { capability, .. }
            | Self::Conflict { capability, .. }
            | Self::InvalidInput { capability, .. }
            | Self::Unsupported { capability, .. }
            | Self::Serialization { capability, .. }
            | Self::IndexMaintenance { capability, .. }
            | Self::Driver { capability, .. } => Some(*capability),
            Self::Pool { .. } | Self::Timeout { .. } | Self::Transaction { .. } => None,
        }
    }

    /// Whether this error is transient and the operation may succeed on retry.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Pool { .. } | Self::Timeout { .. } | Self::Transaction { .. }
        )
    }
}
