/// Errors produced by the query parsing and compilation pipeline.
#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error("parse error at position {position}: {message}")]
    Parse { position: usize, message: String },

    #[error("compile error: {0}")]
    Compile(String),

    #[error("validation error: {0}")]
    Validation(String),

    #[error("unsupported feature: {0}")]
    Unsupported(String),
}
