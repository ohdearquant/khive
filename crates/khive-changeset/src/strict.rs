//! Strict, `deny_unknown_fields` wire-shape mirrors of the three
//! `khive_types` record shapes a preimage can embed. `Entity`, `Note`, and
//! `Link`'s own `Deserialize` impls accept unknown fields, which would let a
//! preimage with an extraneous or misspelled key round-trip silently — this
//! module closes that gap without editing `khive-types` itself. Conversion
//! back into the real types reuses each type's own validation
//! (`Link::is_valid`, `Note::is_valid`) rather than duplicating range checks.

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
mod tests {
    use super::*;

    #[test]
    fn strict_entity_rejects_unknown_field() {
        let json = serde_json::json!({
            "id": "00000000-0000-0000-0000-000000000001",
            "namespace": "local",
            "created_at": 1,
            "updated_at": 1,
            "kind": "concept",
            "entity_type": null,
            "name": "x",
            "description": null,
            "properties": {},
            "tags": [],
            "deleted_at": null,
            "unexpected": true
        });
        let result: Result<StrictEntity, _> = serde_json::from_value(json);
        assert!(result.is_err());
    }

    #[test]
    fn strict_link_rejects_out_of_range_weight() {
        let json = serde_json::json!({
            "id": "00000000-0000-0000-0000-000000000001",
            "namespace": "local",
            "source": "00000000-0000-0000-0000-000000000002",
            "target": "00000000-0000-0000-0000-000000000003",
            "relation": "extends",
            "properties": {},
            "weight": 1.5,
            "created_at": 1,
            "updated_at": 1,
            "deleted_at": null
        });
        let strict: StrictLink = serde_json::from_value(json).unwrap();
        let result: Result<Link, _> = strict.try_into();
        assert!(result.is_err());
    }

    #[test]
    fn strict_note_rejects_out_of_range_salience() {
        let json = serde_json::json!({
            "id": "00000000-0000-0000-0000-000000000001",
            "namespace": "local",
            "created_at": 1,
            "updated_at": 1,
            "kind": "observation",
            "status": "active",
            "content": "x",
            "properties": {},
            "tags": [],
            "salience": 1.5,
            "decay_factor": null,
            "expires_at": null,
            "deleted_at": null
        });
        let strict: StrictNote = serde_json::from_value(json).unwrap();
        let result: Result<Note, _> = strict.try_into();
        assert!(result.is_err());
    }
}
