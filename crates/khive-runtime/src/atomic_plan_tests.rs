use super::*;

fn stmt(label: &str) -> SqlStatement {
    SqlStatement {
        sql: "UPDATE t SET x = ? WHERE id = ?".to_string(),
        params: vec![],
        label: Some(label.to_string()),
    }
}

fn guarded(label: &str, guard: AffectedRowGuard) -> PlanStatement {
    PlanStatement {
        statement: stmt(label),
        guard: Some(guard),
    }
}

fn unguarded(label: &str) -> PlanStatement {
    PlanStatement {
        statement: stmt(label),
        guard: None,
    }
}

#[test]
fn affected_row_guard_exactly_holds_only_for_n() {
    let g = AffectedRowGuard::exactly(1);
    assert!(!g.holds_for(0));
    assert!(g.holds_for(1));
    assert!(!g.holds_for(2));
}

#[test]
fn affected_row_guard_at_least_one_has_no_upper_bound() {
    let g = AffectedRowGuard::at_least_one();
    assert!(!g.holds_for(0));
    assert!(g.holds_for(1));
    assert!(g.holds_for(1_000));
}

#[test]
fn update_plan_guard_is_anchored_to_the_row_statement_not_the_fts_mirror() {
    let id = Uuid::new_v4();
    let plan = UpdatePlan {
        graph_effects: Vec::new(),
        entity_guard: None,
        note_guard: None,
        note_vector_purge: None,
        note_embedding_inheritance: None,
        target_id: id,
        statements: vec![
            guarded("update-row", AffectedRowGuard::exactly(1)),
            unguarded("update-fts-mirror"),
        ],
        post_commit: PostCommitEffect::ReindexEntity { entity_id: id },
        edge_natural_key: None,
        idempotent_noop: false,
    };
    assert_eq!(plan.target_id, id);
    assert_eq!(plan.statements[0].guard, Some(AffectedRowGuard::exactly(1)));
    assert_eq!(plan.statements[1].guard, None);
    assert_eq!(
        plan.post_commit,
        PostCommitEffect::ReindexEntity { entity_id: id }
    );
}

#[test]
fn delete_plan_guard_is_anchored_to_the_target_row_not_the_cascade() {
    let plan = DeletePlan {
        target_id: Uuid::new_v4(),
        post_commit: PostCommitEffect::None,
        statements: vec![
            guarded("delete-row", AffectedRowGuard::exactly(1)),
            unguarded("cascade-edges"),
        ],
    };
    let row_guard = plan.statements[0].guard.expect("row delete is guarded");
    assert!(row_guard.holds_for(1));
    assert!(!row_guard.holds_for(0));
    assert_eq!(plan.statements[1].guard, None);
}

#[test]
fn link_plan_guard_is_the_endpoint_existence_probe_itself() {
    let source = Uuid::new_v4();
    let target = Uuid::new_v4();
    let plan = LinkPlan {
        source_id: source,
        target_id: target,
        statements: vec![guarded(
            "insert-edge-where-exists",
            AffectedRowGuard::exactly(1),
        )],
        disposition: khive_storage::EdgeUpsertDisposition::Created,
        dependency_kind_inference_hint: None,
    };
    assert_eq!(plan.source_id, source);
    assert_eq!(plan.target_id, target);
    // Dangling-edge acceptance criterion: once an endpoint row is gone,
    // the guarded INSERT...WHERE EXISTS affects 0 rows and the guard
    // on *that exact statement* must fail, not silently pass.
    let guard = plan.statements[0].guard.expect("link insert is guarded");
    assert!(!guard.holds_for(0));
}

#[test]
fn merge_plan_rewires_are_never_guarded_lifecycle_writes_always_are() {
    let into = Uuid::new_v4();
    let from = Uuid::new_v4();
    let rewire = PlanPredicate {
        description: "source_id = :from".to_string(),
        statement: SqlStatement {
            sql: "UPDATE graph_edges SET source_id = ? WHERE source_id = ?".to_string(),
            params: vec![],
            label: Some("merge-rewire".to_string()),
        },
    };
    let plan = MergePlan {
        into_id: into,
        from_id: from,
        rewires: vec![rewire],
        lifecycle: vec![guarded(
            "tombstone-from-entity",
            AffectedRowGuard::exactly(1),
        )],
    };
    assert_eq!(plan.into_id, into);
    assert_eq!(plan.from_id, from);
    assert_eq!(plan.rewires[0].description, "source_id = :from");
    // A predicate-based rewire may legitimately touch zero or many rows
    // depending on how many edges an earlier in-file op inserted — the
    // type carries no guard field for it at all.
    let lifecycle_guard = plan.lifecycle[0].guard.expect("lifecycle write is guarded");
    assert!(!lifecycle_guard.holds_for(0));
}

#[test]
fn gtd_transition_plan_triggers_no_reindex_by_construction() {
    let plan = GtdTransitionPlan {
        task_id: Uuid::new_v4(),
        statements: vec![guarded("update-status", AffectedRowGuard::exactly(1))],
        idempotent_noop: false,
        post_commit: PostCommitEffect::None,
    };
    // A status-only transition never triggers a reindex: the type has no
    // *reindex* post-commit variant to construct, only the best-effort
    // `GtdAudit` lifecycle-audit effect, which itself does no embedding work.
    assert_eq!(plan.statements.len(), 1);
    assert!(plan.statements[0].guard.is_some());
    assert_eq!(plan.post_commit, PostCommitEffect::None);
}

#[test]
fn gtd_transition_plan_idempotent_noop_carries_guarded_assertion_and_no_audit() {
    // Atomic v1 keeps current == target mutation-free and audit-free, but
    // must revalidate the prepare snapshot in a multi-op unit. A canonical
    // no-op carrying a note has a separate note-event contract.
    let plan = GtdTransitionPlan {
        task_id: Uuid::new_v4(),
        statements: vec![guarded(
            "assert-noop-snapshot",
            AffectedRowGuard::exactly(1),
        )],
        idempotent_noop: true,
        post_commit: PostCommitEffect::None,
    };
    assert_eq!(plan.statements.len(), 1);
    assert!(plan.statements[0].guard.is_some());
    assert!(plan.is_idempotent_noop());
    assert_eq!(plan.post_commit, PostCommitEffect::None);
}

#[test]
fn gtd_transition_plan_carries_gtd_audit_post_commit_effect() {
    let task_id = Uuid::new_v4();
    let plan = GtdTransitionPlan {
        task_id,
        statements: vec![guarded("update-status", AffectedRowGuard::exactly(1))],
        idempotent_noop: false,
        post_commit: PostCommitEffect::GtdAudit {
            task_id,
            from_status: "inbox".to_string(),
            to_status: "next".to_string(),
            note: Some("handed off".to_string()),
            namespace: "local".to_string(),
        },
    };
    assert_eq!(
        plan.post_commit,
        PostCommitEffect::GtdAudit {
            task_id,
            from_status: "inbox".to_string(),
            to_status: "next".to_string(),
            note: Some("handed off".to_string()),
            namespace: "local".to_string(),
        }
    );
}

#[test]
fn gtd_complete_plan_guards_the_single_snapshot_property_write() {
    let plan = GtdCompletePlan {
        task_id: Uuid::new_v4(),
        statements: vec![guarded(
            "update-status-and-completed-at",
            AffectedRowGuard::exactly(1),
        )],
        post_commit: PostCommitEffect::None,
    };
    assert_eq!(plan.statements.len(), 1);
    let guard = plan.statements[0]
        .guard
        .expect("snapshot property write is guarded");
    assert!(guard.holds_for(1));
}

#[test]
fn governance_plan_covers_all_three_lifecycle_ops() {
    for op in [
        GovernanceOp::Propose,
        GovernanceOp::Review,
        GovernanceOp::Withdraw,
    ] {
        let plan = GovernancePlan {
            op,
            proposal_id: Uuid::new_v4(),
            statements: vec![guarded("governance-event", AffectedRowGuard::exactly(1))],
        };
        assert_eq!(plan.op, op);
        assert!(plan.statements[0].guard.is_some());
    }
}

#[test]
fn add_entity_plan_guard_is_anchored_to_the_row_statement_not_the_fts_mirror() {
    let id = Uuid::new_v4();
    let plan = AddEntityPlan {
        entity_id: id,
        statements: vec![
            guarded("entity-insert", AffectedRowGuard::exactly(1)),
            unguarded("entity-fts-insert"),
        ],
        post_commit: PostCommitEffect::ReindexEntity { entity_id: id },
    };
    assert_eq!(plan.entity_id, id);
    assert_eq!(plan.statements[0].guard, Some(AffectedRowGuard::exactly(1)));
    assert_eq!(plan.statements[1].guard, None);
    assert_eq!(
        plan.post_commit,
        PostCommitEffect::ReindexEntity { entity_id: id }
    );
}

#[test]
fn add_note_plan_guard_is_anchored_to_the_row_statement_not_the_fts_mirror() {
    let id = Uuid::new_v4();
    let plan = AddNotePlan {
        note_guard: None,
        note_id: id,
        statements: vec![
            guarded("note-insert", AffectedRowGuard::exactly(1)),
            unguarded("note-fts-insert"),
        ],
        post_commit: PostCommitEffect::ReindexNote {
            note_id: id,
            version: 1,
        },
    };
    assert_eq!(plan.note_id, id);
    assert_eq!(plan.statements[0].guard, Some(AffectedRowGuard::exactly(1)));
    assert_eq!(plan.statements[1].guard, None);
    assert_eq!(
        plan.post_commit,
        PostCommitEffect::ReindexNote {
            note_id: id,
            version: 1
        }
    );
}

#[test]
fn plans_are_plain_data_no_async_no_embedding() {
    // Documents a compile-time property: every plan type above derives
    // only Debug/Clone/PartialEq, never Future or an embedding-provider
    // trait. Plans must stay inert data: flag any edit that adds an
    // async method or embedding-model field to one of these types.
    let _ = PostCommitEffect::None;
}
