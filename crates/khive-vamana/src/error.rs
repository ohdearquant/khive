//! Error types for the Vamana ANN index crate.

/// Convenience alias for `Result<T, VamanaError>`.
pub type Result<T> = std::result::Result<T, VamanaError>;

/// All error conditions returned by the Vamana ANN index.
#[derive(thiserror::Error, Debug)]
pub enum VamanaError {
    /// Vector dimensionality does not match the index or config.
    #[error("dimension mismatch: expected {expected}, got {actual}")]
    DimensionMismatch { expected: usize, actual: usize },

    /// An empty vector or query slice was supplied where content is required.
    #[error("input vectors must not be empty")]
    EmptyInput,

    /// A configuration parameter violates an invariant (e.g. `alpha < 1.0`).
    #[error("invalid config: {reason}")]
    InvalidConfig { reason: String },

    /// A serialized or loaded index file is structurally invalid.
    #[error("invalid index file: {reason}")]
    InvalidFormat { reason: String },

    /// The corpus exceeds the `u32` node-ID limit.
    #[error("too many vectors for u32 node IDs: {count}")]
    TooManyVectors { count: usize },

    /// An I/O error occurred during save or load.
    #[error("io error: {source}")]
    Io {
        #[from]
        source: std::io::Error,
    },

    /// A supplied vector slice contains non-finite values (`NaN` or `Infinity`).
    #[error("non-finite float in {location}: {detail}")]
    NonFiniteFloat { location: String, detail: String },
}

impl VamanaError {
    /// Construct an `InvalidConfig` error with the given reason string.
    pub fn invalid_config(reason: String) -> Self {
        Self::InvalidConfig { reason }
    }

    /// Construct an `InvalidFormat` error with the given reason string.
    pub fn invalid_format(reason: String) -> Self {
        Self::InvalidFormat { reason }
    }

    /// Construct a `NonFiniteFloat` error identifying where the bad value appeared.
    pub fn non_finite(location: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::NonFiniteFloat {
            location: location.into(),
            detail: detail.into(),
        }
    }
}
