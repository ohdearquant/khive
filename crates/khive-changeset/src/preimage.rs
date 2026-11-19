//! Field-scoped comparison against the record visible to an applying writer.

use khive_types::{Entity, Header, Link, Note};

use crate::{
    DeleteOp, DeletePreimage, EdgePreimage, EntityPreimage, NotePreimage, UpdateOp, UpdatePreimage,
};

#[derive(Clone, Copy, Debug)]
/// A caller-supplied record from the applying transaction, without a storage handle.
pub enum CanonicalRecordRef<'a> {
    Entity(&'a Entity),
    Note(&'a Note),
    Edge(&'a Link),
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// A stable field path without captured or current record contents.
pub struct PreimageMismatch {
    pub field_path: String,
}

impl std::fmt::Display for PreimageMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "captured preimage differs at {}", self.field_path)
    }
}

impl std::error::Error for PreimageMismatch {}

fn equal<T: PartialEq + ?Sized>(
    field: &str,
    captured: &T,
    live: &T,
) -> Result<(), PreimageMismatch> {
    if captured == live {
        Ok(())
    } else {
        Err(PreimageMismatch {
            field_path: field.into(),
        })
    }
}

fn touched<T: PartialEq>(
    field: &str,
    captured: &Option<T>,
    live: &T,
) -> Result<(), PreimageMismatch> {
    match captured {
        Some(captured) => equal(field, captured, live),
        None => Ok(()),
    }
}

/// The caller must read `live` inside the same transaction that applies `op`.
pub fn check_update(op: &UpdateOp, live: CanonicalRecordRef<'_>) -> Result<(), PreimageMismatch> {
    match (op.preimage(), live) {
        (UpdatePreimage::Entity(preimage), CanonicalRecordRef::Entity(live)) => {
            equal("id", &op.target_id(), &live.header.id)?;
            let EntityPreimage {
                name,
                description,
                properties,
                tags,
                entity_type,
            } = preimage;
            touched("name", name, &live.name)?;
            touched("description", description, &live.description)?;
            touched("properties", properties, &live.properties)?;
            touched("tags", tags, &live.tags)?;
            touched("entity_type", entity_type, &live.entity_type)
        }
        (UpdatePreimage::Note(preimage), CanonicalRecordRef::Note(live)) => {
            equal("id", &op.target_id(), &live.header.id)?;
            let NotePreimage {
                content,
                salience,
                decay_factor,
                properties,
                tags,
            } = preimage;
            touched("content", content, &live.content)?;
            touched("salience", salience, &live.salience)?;
            touched("decay_factor", decay_factor, &live.decay_factor)?;
            touched("properties", properties, &live.properties)?;
            touched("tags", tags, &live.tags)
        }
        (UpdatePreimage::Edge(preimage), CanonicalRecordRef::Edge(live)) => {
            equal("id", &op.target_id(), &live.id)?;
            let EdgePreimage { relation, weight } = preimage;
            touched("relation", relation, &live.relation)?;
            touched("weight", weight, &live.weight)
        }
        _ => Err(PreimageMismatch {
            field_path: "substrate".into(),
        }),
    }
}

fn header(captured: &Header, live: &Header) -> Result<(), PreimageMismatch> {
    let Header {
        id,
        namespace,
        created_at,
        updated_at: _,
    } = captured;
    equal("id", id, &live.id)?;
    equal("namespace", namespace, &live.namespace)?;
    equal("created_at", created_at, &live.created_at)
}

/// The captured record is exact except for its applier-stamped `updated_at`.
pub fn check_delete(op: &DeleteOp, live: CanonicalRecordRef<'_>) -> Result<(), PreimageMismatch> {
    match (&op.preimage, live) {
        (DeletePreimage::Entity(captured), CanonicalRecordRef::Entity(live)) => {
            equal("id", &op.target_id, &live.header.id)?;
            let Entity {
                header: captured_header,
                kind,
                entity_type,
                name,
                description,
                properties,
                tags,
                deleted_at,
            } = captured.as_ref();
            header(captured_header, &live.header)?;
            equal("kind", kind, &live.kind)?;
            equal("entity_type", entity_type, &live.entity_type)?;
            equal("name", name, &live.name)?;
            equal("description", description, &live.description)?;
            equal("properties", properties, &live.properties)?;
            equal("tags", tags, &live.tags)?;
            equal("deleted_at", deleted_at, &live.deleted_at)
        }
        (DeletePreimage::Note(captured), CanonicalRecordRef::Note(live)) => {
            equal("id", &op.target_id, &live.header.id)?;
            let Note {
                header: captured_header,
                kind,
                status,
                content,
                properties,
                tags,
                salience,
                decay_factor,
                expires_at,
                deleted_at,
            } = captured.as_ref();
            header(captured_header, &live.header)?;
            equal("kind", kind, &live.kind)?;
            equal("status", status, &live.status)?;
            equal("content", content, &live.content)?;
            equal("properties", properties, &live.properties)?;
            equal("tags", tags, &live.tags)?;
            equal("salience", salience, &live.salience)?;
            equal("decay_factor", decay_factor, &live.decay_factor)?;
            equal("expires_at", expires_at, &live.expires_at)?;
            equal("deleted_at", deleted_at, &live.deleted_at)
        }
        (DeletePreimage::Edge(captured), CanonicalRecordRef::Edge(live)) => {
            equal("id", &op.target_id, &live.id)?;
            let Link {
                id,
                namespace,
                source,
                target,
                relation,
                properties,
                weight,
                created_at,
                updated_at: _,
                deleted_at,
            } = captured.as_ref();
            equal("id", id, &live.id)?;
            equal("namespace", namespace, &live.namespace)?;
            equal("source", source, &live.source)?;
            equal("target", target, &live.target)?;
            equal("relation", relation, &live.relation)?;
            equal("properties", properties, &live.properties)?;
            equal("weight", weight, &live.weight)?;
            equal("created_at", created_at, &live.created_at)?;
            equal("deleted_at", deleted_at, &live.deleted_at)
        }
        _ => Err(PreimageMismatch {
            field_path: "substrate".into(),
        }),
    }
}

#[cfg(test)]
#[path = "preimage_tests.rs"]
mod tests;
