//! Storage capability surface identifiers.

/// Identifies which storage capability surface produced an error or is being queried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageCapability {
    Sql,
    Notes,
    Vectors,
    Text,
    Graph,
    Event,
    Entities,
    Admin,
}
