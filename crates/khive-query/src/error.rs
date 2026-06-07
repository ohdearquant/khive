//! Query-layer error types for the GQL/SPARQL parse and compile pipeline.

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

    /// A query parameter value is out of the allowed range (e.g. max_hops exceeds
    /// the 10-hop depth cap, or a limit value overflows `i64`).
    #[error("invalid input: {0}")]
    InvalidInput(String),
}
