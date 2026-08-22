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
mod tests {
    use super::StorageCapability;

    #[test]
    fn additive_capabilities_preserve_the_public_discriminant_order() {
        assert_eq!(StorageCapability::Blob as usize, 8);
        assert_eq!(StorageCapability::Attachments as usize, 9);
    }
}
