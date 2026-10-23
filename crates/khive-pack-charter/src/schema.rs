//! Recording-only charter storage. Activation, revocation and admission are unavailable.
//!
//! A subject is global to its subject kind and reference. Its
//! policy domain and namespace retain creation attribution; they confer no
//! authority and do not partition its single unresolved-attempt slot. Runs and
//! their evidence remain scoped to a policy domain and pinned definition.
//!
//! Command callers must use their canonical structural identity, not a display
//! label. Their request IDs are caller-scoped across domains and verbs. Exact
//! replay requires reading and validating the held immutable receipt before
//! attempting any insert; the pack's verbs do that, the schema does not.
//!
//! The evidence insert trigger enforces a dense per-run sequence, but transaction
//! owners must still append evidence and command receipts in the same atomic
//! unit as revisions. This schema alone does not enforce that unit. Definition
//! activation and revocation require separate records in a later migration.

/// One entry per table. Each entry opens with its table's `CREATE TABLE` and carries that
/// table's index and triggers: the pack-schema collision check reads the table name from the
/// first statement of an entry, so a table that does not open its own entry is not checked.
pub(crate) const CHARTER_SCHEMA_STATEMENTS: &[&str] = &[
    include_str!("../sql/001-charter-definitions.sql"),
    include_str!("../sql/002-charter-subjects.sql"),
    include_str!("../sql/003-charter-runs.sql"),
    include_str!("../sql/004-charter-phases.sql"),
    include_str!("../sql/005-charter-evidence.sql"),
    include_str!("../sql/006-charter-attempts.sql"),
    include_str!("../sql/007-charter-commands.sql"),
    include_str!("../sql/008-charter-vocabularies.sql"),
    include_str!("../sql/009-charter-waiting-changes.sql"),
];

#[cfg(test)]
#[path = "schema_tests.rs"]
mod tests;
