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

pub(crate) const CHARTER_SCHEMA_STATEMENTS: &[&str] =
    &[include_str!("../sql/001-charter-foundation.sql")];
