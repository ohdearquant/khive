//! ADR-099 migration step 3 (sub-slice B2) — the atomic runner: the
//! synchronous commit-pass mechanism that applies a caller-supplied sequence
//! of prepared write plans ([`crate::atomic_plan`]) as **ONE**
//! [`SqlAccess::atomic_unit`], under a per-op `SAVEPOINT`, committing every
//! plan or rolling back the whole unit.
//!
//! ADR-099 B3 caller: `kkernel exec --ops-file --atomic` — async prepare,
//! then this runner's one synchronous commit pass, then async post-commit
//! effects. Runtime callers also supply prepared plans directly (hard-delete
//! in `operations.rs`). See `docs/api/atomic_runner.md` for the full
//! three-phase shape (ADR-099 D1).
//!
//! # Safety: suspend-free invariant
//!
//! [`run_atomic_unit`] is the one place in this crate that builds an
//! [`AtomicUnitOp`] closure for [`SqlAccess::atomic_unit`], whose contract
//! requires the closure's future to resolve on its first poll — synchronous
//! DML against the provided `&mut dyn SqlWriter` only, never a suspending
//! `.await`. Every statement driven here comes from
//! `AtomicOpPlan::plan_statements`, which can only ever produce
//! [`PlanStatement`]s (plain parameterized SQL), so no code path in this
//! module can hand `atomic_unit` a suspending future. The paired
//! suspend-trap tests at the bottom of this file check both the happy-path
//! (real commit pass resolves on first poll) and the misuse-is-caught case
//! (a hand-built suspending closure fails loudly through the same seam). See
//! `docs/api/atomic_runner.md#suspend-free-invariant` for the full argument.

use std::any::Any;
use std::sync::{Arc, Mutex};

use khive_storage::{AtomicUnitOp, SqlAccess, SqlStatement, SqlWriter, StorageError};

use crate::atomic_plan::{
    AddEntityPlan, AddNotePlan, AffectedRowGuard, DeletePlan, GovernancePlan, GtdCompletePlan,
    GtdTransitionPlan, LinkPlan, MergePlan, PlanStatement, PostCommitEffect, UpdatePlan,
};

/// One admissible op's prepared write plan (ADR-099 D3's v1 admissible verb
/// groups), ready for the commit pass. This is the `Vec<AtomicOpPlan>` shape
/// [`run_atomic_unit`] consumes — the runner is agnostic to which verb
/// produced a given plan; it only needs each plan's ordered statements
/// (`plan_statements`) and any deferred post-commit effect
/// (`post_commit_effect`).
#[derive(Debug, Clone)]
pub enum AtomicOpPlan {
    AddEntity(AddEntityPlan),
    AddNote(AddNotePlan),
    Update(UpdatePlan),
    Delete(DeletePlan),
    Link(LinkPlan),
    Merge(MergePlan),
    GtdTransition(GtdTransitionPlan),
    GtdComplete(GtdCompletePlan),
    Governance(GovernancePlan),
}

impl AtomicOpPlan {
    /// This op's statements, in the order the commit pass must apply them.
    ///
    /// For [`AtomicOpPlan::Merge`], the predicate-based rewires
    /// (`MergePlan::rewires`) come first, converted to unguarded
    /// [`PlanStatement`]s (ADR-099 D1 rule 1 — a rewire may legitimately
    /// touch zero or many rows and is never guarded), followed by the
    /// always-guarded lifecycle writes (`MergePlan::lifecycle`). Every other
    /// variant already carries a flat `Vec<PlanStatement>` in apply order.
    fn plan_statements(&self) -> Vec<PlanStatement> {
        match self {
            AtomicOpPlan::AddEntity(p) => p.statements.clone(),
            AtomicOpPlan::AddNote(p) => p.statements.clone(),
            AtomicOpPlan::Update(p) => p.statements.clone(),
            AtomicOpPlan::Delete(p) => p.statements.clone(),
            AtomicOpPlan::Link(p) => vec![p.statement.clone()],
            AtomicOpPlan::Merge(p) => {
                let mut statements: Vec<PlanStatement> = p
                    .rewires
                    .iter()
                    .map(|rewire| PlanStatement {
                        statement: rewire.statement.clone(),
                        guard: None,
                    })
                    .collect();
                statements.extend(p.lifecycle.clone());
                statements
            }
            AtomicOpPlan::GtdTransition(p) => p.statements.clone(),
            AtomicOpPlan::GtdComplete(p) => p.statements.clone(),
            AtomicOpPlan::Governance(p) => p.statements.clone(),
        }
    }

    /// The deferred post-commit effect this op's plan recorded, if any.
    ///
    /// [`UpdatePlan`] carries a [`PostCommitEffect`] field for the `update`
    /// reindex caveat (ADR-099 D3). Under B3 (GAP-5),
    /// [`GtdTransitionPlan`]/[`GtdCompletePlan`] also carry one, for the
    /// best-effort lifecycle audit row. Since #750, [`DeletePlan`] also
    /// carries one, for the note-mutation
    /// hook fire on a committed note delete. Every other admissible verb's
    /// apply is pure DML with no deferred side effect. `merge`'s existing
    /// post-transaction vector re-insert (D3: "merge already performs its
    /// vector re-insert after its transaction") is the handler's own
    /// pre-existing behavior outside this plan shape — [`MergePlan`] itself
    /// carries no `post_commit` field, so this runner records none for it.
    fn post_commit_effect(&self) -> Option<PostCommitEffect> {
        match self {
            AtomicOpPlan::AddEntity(p) if p.post_commit != PostCommitEffect::None => {
                Some(p.post_commit.clone())
            }
            AtomicOpPlan::AddNote(p) if p.post_commit != PostCommitEffect::None => {
                Some(p.post_commit.clone())
            }
            AtomicOpPlan::Update(p) if p.post_commit != PostCommitEffect::None => {
                Some(p.post_commit.clone())
            }
            AtomicOpPlan::Delete(p) if p.post_commit != PostCommitEffect::None => {
                Some(p.post_commit.clone())
            }
            AtomicOpPlan::GtdTransition(p) if p.post_commit != PostCommitEffect::None => {
                Some(p.post_commit.clone())
            }
            AtomicOpPlan::GtdComplete(p) if p.post_commit != PostCommitEffect::None => {
                Some(p.post_commit.clone())
            }
            _ => None,
        }
    }
}

/// Why a single op's plan failed inside the commit pass (ADR-099 acceptance
/// criteria: "the failing op index is recorded").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AtomicOpFailure {
    /// The statement executed without a SQL error, but its affected-row
    /// count did not satisfy the guard prepare attached to it (ADR-099 D1
    /// rule 2 — "a prepare-time validation is a plan hypothesis, re-verified
    /// under the transaction, never a commitment"). This is the shape both
    /// the dangling-edge and zero-row acceptance criteria trip.
    GuardFailed {
        statement_label: Option<String>,
        expected: AffectedRowGuard,
        observed: u64,
    },
    /// The statement itself returned a storage error (a genuine SQL
    /// failure — malformed SQL, a constraint violation not modeled as a
    /// guard, etc.), not a guard mismatch.
    SqlError {
        statement_label: Option<String>,
        message: String,
    },
}

/// Deferred effects whose owning atomic unit has committed successfully.
///
/// Only [`run_atomic_unit`] can construct this token. Consumers may inspect
/// its effects through [`CommittedPostCommitEffects::as_slice`], while the
/// phase-3 executor takes the token by value; no public API exposes the owned
/// effect collection or accepts a prepare-time [`PostCommitEffect`] in its
/// place.
///
/// A committed token is a one-shot capability and cannot be cloned for
/// replay:
///
/// ```compile_fail
/// use khive_runtime::CommittedPostCommitEffects;
///
/// fn duplicate(
///     committed: &CommittedPostCommitEffects,
/// ) -> CommittedPostCommitEffects {
///     committed.clone()
/// }
/// ```
#[derive(Debug, PartialEq, Eq)]
pub struct CommittedPostCommitEffects {
    effects: Vec<PostCommitEffect>,
}

impl CommittedPostCommitEffects {
    fn new(effects: Vec<PostCommitEffect>) -> Self {
        Self { effects }
    }

    /// Inspect the committed effects without discarding their commit
    /// provenance.
    pub fn as_slice(&self) -> &[PostCommitEffect] {
        &self.effects
    }

    pub(crate) fn into_effects(self) -> Vec<PostCommitEffect> {
        self.effects
    }
}

/// The whole-unit outcome of a completed [`run_atomic_unit`] call — the
/// commit pass ran to a clean, distinguishable verdict (never returned for
/// a seam-level failure; see [`AtomicRunnerError`] for that case).
#[derive(Debug, PartialEq, Eq)]
pub enum AtomicRunOutcome {
    /// Every op's plan applied and the unit committed. Carries an opaque
    /// [`CommittedPostCommitEffects`] token containing the deferred effects
    /// in op order. Phase 3 consumes that token through
    /// [`crate::atomic_prepare::apply_post_commit_effects`] after
    /// `run_atomic_unit` returns and outside any transaction; callers can
    /// inspect the effects without converting them back into an executable
    /// raw collection.
    Committed {
        post_commit: CommittedPostCommitEffects,
    },
    /// The op at `failed_op_index` failed; the whole unit rolled back
    /// (ADR-099 D1: "the whole unit rolls back and the failing op index is
    /// recorded"). No op's writes are present in the database after this
    /// outcome — including any earlier op whose own `SAVEPOINT` had already
    /// been `RELEASE`d, because `RELEASE` only merges a savepoint's changes
    /// into its parent transaction; it does not commit them independently
    /// of the outer `atomic_unit` transaction's own COMMIT/ROLLBACK.
    RolledBack {
        failed_op_index: usize,
        failure: AtomicOpFailure,
    },
}

/// A failure of the `atomic_unit` seam itself — the storage layer refused
/// or could not complete the call at all (read-only backend, no async
/// runtime for the writer-task lookup, or an [`AtomicUnitOp`] that violated
/// the suspend-free invariant and got caught by `block_on_sync`). Never
/// returned for an ordinary op-level guard or SQL failure inside the unit —
/// those surface as `AtomicRunOutcome::RolledBack`, not this error.
#[derive(Debug)]
pub struct AtomicRunnerError(pub StorageError);

impl std::fmt::Display for AtomicRunnerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "atomic_unit seam failure: {}", self.0)
    }
}

impl std::error::Error for AtomicRunnerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

fn raw_stmt(sql: String) -> SqlStatement {
    SqlStatement {
        sql,
        params: vec![],
        label: None,
    }
}

async fn begin_savepoint(writer: &mut dyn SqlWriter, name: &str) -> Result<(), StorageError> {
    writer
        .execute(raw_stmt(format!("SAVEPOINT {name}")))
        .await
        .map(|_| ())
}

async fn release_savepoint(writer: &mut dyn SqlWriter, name: &str) -> Result<(), StorageError> {
    writer
        .execute(raw_stmt(format!("RELEASE {name}")))
        .await
        .map(|_| ())
}

async fn rollback_to_savepoint(writer: &mut dyn SqlWriter, name: &str) -> Result<(), StorageError> {
    writer
        .execute(raw_stmt(format!("ROLLBACK TO {name}")))
        .await
        .map(|_| ())
}

/// Apply one op's plan statements in order, checking each guarded
/// statement's affected-row count before moving to the next (ADR-099 D1
/// rule 2). Returns on the first statement that either errors or fails its
/// guard — never applies a later statement once an earlier one in the same
/// plan has failed.
async fn apply_plan(
    writer: &mut dyn SqlWriter,
    plan: &AtomicOpPlan,
) -> Result<(), AtomicOpFailure> {
    for stmt in plan.plan_statements() {
        let label = stmt.statement.label.clone();
        let affected =
            writer
                .execute(stmt.statement)
                .await
                .map_err(|e| AtomicOpFailure::SqlError {
                    statement_label: label.clone(),
                    message: e.to_string(),
                })?;
        if let Some(guard) = stmt.guard {
            if !guard.holds_for(affected) {
                return Err(AtomicOpFailure::GuardFailed {
                    statement_label: label,
                    expected: guard,
                    observed: affected,
                });
            }
        }
    }
    Ok(())
}

/// Run `plans` as ONE atomic unit (ADR-099 D1 commit pass): open a single
/// [`SqlAccess::atomic_unit`], apply every plan's statements under a named
/// `SAVEPOINT` (`adr099_atomic_op_<n>`), and commit all or roll back all.
///
/// **This is the seam the atomic-unit suspend-free invariant governs (see
/// the module doc comment above).** The closure built here drives only
/// `AtomicOpPlan::plan_statements` — plain DML — against the writer
/// `atomic_unit` hands it; it issues no transaction control of its own
/// (`BEGIN`/`COMMIT`/`ROLLBACK` are owned entirely by `atomic_unit`, exactly
/// like the existing `execute_batch` contract) beyond the per-op
/// `SAVEPOINT`/`RELEASE`/`ROLLBACK TO` statements, which are themselves
/// synchronous DML from SQLite's perspective, not transaction boundaries the
/// storage layer needs to track.
///
/// On the first op whose plan fails (a guard mismatch or a genuine SQL
/// error), this function does **not** propagate a generic storage error for
/// that case: it unwinds just that op's own `SAVEPOINT` (best-effort — the
/// outer `atomic_unit` transaction is rolled back in full regardless, per
/// ADR-099 D1) and returns `Ok(AtomicRunOutcome::RolledBack { .. })` naming
/// the failing op and why. [`AtomicRunnerError`] is reserved for a failure
/// of the `atomic_unit` seam itself — the storage layer refusing or being
/// unable to run the call at all.
pub async fn run_atomic_unit(
    access: &dyn SqlAccess,
    plans: Vec<AtomicOpPlan>,
) -> Result<AtomicRunOutcome, AtomicRunnerError> {
    let failure_slot: Arc<Mutex<Option<(usize, AtomicOpFailure)>>> = Arc::new(Mutex::new(None));
    let failure_slot_for_closure = Arc::clone(&failure_slot);

    let op: AtomicUnitOp = Box::new(move |writer| {
        Box::pin(async move {
            let mut post_commit = Vec::new();
            for (op_index, plan) in plans.iter().enumerate() {
                let savepoint = format!("adr099_atomic_op_{op_index}");
                begin_savepoint(writer, &savepoint).await?;
                match apply_plan(writer, plan).await {
                    Ok(()) => {
                        release_savepoint(writer, &savepoint).await?;
                        if let Some(effect) = plan.post_commit_effect() {
                            post_commit.push(effect);
                        }
                    }
                    Err(failure) => {
                        // Best-effort unwind of just this op's own partial
                        // DML. The outer `atomic_unit` transaction is rolled
                        // back in full once this closure returns `Err`
                        // regardless (ADR-099 D1: "the whole unit rolls
                        // back") — a failure in this cleanup step is not
                        // itself a correctness gap in the no-partial-state
                        // guarantee, only a diagnostic nicety.
                        let _ = rollback_to_savepoint(writer, &savepoint).await;
                        let _ = release_savepoint(writer, &savepoint).await;
                        *failure_slot_for_closure
                            .lock()
                            .expect("atomic runner failure slot poisoned") =
                            Some((op_index, failure));
                        return Err(StorageError::Internal(format!(
                            "ADR-099 atomic unit aborted at op {op_index}"
                        )));
                    }
                }
            }
            Ok(Box::new(post_commit) as Box<dyn Any + Send>)
        })
    });

    match access.atomic_unit(op).await {
        Ok(boxed) => {
            let post_commit = *boxed.downcast::<Vec<PostCommitEffect>>().expect(
                "run_atomic_unit's own closure always returns Box<Vec<PostCommitEffect>> on Ok",
            );
            Ok(AtomicRunOutcome::Committed {
                post_commit: CommittedPostCommitEffects::new(post_commit),
            })
        }
        Err(storage_err) => {
            let recorded = failure_slot
                .lock()
                .expect("atomic runner failure slot poisoned")
                .take();
            match recorded {
                Some((failed_op_index, failure)) => Ok(AtomicRunOutcome::RolledBack {
                    failed_op_index,
                    failure,
                }),
                None => Err(AtomicRunnerError(storage_err)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc as StdArc;

    use khive_db::{ConnectionPool, PoolConfig, SqlBridge};
    use khive_storage::types::{SqlValue, StorageResult as StorageResultAlias};
    use uuid::Uuid;

    /// A scratch pool wired exactly like the daemon.rs / sql_bridge.rs
    /// tests above it: file-backed (atomic_unit's single-writer path is
    /// only reachable file-backed), `write_queue_enabled: true` so
    /// `atomic_unit` routes through the real `WriterTask` + `block_on_sync`
    /// seam rather than the flag-off manual-transaction fallback — the
    /// suspend-trap contract only fires on this path.
    fn scratch_pool(name: &str) -> StdArc<ConnectionPool> {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(format!("{name}.db"));
        let pool = StdArc::new(
            ConnectionPool::new(PoolConfig {
                path: Some(path),
                write_queue_enabled: true,
                ..PoolConfig::default()
            })
            .expect("pool open"),
        );
        // Leak the tempdir so the file lives for the pool's lifetime within
        // one test function — every test here is single-scoped and short.
        std::mem::forget(dir);
        pool
    }

    /// Minimal real schema slice (`entities`, `graph_edges`) — copied from
    /// `crates/khive-db/sql/entities-ddl.sql` / `graph-ddl.sql` rather than
    /// invented ad hoc, since the dangling-edge and merge-rewire tests below
    /// depend on `graph_edges` having no foreign-key enforcement (ADR-099
    /// D1: "`graph_edges` has no foreign-key enforcement ... SQLite will
    /// happily commit the inconsistency" absent the guard/predicate rules
    /// this runner implements).
    fn seed_schema(pool: &ConnectionPool) {
        let writer = pool.try_writer().expect("writer");
        writer
            .conn()
            .execute_batch(
                "CREATE TABLE entities (
                    id             TEXT PRIMARY KEY,
                    namespace      TEXT NOT NULL,
                    kind           TEXT NOT NULL,
                    entity_type    TEXT,
                    name           TEXT NOT NULL,
                    description    TEXT,
                    properties     TEXT,
                    tags           TEXT NOT NULL DEFAULT '[]',
                    created_at     INTEGER NOT NULL,
                    updated_at     INTEGER NOT NULL,
                    deleted_at     INTEGER,
                    merged_into    TEXT,
                    merge_event_id TEXT
                );
                CREATE TABLE graph_edges (
                    namespace      TEXT NOT NULL,
                    id             TEXT NOT NULL,
                    source_id      TEXT NOT NULL,
                    target_id      TEXT NOT NULL,
                    relation       TEXT NOT NULL,
                    weight         REAL NOT NULL DEFAULT 1.0,
                    created_at     INTEGER NOT NULL,
                    updated_at     INTEGER NOT NULL,
                    deleted_at     INTEGER,
                    metadata       TEXT,
                    target_backend TEXT,
                    PRIMARY KEY (namespace, id)
                );",
            )
            .expect("seed schema");
    }

    fn insert_entity(pool: &ConnectionPool, id: Uuid, name: &str) {
        let writer = pool.try_writer().expect("writer");
        writer
            .conn()
            .execute(
                "INSERT INTO entities \
                 (id, namespace, kind, name, created_at, updated_at) \
                 VALUES (?1, 'local', 'concept', ?2, 0, 0)",
                rusqlite::params![id.to_string(), name],
            )
            .expect("insert entity");
    }

    fn entity_exists(pool: &ConnectionPool, id: Uuid) -> bool {
        let writer = pool.try_writer().expect("writer");
        let count: i64 = writer
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM entities WHERE id = ?1 AND deleted_at IS NULL",
                rusqlite::params![id.to_string()],
                |row| row.get(0),
            )
            .expect("query entity");
        count > 0
    }

    fn edge_count(pool: &ConnectionPool, source: Uuid, target: Uuid) -> i64 {
        let writer = pool.try_writer().expect("writer");
        writer
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM graph_edges \
                 WHERE source_id = ?1 AND target_id = ?2 AND deleted_at IS NULL",
                rusqlite::params![source.to_string(), target.to_string()],
                |row| row.get(0),
            )
            .expect("query edge")
    }

    fn entities_snapshot(pool: &ConnectionPool) -> Vec<String> {
        let writer = pool.try_writer().expect("writer");
        let mut stmt = writer
            .conn()
            .prepare("SELECT id, name, deleted_at FROM entities ORDER BY id")
            .expect("prepare snapshot");
        let rows = stmt
            .query_map([], |row| {
                let id: String = row.get(0)?;
                let name: String = row.get(1)?;
                let deleted_at: Option<i64> = row.get(2)?;
                Ok(format!("{id}:{name}:{deleted_at:?}"))
            })
            .expect("query snapshot");
        rows.collect::<Result<Vec<_>, _>>()
            .expect("collect snapshot")
    }

    fn delete_plan(id: Uuid, label: &str) -> AtomicOpPlan {
        AtomicOpPlan::Delete(DeletePlan {
            target_id: id,
            statements: vec![PlanStatement {
                statement: SqlStatement {
                    sql: "DELETE FROM entities WHERE id = ?1 AND deleted_at IS NULL".to_string(),
                    params: vec![SqlValue::Text(id.to_string())],
                    label: Some(label.to_string()),
                },
                guard: Some(AffectedRowGuard::exactly(1)),
            }],
            post_commit: PostCommitEffect::None,
        })
    }

    fn rename_plan(id: Uuid, new_name: &str, label: &str) -> AtomicOpPlan {
        AtomicOpPlan::Update(UpdatePlan {
            target_id: id,
            statements: vec![PlanStatement {
                statement: SqlStatement {
                    sql: "UPDATE entities SET name = ?1, updated_at = 1 \
                          WHERE id = ?2 AND deleted_at IS NULL"
                        .to_string(),
                    params: vec![
                        SqlValue::Text(new_name.to_string()),
                        SqlValue::Text(id.to_string()),
                    ],
                    label: Some(label.to_string()),
                },
                guard: Some(AffectedRowGuard::exactly(1)),
            }],
            post_commit: PostCommitEffect::None,
            edge_natural_key: None,
        })
    }

    /// The guarded `INSERT ... SELECT ... WHERE EXISTS` shape `LinkPlan`
    /// documents: the affected-row count of this ONE statement doubles as
    /// the in-transaction endpoint-existence probe (ADR-099 acceptance
    /// criteria: dangling-edge).
    fn link_plan(edge_id: Uuid, source: Uuid, target: Uuid) -> AtomicOpPlan {
        AtomicOpPlan::Link(LinkPlan {
            source_id: source,
            target_id: target,
            statement: PlanStatement {
                statement: SqlStatement {
                    sql: "INSERT INTO graph_edges \
                          (namespace, id, source_id, target_id, relation, created_at, updated_at) \
                          SELECT 'local', ?1, ?2, ?3, 'annotates', 0, 0 \
                          WHERE EXISTS (SELECT 1 FROM entities WHERE id = ?2 AND deleted_at IS NULL) \
                            AND EXISTS (SELECT 1 FROM entities WHERE id = ?3 AND deleted_at IS NULL)"
                        .to_string(),
                    params: vec![
                        SqlValue::Text(edge_id.to_string()),
                        SqlValue::Text(source.to_string()),
                        SqlValue::Text(target.to_string()),
                    ],
                    label: Some("insert-edge-where-exists".to_string()),
                },
                guard: Some(AffectedRowGuard::exactly(1)),
            },
        })
    }

    fn merge_plan(into_id: Uuid, from_id: Uuid) -> AtomicOpPlan {
        AtomicOpPlan::Merge(MergePlan {
            into_id,
            from_id,
            rewires: vec![crate::atomic_plan::PlanPredicate {
                description: "source_id = :from".to_string(),
                statement: SqlStatement {
                    sql: "UPDATE graph_edges SET source_id = ?1, updated_at = 1 \
                          WHERE source_id = ?2"
                        .to_string(),
                    params: vec![
                        SqlValue::Text(into_id.to_string()),
                        SqlValue::Text(from_id.to_string()),
                    ],
                    label: Some("merge-rewire".to_string()),
                },
            }],
            lifecycle: vec![PlanStatement {
                statement: SqlStatement {
                    sql: "UPDATE entities SET deleted_at = 1, merged_into = ?1 \
                          WHERE id = ?2 AND deleted_at IS NULL"
                        .to_string(),
                    params: vec![
                        SqlValue::Text(into_id.to_string()),
                        SqlValue::Text(from_id.to_string()),
                    ],
                    label: Some("tombstone-from-entity".to_string()),
                },
                guard: Some(AffectedRowGuard::exactly(1)),
            }],
        })
    }

    // ------------------------------------------------------------------
    // 1. Rollback end-to-end
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn rollback_end_to_end_leaves_zero_partial_state() {
        let pool = scratch_pool("rollback_end_to_end");
        seed_schema(&pool);
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        insert_entity(&pool, a, "alpha");
        insert_entity(&pool, b, "bravo");
        let before = entities_snapshot(&pool);

        let bridge = SqlBridge::new(StdArc::clone(&pool), true);
        let plans = vec![
            rename_plan(a, "alpha-renamed", "rename-a"),
            // Induced failure: b does not exist under this id, so the
            // guard (expects exactly 1 affected row) fails.
            delete_plan(Uuid::new_v4(), "delete-nonexistent"),
            rename_plan(b, "bravo-renamed", "rename-b"),
        ];

        let outcome = run_atomic_unit(&bridge, plans).await.expect("seam call ok");
        match outcome {
            AtomicRunOutcome::RolledBack {
                failed_op_index,
                failure,
            } => {
                assert_eq!(failed_op_index, 1);
                assert!(matches!(failure, AtomicOpFailure::GuardFailed { .. }));
            }
            other => panic!("expected RolledBack, got {other:?}"),
        }

        let after = entities_snapshot(&pool);
        assert_eq!(
            before, after,
            "a mid-unit failure must leave the database byte-for-byte unchanged"
        );
    }

    // ------------------------------------------------------------------
    // 2. Suspend-trap paired: the runner's happy path resolves on first
    //    poll (commits); a hand-built suspending closure through the SAME
    //    `atomic_unit` seam fails loudly instead of silently succeeding.
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn commit_pass_resolves_on_first_poll_and_commits() {
        let pool = scratch_pool("suspend_trap_happy_path");
        seed_schema(&pool);
        let a = Uuid::new_v4();
        insert_entity(&pool, a, "alpha");

        let bridge = SqlBridge::new(StdArc::clone(&pool), true);
        let plans = vec![rename_plan(a, "alpha-v2", "rename-a")];

        let outcome = run_atomic_unit(&bridge, plans).await.expect("seam call ok");
        assert!(
            matches!(outcome, AtomicRunOutcome::Committed { .. }),
            "the real commit-pass closure (SAVEPOINT + guarded DML only) must \
             resolve on block_on_sync's first poll and commit: {outcome:?}"
        );

        let writer = pool.try_writer().expect("writer");
        let name: String = writer
            .conn()
            .query_row(
                "SELECT name FROM entities WHERE id = ?1",
                rusqlite::params![a.to_string()],
                |row| row.get(0),
            )
            .expect("query renamed entity");
        assert_eq!(name, "alpha-v2");
    }

    #[tokio::test]
    async fn hand_built_suspending_closure_fails_loudly_through_the_same_seam() {
        // The exact misuse the module-doc "suspend-trap" promise guards
        // against: a closure sent through this crate's OWN integration of
        // `SqlAccess::atomic_unit` (the same `bridge` type `run_atomic_unit`
        // uses) that reaches a real suspension point instead of staying
        // synchronous DML — a stand-in for "a future edit admits a verb
        // whose apply forgot to hoist its embedding out of the transaction"
        // (ADR-099 acceptance criteria). `tokio::task::yield_now` returns
        // `Poll::Pending` on its first poll by construction, so this is a
        // real suspension, not `std::future::pending`'s permanent one.
        let pool = scratch_pool("suspend_trap_misuse");
        seed_schema(&pool);

        let bridge = SqlBridge::new(StdArc::clone(&pool), true);
        let suspending_op: AtomicUnitOp = Box::new(|_writer| {
            Box::pin(async move {
                tokio::task::yield_now().await;
                Ok(Box::new(()) as Box<dyn Any + Send>)
            })
        });

        let result: StorageResultAlias<Box<dyn Any + Send>> =
            khive_storage::SqlAccess::atomic_unit(&bridge, suspending_op).await;
        assert!(
            result.is_err(),
            "a closure that suspends on first poll must fail loudly through \
             `atomic_unit`, never silently succeed or wedge; got {result:?}"
        );
    }

    // ------------------------------------------------------------------
    // 3. Staleness both directions
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn concurrent_mutation_between_prepare_and_apply_trips_guard_and_rolls_back() {
        let pool = scratch_pool("staleness_tripped");
        seed_schema(&pool);
        let a = Uuid::new_v4();
        insert_entity(&pool, a, "alpha");

        // Plan prepared against pre-transaction state (a exists).
        let plan = rename_plan(a, "alpha-v2", "rename-a");

        // Concurrent mutation between prepare and apply: a is deleted by a
        // separate writer before the commit pass runs.
        {
            let writer = pool.try_writer().expect("writer");
            writer
                .conn()
                .execute(
                    "DELETE FROM entities WHERE id = ?1",
                    rusqlite::params![a.to_string()],
                )
                .expect("concurrent delete");
        }

        let bridge = SqlBridge::new(StdArc::clone(&pool), true);
        let outcome = run_atomic_unit(&bridge, vec![plan])
            .await
            .expect("seam call ok");
        match outcome {
            AtomicRunOutcome::RolledBack {
                failed_op_index,
                failure,
            } => {
                assert_eq!(failed_op_index, 0);
                assert!(matches!(
                    failure,
                    AtomicOpFailure::GuardFailed { observed: 0, .. }
                ));
            }
            other => panic!("expected RolledBack from the stale guard, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_mutation_twin_commits() {
        let pool = scratch_pool("staleness_untripped");
        seed_schema(&pool);
        let a = Uuid::new_v4();
        insert_entity(&pool, a, "alpha");

        let plan = rename_plan(a, "alpha-v2", "rename-a");
        let bridge = SqlBridge::new(StdArc::clone(&pool), true);
        let outcome = run_atomic_unit(&bridge, vec![plan])
            .await
            .expect("seam call ok");
        assert!(matches!(outcome, AtomicRunOutcome::Committed { .. }));
        assert!(entity_exists(&pool, a));
    }

    // ------------------------------------------------------------------
    // 4. Dangling-edge
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn dangling_edge_from_delete_then_link_rolls_back_whole_unit() {
        let pool = scratch_pool("dangling_edge");
        seed_schema(&pool);
        let x = Uuid::new_v4();
        let a = Uuid::new_v4();
        insert_entity(&pool, x, "x");
        insert_entity(&pool, a, "a");
        let before = entities_snapshot(&pool);

        let bridge = SqlBridge::new(StdArc::clone(&pool), true);
        let edge_id = Uuid::new_v4();
        let plans = vec![delete_plan(x, "delete-x"), link_plan(edge_id, a, x)];

        let outcome = run_atomic_unit(&bridge, plans).await.expect("seam call ok");
        match outcome {
            AtomicRunOutcome::RolledBack {
                failed_op_index,
                failure,
            } => {
                assert_eq!(
                    failed_op_index, 1,
                    "delete(x) succeeds; link(a, x) is the op whose \
                     endpoint-existence guard fails once x is gone"
                );
                assert!(matches!(
                    failure,
                    AtomicOpFailure::GuardFailed { observed: 0, .. }
                ));
            }
            other => panic!("expected RolledBack, got {other:?}"),
        }

        assert_eq!(
            edge_count(&pool, a, x),
            0,
            "no dangling edge may be committed"
        );
        assert_eq!(
            entities_snapshot(&pool),
            before,
            "x must still exist — the whole unit rolled back, including the delete"
        );
    }

    // ------------------------------------------------------------------
    // 5. Merge-rewire
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn merge_rewire_sees_earlier_in_file_edge_write() {
        let pool = scratch_pool("merge_rewire");
        seed_schema(&pool);
        let z = Uuid::new_v4();
        let from = Uuid::new_v4();
        let into = Uuid::new_v4();
        insert_entity(&pool, z, "z");
        insert_entity(&pool, from, "from-entity");
        insert_entity(&pool, into, "into-entity");

        let bridge = SqlBridge::new(StdArc::clone(&pool), true);
        let edge_id = Uuid::new_v4();
        // [link(from, z), merge(into, from)] — the rewire's predicate-based
        // UPDATE must see the edge the earlier op in THIS SAME unit
        // inserted (ADR-099 acceptance criteria: "merge rewires see earlier
        // in-file writes").
        let plans = vec![link_plan(edge_id, from, z), merge_plan(into, from)];

        let outcome = run_atomic_unit(&bridge, plans).await.expect("seam call ok");
        assert!(
            matches!(outcome, AtomicRunOutcome::Committed { .. }),
            "expected the unit to commit: {outcome:?}"
        );

        assert_eq!(
            edge_count(&pool, from, z),
            0,
            "no live edge may remain sourced from the merged-away entity"
        );
        assert_eq!(
            edge_count(&pool, into, z),
            1,
            "the edge must be rewired onto `into`"
        );
        assert!(
            !entity_exists(&pool, from),
            "the `from` entity must be tombstoned (soft-deleted)"
        );
    }

    // ------------------------------------------------------------------
    // 6. Zero-row
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn zero_row_apply_fails_the_whole_unit() {
        let pool = scratch_pool("zero_row");
        seed_schema(&pool);
        let x = Uuid::new_v4();
        insert_entity(&pool, x, "x");
        let before = entities_snapshot(&pool);

        let bridge = SqlBridge::new(StdArc::clone(&pool), true);
        // [delete(x, hard), update(x)] — update's affected-row guard
        // observes zero rows once x is already gone.
        let plans = vec![
            delete_plan(x, "delete-x"),
            rename_plan(x, "x-v2", "update-x"),
        ];

        let outcome = run_atomic_unit(&bridge, plans).await.expect("seam call ok");
        match outcome {
            AtomicRunOutcome::RolledBack {
                failed_op_index,
                failure,
            } => {
                assert_eq!(failed_op_index, 1);
                assert_eq!(
                    failure,
                    AtomicOpFailure::GuardFailed {
                        statement_label: Some("update-x".to_string()),
                        expected: AffectedRowGuard::exactly(1),
                        observed: 0,
                    }
                );
            }
            other => panic!("expected RolledBack, got {other:?}"),
        }

        assert_eq!(
            entities_snapshot(&pool),
            before,
            "x must be restored — the whole unit rolled back"
        );
    }

    // ------------------------------------------------------------------
    // Post-commit effect collection (UpdatePlan's reindex caveat, D3)
    // ------------------------------------------------------------------

    // ------------------------------------------------------------------
    // 7. Daemon coexistence (ADR-099 B3 acceptance test 5)
    // ------------------------------------------------------------------

    /// An atomic unit must go through the SAME `WriterTaskHandle` queue as
    /// an ordinary single-row write — not a separate connection that would
    /// let it silently bypass single-writer serialization (ADR-067
    /// Component A). This reuses the deterministic occupier pattern from
    /// `khive-db::stores::graph_tests::upsert_edge_routes_through_writer_task_when_flag_enabled`
    /// (Slice A): an occupier closure parks on a oneshot inside the writer
    /// task's one drain slot, so both the atomic unit's `atomic_unit` call
    /// and a concurrent ordinary `upsert_entity` call are forced to queue
    /// behind it. `queue_depth` reaching 2 while both are pending, then
    /// draining to 0 once the occupier releases and both complete, is the
    /// proof that `run_atomic_unit` "discriminates" through the real queue
    /// gauge rather than opening a competing connection.
    #[tokio::test]
    async fn atomic_unit_and_concurrent_normal_write_share_the_same_writer_queue() {
        let pool = scratch_pool("daemon_coexistence");
        seed_schema(&pool);
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        insert_entity(&pool, a, "alpha");

        let writer_task = pool
            .writer_task_handle()
            .expect("writer task lookup")
            .expect("writer task must be spawned with the flag on for a file-backed pool");

        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        let occupier = {
            let writer_task = writer_task.clone();
            tokio::spawn(async move {
                writer_task
                    .send(move |_conn| {
                        let _ = started_tx.send(());
                        let _ = release_rx.blocking_recv();
                        Ok::<(), StorageError>(())
                    })
                    .await
            })
        };

        started_rx
            .await
            .expect("occupier must signal it has started running inside the writer task");
        assert_eq!(
            writer_task.queue_depth(),
            0,
            "channel must start empty once the occupier has been dequeued and is running"
        );

        // The atomic unit: renames `a`.
        let bridge = SqlBridge::new(StdArc::clone(&pool), true);
        let atomic_plans = vec![rename_plan(a, "alpha-atomic", "rename-a-atomic")];
        let atomic_task = tokio::spawn(async move { run_atomic_unit(&bridge, atomic_plans).await });

        // A concurrent ORDINARY write, routed through the SAME
        // `WriterTaskHandle::send` the real `with_writer` path uses
        // (khive-db's own `graph_tests` occupier test asserts this for
        // `upsert_edge`) — NOT a second `pool.try_writer()` connection,
        // which would open a competing SQLite handle and contend on the
        // file lock the occupier's `BEGIN IMMEDIATE` already holds,
        // producing a `DatabaseBusy` false failure unrelated to the queue
        // this test is actually proving something about.
        let normal_write_task = {
            let writer_task = writer_task.clone();
            let b_str = b.to_string();
            tokio::spawn(async move {
                writer_task
                    .send(move |conn| {
                        conn.execute(
                            "INSERT INTO entities \
                             (id, namespace, kind, name, created_at, updated_at) \
                             VALUES (?1, 'local', 'concept', ?2, 0, 0)",
                            rusqlite::params![b_str, "bravo"],
                        )
                        .map_err(|e| StorageError::Internal(e.to_string()))
                    })
                    .await
            })
        };

        // Both the atomic unit's `atomic_unit` call and (best-effort) the
        // ordinary write must appear in the SAME queue while the occupier
        // holds the slot — this is the coexistence proof: neither one opens
        // a side-channel connection that would let it skip the queue.
        let mut saw_both_enqueued = false;
        for _ in 0..200 {
            if writer_task.queue_depth() >= 2 {
                saw_both_enqueued = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            saw_both_enqueued,
            "expected BOTH the atomic unit's atomic_unit() call and the \
             concurrent ordinary write to appear in the writer task's queue \
             while the occupier held the single drain slot (queue_depth \
             should have reached 2) — observed {}; run_atomic_unit is not \
             sharing the same queue as an ordinary write",
            writer_task.queue_depth()
        );

        release_tx.send(()).expect("release occupier");
        occupier
            .await
            .expect("occupier task join")
            .expect("occupier op ok");

        let atomic_outcome = atomic_task
            .await
            .expect("atomic task join")
            .expect("seam call ok");
        assert!(
            matches!(atomic_outcome, AtomicRunOutcome::Committed { .. }),
            "atomic unit must commit once the occupier releases: {atomic_outcome:?}"
        );
        normal_write_task
            .await
            .expect("normal write task join")
            .expect("normal write op ok");

        assert!(
            entity_exists(&pool, a),
            "a must exist (renamed) after commit"
        );
        {
            // Scoped: the `WriterGuard` returned by `try_writer()` holds a
            // non-reentrant `parking_lot::Mutex` for as long as it lives.
            // Left unscoped, it would still be held when `entity_exists`
            // below tries to check out the same writer, deadlocking (in
            // practice, timing out after `checkout_timeout`) against itself
            // rather than against anything the writer task/occupier hold.
            let writer = pool.try_writer().expect("writer");
            let renamed: String = writer
                .conn()
                .query_row(
                    "SELECT name FROM entities WHERE id = ?1",
                    rusqlite::params![a.to_string()],
                    |row| row.get(0),
                )
                .expect("query renamed entity");
            assert_eq!(renamed, "alpha-atomic");
        }
        assert!(
            entity_exists(&pool, b),
            "the concurrent ordinary write must also have landed"
        );
    }

    #[tokio::test]
    async fn post_commit_effects_are_collected_in_op_order_and_only_for_update() {
        let pool = scratch_pool("post_commit_collection");
        seed_schema(&pool);
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        insert_entity(&pool, a, "alpha");
        insert_entity(&pool, b, "bravo");

        let bridge = SqlBridge::new(StdArc::clone(&pool), true);
        let mut update_a = match rename_plan(a, "alpha-v2", "rename-a") {
            AtomicOpPlan::Update(p) => p,
            _ => unreachable!(),
        };
        update_a.post_commit = PostCommitEffect::ReindexEntity { entity_id: a };
        let plans = vec![
            AtomicOpPlan::Update(update_a),
            delete_plan(b, "delete-b"), // no post-commit effect for delete
        ];

        let outcome = run_atomic_unit(&bridge, plans).await.expect("seam call ok");
        match outcome {
            AtomicRunOutcome::Committed { post_commit } => {
                fn require_commit_provenance(_: &CommittedPostCommitEffects) {}
                require_commit_provenance(&post_commit);
                assert_eq!(
                    post_commit.as_slice(),
                    &[PostCommitEffect::ReindexEntity { entity_id: a }]
                );
            }
            other => panic!("expected Committed, got {other:?}"),
        }
    }
}
