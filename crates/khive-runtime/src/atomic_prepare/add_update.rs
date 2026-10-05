#[cfg(doc)]
use super::AtomicDeleteKind;
use super::{
    entity_fts_document, entity_name_patch, entity_replace_if_unchanged_statement,
    entity_upsert_statement, event_append_statements, insert_document_statements,
    note_fts_document, note_upsert_statement, obj, optional_create_string,
    optional_entity_type_patch, optional_f64_patch, optional_properties, optional_str,
    optional_string_patch, optional_tags, prepare_delete, prepare_delete_edge, prepare_link,
    prepare_merge, prepare_update_edge, prepare_update_entity_plan_with_version,
    refuse_pack_registry_tags, require_str, require_uuid, AddEntityPlan, AddNotePlan,
    AffectedRowGuard, AtomicOpPlan, EdgeUpsertDisposition, EventKind, KhiveRuntime, NamespaceToken,
    PlanStatement, PostCommitEffect, Resolved, RuntimeError, RuntimeResult, SubstrateKind,
    UpdatePlan, Uuid, Value,
};

// ---------------------------------------------------------------------------
// dispatch
// ---------------------------------------------------------------------------

/// Build the prepared [`AtomicOpPlan`] for one KG-substrate admissible op
/// (`update`, `delete`, `link`, `merge`). Returns a loud [`RuntimeError`] for
/// `propose`/`review`/`withdraw` (known scope gap, see module doc) and any
/// other verb (the CLI boundary must reject those before calling this — a
/// verb reaching here is either KG-substrate-admissible or a bug upstream).
pub async fn prepare_op(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    tool: &str,
    args: &Value,
) -> RuntimeResult<AtomicOpPlan> {
    match tool {
        // `expected_kind: None` here — same reasoning as the `"delete"` arm
        // below: callers that need `update(kind=...)` parity must resolve
        // the kind spec themselves (it needs a `VerbRegistry`, unreachable
        // from this crate: see `AtomicUpdateKind`'s doc comment) and call
        // `prepare_update` directly with the resolved value; `kkernel`'s
        // `--atomic` seam does exactly this and bypasses this dispatch arm.
        // A caller reaching `prepare_op("update", ...)` without going
        // through that seam gets kind-unchecked behavior.
        "update" => prepare_update(runtime, token, args, None).await,
        // `expected_kind: None` here — callers that need `delete(kind=...)`
        // parity must resolve the kind spec themselves (it needs a
        // `VerbRegistry`, unreachable from this crate: see
        // `AtomicDeleteKind`'s doc comment) and call `prepare_delete`
        // directly with the resolved value; `kkernel`'s `--atomic` seam does
        // exactly this and bypasses this dispatch arm. A caller reaching
        // `prepare_op("delete", ...)` without going through that seam gets
        // kind-unchecked behavior.
        "delete" => prepare_delete(runtime, token, args, None).await,
        "link" => prepare_link(runtime, token, args).await,
        "merge" => prepare_merge(runtime, token, args).await,
        "propose" | "review" | "withdraw" => prepare_governance_unimplemented(tool),
        other => Err(RuntimeError::InvalidInput(format!(
            "{other:?} has no atomic_prepare::prepare_op implementation; the CLI \
             admissibility check should have rejected this before prepare"
        ))),
    }
}

fn prepare_governance_unimplemented(tool: &str) -> RuntimeResult<AtomicOpPlan> {
    Err(RuntimeError::InvalidInput(format!(
        "{tool:?} is on the ADR-099 v1 admissible verb list but has no --atomic \
         prepare/apply implementation yet: its lifecycle (ADR-046) is an \
         event-sourced changeset-interpreter over a dedicated `proposals_open` \
         table, not a small guarded-DML plan — a faithful non-stub atomic \
         prepare for it is tracked as ADR-099 follow-up work, not implemented \
         in slice B3. No write was attempted."
    )))
}

// ---------------------------------------------------------------------------
// create (AddEntity / AddNote)
// ---------------------------------------------------------------------------

/// Build the prepared plan for an `AddEntity` proposal change. The entity
/// row and FTS document are committed together; vector indexing is deferred
/// until after commit because embedding may suspend. `kind` must already be
/// canonicalized by the caller because pack-aware resolution requires a
/// `VerbRegistry`.
pub async fn prepare_add_entity(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    args: &Value,
) -> RuntimeResult<AtomicOpPlan> {
    let kind = require_str(args, "kind")?;
    let name = require_str(args, "name")?;
    runtime.validate_entity_kind(kind)?;
    if name.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(
            "name must not be empty".to_string(),
        ));
    }

    let description = optional_create_string(args, "description")?;
    let properties = optional_properties(args, "properties")?;
    let tags = optional_tags(args)?.unwrap_or_default();

    crate::secret_gate::check_at(name, "entity", "name")?;
    if let Some(ref d) = description {
        crate::secret_gate::check_at(d, "entity", "description")?;
    }
    if let Some(ref p) = properties {
        crate::secret_gate::check_json_at(p, "entity", "properties")?;
    }
    crate::secret_gate::check_tags_at(&tags, "entity", "tags")?;
    crate::secret_gate::reject_reserved_secret_gate_property(properties.as_ref())?;

    let ns = token.namespace().as_str();
    let mut entity = khive_storage::Entity::new(ns, kind, name);
    if let Some(d) = description {
        entity = entity.with_description(d);
    }
    if let Some(p) = properties {
        entity = entity.with_properties(p);
    }
    if !tags.is_empty() {
        entity = entity.with_tags(tags);
    }

    let mut statements = vec![PlanStatement {
        statement: entity_upsert_statement(&entity),
        guard: Some(AffectedRowGuard::exactly(1)),
    }];
    // Order-sensitive pair — see `insert_document_statements`'s adjacency
    // contract: the map upsert's `last_insert_rowid()` must read back the
    // FTS insert immediately before it.
    for statement in insert_document_statements("fts_entities", &entity_fts_document(&entity)) {
        statements.push(PlanStatement {
            statement,
            guard: None,
        });
    }

    Ok(AtomicOpPlan::AddEntity(AddEntityPlan {
        entity_id: entity.id,
        statements,
        post_commit: PostCommitEffect::ReindexEntity {
            entity_id: entity.id,
        },
    }))
}

/// Build the prepared plan for an `AddNote` proposal change. Mirrors
/// [`prepare_add_entity`]'s shape and the same
/// `kind`-already-canonicalized split. `annotates` is out of scope: the
/// proposal `NoteDraft` this backs carries no annotates targets, unlike
/// `KhiveRuntime::create_note`'s general-purpose signature.
pub async fn prepare_add_note(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    args: &Value,
) -> RuntimeResult<AtomicOpPlan> {
    let kind = require_str(args, "kind")?;
    let content = require_str(args, "content")?;
    runtime.validate_note_kind(kind)?;

    let name = optional_create_string(args, "name")?;
    let properties = optional_properties(args, "properties")?;
    // Same note-write validator `create_note_inner` runs: this path builds its
    // args itself and dispatches no pack hook, so without this call a proposal
    // changeset would be the one note-write that stores caller-supplied owned
    // identity properties verbatim. The token here is the applying caller's
    // (threaded in by the apply worker), not the proposer's.
    let properties = runtime.derive_note_write_properties(kind, token, properties)?;

    crate::secret_gate::check_at(content, "note", "content")?;
    if let Some(ref n) = name {
        crate::secret_gate::check_at(n, "note", "name")?;
    }
    if let Some(ref p) = properties {
        crate::secret_gate::check_json_at(p, "note", "properties")?;
    }
    crate::secret_gate::reject_reserved_secret_gate_property(properties.as_ref())?;

    let ns = token.namespace().as_str();
    let mut note = khive_storage::note::Note::new(ns, kind, content);
    if let Some(n) = name {
        note = note.with_name(n);
    }
    if let Some(p) = properties {
        note = note.with_properties(p);
    }

    let mut statements = vec![PlanStatement {
        statement: note_upsert_statement(&note),
        guard: Some(AffectedRowGuard::exactly(1)),
    }];
    // Order-sensitive pair — see `insert_document_statements`'s adjacency
    // contract.
    for statement in insert_document_statements("fts_notes", &note_fts_document(&note)) {
        statements.push(PlanStatement {
            statement,
            guard: None,
        });
    }

    Ok(AtomicOpPlan::AddNote(Box::new(AddNotePlan {
        note_guard: None,
        note_id: note.id,
        statements,
        post_commit: PostCommitEffect::ReindexNote {
            note_id: note.id,
            version: note.version,
        },
    })))
}

// ---------------------------------------------------------------------------
// update
// ---------------------------------------------------------------------------

/// Mirrors `khive-pack-kg::handlers::update::reject_inapplicable_fields`: a
/// hard `InvalidInput` when a caller passes a field that does not apply to
/// the resolved substrate (e.g. `salience` on an entity, or
/// `description` on a note). That function has no dependency edge
/// back to `khive-runtime`, so its exact field-applicability check list and
/// error message shape are reimplemented here rather than imported: same
/// pattern as `optional_string_patch` above. Presence is checked directly on
/// the raw args object (this module has no `UpdateParams` struct); a JSON
/// `null` value is treated as absent, matching `Option<T>` deserialization
/// semantics.
pub(super) fn reject_inapplicable_update_fields(
    args: &Value,
    substrate: &str,
) -> RuntimeResult<()> {
    let o = obj(args)?;
    if substrate == "edge" && o.get("expected_version").is_some_and(|v| !v.is_null()) {
        return Err(RuntimeError::InvalidInput(
            "expected_version applies only to entities and notes".into(),
        ));
    }
    if substrate != "note"
        && ["embed", "fence"]
            .iter()
            .any(|field| o.contains_key(*field))
    {
        return Err(RuntimeError::InvalidInput(
            "embed and fence apply only to notes".into(),
        ));
    }
    let present = |k: &str| o.get(k).is_some_and(|v| !v.is_null());
    let (bad_field, valid): (Option<&str>, &str) = match substrate {
        "entity" => {
            let bad = if present("content") {
                Some("content")
            } else if present("salience") {
                Some("salience")
            } else if present("decay_factor") {
                Some("decay_factor")
            } else if present("relation") {
                Some("relation")
            } else if present("weight") {
                Some("weight")
            } else {
                None
            };
            (bad, "name, description, tags, properties, entity_type")
        }
        "note" => {
            let bad = if present("description") {
                Some("description")
            } else if present("relation") {
                Some("relation")
            } else if present("weight") {
                Some("weight")
            } else if o.contains_key("entity_type") {
                // ADR-014 tri-state: a PRESENT key (including JSON `null`,
                // the explicit clear) is inapplicable to notes.
                Some("entity_type")
            } else {
                None
            };
            (
                bad,
                "name, content, salience, decay_factor, properties, tags",
            )
        }
        // `update` admits `kind="edge"` per `ATOMIC_ADMISSIBLE_VERBS`, so
        // this arm must reject entity/note-only fields (e.g. `name`) on an
        // edge update rather than silently skip the guard, mirroring
        // `khive-pack-kg::handlers::update::reject_inapplicable_fields`'s
        // `KindSpec::Edge` arm.
        "edge" => {
            let bad = if present("name") {
                Some("name")
            } else if present("description") {
                Some("description")
            } else if present("content") {
                Some("content")
            } else if present("tags") {
                Some("tags")
            } else if present("salience") {
                Some("salience")
            } else if present("decay_factor") {
                Some("decay_factor")
            } else if o.contains_key("entity_type") {
                // ADR-014 tri-state: a PRESENT key (including JSON `null`,
                // the explicit clear) is inapplicable to edges.
                Some("entity_type")
            } else {
                None
            };
            (bad, "relation, weight, properties")
        }
        _ => (None, ""),
    };
    if let Some(field) = bad_field {
        let substrate_label = match substrate {
            "entity" => "an entity",
            "note" => "a note",
            "edge" => "an edge",
            other => other,
        };
        return Err(RuntimeError::InvalidInput(format!(
            "field '{field}' is not valid for {substrate_label}; valid fields: {valid}"
        )));
    }
    Ok(())
}

/// Caller-supplied update-kind expectation, resolved via the canonical
/// `resolve_kind_spec` at the kkernel `--atomic` seam: the same pattern
/// [`AtomicDeleteKind`] uses. Without this check, `update(kind="document",
/// id=<concept>)` would be canonically `NotFound` but the atomic path would
/// ignore the explicit kind and mutate the resolved entity anyway.
/// `khive-runtime` must not depend on `khive-pack-kg`, so this is a plain
/// substrate-level shape rather than `khive_pack_kg::handlers::KindSpec`
/// itself: the kkernel seam does the pack-aware resolution and passes down
/// only what `prepare_update` needs to enforce the mismatch check.
pub enum AtomicUpdateKind {
    Entity {
        specific: Option<String>,
        entity_type: Option<String>,
    },
    Note {
        specific: Option<String>,
    },
    Edge,
}

/// Enforce a caller's explicit update-kind discriminator against a resolved
/// note before any pack hook can inspect or normalize the request. Canonical
/// KG dispatch performs this mismatch check before its hook; the atomic
/// adapter calls this same helper to preserve that error ordering.
pub fn validate_note_update_expected_kind(
    note: &khive_storage::note::Note,
    expected_kind: &Option<AtomicUpdateKind>,
) -> RuntimeResult<()> {
    let id = note.id;
    match expected_kind {
        None => Ok(()),
        Some(AtomicUpdateKind::Note {
            specific: Some(expected),
        }) if &note.kind != expected => Err(RuntimeError::NotFound(format!("note {id}"))),
        Some(AtomicUpdateKind::Note { .. }) => Ok(()),
        Some(AtomicUpdateKind::Entity { .. }) => {
            Err(RuntimeError::NotFound(format!("entity {id}")))
        }
        Some(AtomicUpdateKind::Edge) => Err(RuntimeError::NotFound(format!("edge {id}"))),
    }
}

async fn prepare_note_update_plan_from_snapshot(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    args: &Value,
    expected_kind: &Option<AtomicUpdateKind>,
    note: khive_storage::note::Note,
    policy: crate::NoteUpdatePolicy,
    registry: Option<&crate::VerbRegistry>,
) -> RuntimeResult<(khive_storage::Note, UpdatePlan)> {
    let id = require_uuid(args, "id")?;
    if note.id != id {
        return Err(RuntimeError::NotFound(format!("note {id}")));
    }
    validate_note_update_expected_kind(&note, expected_kind)?;

    reject_inapplicable_update_fields(args, "note")?;
    let mut normalized_args = args.clone();
    crate::curation::normalize_note_update_tags(&mut normalized_args)?;
    let args = &normalized_args;
    let name = optional_string_patch(args, "name")?;
    let content = optional_str(args, "content").map(str::to_string);
    let properties = optional_properties(args, "properties")?;
    let salience = optional_f64_patch(args, "salience")?;
    let decay_factor = optional_f64_patch(args, "decay_factor")?;
    let options = crate::note_write::NoteWriteOptions {
        expected_version: obj(args)?
            .get("expected_version")
            .filter(|v| !v.is_null())
            .map(|v| {
                v.as_i64().ok_or_else(|| {
                    RuntimeError::InvalidInput("expected_version must be an integer".into())
                })
            })
            .transpose()?,
        fence: obj(args)?
            .get("fence")
            .map(|v| {
                serde_json::from_value(v.clone())
                    .map_err(|error| RuntimeError::InvalidInput(format!("invalid fence: {error}")))
            })
            .transpose()?,
        embed: obj(args)?
            .get("embed")
            .filter(|v| !v.is_null())
            .map(|v| {
                v.as_bool()
                    .ok_or_else(|| RuntimeError::InvalidInput("embed must be boolean".into()))
            })
            .transpose()?,
        key: None,
    };
    let patch = crate::curation::NotePatch::new(name, content, salience, decay_factor, properties)
        .with_update_policy(policy)
        .with_write_options(options);
    let (updated, mut plan) = runtime
        .prepare_versioned_note_update(token, note.clone(), patch.clone())
        .await?;
    if let Some(registry) = registry {
        attach_note_update_effects(runtime, token, registry, &note, &patch, &mut plan).await?;
    }
    Ok((updated, plan))
}

/// Build an atomic update plan from the exact note snapshot already supplied
/// to a pack update hook. Persistence is guarded by that snapshot's revision
/// and deletion marker, so hook normalization cannot race a second read. The
/// caller must first run the registry normalizer/validator against this snapshot.
/// This shared canonical/atomic seam prepares all patch fields with the owning
/// kind's property policy (the value `VerbRegistry::prepare_note_update_policy`
/// returned for this snapshot), then derives and attaches the owner's typed
/// graph effects. It returns the projected note and one atomic plan, preserving
/// the caller's operation index.
pub async fn prepare_update_from_note_snapshot(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    args: &Value,
    expected_kind: Option<AtomicUpdateKind>,
    note: khive_storage::note::Note,
    policy: crate::NoteUpdatePolicy,
    registry: &crate::VerbRegistry,
) -> RuntimeResult<(khive_storage::Note, AtomicOpPlan)> {
    if obj(args)?.get("entity_kind").is_some_and(|v| !v.is_null()) {
        return Err(RuntimeError::InvalidInput(
            "entity_kind is immutable; to change kind, delete then re-create the entity, \
             or use merge() if this is a deduplication correction"
                .into(),
        ));
    }
    let (note, plan) = prepare_note_update_plan_from_snapshot(
        runtime,
        token,
        args,
        &expected_kind,
        note,
        policy,
        Some(registry),
    )
    .await?;
    Ok((note, AtomicOpPlan::Update(Box::new(plan))))
}

/// The only companion attachment site. Canonical, CLI atomic, and stream note
/// updates all enter through `prepare_update_from_note_snapshot` after running
/// the registry's normalizer/validator on the same snapshot.
async fn attach_note_update_effects(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    registry: &crate::VerbRegistry,
    snapshot: &khive_storage::Note,
    patch: &crate::curation::NotePatch,
    plan: &mut UpdatePlan,
) -> RuntimeResult<()> {
    use crate::atomic_plan::NoteUpdateStatement;
    use crate::NoteUpdateEffect;

    let Some(hook) = registry.find_kind_hook(&snapshot.kind) else {
        return Ok(());
    };
    // Refuse already-known stale input before asking the owner to derive effects.
    // A change after this read remains protected by the note's first CAS statement.
    if let Some(expected) = patch.write_options.expected_version {
        if expected != snapshot.version {
            return Err(crate::note_write::NoteWriteConflict::Version {
                expected,
                current: snapshot.version,
            }
            .into_error()
            .into());
        }
    }
    let current = runtime.notes(token)?.get_note(snapshot.id).await?;
    if !current.is_some_and(|current| {
        current.updated_at == snapshot.updated_at
            && current.deleted_at == snapshot.deleted_at
            && current.version == snapshot.version
    }) {
        return Err(crate::curation::stale_note_snapshot_error(snapshot.id));
    }
    let effects = hook
        .note_update_effects(runtime, token, snapshot, patch)
        .await?;
    if plan.idempotent_noop && !effects.is_empty() {
        return Err(RuntimeError::InvalidInput(
            "an unchanged note update cannot carry graph effects".into(),
        ));
    }
    let edge_token = token.with_namespace(
        crate::Namespace::parse(&snapshot.namespace)
            .map_err(|error| RuntimeError::Internal(format!("invalid note namespace: {error}")))?,
    );
    for effect in effects {
        match effect {
            NoteUpdateEffect::Link(spec) => {
                if spec.source_id != snapshot.id
                    || spec
                        .namespace
                        .as_deref()
                        .is_some_and(|ns| ns != snapshot.namespace)
                {
                    return Err(RuntimeError::InvalidInput(
                        "note update links must originate from the note in its namespace".into(),
                    ));
                }
                let mut args = serde_json::json!({
                    "source_id": spec.source_id, "target_id": spec.target_id,
                    "relation": spec.relation, "weight": spec.weight,
                    "resurrect": spec.resurrect,
                });
                if let Some(metadata) = spec.metadata {
                    args["metadata"] = metadata;
                }
                let AtomicOpPlan::Link(link) = prepare_link(runtime, &edge_token, &args).await?
                else {
                    return Err(RuntimeError::Internal("expected a link plan".into()));
                };
                // A live annotation inserted between the owner's read and this
                // lookup belongs to that writer. Refuse instead of replacing its
                // weight/metadata; a fresh preparation can preserve it explicitly.
                if link.disposition == EdgeUpsertDisposition::Updated {
                    return Err(khive_types::KhiveError::conflict(
                        "a live edge appeared while preparing the note update; retry with fresh state",
                    ).into());
                }
                plan.graph_effects
                    .extend(link.statements.into_iter().map(NoteUpdateStatement::Write));
            }
            NoteUpdateEffect::DeleteEdge(edge) => {
                let id = Uuid::from(edge.id);
                if edge.source_id != snapshot.id
                    || edge.namespace != snapshot.namespace
                    || edge.deleted_at.is_some()
                {
                    return Err(RuntimeError::InvalidInput(
                        "note update deletes must select an outgoing edge in the note namespace"
                            .into(),
                    ));
                }
                plan.graph_effects
                    .push(NoteUpdateStatement::Assert(PlanStatement {
                        statement: khive_db::stores::graph::edge_snapshot_assertion_statement(
                            &edge, false,
                        ),
                        guard: Some(AffectedRowGuard::exactly(1)),
                    }));
                let actor = format!("{}:{}", token.actor().kind, token.actor().id);
                let AtomicOpPlan::Delete(delete) =
                    prepare_delete_edge(&edge_token, id, edge, false, &actor).await?
                else {
                    return Err(RuntimeError::Internal(
                        "expected an edge delete plan".into(),
                    ));
                };
                if delete.post_commit != PostCommitEffect::None {
                    return Err(RuntimeError::Internal(
                        "edge delete has a deferred effect".into(),
                    ));
                }
                plan.graph_effects.extend(
                    delete
                        .statements
                        .into_iter()
                        .map(NoteUpdateStatement::Write),
                );
            }
            NoteUpdateEffect::AssertLink(edge) => {
                if edge.source_id != snapshot.id
                    || edge.namespace != snapshot.namespace
                    || edge.deleted_at.is_some()
                {
                    return Err(RuntimeError::InvalidInput(
                        "note update assertions must select a live outgoing edge in the note namespace".into(),
                    ));
                }
                plan.graph_effects
                    .push(NoteUpdateStatement::Assert(PlanStatement {
                        statement: khive_db::stores::graph::edge_snapshot_assertion_statement(
                            &edge, true,
                        ),
                        guard: Some(AffectedRowGuard::exactly(1)),
                    }));
            }
        }
    }
    Ok(())
}

/// `expected_kind`: `None` when the caller omitted `kind` (no check, parity
/// with canonical's own optional discriminator); `Some(_)` enforces an
/// exact-parity mismatch check against the resolved record's actual
/// substrate/specific kind, mirroring `handle_update`'s
/// `entity.kind != *k` / note kind checks (update.rs:200-201, :229-234).
/// Task notes must pass the GTD pack hook before this or
/// `prepare_op("update", ..)`; neither runs it.
pub async fn prepare_update(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    args: &Value,
    expected_kind: Option<AtomicUpdateKind>,
) -> RuntimeResult<AtomicOpPlan> {
    let id = require_uuid(args, "id")?;

    // Mirrors update.rs's entity_kind immutability guard: entity_kind is a
    // legacy top-level field, independent of the `kind` substrate
    // discriminator handled elsewhere.
    if obj(args)?.get("entity_kind").is_some_and(|v| !v.is_null()) {
        return Err(RuntimeError::InvalidInput(
            "entity_kind is immutable; to change kind, delete then re-create the entity, \
             or use merge() if this is a deduplication correction"
                .into(),
        ));
    }

    match runtime.resolve_by_id(token, id).await? {
        Some(Resolved::Entity(entity)) => {
            match &expected_kind {
                None => {}
                Some(AtomicUpdateKind::Entity {
                    specific: Some(expected),
                    ..
                }) if &entity.kind != expected => {
                    return Err(RuntimeError::NotFound(format!("entity {id}")));
                }
                Some(AtomicUpdateKind::Entity { .. }) => {}
                Some(AtomicUpdateKind::Note { .. }) => {
                    return Err(RuntimeError::NotFound(format!("note {id}")));
                }
                Some(AtomicUpdateKind::Edge) => {
                    return Err(RuntimeError::NotFound(format!("edge {id}")));
                }
            }
            if let Some(AtomicUpdateKind::Entity {
                entity_type: Some(expected),
                ..
            }) = &expected_kind
            {
                if entity
                    .entity_type
                    .as_deref()
                    .is_some_and(|actual| actual != expected.as_str())
                {
                    return Err(RuntimeError::NotFound(format!("entity {id}")));
                }
            }
            refuse_pack_registry_tags(&entity.tags, "update")?;
            // Decide step lives in curation.rs's `prepare_update_entity` —
            // the SAME function canonical `update_entity` calls. Only the
            // arg-extraction (raw JSON -> `EntityPatch`) and the plan-shape
            // wiring are atomic-path-specific: the domain object becomes a
            // `PlanStatement` via `entity_replace_if_unchanged_statement`,
            // whose CAS predicate binds the snapshot's revision and deletion
            // marker, under an exactly-one affected-row guard.
            reject_inapplicable_update_fields(args, "entity")?;
            let name = entity_name_patch(args)?;
            let description = optional_string_patch(args, "description")?;
            let properties = optional_properties(args, "properties")?;
            let tags = optional_tags(args)?;
            if let Some(ref tags) = tags {
                refuse_pack_registry_tags(tags, "update")?;
            }
            let entity_type = optional_entity_type_patch(args, "entity_type")?;

            let expected_version = obj(args)?
                .get("expected_version")
                .filter(|v| !v.is_null())
                .map(|value| {
                    value.as_i64().ok_or_else(|| {
                        RuntimeError::InvalidInput("expected_version must be an integer".into())
                    })
                })
                .transpose()?;
            let required_entity_type = match &expected_kind {
                Some(AtomicUpdateKind::Entity { entity_type, .. }) => entity_type.as_deref(),
                _ => None,
            };
            prepare_update_entity_plan_with_version_and_type(
                runtime,
                token,
                id,
                crate::curation::EntityPatch {
                    name,
                    description,
                    properties,
                    tags,
                    entity_type,
                },
                expected_version,
                required_entity_type,
            )
            .await
        }
        Some(Resolved::Note(note)) => {
            // Patch application lives in curation.rs's
            // `prepare_update_note_from_snapshot` — the same implementation
            // canonical guarded update calls, including salience/decay range
            // validation. The plan retains this exact snapshot's revision.
            let (_, plan) = prepare_note_update_plan_from_snapshot(
                runtime,
                token,
                args,
                &expected_kind,
                note,
                crate::NoteUpdatePolicy::default(),
                None,
            )
            .await?;
            Ok(AtomicOpPlan::Update(Box::new(plan)))
        }
        Some(_) => Err(RuntimeError::InvalidInput(format!(
            "update target {id} must be an entity, note, or edge"
        ))),
        // `Resolved` (khive-runtime::operations) has no `Edge` variant — an
        // id that isn't an entity/note/pack-private/event record is checked
        // against the graph store directly, mirroring
        // `khive-pack-kg::handlers::KgPack::infer_kind_from_uuid`'s own
        // entity/note-then-edge fallback order. `update` admits
        // `kind="edge"`, so this arm must be able to build a plan for one.
        None => match &expected_kind {
            Some(AtomicUpdateKind::Entity { .. }) => {
                Err(RuntimeError::NotFound(format!("entity/note {id}")))
            }
            Some(AtomicUpdateKind::Note { .. }) => {
                Err(RuntimeError::NotFound(format!("entity/note {id}")))
            }
            Some(AtomicUpdateKind::Edge) | None => match runtime.get_edge(token, id).await? {
                Some(edge) => prepare_update_edge(runtime, token, id, edge, args).await,
                None => Err(RuntimeError::NotFound(format!("entity/note/edge {id}"))),
            },
        },
    }
}

/// Build an entity update plan from a typed patch. Proposal changesets use
/// this entry point so their explicit `description: null` clear operation is
/// preserved instead of being collapsed by raw verb deserialization.
pub async fn prepare_update_entity_plan(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
    patch: crate::curation::EntityPatch,
) -> RuntimeResult<AtomicOpPlan> {
    prepare_update_entity_plan_with_version(runtime, token, id, patch, None).await
}

pub(super) async fn prepare_update_entity_plan_with_version_and_type(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
    patch: crate::curation::EntityPatch,
    expected_version: Option<i64>,
    required_entity_type: Option<&str>,
) -> RuntimeResult<AtomicOpPlan> {
    crate::entity_write::validate_expected_version(expected_version)?;
    let explicit_entity_type_patch = patch.entity_type.is_some();
    let (entity, reindex_required, changed_fields, expected_updated_at, expected_deleted_at) =
        runtime.prepare_update_entity(token, id, patch).await?;
    if required_entity_type.is_some_and(|expected| {
        (explicit_entity_type_patch || entity.entity_type.is_some())
            && entity.entity_type.as_deref() != Some(expected)
    }) {
        return Err(RuntimeError::InvalidInput(
            "kind subtype contradicts the requested entity_type update".into(),
        ));
    }
    let mut statements = vec![PlanStatement {
        statement: entity_replace_if_unchanged_statement(
            &entity,
            expected_updated_at,
            expected_deleted_at,
        ),
        guard: Some(AffectedRowGuard::exactly(1)),
    }];
    statements.extend(event_append_statements(
        token,
        &entity.namespace,
        "update",
        EventKind::EntityUpdated,
        SubstrateKind::Entity,
        id,
        serde_json::json!({
            "id": id,
            "namespace": entity.namespace,
            "changed_fields": changed_fields,
        }),
    )?);
    let post_commit = if reindex_required {
        PostCommitEffect::ReindexEntity { entity_id: id }
    } else {
        PostCommitEffect::None
    };
    Ok(AtomicOpPlan::Update(Box::new(UpdatePlan {
        graph_effects: Vec::new(),
        note_vector_purge: None,
        note_embedding_inheritance: None,
        entity_guard: expected_version.map(|expected_version| {
            crate::entity_write::EntityWriteGuard {
                id,
                expected_version,
            }
        }),
        note_guard: None,
        target_id: id,
        statements,
        post_commit,
        edge_natural_key: None,
        idempotent_noop: false,
    })))
}
