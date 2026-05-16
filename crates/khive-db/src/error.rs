use thiserror::Error;

#[derive(Debug, Error)]
pub enum SqliteError {
    #[error("sqlite error: {0}")]
    Rusqlite(#[from] rusqlite::Error),

    #[error("invalid data: {0}")]
    InvalidData(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("migration v{version} failed: {error}")]
    Migration { version: u32, error: String },
}
