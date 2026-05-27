pub type Result<T> = std::result::Result<T, VamanaError>;

#[derive(thiserror::Error, Debug)]
pub enum VamanaError {
    #[error("dimension mismatch: expected {expected}, got {actual}")]
    DimensionMismatch { expected: usize, actual: usize },

    #[error("input vectors must not be empty")]
    EmptyInput,

    #[error("invalid config: {reason}")]
    InvalidConfig { reason: String },

    #[error("invalid index file: {reason}")]
    InvalidFormat { reason: String },

    #[error("too many vectors for u32 node IDs: {count}")]
    TooManyVectors { count: usize },

    #[error("io error: {source}")]
    Io {
        #[from]
        source: std::io::Error,
    },
}

impl VamanaError {
    pub fn invalid_config(reason: String) -> Self {
        Self::InvalidConfig { reason }
    }

    pub fn invalid_format(reason: String) -> Self {
        Self::InvalidFormat { reason }
    }
}
