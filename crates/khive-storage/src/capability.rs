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
}
