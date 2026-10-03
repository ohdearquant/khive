//! Durable ownership evidence captured from the database SQLite opened.

#[cfg(any(unix, windows))]
use crate::file_identity::DatabaseFileIdentity;

/// The stored database UUID paired with its opened physical file identity.
/// A copied database preserves the UUID but cannot preserve this owner pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DatabaseOwnerIdentity {
    pub(crate) durable_id: uuid::Uuid,
    #[cfg(any(unix, windows))]
    pub(crate) file_identity: DatabaseFileIdentity,
}

impl DatabaseOwnerIdentity {
    /// UUID read from the pool-installed `_khive_database_identity` singleton.
    pub fn durable_id(&self) -> uuid::Uuid {
        self.durable_id
    }

    /// The existing physical identity validated against SQLite's opened file.
    #[cfg(any(unix, windows))]
    pub fn file_identity(&self) -> DatabaseFileIdentity {
        self.file_identity
    }

    pub(crate) fn verify_owner(&self, expected: &Self) -> Result<(), DatabaseOwnerIdentityError> {
        if self != expected {
            return Err(DatabaseOwnerIdentityError::OwnerMismatch);
        }
        Ok(())
    }
}

/// A database cannot supply the required durable ownership evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DatabaseOwnerIdentityError {
    #[error("an in-memory database cannot own a durable filesystem root")]
    InMemory,
    #[error("the opened database has no installed durable identity; reopen after installation")]
    DurableIdentityUnavailable,
    #[error("the opened database has no validated physical file identity")]
    PhysicalIdentityUnavailable,
    #[error("physical database ownership identity is unsupported on this platform")]
    UnsupportedPlatform,
    #[error("the stored database identity or opened file identity does not match the owner")]
    OwnerMismatch,
}
