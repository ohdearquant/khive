//! Operation storage. Definitions, operations, records and slot decisions are append-only: every
//! table refuses
//! an update, a delete and an insert over an existing key, so a pinned definition and a recorded
//! fact read the same forever. An operation never lives in the namespace that holds its type.

/// One entry per table. Each entry opens with its table's `CREATE TABLE` and carries that
/// table's triggers: the pack-schema collision check reads the table name from the first
/// statement of an entry.
pub(crate) const OPERATION_SCHEMA_STATEMENTS: &[&str] = &[
    include_str!("../sql/001-operation-definitions.sql"),
    include_str!("../sql/002-operations.sql"),
    include_str!("../sql/003-operation-records.sql"),
    include_str!("../sql/004-operation-slot-decisions.sql"),
];
