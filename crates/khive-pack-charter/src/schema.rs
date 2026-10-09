//! Recording-only charter storage. Activation, revocation and admission are unavailable.
//!
//! A subject is global to its physical forge/repository/PR/target tuple. Its
//! policy domain and namespace retain creation attribution; they confer no
//! authority and do not partition its single unresolved-attempt slot. Runs and
//! their evidence remain scoped to a policy domain and pinned definition.
//!
//! Command callers must use their canonical structural identity, not a display
//! label. Their request IDs are caller-scoped across domains and verbs. Exact
//! replay requires reading and validating the held immutable receipt before
//! attempting any insert; this schema does not implement replay handling.
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
];

#[cfg(test)]
mod tests {
    use super::CHARTER_SCHEMA_STATEMENTS;

    #[test]
    fn every_schema_entry_opens_with_its_one_table() {
        let mut tables = Vec::new();
        for entry in CHARTER_SCHEMA_STATEMENTS {
            let first = entry.lines().next().unwrap_or_default();
            assert!(
                first.starts_with("CREATE TABLE IF NOT EXISTS charter_"),
                "schema entry does not open with its table: {first}"
            );
            assert_eq!(
                entry.matches("CREATE TABLE").count(),
                1,
                "schema entry declares more than one table: {first}"
            );
            tables.push(first.split_whitespace().nth(5).unwrap_or_default());
        }
        tables.sort_unstable();
        tables.dedup();
        assert_eq!(tables.len(), 7, "{tables:?}");
    }
}
