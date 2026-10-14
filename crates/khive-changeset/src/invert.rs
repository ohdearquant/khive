//! Pure, stage-time inversion against caller-supplied current records.

use std::collections::BTreeMap;

use khive_types::{Entity, Id128, Link, Note, NoteStatus};

use crate::{
    ChangeSet, CreateOp, CreateTarget, DeleteOp, DeletePreimage, EdgePatch, EdgePreimage,
    EntityPatch, EntityPreimage, Envelope, LinkOp, NotePatch, NotePreimage, Op, UpdateOp,
    UpdatePatch, UpdatePreimage,
};

/// Live records read by the caller when staging a revert.
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
    /// Refuses a reused envelope, delete/merge operations, missing or invalidated
    /// current records, or a swapped update rejected by [`UpdateOp::new`].
    pub fn invert(
        &self,
        envelope: Envelope,
        current: &CurrentState,
    ) -> Result<ChangeSet, InvertError> {
        if envelope == self.envelope {
            return Err(InvertError::ReusedEnvelope);
        }
        let mut working = current.clone();
        let mut ops = Vec::with_capacity(self.ops.len());
        for (index, op) in self.ops.iter().enumerate().rev() {
            let inverse = match op {
                Op::Create(create) => invert_create(index, create, &mut working)?,
                Op::Link(link) => invert_link(index, link, &mut working)?,
                Op::Update(update) => invert_update(index, update, &mut working)?,
                Op::Delete(_) => {
                    return Err(InvertError::NotInvertible {
                        index,
                        op: "delete",
                    });
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
