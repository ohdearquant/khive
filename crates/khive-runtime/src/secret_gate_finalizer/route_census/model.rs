use std::collections::{BTreeMap, BTreeSet};

use super::super::declaration::Substrate;

pub(super) const STORE_WRITES: &[&str] = &[
    "upsert_entity",
    "upsert_entities",
    "upsert_entity_with_attachments",
    "insert_entity_if_absent",
    "replace_entity_if_unchanged",
    "upsert_note",
    "upsert_notes",
    "insert_note_if_absent",
    "try_insert_note",
    "try_insert_note_with_attachments",
    "replace_note_if_unchanged",
    "update_note_properties",
    "set_note_property",
    "try_patch_note_property",
    "patch_note_property_atomic",
    "try_patch_note_properties",
];

// The complement is explicit, including readers: a new trait method of any
// spelling makes the census red until its properties behavior is classified.
pub(super) const NON_PROPERTIES_STORE_METHODS: &[&str] = &[
    "get_entity",
    "delete_entity",
    "query_entities",
    "query_entities_count_free",
    "entity_sequence",
    "query_entities_after",
    "count_entities",
    "count_entities_by_type",
    "get_entity_including_deleted",
    "get_live_notes_by_key",
    "query_keyed_notes",
    "assign_note_key_if_unset",
    "get_note",
    "get_note_including_deleted",
    "delete_note",
    "query_notes",
    "query_notes_count_free",
    "query_notes_filtered",
    "query_notes_filtered_count_free",
    "count_notes_filtered_in_snapshot",
    "count_notes_filtered_bounded_in_snapshot",
    "note_sequence",
    "query_notes_filtered_after",
    "query_notes_filtered_bounded",
    "count_notes",
    "count_notes_in_namespaces",
    "get_notes_batch",
    "get_notes_batch_including_deleted",
    "get_note_visibility_batch",
];

pub(super) const ENTITY_BUILDERS: &[&str] = &[
    "entity_upsert_statement",
    "entity_insert_if_absent_statement",
    "entity_replace_if_unchanged_statement",
];
pub(super) const NOTE_BUILDERS: &[&str] = &[
    "note_insert_if_absent_statement",
    "note_insert_keyed_statement",
    "note_upsert_statement",
    "note_replace_if_unchanged_statement",
    "note_update_properties_statement",
    "note_set_property_statement",
];

// External references to these khive-db statements bypass the builder and
// store-method detectors. Both statements insert a whole note properties
// object, even when an ON CONFLICT clause does not replace an existing row.
pub(super) const NOTE_PROPERTY_SQL_CONSTANTS: &[&str] =
    &["NOTE_UPSERT_SQL", "NOTE_INSERT_IF_ABSENT_SQL"];

// Evidence for a site that reaches a note SQL constant only if a name that the
// census cannot classify is not a type. The census refuses it wherever it
// occurs, instead of routing or dropping it.
pub(super) const UNRESOLVED_CONSTANT: &str = "UNRESOLVED note SQL constant";

// Evidence prefix for a path that resolves to a note SQL constant.
pub(super) const SQL_CONSTANT_EVIDENCE: &str = "SQL constant ";

// Evidence prefix for a statement that writes a guarded table in a spelling the
// census cannot classify. The census refuses it wherever it occurs, because a
// spelling it cannot read is a route it cannot inventory.
pub(super) const UNCLASSIFIED_SQL: &str = "UNCLASSIFIED guarded-table SQL write";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DetectedClass {
    WholeObject,
    SingleKey(String),
    FixedKeySet(BTreeSet<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RouteClass {
    Application,
    Migration,
}

#[derive(Debug, Clone)]
pub(super) struct Site {
    pub(super) key: String,
    pub(super) target: Substrate,
    pub(super) route_class: RouteClass,
    pub(super) class: DetectedClass,
    pub(super) write_count: usize,
    pub(super) calls: BTreeSet<String>,
    pub(super) evidence: BTreeSet<String>,
}

#[derive(Debug, Clone)]
pub(super) struct RuntimeTableSite {
    pub(super) key: String,
    pub(super) write_count: usize,
    pub(super) calls: BTreeSet<String>,
    pub(super) evidence: BTreeSet<String>,
}

#[derive(Debug)]
pub(super) struct SourcePopulation {
    pub(super) properties: Vec<Site>,
    pub(super) runtime_tables: Vec<RuntimeTableSite>,
    /// SQL assets that a followed loader resolved, for the asset-side reach check.
    pub(super) resolved_sql: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SqlConstantReference {
    pub(super) site: String,
    pub(super) constant: String,
}

pub(super) struct ScannedSource {
    pub(super) sql_errors: Vec<String>,
    pub(super) resolved_sql: BTreeSet<String>,
    pub(super) sites: Vec<Site>,
    pub(super) runtime_tables: Vec<RuntimeTableSite>,
    /// Every expression path has an ordinal, whether or not it resolves. Both
    /// passes visit the same AST, so a sibling reference cannot stand in for it.
    pub(super) constant_references: BTreeMap<usize, SqlConstantReference>,
}
