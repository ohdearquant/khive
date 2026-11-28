//! Operation storage. Definitions, operations, records, slot decisions and source orders are
//! append-only: every table refuses
//! an update, a delete and an insert over an existing key, so a pinned definition and a recorded
//! fact read the same forever. An operation never lives in the namespace that holds its type.

/// One entry per table. Each entry opens with its table's `CREATE TABLE` and carries that
/// table's triggers: the pack-schema collision check reads the table name from the first
/// statement of an entry.
///
/// Every entry is applied with `CREATE ... IF NOT EXISTS` at each start and is never versioned,
/// so a table's columns and checks are fixed once a store has created it. A new kind of record
/// gets a table of its own: the source order is kept beside the definitions, not as a third
/// definition kind, because a store that already holds `operation_definitions` keeps its `kind`
/// check (ADR-222 A2.6).
pub(crate) const OPERATION_SCHEMA_STATEMENTS: &[&str] = &[
    include_str!("../sql/001-operation-definitions.sql"),
    include_str!("../sql/002-operations.sql"),
    include_str!("../sql/003-operation-records.sql"),
    include_str!("../sql/004-operation-slot-decisions.sql"),
    include_str!("../sql/005-operation-source-orders.sql"),
];
