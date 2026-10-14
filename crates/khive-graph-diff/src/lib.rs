//! Deterministic, I/O-free graph-state differences (ADR-101 D5).
//!
//! Callers load three substrate-labelled NDJSON streams. This crate validates
//! record identities, then compares supplied fields without domain defaults or
//! schema interpretation. Nested JSON values change as whole top-level fields.

use std::collections::{BTreeMap, BTreeSet};

pub use khive_types::Id128;
use serde::Serialize;
use serde_json::Value;

mod parse;
pub use parse::DiffInputError;

/// Independently keyed record collections; equal IDs across substrates coexist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Substrate {
    Entity,
    Edge,
    Note,
}

impl Substrate {
    /// Identity field in the existing NDJSON representation.
    pub const fn identity_field(self) -> &'static str {
        match self {
            Self::Entity | Self::Note => "id",
            Self::Edge => "edge_id",
        }
    }
}

impl std::fmt::Display for Substrate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Entity => "entity",
            Self::Edge => "edge",
            Self::Note => "note",
        })
    }
}

/// Supplied fields other than the substrate's extracted identity.
pub type Fields = BTreeMap<String, Value>;
type Records = BTreeMap<Id128, Fields>;

/// Validated, privately held graph state. Construction performs no I/O.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphState {
    entities: Records,
    edges: Records,
    notes: Records,
}

impl GraphState {
    /// Parse already-loaded NDJSON. Empty/blank lines are ignored; errors carry
    /// one-based physical line numbers. Duplicate IDs within a substrate refuse.
    ///
    /// Entity/note IDs use `id`; edge IDs use `edge_id`. All other fields are
    /// retained, including unknown kinds/properties and explicitly null values.
    pub fn from_ndjson(entities: &str, edges: &str, notes: &str) -> Result<Self, DiffInputError> {
        Ok(Self {
            entities: parse::records(entities, Substrate::Entity)?,
            edges: parse::records(edges, Substrate::Edge)?,
            notes: parse::records(notes, Substrate::Note)?,
        })
    }
}

/// A complete added/removed record, with its identity represented exactly once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Record {
    pub id: Id128,
    pub fields: Fields,
}

/// One changed top-level field. Missing sides are omitted during serialization;
/// present JSON null is serialized explicitly. These DTOs are Serialize-only:
/// ordinary `Option<Value>` deserialization would conflate missing and null.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FieldChange {
    pub field: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<Value>,
}

/// A record whose supplied fields changed, ordered lexicographically by field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModifiedRecord {
    pub id: Id128,
    pub changes: Vec<FieldChange>,
}

/// Additions, removals and changes, each ordered by UUID ascending.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SubstrateDiff {
    pub added: Vec<Record>,
    pub removed: Vec<Record>,
    pub modified: Vec<ModifiedRecord>,
}

impl SubstrateDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.modified.is_empty()
    }
}

/// Structured diff over three independent substrate collections.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct GraphDiff {
    pub entities: SubstrateDiff,
    pub edges: SubstrateDiff,
    pub notes: SubstrateDiff,
}

impl GraphDiff {
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty() && self.edges.is_empty() && self.notes.is_empty()
    }
}

/// Compare validated graph states without mutation, I/O, time or randomness.
///
/// Identity aliases are normalized during parsing. Arrays remain ordered and
/// nested objects compare by their complete typed JSON value. No supplied
/// non-identity field is ignored or filled in from a runtime default.
pub fn diff(before: &GraphState, after: &GraphState) -> GraphDiff {
    GraphDiff {
        entities: diff_records(&before.entities, &after.entities),
        edges: diff_records(&before.edges, &after.edges),
        notes: diff_records(&before.notes, &after.notes),
    }
}

fn diff_records(before: &Records, after: &Records) -> SubstrateDiff {
    let mut result = SubstrateDiff::default();
    for (&id, fields) in before {
        let Some(next) = after.get(&id) else {
            result.removed.push(Record {
                id,
                fields: fields.clone(),
            });
            continue;
        };
        let keys: BTreeSet<_> = fields.keys().chain(next.keys()).collect();
        let changes: Vec<_> = keys
            .into_iter()
            .filter_map(|field| {
                let old = fields.get(field);
                let new = next.get(field);
                if old != new {
                    Some(FieldChange {
                        field: field.clone(),
                        before: old.cloned(),
                        after: new.cloned(),
                    })
                } else {
                    None
                }
            })
            .collect();
        if !changes.is_empty() {
            result.modified.push(ModifiedRecord { id, changes });
        }
    }
    for (&id, fields) in after {
        if !before.contains_key(&id) {
            result.added.push(Record {
                id,
                fields: fields.clone(),
            });
        }
    }
    result
}
