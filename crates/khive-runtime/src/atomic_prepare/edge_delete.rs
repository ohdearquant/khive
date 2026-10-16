use super::{
    canonical_edge_endpoints, delete_record_attachments_statement, edge_hard_delete_statement,
    edge_replace_if_unchanged_statement, edge_soft_delete_statement,
    edge_symmetric_absorb_or_update_inplace_statement, edge_symmetric_delete_if_conflict_statement,
    entity_hard_delete_statement, entity_soft_delete_statement, event_append_statements,
    hard_delete_lineage_warning_statements, note_hard_delete_statement, note_soft_delete_statement,
    obj, optional_f64, optional_properties, optional_str, parse_edge_relation,
    purge_incident_edges_statement, push_index_purge_statements, refuse_pack_registry_tags,
    reject_inapplicable_update_fields, require_uuid, AffectedRowGuard, AtomicOpPlan,
    AttachmentSubstrate, DeletePlan, EdgeNaturalKey, EventKind, KhiveRuntime, NamespaceToken,
    PlanStatement, PostCommitEffect, Resolved, RuntimeError, RuntimeResult, SubstrateKind,
    UpdatePlan, Uuid, Value,
};

/// Edge branch of `prepare_update`. Mirrors `KhiveRuntime::update_edge`'s
/// patch semantics: `relation`/`weight`/`properties` are the only applicable
/// fields, a changed `relation` is endpoint-validated first, `weight` is
/// range-checked, and `properties` REPLACES `metadata` wholesale (no merge).
/// See `docs/api/atomic_prepare.md#prepare_update_edge` for the DML-shape parity
/// detail with `update_edge`.
///
/// Invariant (symmetric relations `competes_with`/`composed_with`): this
/// function must never branch on a prepare-time conflict probe — a different
/// op in the same atomic unit could change the conflict landscape between
/// probe and commit, making any such branch stale by construction. It always
/// emits BOTH statements from [`edge_symmetric_delete_if_conflict_statement`]
/// and [`edge_symmetric_absorb_or_update_inplace_statement`], each carrying
/// its own commit-time `WHERE`/`CASE WHEN` predicate that re-evaluates the
/// conflict condition fresh inside the transaction. This function reads no
/// state to guess a surviving id; the plan instead carries `edge_natural_key`
/// so a post-commit caller derives the actual surviving id from the
/// committed row, never from a value computed before the rest of this atomic
/// unit has even run.
pub(super) async fn prepare_update_edge(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
    mut edge: khive_storage::types::Edge,
    args: &Value,
) -> RuntimeResult<AtomicOpPlan> {
    reject_inapplicable_update_fields(args, "edge")?;

    let expected_updated_at = edge.updated_at;
    let expected_deleted_at = edge.deleted_at;

    let relation_raw = optional_str(args, "relation");
    let weight = optional_f64(args, "weight")?;
    let properties = optional_properties(args, "properties")?;

    if let Some(ref p) = properties {
        crate::secret_gate::check_json_at(p, "edge", "properties")?;
    }
    crate::secret_gate::reject_reserved_secret_gate_property(properties.as_ref())?;

    let namespace = edge.namespace.clone();
    let record_tok = token.with_namespace(
        khive_types::Namespace::parse(&namespace)
            .map_err(|e| RuntimeError::Internal(format!("edge namespace invalid: {e}")))?,
    );

    let mut changed_fields: Vec<&'static str> = Vec::new();
    if let Some(raw) = relation_raw {
        let relation = parse_edge_relation(raw)?;
        runtime
            .validate_edge_relation_endpoints(&record_tok, edge.source_id, edge.target_id, relation)
            .await?;
        edge.relation = relation;
        changed_fields.push("relation");
    }
    if let Some(w) = weight {
        if !khive_types::validate_edge_weight(w) {
            return Err(RuntimeError::InvalidInput(format!(
                "edge weight must be a finite value in [0.0, 1.0]; got {w}"
            )));
        }
        edge.weight = w;
        changed_fields.push("weight");
    }
    if let Some(p) = properties {
        edge.metadata = Some(p);
        changed_fields.push("properties");
    }

    let (canon_src, canon_tgt) =
        canonical_edge_endpoints(edge.relation, edge.source_id, edge.target_id);
    let now = chrono::Utc::now();

    let mut statements: Vec<PlanStatement> = Vec::new();
    let mut edge_natural_key: Option<EdgeNaturalKey> = None;

    if edge.relation.is_symmetric() {
        // The write for a symmetric relation never branches on a
        // prepare-time probe result: it always carries both self-guarding,
        // commit-time-predicate statements (see their doc comment in
        // khive-db's graph.rs for the full rationale). This avoids the
        // staleness window a prepare-time probe would expose: an earlier op
        // in the same atomic unit could change the conflict landscape before
        // commit. Canonical's own probe-then-branch
        // `update_edge_symmetric_dml` has no such exposure (single
        // transaction, no interleaving) and is unaffected.
        let metadata_str = edge
            .metadata
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default());

        // `updated_at` must strictly advance past the snapshot even when two
        // operations land inside one clock microsecond; saturating to
        // i64::MAX would let the CAS accept a write without advancing its
        // revision, so that is not a valid fallback (mirrors the note path).
        let minimum_updated_at_micros = expected_updated_at
            .timestamp_micros()
            .checked_add(1)
            .ok_or_else(|| {
                RuntimeError::Internal(format!(
                    "edge {id} updated_at is already at i64::MAX and cannot advance"
                ))
            })?;
        let symmetric_updated_at_micros = now.timestamp_micros().max(minimum_updated_at_micros);
        let expected_deleted_at_micros = expected_deleted_at.map(|v| v.timestamp_micros());

        statements.push(PlanStatement {
            statement: edge_symmetric_delete_if_conflict_statement(
                &namespace,
                id,
                canon_src,
                canon_tgt,
                edge.relation,
                expected_updated_at.timestamp_micros(),
                expected_deleted_at_micros,
            ),
            guard: Some(AffectedRowGuard {
                expected_min: 0,
                expected_max: Some(1),
            }),
        });
        statements.push(PlanStatement {
            statement: edge_symmetric_absorb_or_update_inplace_statement(
                &namespace,
                id,
                canon_src,
                canon_tgt,
                edge.relation,
                edge.weight,
                symmetric_updated_at_micros,
                metadata_str.as_deref(),
                edge.target_backend.as_deref(),
                expected_updated_at.timestamp_micros(),
                expected_deleted_at_micros,
            ),
            guard: Some(AffectedRowGuard::exactly(1)),
        });

        // No prepare-time read needed: the two statements above are
        // self-guarding at commit time (see their doc comment). Post-commit
        // result rendering derives the actual surviving id from THIS
        // natural key, never from a value computed here.
        edge_natural_key = Some(EdgeNaturalKey {
            namespace: namespace.clone(),
            canon_source_id: canon_src,
            canon_target_id: canon_tgt,
            relation: edge.relation,
        });
    } else {
        // Non-symmetric: guarded replace of the read snapshot rather than
        // `graph.upsert_edge`'s unconditional natural-key upsert — a zero
        // affected-row result now means a concurrent writer moved this edge
        // between PREPARE and commit, and the atomic unit must roll back
        // instead of silently overwriting it.
        //
        // `updated_at` must strictly advance past the snapshot even when two
        // operations land inside one clock microsecond; saturating to
        // i64::MAX would let the CAS accept a write without advancing its
        // revision, so that is not a valid fallback (mirrors the note path).
        let minimum_updated_at_micros = expected_updated_at
            .timestamp_micros()
            .checked_add(1)
            .ok_or_else(|| {
                RuntimeError::Internal(format!(
                    "edge {id} updated_at is already at i64::MAX and cannot advance"
                ))
            })?;
        let now_micros = now.timestamp_micros().max(minimum_updated_at_micros);
        edge.updated_at = chrono::DateTime::from_timestamp_micros(now_micros).ok_or_else(|| {
            RuntimeError::Internal(format!(
                "edge {id}: computed updated_at {now_micros} is not a valid timestamp"
            ))
        })?;
        statements.push(PlanStatement {
            statement: edge_replace_if_unchanged_statement(
                &edge,
                expected_updated_at,
                expected_deleted_at,
            ),
            guard: Some(AffectedRowGuard::exactly(1)),
        });
    }

    // Mirrors `update_edge`'s unconditional post-mutation `EdgeUpdated`
    // event append, keyed on the original `edge_id` the caller supplied:
    // canonical does the same (the event target is `edge_id`, not the
    // post-absorption surviving id).
    statements.extend(event_append_statements(
        token,
        &namespace,
        "update",
        EventKind::EdgeUpdated,
        SubstrateKind::Entity,
        id,
        serde_json::json!({"id": id, "namespace": namespace, "changed_fields": changed_fields}),
    )?);

    Ok(AtomicOpPlan::Update(Box::new(UpdatePlan {
        graph_effects: Vec::new(),
        note_vector_purge: None,
        note_embedding_inheritance: None,
        entity_guard: None,
        note_guard: None,
        target_id: id,
        statements,
        post_commit: PostCommitEffect::None,
        edge_natural_key,
        idempotent_noop: false,
    })))
}

// ---------------------------------------------------------------------------
// delete
// ---------------------------------------------------------------------------

/// Caller-supplied delete-kind expectation, resolved via the canonical
/// `resolve_kind_spec` at the kkernel `--atomic` seam. `khive-runtime` must
/// not depend on `khive-pack-kg` (packs depend on the runtime, not the other
/// way around), so this is a plain substrate-level shape rather than
/// `khive_pack_kg::handlers::KindSpec` itself: the kkernel seam does the
/// pack-aware `resolve_kind_spec` resolution (which needs a `VerbRegistry`,
/// unreachable from this crate) and passes down only what `prepare_delete`
/// needs to enforce the mismatch check.
///
/// `delete` admits `kind="edge"` per `ATOMIC_ADMISSIBLE_VERBS`, hence the
/// `Edge` variant. `Event`/`Proposal` remain rejected at the kkernel seam
/// (not v1-admissible for atomic delete at all).
pub enum AtomicDeleteKind {
    Entity {
        specific: Option<String>,
        entity_type: Option<String>,
    },
    Note {
        specific: Option<String>,
    },
    Edge,
}

/// `expected_kind`: `None` when the caller omitted `kind` (no check, parity
/// with canonical's own optional discriminator); `Some(_)` enforces an
/// exact-parity mismatch check against the resolved record's actual
/// substrate/specific kind, mirroring `handle_delete`'s
/// `entity.kind != *expected` / `note.kind != *expected` checks.
pub async fn prepare_delete(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    args: &Value,
    expected_kind: Option<AtomicDeleteKind>,
) -> RuntimeResult<AtomicOpPlan> {
    let id = require_uuid(args, "id")?;
    let actor = format!("{}:{}", token.actor().kind, token.actor().id);
    let hard = obj(args)?
        .get("hard")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // `delete(id, hard=true)` is the public purge route after a prior soft
    // delete, so it must resolve including already-tombstoned rows (a
    // live-only resolve would never find one). Soft delete keeps the
    // live-only resolve: a soft delete of an already-tombstoned row is a
    // no-op, matching non-atomic behavior.
    let resolved = if hard {
        runtime.resolve_by_id_including_deleted(token, id).await?
    } else {
        runtime.resolve_by_id(token, id).await?
    };

    match resolved {
        Some(Resolved::Entity(entity)) => {
            match &expected_kind {
                None => {}
                Some(AtomicDeleteKind::Entity {
                    specific: Some(expected),
                    ..
                }) if &entity.kind != expected => {
                    return Err(RuntimeError::NotFound(format!("{expected} {id}")));
                }
                Some(AtomicDeleteKind::Entity { .. }) => {}
                Some(AtomicDeleteKind::Note { .. }) => {
                    return Err(RuntimeError::NotFound(format!("note {id}")));
                }
                Some(AtomicDeleteKind::Edge) => {
                    return Err(RuntimeError::NotFound(format!("edge {id}")));
                }
            }
            if let Some(AtomicDeleteKind::Entity {
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
            refuse_pack_registry_tags(&entity.tags, "delete")?;
            let namespace = entity.namespace.clone();
            // Storage parity: `entity_soft_delete_statement`/
            // `entity_hard_delete_statement` are the SAME khive-db builders
            // khive-db's own `SqlEntityStore::delete_entity` calls — no DML
            // text is hand-duplicated here.
            let mut statements = if hard {
                vec![
                    PlanStatement {
                        statement: delete_record_attachments_statement(
                            id,
                            AttachmentSubstrate::Entity,
                        ),
                        guard: None,
                    },
                    PlanStatement {
                        statement: entity_hard_delete_statement(id),
                        guard: Some(AffectedRowGuard::exactly(1)),
                    },
                ]
            } else {
                let deleted_at = chrono::Utc::now().timestamp_micros();
                vec![PlanStatement {
                    statement: entity_soft_delete_statement(id, deleted_at),
                    guard: Some(AffectedRowGuard::exactly(1)),
                }]
            };
            if hard {
                statements.extend(
                    hard_delete_lineage_warning_statements(
                        &namespace,
                        &actor,
                        id,
                        SubstrateKind::Entity,
                    )
                    .into_iter()
                    .map(|statement| PlanStatement {
                        statement,
                        guard: None,
                    }),
                );
                // Same builder canonical `delete_entity`'s hard-delete
                // cascade calls (`graph.purge_incident_edges`).
                statements.push(PlanStatement {
                    statement: purge_incident_edges_statement(id),
                    guard: None,
                });
            }
            // FTS + vector index purge, matching operations.rs
            // `delete_entity`: both soft and hard delete clean indexes (a
            // hard delete of an already-tombstoned record must still purge
            // them); only hard additionally cascades edges above.
            push_index_purge_statements(
                runtime,
                &mut statements,
                "fts_entities",
                &namespace,
                id,
                "atomic-delete-entity",
            )
            .await?;
            // operations.rs's `delete_entity` appends an `EntityDeleted`
            // event after a successful row delete, on both soft and hard
            // delete. `apply_plan` never reaches this statement unless the
            // guarded row statement above affected a row, so no extra `if`
            // is needed here.
            statements.extend(event_append_statements(
                token,
                &namespace,
                "delete",
                EventKind::EntityDeleted,
                SubstrateKind::Entity,
                id,
                serde_json::json!({"id": id, "namespace": namespace, "hard": hard}),
            )?);
            Ok(AtomicOpPlan::Delete(DeletePlan {
                target_id: id,
                statements,
                post_commit: PostCommitEffect::None,
            }))
        }
        Some(Resolved::Note(note)) => {
            match &expected_kind {
                None => {}
                Some(AtomicDeleteKind::Note {
                    specific: Some(expected),
                }) if &note.kind != expected => {
                    return Err(RuntimeError::NotFound(format!("{expected} {id}")));
                }
                Some(AtomicDeleteKind::Note { .. }) => {}
                Some(AtomicDeleteKind::Entity { .. }) => {
                    return Err(RuntimeError::NotFound(format!("entity {id}")));
                }
                Some(AtomicDeleteKind::Edge) => {
                    return Err(RuntimeError::NotFound(format!("edge {id}")));
                }
            }
            if let Some(error) = runtime.stream_member_error(&note).await? {
                return Err(error);
            }
            let namespace = note.namespace.clone();
            // Storage parity: `note_soft_delete_statement`/
            // `note_hard_delete_statement` are the SAME khive-db builders
            // khive-db's own `SqlNoteStore::delete_note` calls.
            let mut statements = if hard {
                vec![
                    PlanStatement {
                        statement: delete_record_attachments_statement(
                            id,
                            AttachmentSubstrate::Note,
                        ),
                        guard: None,
                    },
                    PlanStatement {
                        statement: note_hard_delete_statement(id),
                        guard: Some(AffectedRowGuard::exactly(1)),
                    },
                ]
            } else {
                let deleted_at = chrono::Utc::now().timestamp_micros();
                vec![PlanStatement {
                    statement: note_soft_delete_statement(id, deleted_at),
                    guard: Some(AffectedRowGuard::exactly(1)),
                }]
            };
            if hard {
                statements.extend(
                    hard_delete_lineage_warning_statements(
                        &namespace,
                        &actor,
                        id,
                        SubstrateKind::Note,
                    )
                    .into_iter()
                    .map(|statement| PlanStatement {
                        statement,
                        guard: None,
                    }),
                );
                statements.push(PlanStatement {
                    statement: purge_incident_edges_statement(id),
                    guard: None,
                });
            }
            // FTS + vector index purge, matching operations.rs
            // `delete_note`: both soft and hard delete clean indexes (a hard
            // delete of an already-tombstoned record must still purge
            // them); only hard additionally cascades edges above.
            push_index_purge_statements(
                runtime,
                &mut statements,
                "fts_notes",
                &namespace,
                id,
                "atomic-delete-note",
            )
            .await?;
            // operations.rs's `delete_note` appends a `NoteDeleted` event
            // after a successful row delete, on both soft and hard delete:
            // same reasoning as the entity branch above.
            statements.extend(event_append_statements(
                token,
                &namespace,
                "delete",
                EventKind::NoteDeleted,
                SubstrateKind::Note,
                id,
                serde_json::json!({"id": id, "namespace": namespace, "hard": hard}),
            )?);
            Ok(AtomicOpPlan::Delete(DeletePlan {
                target_id: id,
                statements,
                // A committed atomic note delete must fire the same
                // pack-installed note-mutation hook `operations.rs::
                // delete_note` fires, so a warm ANN cache sees the deletion
                // even when the mutation went through the atomic-plan path.
                post_commit: PostCommitEffect::NoteDeleted {
                    note_id: id,
                    kind: note.kind.clone(),
                },
            }))
        }
        Some(_) => Err(RuntimeError::InvalidInput(format!(
            "delete target {id} must be an entity, note, or edge"
        ))),
        // `Resolved` has no `Edge` variant (same reasoning as
        // `prepare_update`'s fallback above) — probe the graph store
        // directly.
        None => match &expected_kind {
            Some(AtomicDeleteKind::Entity { .. }) => {
                Err(RuntimeError::NotFound(format!("entity/note {id}")))
            }
            Some(AtomicDeleteKind::Note { .. }) => {
                Err(RuntimeError::NotFound(format!("entity/note {id}")))
            }
            Some(AtomicDeleteKind::Edge) | None => {
                let edge = if hard {
                    runtime.get_edge_including_deleted(token, id).await?
                } else {
                    runtime.get_edge(token, id).await?
                };
                match edge {
                    Some(edge) => prepare_delete_edge(token, id, edge, hard, &actor).await,
                    None => Err(RuntimeError::NotFound(format!("entity/note/edge {id}"))),
                }
            }
        },
    }
}

/// Edge branch of `prepare_delete`. Mirrors
/// `khive-runtime::operations::KhiveRuntime::delete_edge` exactly: hard
/// delete cascades `purge_incident_edges` (any `annotates` edge — or any
/// other edge — pointing AT this edge as a node) BEFORE deleting the edge
/// row itself, then a soft or hard delete statement, then an unconditional
/// `EdgeDeleted` event (edges are never FTS/vector-indexed, so unlike the
/// entity/note branches there is no index purge here — `delete_edge` has
/// none either).
pub(super) async fn prepare_delete_edge(
    token: &NamespaceToken,
    id: Uuid,
    edge: khive_storage::types::Edge,
    hard: bool,
    actor: &str,
) -> RuntimeResult<AtomicOpPlan> {
    let namespace = edge.namespace.clone();
    let mut statements: Vec<PlanStatement> = Vec::new();

    if hard {
        statements.extend(
            hard_delete_lineage_warning_statements(&namespace, actor, id, SubstrateKind::Entity)
                .into_iter()
                .map(|statement| PlanStatement {
                    statement,
                    guard: None,
                }),
        );
        // Mirrors `delete_edge`'s `graph.purge_incident_edges(edge_id)` —
        // unguarded: zero incident edges is a legitimate outcome, not a
        // failure (same reasoning as the entity/note cascade-edges
        // statements above).
        statements.push(PlanStatement {
            statement: purge_incident_edges_statement(id),
            guard: None,
        });
        statements.push(PlanStatement {
            statement: edge_hard_delete_statement(id),
            guard: Some(AffectedRowGuard::exactly(1)),
        });
    } else {
        let now = chrono::Utc::now().timestamp_micros();
        statements.push(PlanStatement {
            statement: edge_soft_delete_statement(id, now),
            guard: Some(AffectedRowGuard::exactly(1)),
        });
    }

    statements.extend(event_append_statements(
        token,
        &namespace,
        "delete",
        EventKind::EdgeDeleted,
        SubstrateKind::Entity,
        id,
        serde_json::json!({"id": id, "namespace": namespace, "hard": hard}),
    )?);

    Ok(AtomicOpPlan::Delete(DeletePlan {
        target_id: id,
        statements,
        post_commit: PostCommitEffect::None,
    }))
}
