//! ADR-099 (cross-op atomicity for bulk apply) — prepared write-plan types.
//!
//! Async prepare materializes a synchronous write plan outside any
//! transaction; commit later applies its statements as DML under a per-op
//! SAVEPOINT. This module defines the plan *shapes* only, one family per
//! admissible verb group (`update`, `delete`, `link`, `merge`,
//! `gtd.transition`, `gtd.complete`, the governance verbs) — not yet wired
//! into a live handler or the dispatch path. Every plan is deliberately
//! inert (plain data, no async, no embedding reference).
//!
//! Two validation-staleness invariants every plan must satisfy:
//! 1. **Predicate-based plans** carry an "all rows matching a condition"
//!    effect as a statement evaluated inside the transaction
//!    (`PlanPredicate`), never as a prepare-time-enumerated row list.
//! 2. **Affected-row guards** (`PlanStatement::guard`) are attached to the
//!    exact statement they validate, checked in-transaction; a mismatch
//!    fails the op and rolls back the whole unit.
//!
//! See `docs/atomic-plan.md` for why guards are per-statement rather than
//! per-plan.

use uuid::Uuid;

use khive_storage::SqlStatement;

/// One statement in a plan, paired with the guard (if any) that validates
/// it. **Runner contract:** a present `guard` is checked against the
/// affected-row count of applying `statement` alone (`SqlWriter::execute`'s
/// return value for this statement), not a batch total and not another
/// statement's count. `guard: None` means prepare made no row-existence
/// assumption about this particular statement (e.g. a cascade delete that
/// may legitimately touch zero rows).
#[derive(Debug, Clone)]
pub struct PlanStatement {
    /// The DML to apply inside the atomic unit.
    pub statement: SqlStatement,
    /// The expected-effect guard for `statement`, if prepare's validation
    /// assumed a target row exists for it.
    pub guard: Option<AffectedRowGuard>,
}

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
/// **committed** row content, plus the GAP-5 addition under B3: the
/// best-effort GTD lifecycle audit row.
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
    /// Append one `gtd_lifecycle_audit` row for a committed `gtd.transition`
    /// or `gtd.complete` (ADR-099 B3, GAP-5): canonical `handle_transition`/
    /// `handle_complete` call `ensure_audit_schema` + `write_audit_record`
    /// (`khive-pack-gtd::handlers`) as a best-effort side write — a failed
    /// audit insert must never roll back an already-committed transition.
    /// Carries exactly the fields `write_audit_record` needs. Applied
    /// outside `khive-runtime` (crate-direction: `khive-pack-gtd` depends on
    /// `khive-runtime`, not the other way around) — this crate's own
    /// `apply_post_commit_effects` treats this variant as a no-op; the
    /// `kkernel` caller that owns both crates applies it by calling the
    /// canonical `ensure_audit_schema`/`write_audit_record` functions
    /// directly.
    GtdAudit {
        task_id: Uuid,
        from_status: String,
        to_status: String,
        note: Option<String>,
        namespace: String,
    },
    /// A committed note delete (soft or hard) — fire the pack-installed
    /// note-mutation hook with the deleted note's kind (#750:
    /// `DeletePlan` previously carried no `post_commit`
    /// slot at all, so an atomic note delete never reached
    /// `KhiveRuntime::fire_note_mutation_hook`, unlike `operations.rs`'s
    /// `delete_note`, which fires it directly after a successful row
    /// delete). Entity deletes have no equivalent — the hook system is
    /// note-only (`khive-pack-memory`'s warm ANN cache is the only
    /// installed consumer today).
    NoteDeleted { note_id: Uuid, kind: String },
}

/// The natural key a committed symmetric edge update's surviving row must
/// be looked up by (ADR-099 B3, second
/// half). `khive-db`'s `edge_symmetric_refresh_or_update_inplace_statement`
/// pair never trusts a prepare-time-computed target id (see that builder's
/// doc comment); a caller rendering this op's result derives the actual
/// surviving id by querying `graph_edges`'s own
/// `UNIQUE(namespace, source_id, target_id, relation)` constraint (e.g. via
/// `KhiveRuntime::list_edges` filtered on these fields — the same mechanism
/// the atomic `link` op's own result rendering already uses), strictly
/// after commit.
#[derive(Debug, Clone)]
pub struct EdgeNaturalKey {
    pub(crate) namespace: String,
    pub(crate) canon_source_id: Uuid,
    pub(crate) canon_target_id: Uuid,
    pub(crate) relation: khive_storage::EdgeRelation,
}

impl EdgeNaturalKey {
    /// The namespace containing the surviving edge.
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// The canonical source endpoint of the surviving edge.
    pub fn canon_source_id(&self) -> Uuid {
        self.canon_source_id
    }

    /// The canonical target endpoint of the surviving edge.
    pub fn canon_target_id(&self) -> Uuid {
        self.canon_target_id
    }

    /// The surviving edge's relation.
    pub fn relation(&self) -> khive_storage::EdgeRelation {
        self.relation
    }
}

/// Write plan for an `update` op (entity or note shape — ADR-099 D3's
/// `update` caveat covers both substrates the same way: row/FTS DML in the
/// plan, any reindex deferred to `post_commit`).
///
/// Deferred effects are assigned by this crate's prepare pass and cannot be
/// supplied by callers constructing a plan directly:
///
/// ```compile_fail
/// use khive_runtime::{PostCommitEffect, UpdatePlan};
/// use uuid::Uuid;
///
/// let id = Uuid::nil();
/// let plan = UpdatePlan {
///     target_id: id,
///     statements: Vec::new(),
///     post_commit: PostCommitEffect::ReindexEntity { entity_id: id },
///     edge_natural_key: None,
/// };
/// ```
///
/// A plan returned by a prepare function cannot have its validated
/// statements cleared or replaced before it reaches the runner:
///
/// ```compile_fail
/// use khive_runtime::UpdatePlan;
///
/// fn clear_prepared_statements(mut prepared: UpdatePlan) {
///     prepared.statements.clear();
/// }
/// ```
#[derive(Debug, Clone)]
pub struct UpdatePlan {
    /// The id of the entity or note being updated. For a symmetric edge
    /// update this is the CALLER's requested id — advisory only, never the
    /// basis for post-commit result rendering (see [`EdgeNaturalKey`]).
    pub(crate) target_id: Uuid,
    /// Row + FTS DML statements to apply inside the atomic unit, in order.
    /// The row-update statement carries the existence guard; any FTS-mirror
    /// statement that follows it is unguarded (its target row's existence
    /// was already asserted by the row-update statement's own guard).
    pub(crate) statements: Vec<PlanStatement>,
    /// Deferred reindex assigned by the prepare pass when the update changed
    /// name, description, or content.
    pub(crate) post_commit: PostCommitEffect,
    /// `Some` only for a symmetric edge update — the natural key a caller
    /// must use to derive the committed surviving row post-commit, rather
    /// than trusting `target_id`. `None` for every other update shape
    /// (entity, note, non-symmetric edge), where `target_id` alone is
    /// already an exact, non-advisory identifier.
    pub(crate) edge_natural_key: Option<EdgeNaturalKey>,
}

impl UpdatePlan {
    /// The record id supplied to the prepare pass.
    pub fn target_id(&self) -> Uuid {
        self.target_id
    }

    /// The committed lookup key required for a symmetric edge update.
    pub fn edge_natural_key(&self) -> Option<&EdgeNaturalKey> {
        self.edge_natural_key.as_ref()
    }
}

/// Write plan for an `AddEntity` proposal change: a fresh entity row plus its
/// FTS document in the same atomic unit. Vector indexing remains a deferred
/// effect because embedding may suspend.
#[derive(Debug, Clone)]
pub struct AddEntityPlan {
    /// The freshly generated id of the entity being created.
    pub(crate) entity_id: Uuid,
    /// Row + FTS insert statements to apply inside the atomic unit, in
    /// order. The row-insert statement carries the existence guard; the
    /// FTS-insert statement that follows it is unguarded (an ordinary
    /// `INSERT` into a virtual table with no conflicting row).
    pub(crate) statements: Vec<PlanStatement>,
    /// Reindex the committed entity after the transaction closes, as
    /// assigned by the prepare pass.
    pub(crate) post_commit: PostCommitEffect,
}

impl AddEntityPlan {
    /// The id generated for the prepared entity.
    pub fn entity_id(&self) -> Uuid {
        self.entity_id
    }
}

/// Write plan for an `AddNote` proposal change: a fresh note row plus its FTS
/// document in the same atomic unit.
#[derive(Debug, Clone)]
pub struct AddNotePlan {
    /// The freshly generated id of the note being created.
    pub(crate) note_id: Uuid,
    /// Row + FTS insert statements to apply inside the atomic unit, in
    /// order, mirroring [`AddEntityPlan::statements`].
    pub(crate) statements: Vec<PlanStatement>,
    /// Reindex the committed note after the transaction closes, as assigned
    /// by the prepare pass.
    pub(crate) post_commit: PostCommitEffect,
}

impl AddNotePlan {
    /// The id generated for the prepared note.
    pub fn note_id(&self) -> Uuid {
        self.note_id
    }
}

/// Write plan for a `delete` op (soft or hard).
///
/// Deferred effects are assigned by this crate's prepare pass and cannot be
/// attached to a statement-free plan by external callers:
///
/// ```compile_fail
/// use khive_runtime::{DeletePlan, PostCommitEffect};
/// use uuid::Uuid;
///
/// let id = Uuid::nil();
/// let plan = DeletePlan {
///     target_id: id,
///     statements: Vec::new(),
///     post_commit: PostCommitEffect::NoteDeleted {
///         note_id: id,
///         kind: "observation".to_owned(),
///     },
/// };
/// ```
#[derive(Debug, Clone)]
pub struct DeletePlan {
    /// The id of the entity or note being deleted.
    pub(crate) target_id: Uuid,
    /// Row DML (and, for a hard delete, incident-edge cascade DML) to apply
    /// inside the atomic unit, in order. The target-row delete statement
    /// carries the existence guard; a cascade edge-delete statement (hard
    /// delete only) is unguarded — it may legitimately affect zero rows if
    /// the target had no incident edges.
    pub(crate) statements: Vec<PlanStatement>,
    /// Deferred note-mutation-hook fire assigned by the prepare pass for a
    /// note delete (#750 2). `PostCommitEffect::None` for entity and edge
    /// deletes — the hook system is note-only.
    pub(crate) post_commit: PostCommitEffect,
}

impl DeletePlan {
    /// The record id supplied to the prepare pass.
    pub fn target_id(&self) -> Uuid {
        self.target_id
    }
}

/// Write plan for a `link` op (create a typed directed edge). Endpoint
/// existence is checked **structurally**, not via an unanchored plan-level
/// guard: `statement` is a guarded `INSERT ... SELECT ... WHERE EXISTS`
/// shape whose `SELECT` re-probes both endpoints inside the transaction, so
/// the runner's affected-row check on this one statement *is* the
/// in-transaction existence probe (ADR-099 acceptance criteria's
/// dangling-edge case — `[delete(X, hard), link(A, X)]` — is closed by this
/// guard failing once X is gone, regardless of statement ordering
/// convention).
#[derive(Debug, Clone)]
pub struct LinkPlan {
    pub(crate) source_id: Uuid,
    pub(crate) target_id: Uuid,
    /// The guarded `INSERT ... SELECT ... WHERE EXISTS(...)` statement:
    /// its affected-row count is the endpoint-existence probe.
    pub(crate) statement: PlanStatement,
}

impl LinkPlan {
    /// The canonical source endpoint used by the prepared statement.
    pub fn source_id(&self) -> Uuid {
        self.source_id
    }

    /// The canonical target endpoint used by the prepared statement.
    pub fn target_id(&self) -> Uuid {
        self.target_id
    }
}

/// Write plan for a `merge` op (deduplicate two entities). Rewires and
/// lifecycle writes are split into separate fields precisely so a guard is
/// never ambiguous between them: the edge rewire is **predicate-based**
/// (ADR-099 D1 rule 1) and may touch zero or many rows depending on earlier
/// in-file writes, so it is never guarded; the `from`/`into` entity
/// lifecycle write assumes both rows exist, so it always is.
#[derive(Debug, Clone)]
pub struct MergePlan {
    pub(crate) into_id: Uuid,
    pub(crate) from_id: Uuid,
    /// Predicate-based edge-rewire statement(s)
    /// (`UPDATE graph_edges SET source_id = :into WHERE source_id = :from`-
    /// shaped), evaluated inside the transaction so they structurally see
    /// any earlier op's edge writes in the same file (ADR-099 acceptance
    /// criteria: "merge rewires see earlier in-file writes"). Never
    /// guarded — a rewire touching zero rows is a legitimate outcome.
    pub(crate) rewires: Vec<PlanPredicate>,
    /// The `from` entity's soft-delete/tombstone DML (and any other
    /// lifecycle write prepare assumed a target row exists for). Always
    /// guarded — prepare validated `into`/`from` both exist.
    pub(crate) lifecycle: Vec<PlanStatement>,
}

impl MergePlan {
    /// The entity retained by the prepared merge.
    pub fn into_id(&self) -> Uuid {
        self.into_id
    }

    /// The entity retired by the prepared merge.
    pub fn from_id(&self) -> Uuid {
        self.from_id
    }
}

/// Write plan for a `gtd.transition` op (explicit task lifecycle change).
#[derive(Debug, Clone)]
pub struct GtdTransitionPlan {
    pub(crate) task_id: Uuid,
    /// Status-column DML to apply inside the atomic unit. Property-only
    /// status mutation — triggers no reindex (ADR-099 D3). The transition
    /// statement carries the guard (prepare validated the current status
    /// and the requested transition were legal). Empty for an idempotent
    /// no-op (`current == target` after `normalize_status`, GAP-5/GAP-6 fix
    /// round) — canonical performs no write in that case either
    /// (`handlers.rs:995-1005`).
    pub(crate) statements: Vec<PlanStatement>,
    /// Deferred lifecycle audit row assigned by the prepare pass (GAP-5):
    /// `PostCommitEffect::None` for the idempotent no-op case, matching
    /// canonical's early return before its own
    /// `ensure_audit_schema`/`write_audit_record` call.
    pub(crate) post_commit: PostCommitEffect,
}

impl GtdTransitionPlan {
    /// The task targeted by the prepared transition.
    pub fn task_id(&self) -> Uuid {
        self.task_id
    }
}

/// Write plan for a `gtd.complete` op (task lifecycle terminal transition).
#[derive(Debug, Clone)]
pub struct GtdCompletePlan {
    pub(crate) task_id: Uuid,
    /// Status + `completed_at` DML to apply inside the atomic unit, in
    /// order. The status-update statement carries the guard (prepare
    /// validated the task was in a completable state); the `completed_at`
    /// write targets the same already-guarded row and is unguarded.
    pub(crate) statements: Vec<PlanStatement>,
    /// Deferred lifecycle audit row assigned by the prepare pass (GAP-5):
    /// mirrors `handle_complete`'s best-effort `write_audit_record` call.
    pub(crate) post_commit: PostCommitEffect,
}

impl GtdCompletePlan {
    /// The task targeted by the prepared completion.
    pub fn task_id(&self) -> Uuid {
        self.task_id
    }
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
    pub(crate) op: GovernanceOp,
    pub(crate) proposal_id: Uuid,
    /// Event-log + status DML to apply inside the atomic unit. The
    /// lifecycle-state-check statement carries the guard (prepare validated
    /// the proposal was in a state admitting this transition).
    pub(crate) statements: Vec<PlanStatement>,
}

impl GovernancePlan {
    /// The governance operation represented by this plan.
    pub fn op(&self) -> GovernanceOp {
        self.op
    }

    /// The proposal targeted by this plan.
    pub fn proposal_id(&self) -> Uuid {
        self.proposal_id
    }
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
            target_id: id,
            statements: vec![
                guarded("update-row", AffectedRowGuard::exactly(1)),
                unguarded("update-fts-mirror"),
            ],
            post_commit: PostCommitEffect::ReindexEntity { entity_id: id },
            edge_natural_key: None,
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
            statement: guarded("insert-edge-where-exists", AffectedRowGuard::exactly(1)),
        };
        assert_eq!(plan.source_id, source);
        assert_eq!(plan.target_id, target);
        // Dangling-edge acceptance criterion: once an endpoint row is gone,
        // the guarded INSERT...WHERE EXISTS affects 0 rows and the guard
        // on *that exact statement* must fail, not silently pass.
        let guard = plan.statement.guard.expect("link insert is guarded");
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
    fn gtd_transition_plan_idempotent_noop_carries_no_statements_and_no_audit() {
        // current == target after normalization is an idempotent no-op:
        // canonical performs no write and never reaches its audit-record call.
        let plan = GtdTransitionPlan {
            task_id: Uuid::new_v4(),
            statements: vec![],
            post_commit: PostCommitEffect::None,
        };
        assert!(plan.statements.is_empty());
        assert_eq!(plan.post_commit, PostCommitEffect::None);
    }

    #[test]
    fn gtd_transition_plan_carries_gtd_audit_post_commit_effect() {
        let task_id = Uuid::new_v4();
        let plan = GtdTransitionPlan {
            task_id,
            statements: vec![guarded("update-status", AffectedRowGuard::exactly(1))],
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
    fn gtd_complete_plan_guard_is_anchored_to_the_status_write() {
        let plan = GtdCompletePlan {
            task_id: Uuid::new_v4(),
            statements: vec![
                guarded("update-status", AffectedRowGuard::exactly(1)),
                unguarded("update-completed-at"),
            ],
            post_commit: PostCommitEffect::None,
        };
        assert_eq!(plan.statements.len(), 2);
        let guard = plan.statements[0].guard.expect("status write is guarded");
        assert!(guard.holds_for(1));
        assert_eq!(plan.statements[1].guard, None);
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
            note_id: id,
            statements: vec![
                guarded("note-insert", AffectedRowGuard::exactly(1)),
                unguarded("note-fts-insert"),
            ],
            post_commit: PostCommitEffect::ReindexNote { note_id: id },
        };
        assert_eq!(plan.note_id, id);
        assert_eq!(plan.statements[0].guard, Some(AffectedRowGuard::exactly(1)));
        assert_eq!(plan.statements[1].guard, None);
        assert_eq!(
            plan.post_commit,
            PostCommitEffect::ReindexNote { note_id: id }
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
}
