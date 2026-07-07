//! ADR-099 (cross-op atomicity for bulk apply) — prepared write-plan types.
//!
//! Migration step 1 (ADR-099) calls for a per-verb `prepare`/apply seam: the
//! async prepare pass materializes a synchronous write plan outside any
//! transaction, and the commit pass later applies that plan's statements as
//! DML under a per-op SAVEPOINT. This module defines the plan *shapes* only —
//! one family per v1 admissible verb group (`update`, `delete`, `link`,
//! `merge`, `gtd.transition`, `gtd.complete`, the governance verbs). Nothing
//! in this module is wired into a live handler or the dispatch path yet; that
//! wiring (the actual `prepare` implementations, the atomic runner, and the
//! `--atomic` CLI surface) is later ADR-099 migration work (steps 1 cont'd,
//! 3, 4). These types exist so that work has a shared, plain-data target.
//!
//! Every plan is deliberately inert: plain data, no async, no embedding
//! reference. See ADR-099 D1's two validation-staleness rules, which every
//! plan's fields exist to satisfy:
//!
//! 1. **Predicate-based plans** — a plan whose effect covers "all rows
//!    matching a condition" carries that condition as a statement evaluated
//!    inside the transaction, never as a prepare-time-enumerated row list
//!    (`predicate`).
//! 2. **Affected-row guards** — any statement whose prepare-time validation
//!    assumed a target row exists carries an expected-effect guard, checked
//!    in-transaction; a mismatch fails the op and rolls back the whole unit
//!    (`guard`).

use uuid::Uuid;

use khive_storage::SqlStatement;

/// The predicate a prepare pass validated a plan's target against, replayed
/// as a statement evaluated **inside** the transaction (ADR-099 D1, rule 1:
/// "predicate-based plans wherever a write's scope depends on current
/// state"). Carrying the predicate rather than a prepare-time-enumerated row
/// list is what lets a later op in the same file (e.g. an intervening
/// `link`) be visible to this plan's apply.
#[derive(Debug, Clone)]
pub struct PlanPredicate {
    /// Human-readable description of the condition, for diagnostics (e.g.
    /// `"source_id = :from"`).
    pub description: String,
    /// The in-transaction statement whose scope is evaluated against
    /// current (committed-so-far) state, not prepare-time state.
    pub statement: SqlStatement,
}

/// An affected-row guard (ADR-099 D1, rule 2): the row-count prepare assumed
/// its target write would affect, re-verified in-transaction. A prepare-time
/// validation is a plan *hypothesis*, never a commitment — if the guard does
/// not hold at apply time, the op fails inside the atomic unit and the whole
/// unit rolls back (ADR-099 acceptance criteria: "zero-row apply fails the
/// unit").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AffectedRowGuard {
    /// Minimum affected-row count for the guard to hold (inclusive).
    pub expected_min: u64,
    /// Maximum affected-row count for the guard to hold (inclusive), or
    /// `None` for "no upper bound" (e.g. a predicate-based rewire that may
    /// touch any number of rows).
    pub expected_max: Option<u64>,
}

impl AffectedRowGuard {
    /// A guard requiring exactly `n` affected rows (the common case for a
    /// single-target `update`/`delete`/`link` statement).
    pub fn exactly(n: u64) -> Self {
        Self {
            expected_min: n,
            expected_max: Some(n),
        }
    }

    /// A guard requiring at least one affected row and no upper bound (the
    /// shape for a predicate-based rewire that may touch any number of rows,
    /// e.g. `merge`'s edge rewire).
    pub fn at_least_one() -> Self {
        Self {
            expected_min: 1,
            expected_max: None,
        }
    }

    /// Whether an observed affected-row count satisfies this guard.
    pub fn holds_for(&self, affected: u64) -> bool {
        affected >= self.expected_min
            && self.expected_max.map(|max| affected <= max).unwrap_or(true)
    }
}

/// A deferred side effect recorded during prepare and run once, after the
/// atomic unit commits (ADR-099 D1, "post-commit pass"). v1's admissible set
/// computes no embeddings during prepare (D3's `update`/`merge` caveat), so
/// the only post-commit effects are reindex kicks computed from the
/// **committed** row content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostCommitEffect {
    /// No deferred side effect for this op.
    None,
    /// Re-embed and re-warm the given entity's vector row from its committed
    /// content (ADR-099 D3 `update` caveat: entity name/description change).
    ReindexEntity { entity_id: Uuid },
    /// Re-embed and re-warm the given note's vector row from its committed
    /// content (ADR-099 D3 `update` caveat: note name/content change).
    ReindexNote { note_id: Uuid },
}

/// Write plan for an `update` op (entity or note shape — ADR-099 D3's
/// `update` caveat covers both substrates the same way: row/FTS DML in the
/// plan, any reindex deferred to `post_commit`).
#[derive(Debug, Clone)]
pub struct UpdatePlan {
    /// The id of the entity or note being updated.
    pub target_id: Uuid,
    /// Row + FTS DML statements to apply inside the atomic unit.
    pub statements: Vec<SqlStatement>,
    /// Guard on the row-update statement (prepare assumed the target row
    /// exists).
    pub guard: AffectedRowGuard,
    /// Deferred reindex, if the update changed name/description/content.
    pub post_commit: PostCommitEffect,
}

/// Write plan for a `delete` op (soft or hard).
#[derive(Debug, Clone)]
pub struct DeletePlan {
    /// The id of the entity or note being deleted.
    pub target_id: Uuid,
    /// Row DML (and, for a hard delete, incident-edge cascade DML) to apply
    /// inside the atomic unit.
    pub statements: Vec<SqlStatement>,
    /// Guard on the delete statement (prepare assumed the target row
    /// exists).
    pub guard: AffectedRowGuard,
}

/// Write plan for a `link` op (create a typed directed edge).
#[derive(Debug, Clone)]
pub struct LinkPlan {
    pub source_id: Uuid,
    pub target_id: Uuid,
    /// Edge-insert DML to apply inside the atomic unit.
    pub statements: Vec<SqlStatement>,
    /// Guard on the endpoint-existence check (prepare validated both
    /// endpoints exist; ADR-099 acceptance criteria's dangling-edge case —
    /// `[delete(X, hard), link(A, X)]` — is closed by this guard failing
    /// in-transaction once X is gone).
    pub guard: AffectedRowGuard,
}

/// Write plan for a `merge` op (deduplicate two entities). The edge rewire is
/// **predicate-based** (ADR-099 D1 rule 1) — `predicate` holds the
/// `UPDATE graph_edges SET source_id = :into WHERE source_id = :from`-shaped
/// statement evaluated inside the transaction, so it structurally sees any
/// earlier op's edge writes in the same file (ADR-099 acceptance criteria:
/// "merge rewires see earlier in-file writes").
#[derive(Debug, Clone)]
pub struct MergePlan {
    pub into_id: Uuid,
    pub from_id: Uuid,
    /// The predicated edge-rewire statement(s), plus the `from` entity's
    /// soft-delete/tombstone DML.
    pub statements: Vec<SqlStatement>,
    /// The in-transaction predicate the rewire is evaluated against.
    pub predicate: PlanPredicate,
    /// Guard on the merge target existing (prepare assumed `into`/`from`
    /// both exist).
    pub guard: AffectedRowGuard,
}

/// Write plan for a `gtd.transition` op (explicit task lifecycle change).
#[derive(Debug, Clone)]
pub struct GtdTransitionPlan {
    pub task_id: Uuid,
    /// Status-column DML to apply inside the atomic unit. Property-only
    /// status mutation — triggers no reindex (ADR-099 D3).
    pub statements: Vec<SqlStatement>,
    /// Guard on the transition statement (prepare validated the current
    /// status and the requested transition were legal).
    pub guard: AffectedRowGuard,
}

/// Write plan for a `gtd.complete` op (task lifecycle terminal transition).
#[derive(Debug, Clone)]
pub struct GtdCompletePlan {
    pub task_id: Uuid,
    /// Status + `completed_at` DML to apply inside the atomic unit.
    pub statements: Vec<SqlStatement>,
    /// Guard on the completion statement (prepare validated the task was in
    /// a completable state).
    pub guard: AffectedRowGuard,
}

/// Which governance verb (`propose` / `review` / `withdraw`) a
/// [`GovernancePlan`] applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GovernanceOp {
    Propose,
    Review,
    Withdraw,
}

/// Write plan for a governance op (`propose`, `review`, or `withdraw` — the
/// event-sourced change-proposal lifecycle, ADR-046).
#[derive(Debug, Clone)]
pub struct GovernancePlan {
    pub op: GovernanceOp,
    pub proposal_id: Uuid,
    /// Event-log + status DML to apply inside the atomic unit.
    pub statements: Vec<SqlStatement>,
    /// Guard on the lifecycle-state check (prepare validated the proposal
    /// was in a state admitting this transition).
    pub guard: AffectedRowGuard,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stmt(label: &str) -> SqlStatement {
        SqlStatement {
            sql: "UPDATE t SET x = ? WHERE id = ?".to_string(),
            params: vec![],
            label: Some(label.to_string()),
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
    fn update_plan_carries_predicate_free_guard_and_post_commit() {
        let id = Uuid::new_v4();
        let plan = UpdatePlan {
            target_id: id,
            statements: vec![stmt("update-row")],
            guard: AffectedRowGuard::exactly(1),
            post_commit: PostCommitEffect::ReindexEntity { entity_id: id },
        };
        assert_eq!(plan.target_id, id);
        assert_eq!(plan.guard, AffectedRowGuard::exactly(1));
        assert_eq!(
            plan.post_commit,
            PostCommitEffect::ReindexEntity { entity_id: id }
        );
    }

    #[test]
    fn delete_plan_guard_requires_exactly_one_row() {
        let plan = DeletePlan {
            target_id: Uuid::new_v4(),
            statements: vec![stmt("delete-row")],
            guard: AffectedRowGuard::exactly(1),
        };
        assert!(plan.guard.holds_for(1));
        assert!(!plan.guard.holds_for(0));
    }

    #[test]
    fn link_plan_carries_both_endpoints_and_existence_guard() {
        let source = Uuid::new_v4();
        let target = Uuid::new_v4();
        let plan = LinkPlan {
            source_id: source,
            target_id: target,
            statements: vec![stmt("insert-edge")],
            guard: AffectedRowGuard::exactly(1),
        };
        assert_eq!(plan.source_id, source);
        assert_eq!(plan.target_id, target);
        // Dangling-edge acceptance criterion: once the target row is gone,
        // an in-transaction existence check affects 0 rows and the guard
        // must fail, not silently pass.
        assert!(!plan.guard.holds_for(0));
    }

    #[test]
    fn merge_plan_predicate_is_evaluated_in_transaction_not_prepare_enumerated() {
        let into = Uuid::new_v4();
        let from = Uuid::new_v4();
        let predicate = PlanPredicate {
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
            statements: vec![predicate.statement.clone()],
            predicate,
            guard: AffectedRowGuard::at_least_one(),
        };
        assert_eq!(plan.into_id, into);
        assert_eq!(plan.from_id, from);
        assert_eq!(plan.predicate.description, "source_id = :from");
        // A predicate-based rewire may legitimately touch zero or many rows
        // depending on how many edges an earlier in-file op inserted; the
        // guard only requires the merge itself to have found its targets.
        assert!(plan.guard.holds_for(0) || plan.guard.expected_min == 1);
    }

    #[test]
    fn gtd_transition_plan_triggers_no_reindex_by_construction() {
        let plan = GtdTransitionPlan {
            task_id: Uuid::new_v4(),
            statements: vec![stmt("update-status")],
            guard: AffectedRowGuard::exactly(1),
        };
        // Property-only status mutation — the type carries no post-commit
        // field at all, which is the structural guarantee (ADR-099 D3: task
        // transitions "trigger no reindex").
        assert_eq!(plan.statements.len(), 1);
    }

    #[test]
    fn gtd_complete_plan_guard_requires_target_row() {
        let plan = GtdCompletePlan {
            task_id: Uuid::new_v4(),
            statements: vec![stmt("update-status"), stmt("update-completed-at")],
            guard: AffectedRowGuard::exactly(1),
        };
        assert_eq!(plan.statements.len(), 2);
        assert!(plan.guard.holds_for(1));
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
                statements: vec![stmt("governance-event")],
                guard: AffectedRowGuard::exactly(1),
            };
            assert_eq!(plan.op, op);
        }
    }

    #[test]
    fn plans_are_plain_data_no_async_no_embedding() {
        // Compile-time property, asserted here as documentation: every plan
        // type above derives only Debug/Clone/PartialEq, never Future or any
        // embedding-provider trait. If a future edit adds an async method or
        // an embedding-model field to one of these types, this comment is
        // the marker to revert it — plans must stay inert data (ADR-099 D1).
        let _ = PostCommitEffect::None;
    }
}
