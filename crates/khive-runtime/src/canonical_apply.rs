//! Ordered canonical changesets use ordinary atomic plans after writer-local checks.

use std::collections::HashMap;

use khive_changeset::{
    CanonicalRecordRef, ChangeSet, CreateTarget, DeleteOp, Op, UpdateOp, UpdatePatch,
};
use khive_storage::{Edge, Entity, Note, SqlStatement, SqlValue, SqlWriter};
use khive_types::EdgeEndpointRule;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::atomic_runner::{AtomicOpFailure, AtomicOpPlan};
use crate::pack::CanonicalNoteHookLookup;
use crate::{
    EdgeEndpointKind, KhiveRuntime, NamespaceToken, Resolved, RuntimeError, RuntimeResult,
    VerbRegistry,
};

/// Change-set identifiers are `Id128`; runtime records key on `Uuid`. Both carry the same 16 bytes.
fn uuid(id: khive_types::Id128) -> Uuid {
    Uuid::from_bytes(*id.as_bytes())
}

fn id128(id: Uuid) -> khive_types::Id128 {
    khive_types::Id128::from_bytes(*id.as_bytes())
}

mod entities;
mod links;
mod notes;
mod projection;
mod rows;

/// An opaque, admitted operation. Only this module can construct its captured state.
#[derive(Clone, Debug)]
pub struct CanonicalMutationPlan {
    token: NamespaceToken,
    target_id: Uuid,
    now: i64,
    mutation: Mutation,
}

#[derive(Clone, Debug)]
enum Mutation {
    Create(Box<AtomicOpPlan>),
    Link {
        edge: Edge,
        rules: Vec<EdgeEndpointRule>,
    },
    Entity {
        op: UpdateOp,
        prepared: Box<entities::PreparedEntityUpdate>,
    },
    Note {
        op: UpdateOp,
        prepared: Box<notes::PreparedCanonicalNote>,
        lookup: CanonicalNoteHookLookup,
    },
    Edge {
        op: UpdateOp,
        rules: Vec<EdgeEndpointRule>,
    },
    Delete {
        op: DeleteOp,
        plan: Box<AtomicOpPlan>,
    },
    Unresolved(Op),
}

#[derive(Clone, Debug)]
enum Record {
    Entity(Entity),
    Note(Note),
    Edge(Edge),
    Event,
}
impl Record {
    fn live(&self) -> bool {
        match self {
            Self::Entity(x) => x.deleted_at.is_none() && x.merged_into.is_none(),
            Self::Note(x) => x.deleted_at.is_none(),
            Self::Edge(x) => x.deleted_at.is_none(),
            Self::Event => true,
        }
    }
    fn resolved(&self) -> Option<Resolved> {
        match self {
            Self::Entity(x) => Some(Resolved::Entity(x.clone())),
            Self::Note(x) => Some(Resolved::Note(x.clone())),
            _ => None,
        }
    }
    fn kind(&self) -> EdgeEndpointKind {
        match self {
            Self::Entity(_) => EdgeEndpointKind::Entity,
            Self::Note(_) => EdgeEndpointKind::Note,
            Self::Edge(_) => EdgeEndpointKind::Edge,
            Self::Event => EdgeEndpointKind::Event,
        }
    }
    fn check_update(&self, op: &UpdateOp) -> Result<(), AtomicOpFailure> {
        let result = match self {
            Self::Entity(x) => {
                let mut live = projection::entity(x).map_err(storage)?;
                if let khive_changeset::UpdatePreimage::Entity(captured) = op.preimage() {
                    omit_unclaimed_reserved(captured.properties.as_ref(), &mut live.properties);
                }
                khive_changeset::check_update(op, CanonicalRecordRef::Entity(&live))
            }
            Self::Note(x) => {
                let mut live = projection::note(x).map_err(storage)?;
                if let khive_changeset::UpdatePreimage::Note(captured) = op.preimage() {
                    omit_unclaimed_reserved(captured.properties.as_ref(), &mut live.properties);
                }
                khive_changeset::check_update(op, CanonicalRecordRef::Note(&live))
            }
            Self::Edge(x) => khive_changeset::check_update(
                op,
                CanonicalRecordRef::Edge(&projection::edge(x).map_err(storage)?),
            ),
            Self::Event => return Err(mismatch(uuid(op.target_id()), "substrate")),
        };
        result.map_err(|e| mismatch(uuid(op.target_id()), &e.field_path))
    }
    fn check_delete(&self, op: &DeleteOp) -> Result<(), AtomicOpFailure> {
        let result = match self {
            Self::Entity(x) => khive_changeset::check_delete(
                op,
                CanonicalRecordRef::Entity(&projection::entity(x).map_err(storage)?),
            ),
            Self::Note(x) => khive_changeset::check_delete(
                op,
                CanonicalRecordRef::Note(&projection::note(x).map_err(storage)?),
            ),
            Self::Edge(x) => khive_changeset::check_delete(
                op,
                CanonicalRecordRef::Edge(&projection::edge(x).map_err(storage)?),
            ),
            Self::Event => return Err(mismatch(uuid(op.target_id), "substrate")),
        };
        result.map_err(|e| mismatch(uuid(op.target_id), &e.field_path))
    }
}

/// Prepare a bounded canonical changeset without admitting any writer work.
///
/// Schema, count, and unsupported operation checks run before asynchronous preparation.
/// Every constituent is authorized with the applying caller. The returned opaque
/// plans must run together through [`crate::run_atomic_unit`].
pub async fn prepare_canonical_changeset(
    runtime: &KhiveRuntime,
    registry: &VerbRegistry,
    token: &NamespaceToken,
    changeset: &ChangeSet,
) -> RuntimeResult<Vec<AtomicOpPlan>> {
    validate(changeset)?;
    let now = chrono::Utc::now().timestamp_micros();
    let mut shadow: HashMap<Uuid, Option<Record>> = HashMap::new();
    let mut plans = Vec::with_capacity(changeset.ops.len());
    let mut hook_links: HashMap<String, (usize, Vec<crate::LinkSpec>)> = HashMap::new();
    for (op_index, op) in changeset.ops.iter().enumerate() {
        let prepared: RuntimeResult<AtomicOpPlan> = async {
            let (verb, id, args) = gate_args(op)?;
            let op_token = registry
                .authorize_canonical_operation(token, verb, args.clone(), id)
                .await?;
            let mutation = match op {
                Op::Create(create) => {
                    if shadow.contains_key(&id) {
                        return Err(RuntimeError::InvalidInput(
                            "canonical staged id is reused".into(),
                        ));
                    }
                    // Repeat the immutable owner draft check at this public
                    // runtime boundary, without dispatching create lifecycle hooks.
                    validate_create_owner(registry, create)?;
                    let (record, plan) = match &create.target {
                        CreateTarget::Entity(_) => {
                            let (row, plan) = crate::atomic_prepare::prepare_add_entity_record(
                                runtime,
                                &op_token,
                                &args,
                                Some(id),
                            )
                            .await?;
                            crate::atomic_prepare::canonical_refuse_registry_tags(
                                &row.tags, "create",
                            )?;
                            (Record::Entity(row), plan)
                        }
                        CreateTarget::Note(_) => {
                            let (row, plan) = crate::atomic_prepare::prepare_add_note_record(
                                runtime,
                                &op_token,
                                &args,
                                Some(id),
                            )
                            .await?;
                            projection::note(&row)?;
                            (Record::Note(row), plan)
                        }
                    };
                    shadow.insert(id, Some(record));
                    Mutation::Create(Box::new(plan))
                }
                Op::Link(link) => {
                    if shadow.contains_key(&id) {
                        return Err(RuntimeError::InvalidInput(
                            "canonical staged id is reused".into(),
                        ));
                    }
                    let source = read_prepare(runtime, &op_token, uuid(link.source), &mut shadow)
                        .await?
                        .ok_or_else(|| RuntimeError::NotFound("canonical link source".into()))?;
                    let target = read_prepare(runtime, &op_token, uuid(link.target), &mut shadow)
                        .await?
                        .ok_or_else(|| RuntimeError::NotFound("canonical link target".into()))?;
                    let rules = runtime.pack_edge_rules();
                    links::endpoints(&source, &target, link.relation, &rules)?;
                    let edge = links::draft(link, &source, &target, now)?;
                    if let Record::Note(note) = source {
                        hook_links
                            .entry(note.kind)
                            .or_insert_with(|| (op_index, Vec::new()))
                            .1
                            .push(crate::LinkSpec {
                                namespace: Some(link.namespace.as_str().into()),
                                source_id: uuid(link.source),
                                target_id: uuid(link.target),
                                relation: link.relation,
                                weight: link.weight,
                                metadata: edge.metadata.clone(),
                                resurrect: false,
                            });
                    }
                    shadow.insert(id, Some(Record::Edge(edge.clone())));
                    Mutation::Link { edge, rules }
                }
                Op::Update(update) => {
                    reject_note_alias(update)?;
                    let current = read_prepare(runtime, &op_token, id, &mut shadow).await?;
                    match (update.patch(), current) {
                        (UpdatePatch::Entity(_), Some(Record::Entity(row))) => {
                            let prepared =
                                entities::prepare(runtime, &op_token, row, update).await?;
                            shadow.insert(id, Some(Record::Entity(prepared.projected(now)?)));
                            Mutation::Entity {
                                op: update.clone(),
                                prepared: Box::new(prepared),
                            }
                        }
                        (UpdatePatch::Note(_), Some(Record::Note(row))) => {
                            reject_note_alias(update)?;
                            let prepared =
                                notes::prepare(runtime, registry, &op_token, row, update).await?;
                            shadow.insert(id, Some(Record::Note(prepared.projected(now)?)));
                            Mutation::Note {
                                op: update.clone(),
                                prepared: Box::new(prepared),
                                lookup: registry.canonical_note_hook_lookup(),
                            }
                        }
                        (UpdatePatch::Edge(patch), Some(Record::Edge(mut row))) => {
                            if let Some(relation) = patch.relation {
                                if row.source_id == row.target_id {
                                    return Err(RuntimeError::InvalidInput(
                                        "self-loop edges are not allowed".into(),
                                    ));
                                }
                                let source =
                                    read_prepare(runtime, &op_token, row.source_id, &mut shadow)
                                        .await?
                                        .ok_or_else(|| {
                                            RuntimeError::NotFound("edge source".into())
                                        })?;
                                let target =
                                    read_prepare(runtime, &op_token, row.target_id, &mut shadow)
                                        .await?
                                        .ok_or_else(|| {
                                            RuntimeError::NotFound("edge target".into())
                                        })?;
                                links::endpoints(
                                    &source,
                                    &target,
                                    relation,
                                    &runtime.pack_edge_rules(),
                                )?;
                                row.relation = relation;
                            }
                            if let Some(weight) = patch.weight {
                                crate::operations::validate_edge_weight(weight)?;
                                row.weight = weight;
                            }
                            let (source_id, target_id) =
                                crate::operations::canonical_edge_endpoints(
                                    row.relation,
                                    row.source_id,
                                    row.target_id,
                                );
                            row.source_id = source_id;
                            row.target_id = target_id;
                            let next = row
                                .updated_at
                                .timestamp_micros()
                                .checked_add(1)
                                .ok_or_else(|| {
                                    RuntimeError::InvalidInput("edge revision exhausted".into())
                                })?;
                            row.updated_at = chrono::DateTime::from_timestamp_micros(now.max(next))
                                .ok_or_else(|| {
                                    RuntimeError::InvalidInput("invalid edge revision".into())
                                })?;
                            shadow.insert(id, Some(Record::Edge(row)));
                            Mutation::Edge {
                                op: update.clone(),
                                rules: runtime.pack_edge_rules(),
                            }
                        }
                        _ => Mutation::Unresolved(op.clone()),
                    }
                }
                Op::Delete(delete) => {
                    reject_delete_alias(delete)?;
                    let current = read_prepare(runtime, &op_token, id, &mut shadow).await?;
                    let plan = match current {
                        Some(Record::Entity(row)) => Some(
                            crate::atomic_prepare::prepare_delete_from_snapshot(
                                runtime,
                                &op_token,
                                &args,
                                None,
                                Some(Resolved::Entity(row)),
                            )
                            .await?,
                        ),
                        Some(Record::Note(row)) => Some(
                            crate::atomic_prepare::prepare_delete_from_snapshot(
                                runtime,
                                &op_token,
                                &args,
                                None,
                                Some(Resolved::Note(row)),
                            )
                            .await?,
                        ),
                        Some(Record::Edge(row)) => Some(
                            crate::atomic_prepare::prepare_delete_edge(
                                &op_token,
                                id,
                                row,
                                delete.hard,
                                &format!("{}:{}", op_token.actor().kind, op_token.actor().id),
                            )
                            .await?,
                        ),
                        _ => None,
                    };
                    shadow.insert(id, None);
                    match plan {
                        Some(plan) => Mutation::Delete {
                            op: delete.clone(),
                            plan: Box::new(plan),
                        },
                        None => Mutation::Unresolved(op.clone()),
                    }
                }
                Op::Merge(_) | Op::Restore(_) => unreachable!("validated before prepare"),
            };
            Ok(AtomicOpPlan::Canonical(Box::new(CanonicalMutationPlan {
                token: op_token,
                target_id: id,
                now,
                mutation,
            })))
        }
        .await;
        plans.push(prepared.map_err(|source| RuntimeError::CanonicalAdmission {
            op_index,
            source: Box::new(source),
        })?);
    }
    // Preserve the owner hook's batch view so cycles spanning several links
    // are checked together, including links from earlier staged notes.
    for (kind, (op_index, specs)) in hook_links {
        if let Some(hook) = registry.find_kind_hook(&kind) {
            hook.validate_links(runtime, token, &specs)
                .await
                .map_err(|source| RuntimeError::CanonicalAdmission {
                    op_index,
                    source: Box::new(source),
                })?;
        }
    }
    Ok(plans)
}

fn validate(changeset: &ChangeSet) -> RuntimeResult<()> {
    if changeset.envelope.schema_version != khive_changeset::CURRENT_SCHEMA_VERSION {
        return Err(RuntimeError::InvalidInput(
            "unsupported canonical schema_version".into(),
        ));
    }
    let max = khive_types::pack::ATOMIC_MAX_OPS_DEFAULT;
    if changeset.ops.len() > max {
        return Err(RuntimeError::InvalidInput(format!(
            "canonical changeset exceeds {max} operations"
        )));
    }
    for (op_index, op) in changeset.ops.iter().enumerate() {
        match op {
            Op::Restore(_) => {
                return Err(RuntimeError::CanonicalUnsupportedOperation {
                    op_index,
                    operation: "restore",
                });
            }
            Op::Merge(_) => {
                return Err(RuntimeError::CanonicalAdmission {
                    op_index,
                    source: Box::new(RuntimeError::InvalidInput(
                        "KnownUnimplemented: canonical merge".into(),
                    )),
                });
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_create_owner(
    registry: &VerbRegistry,
    create: &khive_changeset::CreateOp,
) -> RuntimeResult<()> {
    match &create.target {
        CreateTarget::Entity(fields) => {
            if let Some(hook) = registry.find_kind_hook(fields.entity_kind.name()) {
                hook.validate_proposal_entity(&khive_types::EntityDraft {
                    kind: fields.entity_kind.name().into(),
                    name: fields.name.clone(),
                    description: fields.description.clone(),
                    properties: Some(serde_json::to_value(&fields.properties).map_err(|_| {
                        RuntimeError::InvalidInput("invalid canonical properties".into())
                    })?),
                    tags: fields.tags.clone(),
                })?;
            }
        }
        CreateTarget::Note(fields) => {
            if let Some(hook) = registry.find_kind_hook(&fields.note_kind) {
                hook.validate_proposal_note(&khive_types::NoteDraft {
                    kind: fields.note_kind.clone(),
                    content: fields.content.clone(),
                    name: None,
                    properties: Some(serde_json::to_value(&fields.properties).map_err(|_| {
                        RuntimeError::InvalidInput("invalid canonical properties".into())
                    })?),
                })?;
            }
        }
    }
    Ok(())
}

fn gate_args(op: &Op) -> RuntimeResult<(&'static str, Uuid, Value)> {
    Ok(match op {
        Op::Create(x) => {
            let mut args = match &x.target {
                CreateTarget::Entity(e) => {
                    json!({"kind":e.entity_kind.name(),"name":e.name,"description":e.description,
                "entity_type":e.entity_type,"properties":e.properties,"tags":e.tags})
                }
                CreateTarget::Note(n) => {
                    if n.properties.contains_key("tags") {
                        return Err(RuntimeError::InvalidInput(
                            "canonical note properties contain tags storage alias".into(),
                        ));
                    }
                    let mut properties = serde_json::to_value(&n.properties)
                        .map_err(|_| RuntimeError::InvalidInput("invalid properties".into()))?;
                    if !n.tags.is_empty() {
                        properties["tags"] = json!(n.tags);
                    }
                    json!({"kind":n.note_kind,"content":n.content,"properties":properties,"salience":n.salience,"decay_factor":n.decay_factor})
                }
            };
            args["namespace"] = json!(x.namespace.as_str());
            ("create", uuid(x.id), args)
        }
        Op::Link(x) => (
            "link",
            uuid(x.id),
            json!({"namespace":x.namespace.as_str(),"source_id":x.source,"target_id":x.target,
            "relation":x.relation,"weight":x.weight,"metadata":x.properties}),
        ),
        Op::Update(x) => {
            let mut args = serde_json::to_value(x.patch())
                .map_err(|_| RuntimeError::InvalidInput("invalid canonical patch".into()))?;
            args.as_object_mut()
                .expect("typed patch is an object")
                .remove("target");
            args["id"] = json!(x.target_id());
            ("update", uuid(x.target_id()), args)
        }
        Op::Delete(x) => (
            "delete",
            uuid(x.target_id),
            json!({"id":x.target_id,"hard":x.hard}),
        ),
        Op::Restore(_) => unreachable!("validated before gate arguments"),
        Op::Merge(_) => {
            return Err(RuntimeError::InvalidInput(
                "KnownUnimplemented: canonical merge".into(),
            ))
        }
    })
}

async fn read_prepare(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
    shadow: &mut HashMap<Uuid, Option<Record>>,
) -> RuntimeResult<Option<Record>> {
    if let Some(row) = shadow.get(&id) {
        return Ok(row.clone());
    }
    let row = match runtime.resolve_by_id_including_deleted(token, id).await? {
        Some(Resolved::Entity(x)) => Some(Record::Entity(x)),
        Some(Resolved::Note(x)) => Some(Record::Note(x)),
        Some(Resolved::Event(_)) => Some(Record::Event),
        Some(Resolved::PackRecord { .. }) => {
            return Err(RuntimeError::InvalidInput(
                "pack-private record is not a canonical target".into(),
            ))
        }
        None => runtime
            .get_edge_including_deleted(token, id)
            .await?
            .map(Record::Edge),
    };
    shadow.insert(id, row.clone());
    Ok(row)
}

pub(crate) async fn materialize(
    writer: &mut dyn SqlWriter,
    plan: &CanonicalMutationPlan,
) -> Result<AtomicOpPlan, AtomicOpFailure> {
    let id = plan.target_id;
    match &plan.mutation {
        Mutation::Create(ordinary) => {
            identity_absent(writer, id).await?;
            Ok((**ordinary).clone())
        }
        Mutation::Link { edge, rules } => {
            identity_absent(writer, id).await?;
            links::materialize(writer, &plan.token, edge, rules).await
        }
        mutation => {
            let live = read_writer(writer, id)
                .await?
                .ok_or_else(|| mismatch(id, "id"))?;
            match mutation {
                Mutation::Entity { op, prepared } => {
                    live.check_update(op)?;
                    if !live.live() {
                        return Err(failure("canonical update target is not live"));
                    }
                    let Record::Entity(row) = live else {
                        return Err(mismatch(id, "substrate"));
                    };
                    entities::materialize(&plan.token, row, prepared, plan.now)
                }
                Mutation::Note {
                    op,
                    prepared,
                    lookup,
                } => {
                    live.check_update(op)?;
                    if !live.live() {
                        return Err(failure("canonical update target is not live"));
                    }
                    let Record::Note(row) = live else {
                        return Err(mismatch(id, "substrate"));
                    };
                    let has_owner = lookup.has_hook(&row.kind);
                    let snapshot = prepared.snapshot();
                    if has_owner != prepared.prepared_with_owner()
                        || has_owner
                            && (row.updated_at != snapshot.updated_at
                                || row.deleted_at != snapshot.deleted_at
                                || row.version != snapshot.version)
                    {
                        return Err(AtomicOpFailure::OwnerSnapshotConflict {
                            target_id: id,
                            kind: row.kind,
                        });
                    }
                    notes::materialize(&plan.token, row, prepared, plan.now)
                }
                Mutation::Edge { op, rules } => {
                    live.check_update(op)?;
                    if !live.live() {
                        return Err(failure("canonical update target is not live"));
                    }
                    let Record::Edge(mut row) = live else {
                        return Err(mismatch(id, "substrate"));
                    };
                    let UpdatePatch::Edge(patch) = op.patch() else {
                        return Err(mismatch(id, "substrate"));
                    };
                    let old_updated = row.updated_at;
                    let old_deleted = row.deleted_at;
                    let mut changed = Vec::new();
                    if let Some(relation) = patch.relation {
                        if row.source_id == row.target_id {
                            return Err(failure("self-loop edges are not allowed"));
                        }
                        let source = read_writer(writer, row.source_id)
                            .await?
                            .ok_or_else(|| failure("edge source is missing"))?;
                        let target = read_writer(writer, row.target_id)
                            .await?
                            .ok_or_else(|| failure("edge target is missing"))?;
                        links::endpoints(&source, &target, relation, rules)
                            .map_err(|e| failure(e.to_string()))?;
                        row.relation = relation;
                        changed.push("relation");
                    }
                    if let Some(weight) = patch.weight {
                        row.weight = weight;
                        changed.push("weight");
                    }
                    crate::atomic_prepare::prepare_update_edge_plan(
                        &plan.token,
                        id,
                        row,
                        old_updated,
                        old_deleted,
                        changed,
                        plan.now,
                    )
                    .map_err(|e| failure(e.to_string()))
                }
                Mutation::Delete { op, plan: ordinary } => {
                    live.check_delete(op)?;
                    Ok((**ordinary).clone())
                }
                Mutation::Unresolved(Op::Update(op)) => {
                    live.check_update(op)?;
                    Err(failure("canonical target changed after preparation"))
                }
                Mutation::Unresolved(Op::Delete(op)) => {
                    live.check_delete(op)?;
                    Err(failure("canonical target changed after preparation"))
                }
                _ => Err(failure("invalid canonical mutation plan")),
            }
        }
    }
}

async fn identity_absent(writer: &mut dyn SqlWriter, id: Uuid) -> Result<(), AtomicOpFailure> {
    let rows = writer
        .query_all(
            SqlStatement::new(
                include_str!("../sql/canonical_identity_exists.sql"),
                vec![SqlValue::Text(id.to_string())],
            )
            .labelled("canonical-id-collision"),
        )
        .await
        .map_err(storage)?;
    if rows.is_empty() {
        Ok(())
    } else {
        Err(failure("canonical staged id already exists"))
    }
}
async fn read_writer(
    writer: &mut dyn SqlWriter,
    id: Uuid,
) -> Result<Option<Record>, AtomicOpFailure> {
    let entity = rows::entity(writer, id).await.map_err(storage)?;
    let note = rows::note(writer, id).await.map_err(storage)?;
    let edge = rows::edge(writer, id).await.map_err(storage)?;
    if usize::from(entity.is_some()) + usize::from(note.is_some()) + usize::from(edge.is_some()) > 1
    {
        return Err(failure("canonical record identity is ambiguous"));
    }
    if let Some(row) = entity {
        return Ok(Some(Record::Entity(row)));
    }
    if let Some(row) = note {
        return Ok(Some(Record::Note(row)));
    }
    if let Some(row) = edge {
        return Ok(Some(Record::Edge(row)));
    }
    let events = writer
        .query_all(
            SqlStatement::new(
                include_str!("../sql/canonical_event_exists.sql"),
                vec![SqlValue::Text(id.to_string())],
            )
            .labelled("canonical-event-endpoint"),
        )
        .await
        .map_err(storage)?;
    Ok((!events.is_empty()).then_some(Record::Event))
}
/// ADR-115 §4 applied to a W1 property preimage, the same rule as the incoming map: a captured
/// map that omits a reserved key makes no claim about it, so the live value leaves the
/// comparison; a captured reserved value is compared like any other key.
fn omit_unclaimed_reserved(
    captured: Option<&std::collections::BTreeMap<String, khive_types::PropertyValue>>,
    live: &mut std::collections::BTreeMap<String, khive_types::PropertyValue>,
) {
    if let Some(captured) = captured {
        for key in crate::secret_gate::RESERVED_PROPERTY_KEYS {
            if !captured.contains_key(key) {
                live.remove(key);
            }
        }
    }
}
fn mismatch(target_id: Uuid, field_path: &str) -> AtomicOpFailure {
    AtomicOpFailure::PreimageMismatch {
        target_id,
        field_path: field_path.into(),
    }
}
fn storage(error: khive_storage::StorageError) -> AtomicOpFailure {
    failure(error.to_string())
}
fn failure(message: impl Into<String>) -> AtomicOpFailure {
    AtomicOpFailure::SqlError {
        statement_label: Some("canonical-materialize".into()),
        message: message.into(),
    }
}
fn reject_note_alias(op: &UpdateOp) -> RuntimeResult<()> {
    let conflict = matches!(op.patch(),UpdatePatch::Note(p) if p.properties.as_ref().is_some_and(|m|m.contains_key("tags")))
        || matches!(op.preimage(),khive_changeset::UpdatePreimage::Note(p) if p.properties.as_ref().is_some_and(|m|m.contains_key("tags")));
    if conflict {
        return Err(RuntimeError::InvalidInput(
            "canonical note properties contain tags storage alias".into(),
        ));
    }
    Ok(())
}
fn reject_delete_alias(op: &DeleteOp) -> RuntimeResult<()> {
    if matches!(&op.preimage,khive_changeset::DeletePreimage::Note(n) if n.properties.contains_key("tags"))
    {
        return Err(RuntimeError::InvalidInput(
            "canonical note properties contain tags storage alias".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod reserved_property_tests;
#[cfg(test)]
mod tests;
