//! Pure, stage-time inversion against caller-supplied current records.

use std::collections::{BTreeMap, BTreeSet};

use khive_types::{Entity, Id128, Link, Note, NoteStatus};

use crate::{
    ChangeSet, CreateOp, CreateTarget, DeleteOp, DeletePreimage, EdgePatch, EdgePreimage,
    EntityPatch, EntityPreimage, Envelope, LinkOp, NotePatch, NotePreimage, Op, RestoreOp,
    UpdateOp, UpdatePatch, UpdatePreimage,
};

/// Current records, including soft-deleted records, read when staging a revert.
///
/// Each key must equal its record's identifier. The caller supplies records in
/// the same field representation as the staged operations; inversion does not
/// normalize properties, tags, namespaces, or default values. Ordered maps keep
/// this input deterministic on native and wasm32 targets.
///
/// See `crates/khive-changeset/docs/api/invert.md` for caller responsibilities.
#[derive(Clone, Debug, Default)]
pub struct CurrentState {
    /// Current entity records, keyed by their header identifiers.
    pub entities: BTreeMap<Id128, Entity>,
    /// Current note records, keyed by their header identifiers.
    pub notes: BTreeMap<Id128, Note>,
    /// Current edge records, keyed by their edge identifiers.
    pub edges: BTreeMap<Id128, Link>,
}

/// A refusal to stage an inverse; no partially inverted change-set is returned.
///
/// Operation indexes are zero-based indexes in the original forward op-list.
#[derive(Debug, thiserror::Error)]
pub enum InvertError {
    /// A revert must have its own provenance rather than reuse the forward envelope.
    #[error("the inverse envelope must differ from the forward envelope")]
    ReusedEnvelope,
    /// This format cannot yet represent the exact inverse of this operation.
    #[error("operation {index} ({op}) is not invertible")]
    NotInvertible {
        /// Original forward operation index.
        index: usize,
        /// Forward operation name.
        op: &'static str,
    },
    /// The caller did not supply the target record in the required substrate map.
    #[error("operation {index} is missing current record {id}")]
    MissingCurrent {
        /// Original forward operation index.
        index: usize,
        /// Identifier of the required current record.
        id: Id128,
    },
    /// The working record no longer matches a field required by the forward operation.
    #[error("operation {index} record {id} differs in field {field}")]
    Invalidated {
        /// Original forward operation index.
        index: usize,
        /// Identifier of the affected record.
        id: Id128,
        /// Mismatched field name; `id` also names map-key/record-ID mismatches.
        field: &'static str,
    },
    /// The captured delete preimage cannot describe a valid live restored record.
    #[error("operation {index} inverse restore is invalid: {reason}")]
    InverseRestore {
        /// Original forward operation index.
        index: usize,
        /// The checked restore constructor's diagnostic.
        reason: String,
    },
    /// The existing checked update constructor refused the swapped patch/preimage.
    #[error("operation {index} inverse update is invalid: {reason}")]
    InverseUpdate {
        /// Original forward operation index.
        index: usize,
        /// The checked constructor's diagnostic.
        reason: String,
    },
}

impl ChangeSet {
    /// Produces a reverse-ordered inverse with the caller's fresh envelope.
    ///
    /// Current records are checked and rewound in a private working copy, so
    /// repeated updates compare against the correct intermediate values. An
    /// update checks only fields it touches. A create or link must still match
    /// its staged fields and be live; its inverse is a soft delete carrying the
    /// supplied full record, including its actual timestamps.
    ///
    /// This is a pure stage-time check, not an atomic comparison at landing.
    /// The caller reads current records and supplies the revert's provenance.
    /// Neither this change-set nor `current` is modified, even on refusal.
    ///
    /// # Errors
    ///
    /// Refuses a reused envelope, hard deletes/merges, missing or invalidated
    /// current records, an unrestorable delete preimage, or an invalid swapped update.
    pub fn invert(
        &self,
        envelope: Envelope,
        current: &CurrentState,
    ) -> Result<ChangeSet, InvertError> {
        if envelope == self.envelope {
            return Err(InvertError::ReusedEnvelope);
        }
        let mut working = current.clone();
        // Restore records do not capture the earlier tombstone timestamp. Track
        // rewound lifecycle state separately instead of fabricating a timestamp.
        let mut tombstones = BTreeSet::new();
        let mut ops = Vec::with_capacity(self.ops.len());
        for (index, op) in self.ops.iter().enumerate().rev() {
            let inverse = match op {
                Op::Create(create) => {
                    let substrate = match &create.target {
                        CreateTarget::Entity(_) => 0,
                        CreateTarget::Note(_) => 1,
                    };
                    check(
                        index,
                        create.id,
                        "deleted_at",
                        !tombstones.contains(&(substrate, create.id)),
                    )?;
                    invert_create(index, create, &mut working)?
                }
                Op::Link(link) => {
                    check(
                        index,
                        link.id,
                        "deleted_at",
                        !tombstones.contains(&(2, link.id)),
                    )?;
                    invert_link(index, link, &mut working)?
                }
                Op::Update(update) => invert_update(index, update, &mut working)?,
                Op::Delete(delete) if delete.hard => {
                    return Err(InvertError::NotInvertible {
                        index,
                        op: "delete",
                    });
                }
                Op::Delete(delete) => invert_delete(index, delete, &mut working, &mut tombstones)?,
                Op::Restore(restore) => {
                    invert_restore(index, restore, &mut working, &mut tombstones)?
                }
                Op::Merge(_) => {
                    return Err(InvertError::NotInvertible { index, op: "merge" });
                }
            };
            ops.push(inverse);
        }
        Ok(ChangeSet::new(envelope, ops))
    }
}

fn check(index: usize, id: Id128, field: &'static str, matches: bool) -> Result<(), InvertError> {
    if matches {
        Ok(())
    } else {
        Err(InvertError::Invalidated { index, id, field })
    }
}

type RecordKey = (u8, Id128);

fn record_key(record: &DeletePreimage) -> RecordKey {
    match record {
        DeletePreimage::Entity(row) => (0, row.header.id),
        DeletePreimage::Note(row) => (1, row.header.id),
        DeletePreimage::Edge(row) => (2, row.id),
    }
}

fn is_deleted(record: &DeletePreimage) -> bool {
    match record {
        DeletePreimage::Entity(row) => row.deleted_at.is_some(),
        DeletePreimage::Note(row) => row.deleted_at.is_some(),
        DeletePreimage::Edge(row) => row.deleted_at.is_some(),
    }
}

fn current_record(
    index: usize,
    id: Id128,
    expected: &DeletePreimage,
    working: &CurrentState,
) -> Result<DeletePreimage, InvertError> {
    let record = match expected {
        DeletePreimage::Entity(_) => working
            .entities
            .get(&id)
            .cloned()
            .map(Box::new)
            .map(DeletePreimage::Entity),
        DeletePreimage::Note(_) => working
            .notes
            .get(&id)
            .cloned()
            .map(Box::new)
            .map(DeletePreimage::Note),
        DeletePreimage::Edge(_) => working
            .edges
            .get(&id)
            .cloned()
            .map(Box::new)
            .map(DeletePreimage::Edge),
    }
    .ok_or(InvertError::MissingCurrent { index, id })?;
    check(index, id, "id", record_key(&record).1 == id)?;
    Ok(record)
}

fn rewind_record(record: &DeletePreimage, working: &mut CurrentState) {
    match record {
        DeletePreimage::Entity(row) => {
            working.entities.insert(row.header.id, *row.clone());
        }
        DeletePreimage::Note(row) => {
            working.notes.insert(row.header.id, *row.clone());
        }
        DeletePreimage::Edge(row) => {
            working.edges.insert(row.id, *row.clone());
        }
    }
}

fn check_record_content(
    index: usize,
    id: Id128,
    current: &DeletePreimage,
    expected: &DeletePreimage,
) -> Result<(), InvertError> {
    macro_rules! fields {
        ($current:ident, $expected:ident, $($field:ident),+ $(,)?) => {
            $(check(index, id, stringify!($field), $current.$field == $expected.$field)?;)+
        };
    }
    match (current, expected) {
        (DeletePreimage::Entity(row), DeletePreimage::Entity(prior)) => {
            fields!(
                row,
                prior,
                kind,
                entity_type,
                name,
                description,
                properties,
                tags
            );
            check(
                index,
                id,
                "namespace",
                row.header.namespace == prior.header.namespace,
            )?;
            check(
                index,
                id,
                "created_at",
                row.header.created_at == prior.header.created_at,
            )?;
        }
        (DeletePreimage::Note(row), DeletePreimage::Note(prior)) => {
            fields!(
                row,
                prior,
                kind,
                status,
                content,
                properties,
                tags,
                salience,
                decay_factor,
                expires_at
            );
            check(
                index,
                id,
                "namespace",
                row.header.namespace == prior.header.namespace,
            )?;
            check(
                index,
                id,
                "created_at",
                row.header.created_at == prior.header.created_at,
            )?;
        }
        (DeletePreimage::Edge(row), DeletePreimage::Edge(prior)) => {
            fields!(
                row, prior, namespace, source, target, relation, weight, properties, created_at
            );
        }
        _ => check(index, id, "substrate", false)?,
    }
    Ok(())
}

fn invert_delete(
    index: usize,
    delete: &DeleteOp,
    working: &mut CurrentState,
    tombstones: &mut BTreeSet<RecordKey>,
) -> Result<Op, InvertError> {
    let id = delete.target_id;
    let restore = RestoreOp::new(id, delete.preimage.clone())
        .map_err(|reason| InvertError::InverseRestore { index, reason })?;
    let mut current = current_record(index, id, &delete.preimage, working)?;
    let key = record_key(&delete.preimage);
    check(
        index,
        id,
        "deleted_at",
        is_deleted(&current) || tombstones.contains(&key),
    )?;
    if !tombstones.contains(&key) {
        match (&mut current, &delete.preimage) {
            (DeletePreimage::Entity(row), DeletePreimage::Entity(prior)) => {
                check(
                    index,
                    id,
                    "updated_at",
                    row.header.updated_at == prior.header.updated_at,
                )?;
            }
            (DeletePreimage::Note(row), DeletePreimage::Note(prior)) => {
                check(
                    index,
                    id,
                    "updated_at",
                    row.header.updated_at == prior.header.updated_at,
                )?;
                check(index, id, "status", row.status == NoteStatus::Deleted)?;
                row.status = prior.status;
            }
            _ => {}
        }
    }
    check_record_content(index, id, &current, &delete.preimage)?;
    rewind_record(&delete.preimage, working);
    tombstones.remove(&key);
    Ok(Op::Restore(restore))
}

fn invert_restore(
    index: usize,
    restore: &RestoreOp,
    working: &mut CurrentState,
    tombstones: &mut BTreeSet<RecordKey>,
) -> Result<Op, InvertError> {
    let id = restore.target_id();
    let key = record_key(restore.record());
    let current = current_record(index, id, restore.record(), working)?;
    check(
        index,
        id,
        "deleted_at",
        !is_deleted(&current) && !tombstones.contains(&key),
    )?;
    check_record_content(index, id, &current, restore.record())?;
    tombstones.insert(key);
    Ok(Op::Delete(DeleteOp {
        target_id: id,
        hard: false,
        preimage: current,
    }))
}

fn invert_create(
    index: usize,
    create: &CreateOp,
    working: &mut CurrentState,
) -> Result<Op, InvertError> {
    let id = create.id;
    let preimage = match &create.target {
        CreateTarget::Entity(fields) => {
            let row = working
                .entities
                .get(&id)
                .ok_or(InvertError::MissingCurrent { index, id })?;
            check(index, id, "id", row.header.id == id)?;
            check(
                index,
                id,
                "namespace",
                row.header.namespace == create.namespace,
            )?;
            check(index, id, "deleted_at", row.deleted_at.is_none())?;
            check(index, id, "kind", row.kind == fields.entity_kind)?;
            check(
                index,
                id,
                "entity_type",
                row.entity_type == fields.entity_type,
            )?;
            check(index, id, "name", row.name == fields.name)?;
            check(
                index,
                id,
                "description",
                row.description == fields.description,
            )?;
            check(index, id, "properties", row.properties == fields.properties)?;
            check(index, id, "tags", row.tags == fields.tags)?;
            let captured = row.clone();
            working.entities.remove(&id);
            DeletePreimage::Entity(Box::new(captured))
        }
        CreateTarget::Note(fields) => {
            let row = working
                .notes
                .get(&id)
                .ok_or(InvertError::MissingCurrent { index, id })?;
            check(index, id, "id", row.header.id == id)?;
            check(
                index,
                id,
                "namespace",
                row.header.namespace == create.namespace,
            )?;
            check(index, id, "deleted_at", row.deleted_at.is_none())?;
            check(index, id, "kind", row.kind == fields.note_kind)?;
            check(index, id, "status", row.status == NoteStatus::default())?;
            check(index, id, "expires_at", row.expires_at.is_none())?;
            check(index, id, "content", row.content == fields.content)?;
            check(index, id, "properties", row.properties == fields.properties)?;
            check(index, id, "tags", row.tags == fields.tags)?;
            check(index, id, "salience", row.salience == fields.salience)?;
            check(
                index,
                id,
                "decay_factor",
                row.decay_factor == fields.decay_factor,
            )?;
            let captured = row.clone();
            working.notes.remove(&id);
            DeletePreimage::Note(Box::new(captured))
        }
    };
    Ok(Op::Delete(DeleteOp {
        target_id: id,
        hard: false,
        preimage,
    }))
}

fn invert_link(index: usize, link: &LinkOp, working: &mut CurrentState) -> Result<Op, InvertError> {
    let id = link.id;
    let row = working
        .edges
        .get(&id)
        .ok_or(InvertError::MissingCurrent { index, id })?;
    check(index, id, "id", row.id == id)?;
    check(
        index,
        id,
        "namespace",
        row.namespace == link.namespace.as_str(),
    )?;
    check(index, id, "deleted_at", row.deleted_at.is_none())?;
    check(index, id, "source", row.source == link.source)?;
    check(index, id, "target", row.target == link.target)?;
    check(index, id, "relation", row.relation == link.relation)?;
    check(index, id, "weight", row.weight == link.weight)?;
    check(index, id, "properties", row.properties == link.properties)?;
    let captured = row.clone();
    working.edges.remove(&id);
    Ok(Op::Delete(DeleteOp {
        target_id: id,
        hard: false,
        preimage: DeletePreimage::Edge(Box::new(captured)),
    }))
}

fn invert_update(
    index: usize,
    update: &UpdateOp,
    working: &mut CurrentState,
) -> Result<Op, InvertError> {
    let id = update.target_id();
    match (update.patch(), update.preimage()) {
        (UpdatePatch::Entity(patch), UpdatePreimage::Entity(prior)) => {
            let row = working
                .entities
                .get_mut(&id)
                .ok_or(InvertError::MissingCurrent { index, id })?;
            check(index, id, "id", row.header.id == id)?;
            if let Some(value) = &patch.name {
                check(index, id, "name", &row.name == value)?;
            }
            if let Some(value) = &patch.description {
                check(
                    index,
                    id,
                    "description",
                    row.description.as_ref() == value.as_ref(),
                )?;
            }
            if let Some(value) = &patch.entity_type {
                check(
                    index,
                    id,
                    "entity_type",
                    row.entity_type.as_ref() == value.as_ref(),
                )?;
            }
            if let Some(value) = &patch.properties {
                check(index, id, "properties", &row.properties == value)?;
            }
            if let Some(value) = &patch.tags {
                check(index, id, "tags", &row.tags == value)?;
            }
            let inverse = swapped_update(index, update)?;
            if let Some(value) = &prior.name {
                row.name = value.clone();
            }
            if let Some(value) = &prior.description {
                row.description = value.clone();
            }
            if let Some(value) = &prior.entity_type {
                row.entity_type = value.clone();
            }
            if let Some(value) = &prior.properties {
                row.properties = value.clone();
            }
            if let Some(value) = &prior.tags {
                row.tags = value.clone();
            }
            Ok(Op::Update(inverse))
        }
        (UpdatePatch::Note(patch), UpdatePreimage::Note(prior)) => {
            let row = working
                .notes
                .get_mut(&id)
                .ok_or(InvertError::MissingCurrent { index, id })?;
            check(index, id, "id", row.header.id == id)?;
            if let Some(value) = &patch.content {
                check(index, id, "content", &row.content == value)?;
            }
            if let Some(value) = patch.salience {
                check(index, id, "salience", row.salience == value)?;
            }
            if let Some(value) = patch.decay_factor {
                check(index, id, "decay_factor", row.decay_factor == value)?;
            }
            if let Some(value) = &patch.properties {
                check(index, id, "properties", &row.properties == value)?;
            }
            if let Some(value) = &patch.tags {
                check(index, id, "tags", &row.tags == value)?;
            }
            let inverse = swapped_update(index, update)?;
            if let Some(value) = &prior.content {
                row.content = value.clone();
            }
            if let Some(value) = prior.salience {
                row.salience = value;
            }
            if let Some(value) = prior.decay_factor {
                row.decay_factor = value;
            }
            if let Some(value) = &prior.properties {
                row.properties = value.clone();
            }
            if let Some(value) = &prior.tags {
                row.tags = value.clone();
            }
            Ok(Op::Update(inverse))
        }
        (UpdatePatch::Edge(patch), UpdatePreimage::Edge(prior)) => {
            let row = working
                .edges
                .get_mut(&id)
                .ok_or(InvertError::MissingCurrent { index, id })?;
            check(index, id, "id", row.id == id)?;
            if let Some(value) = patch.relation {
                check(index, id, "relation", row.relation == value)?;
            }
            if let Some(value) = patch.weight {
                check(index, id, "weight", row.weight == value)?;
            }
            let inverse = swapped_update(index, update)?;
            if let Some(value) = prior.relation {
                row.relation = value;
            }
            if let Some(value) = prior.weight {
                row.weight = value;
            }
            Ok(Op::Update(inverse))
        }
        // The checked constructor and decoder prevent this pairing. Keep the
        // refusal typed rather than panic if that invariant ever changes.
        _ => swapped_update(index, update).map(Op::Update),
    }
}

fn swapped_update(index: usize, update: &UpdateOp) -> Result<UpdateOp, InvertError> {
    let patch = match update.preimage() {
        UpdatePreimage::Entity(prior) => UpdatePatch::Entity(EntityPatch {
            name: prior.name.clone(),
            description: prior.description.clone(),
            properties: prior.properties.clone(),
            tags: prior.tags.clone(),
            entity_type: prior.entity_type.clone(),
        }),
        UpdatePreimage::Note(prior) => UpdatePatch::Note(NotePatch {
            content: prior.content.clone(),
            salience: prior.salience,
            decay_factor: prior.decay_factor,
            properties: prior.properties.clone(),
            tags: prior.tags.clone(),
        }),
        UpdatePreimage::Edge(prior) => UpdatePatch::Edge(EdgePatch {
            relation: prior.relation,
            weight: prior.weight,
        }),
    };
    let preimage = match update.patch() {
        UpdatePatch::Entity(patch) => UpdatePreimage::Entity(EntityPreimage {
            name: patch.name.clone(),
            description: patch.description.clone(),
            properties: patch.properties.clone(),
            tags: patch.tags.clone(),
            entity_type: patch.entity_type.clone(),
        }),
        UpdatePatch::Note(patch) => UpdatePreimage::Note(NotePreimage {
            content: patch.content.clone(),
            salience: patch.salience,
            decay_factor: patch.decay_factor,
            properties: patch.properties.clone(),
            tags: patch.tags.clone(),
        }),
        UpdatePatch::Edge(patch) => UpdatePreimage::Edge(EdgePreimage {
            relation: patch.relation,
            weight: patch.weight,
        }),
    };
    UpdateOp::new(update.target_id(), patch, preimage)
        .map_err(|reason| InvertError::InverseUpdate { index, reason })
}
