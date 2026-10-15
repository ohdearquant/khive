//! Strict embedded-record wire mirrors that reject unknown preimage fields.
//! Conversion reuses the domain types' live-record validation.

use std::collections::BTreeMap;

use khive_types::{
    EdgeRelation, Entity, EntityKind, Header, Id128, Link, Namespace, Note, NoteStatus,
    PropertyValue, Timestamp,
};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StrictEntity {
    id: Id128,
    namespace: Namespace,
    created_at: Timestamp,
    updated_at: Timestamp,
    kind: EntityKind,
    #[serde(default)]
    entity_type: Option<String>,
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    properties: BTreeMap<String, PropertyValue>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    deleted_at: Option<Timestamp>,
}

impl From<StrictEntity> for Entity {
    fn from(s: StrictEntity) -> Self {
        Entity {
            header: Header {
                id: s.id,
                namespace: s.namespace,
                created_at: s.created_at,
                updated_at: s.updated_at,
            },
            kind: s.kind,
            entity_type: s.entity_type,
            name: s.name,
            description: s.description,
            properties: s.properties,
            tags: s.tags,
            deleted_at: s.deleted_at,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StrictNote {
    id: Id128,
    namespace: Namespace,
    created_at: Timestamp,
    updated_at: Timestamp,
    kind: String,
    status: NoteStatus,
    content: String,
    #[serde(default)]
    properties: BTreeMap<String, PropertyValue>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    salience: Option<f64>,
    #[serde(default)]
    decay_factor: Option<f64>,
    #[serde(default)]
    expires_at: Option<Timestamp>,
    #[serde(default)]
    deleted_at: Option<Timestamp>,
}

impl TryFrom<StrictNote> for Note {
    type Error = String;

    fn try_from(s: StrictNote) -> Result<Self, Self::Error> {
        let note = Note {
            header: Header {
                id: s.id,
                namespace: s.namespace,
                created_at: s.created_at,
                updated_at: s.updated_at,
            },
            kind: s.kind,
            status: s.status,
            content: s.content,
            properties: s.properties,
            tags: s.tags,
            salience: s.salience,
            decay_factor: s.decay_factor,
            expires_at: s.expires_at,
            deleted_at: s.deleted_at,
        };
        if !note.is_valid() {
            return Err(format!(
                "preimage note {} has an out-of-range salience or decay_factor",
                note.header.id
            ));
        }
        Ok(note)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StrictLink {
    id: Id128,
    namespace: String,
    source: Id128,
    target: Id128,
    relation: EdgeRelation,
    #[serde(default)]
    properties: BTreeMap<String, PropertyValue>,
    weight: f64,
    created_at: Timestamp,
    updated_at: Timestamp,
    #[serde(default)]
    deleted_at: Option<Timestamp>,
}

impl TryFrom<StrictLink> for Link {
    type Error = String;

    fn try_from(s: StrictLink) -> Result<Self, Self::Error> {
        let link = Link {
            id: s.id,
            namespace: s.namespace,
            source: s.source,
            target: s.target,
            relation: s.relation,
            properties: s.properties,
            weight: s.weight,
            created_at: s.created_at,
            updated_at: s.updated_at,
            deleted_at: s.deleted_at,
        };
        if !link.is_valid() {
            return Err(format!(
                "preimage edge {} has weight {} outside [0.0, 1.0]",
                link.id, link.weight
            ));
        }
        Ok(link)
    }
}

#[cfg(test)]
#[path = "strict_tests.rs"]
mod tests;
