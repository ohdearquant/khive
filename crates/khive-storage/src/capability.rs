//! Storage capability surface identifiers.

/// Identifies which storage capability surface produced an error or is being queried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageCapability {
    Sql,
    Notes,
    Entities,
    Graph,
    Events,
    Vectors,
    Sparse,
    Text,
    /// Content-addressed binary object storage (`BlobStore`, khive#292).
    Blob,
    /// Role-keyed blob references attached to entities or notes.
    Attachments,
}

#[cfg(test)]
#[path = "capability/tests.rs"]
mod tests;
