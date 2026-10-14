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
//! `run_prepared_atomic_unit` owns the failure and commit protocol shared by
//! the atomic-plan and stream-batch paths. It builds an [`AtomicUnitOp`]
//! closure for [`SqlAccess::atomic_unit`], whose contract
//! requires the closure's future to resolve on its first poll — synchronous
//! DML against the provided `&mut dyn SqlWriter` only, never a suspending
//! `.await`. Prepared callbacks drive only parameterized statements, guards
//! and stream-ledger reads through the provided writer. They must not perform
//! asynchronous preparation or other I/O. The paired
//! suspend-trap tests at the bottom of this file check both the happy-path
//! (real commit pass resolves on first poll) and the misuse-is-caught case
//! (a hand-built suspending closure fails loudly through the same seam). See
//! `docs/api/atomic_runner.md#suspend-free-invariant` for the full argument.

use std::any::Any;
use std::sync::{Arc, Mutex};

use khive_storage::{AtomicUnitOp, BoxFuture, SqlAccess, SqlStatement, SqlWriter, StorageError};

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
    FinalizeEntity(Box<crate::EntityFinalizationPlan>),
    AddNote(Box<AddNotePlan>),
    Update(Box<UpdatePlan>),
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
    /// variant carries ordered statements. Updates additionally append typed graph
    /// effects; the tuple flag marks a read assertion for `query_all`, never DML.
    fn plan_statements(&self) -> Vec<(PlanStatement, bool)> {
        let statements = match self {
            AtomicOpPlan::AddEntity(p) => p.statements.clone(),
            // The opaque finalizer applies its own phased statements.
            AtomicOpPlan::FinalizeEntity(_) => Vec::new(),
            AtomicOpPlan::AddNote(p) => p.statements.clone(),
            AtomicOpPlan::Update(p) => p.statements.clone(),
            AtomicOpPlan::Delete(p) => p.statements.clone(),
            AtomicOpPlan::Link(p) => p.statements.clone(),
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
        };
        let mut statements: Vec<_> = statements
            .into_iter()
            .map(|statement| (statement, false))
            .collect();
        if let AtomicOpPlan::Update(plan) = self {
            statements.extend(plan.graph_effects.iter().map(|effect| match effect {
                crate::atomic_plan::NoteUpdateStatement::Write(statement) => {
                    (statement.clone(), false)
                }
                crate::atomic_plan::NoteUpdateStatement::Assert(statement) => {
                    (statement.clone(), true)
                }
            }));
        }
        statements
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
    NoteConflict(crate::note_write::NoteWriteConflict),
    EntityConflict(crate::entity_write::EntityVersionConflict),
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
/// Only the shared prepared-unit commit owner constructs this token.
/// Consumers may inspect
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

async fn execute_savepoint(writer: &mut dyn SqlWriter, sql: String) -> Result<(), StorageError> {
    writer
        .execute(SqlStatement::new(sql, vec![]))
        .await
        .map(|_| ())
}

async fn begin_savepoint(writer: &mut dyn SqlWriter, name: &str) -> Result<(), StorageError> {
    execute_savepoint(writer, format!("SAVEPOINT {name}")).await
}

async fn release_savepoint(writer: &mut dyn SqlWriter, name: &str) -> Result<(), StorageError> {
    execute_savepoint(writer, format!("RELEASE {name}")).await
}

async fn rollback_to_savepoint(writer: &mut dyn SqlWriter, name: &str) -> Result<(), StorageError> {
    execute_savepoint(writer, format!("ROLLBACK TO {name}")).await
}

/// Apply one op's plan statements in order, checking each guarded
/// statement's affected-row count (result-row count for a GTD no-op assertion)
/// before moving to the next (ADR-099 D1
/// rule 2). Returns on the first statement that either errors or fails its
/// guard — never applies a later statement once an earlier one in the same
/// plan has failed.
pub(crate) struct AppliedPlan {
    pub(crate) effect: Option<PostCommitEffect>,
    pub(crate) note_version: Option<i64>,
}

pub(crate) async fn apply_plan(
    writer: &mut dyn SqlWriter,
    plan: &AtomicOpPlan,
    capture_note_versions: bool,
) -> Result<AppliedPlan, AtomicOpFailure> {
    if let AtomicOpPlan::FinalizeEntity(plan) = plan {
        return plan.apply(writer).await.map(|effect| AppliedPlan {
            effect,
            note_version: None,
        });
    }
    if let AtomicOpPlan::Update(plan) = plan {
        if let Some(guard) = &plan.entity_guard {
            if let Some(conflict) =
                guard
                    .check(writer)
                    .await
                    .map_err(|error| AtomicOpFailure::SqlError {
                        statement_label: Some("entity-version-precondition".into()),
                        message: error.to_string(),
                    })?
            {
                return Err(AtomicOpFailure::EntityConflict(conflict));
            }
        }
    }
    let note_guard = match plan {
        AtomicOpPlan::Update(plan) => plan.note_guard.as_ref(),
        AtomicOpPlan::AddNote(plan) => plan.note_guard.as_ref(),
        _ => None,
    };
    if let Some(guard) = note_guard {
        if let Some(conflict) =
            guard
                .check_fence(writer)
                .await
                .map_err(|error| AtomicOpFailure::SqlError {
                    statement_label: Some("note-fence".into()),
                    message: error.to_string(),
                })?
        {
            return Err(AtomicOpFailure::NoteConflict(conflict));
        }
    }
    let mut post_commit = plan.post_commit_effect();
    if let AtomicOpPlan::Update(plan) = plan {
        if let Some(inheritance) = &plan.note_embedding_inheritance {
            if !inheritance
                .vectors
                .has_rows(writer)
                .await
                .map_err(|error| AtomicOpFailure::SqlError {
                    statement_label: Some("note-embedding-inheritance".into()),
                    message: error.to_string(),
                })?
            {
                post_commit = Some(PostCommitEffect::NoteChanged {
                    note_id: plan.target_id,
                    kind: inheritance.kind.clone(),
                });
            }
        }
    }
    for (index, (stmt, read_assertion)) in plan.plan_statements().into_iter().enumerate() {
        let label = stmt.statement.label.clone();
        // No-op GTD plans are typed read assertions, not equal-value writes:
        // even assigning a column to itself would fire the version trigger.
        let result = if read_assertion
            || matches!(plan, AtomicOpPlan::GtdTransition(p) if p.idempotent_noop)
            || matches!(plan, AtomicOpPlan::Update(p) if p.idempotent_noop)
        {
            writer
                .query_all(stmt.statement)
                .await
                .map(|rows| rows.len() as u64)
        } else {
            writer.execute(stmt.statement).await
        };
        let affected = result.map_err(|e| AtomicOpFailure::SqlError {
            statement_label: label.clone(),
            message: e.to_string(),
        })?;
        if let Some(guard) = stmt.guard {
            if !guard.holds_for(affected) {
                if let Some(note_guard) = note_guard.filter(|_| index == 0) {
                    if let Some(conflict) =
                        note_guard.classify_refusal(writer).await.map_err(|error| {
                            AtomicOpFailure::SqlError {
                                statement_label: label.clone(),
                                message: error.to_string(),
                            }
                        })?
                    {
                        return Err(AtomicOpFailure::NoteConflict(conflict));
                    }
                }
                return Err(AtomicOpFailure::GuardFailed {
                    statement_label: label,
                    expected: guard,
                    observed: affected,
                });
            }
        }
    }
    if let AtomicOpPlan::Update(plan) = plan {
        if let Some(purge) = &plan.note_vector_purge {
            purge
                .apply(writer)
                .await
                .map_err(|error| AtomicOpFailure::SqlError {
                    statement_label: Some("note-vector-purge".into()),
                    message: error.to_string(),
                })?;
        }
    }
    let note_version = if let AtomicOpPlan::AddNote(plan) = plan {
        if capture_note_versions {
            let current = writer
                .query_scalar(
                    SqlStatement::new(
                        "SELECT version FROM notes WHERE id=?1",
                        vec![khive_storage::SqlValue::Text(plan.note_id.to_string())],
                    )
                    .labelled("note-write-guard"),
                )
                .await
                .map_err(|error| AtomicOpFailure::SqlError {
                    statement_label: Some("note-version-receipt".into()),
                    message: error.to_string(),
                })?;
            let Some(khive_storage::SqlValue::Integer(version)) = current else {
                return Err(AtomicOpFailure::SqlError {
                    statement_label: Some("note-version-receipt".into()),
                    message: "missing persisted note version".into(),
                });
            };
            Some(version)
        } else {
            None
        }
    } else {
        None
    };
    Ok(AppliedPlan {
        effect: post_commit,
        note_version,
    })
}

/// Run `plans` as ONE atomic unit (ADR-099 D1 commit pass): open a single
/// [`SqlAccess::atomic_unit`], apply every plan's statements under a named
/// `SAVEPOINT` (`adr099_atomic_op_<n>`), and commit all or roll back all.
///
/// **This is the seam the atomic-unit suspend-free invariant governs (see
/// the module doc comment above).** The closure built here drives only
/// `AtomicOpPlan::plan_statements` — DML or a typed read assertion — against the writer
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
    run_atomic_unit_with_note_versions(access, plans, false)
        .await
        .map(|(outcome, _)| outcome)
}

/// Optional per-AddNote receipts remain private and are returned only on whole
/// unit commit. Capturing after each op preserves versions for repeated IDs.
pub(crate) async fn run_atomic_unit_with_note_versions(
    access: &dyn SqlAccess,
    plans: Vec<AtomicOpPlan>,
    capture_note_versions: bool,
) -> Result<(AtomicRunOutcome, Vec<i64>), AtomicRunnerError> {
    let finalizers: Vec<_> = plans
        .iter()
        .enumerate()
        .filter_map(|(index, plan)| match plan {
            AtomicOpPlan::FinalizeEntity(plan) => Some((index, plan.for_settlement())),
            _ => None,
        })
        .collect();
    let op: PreparedAtomicOp<Vec<i64>, (usize, AtomicOpFailure)> = Box::new(move |writer| {
        Box::pin(async move {
            let mut post_commit = Vec::new();
            let mut note_versions = Vec::new();
            for (op_index, plan) in plans.iter().enumerate() {
                let savepoint = format!("adr099_atomic_op_{op_index}");
                begin_savepoint(writer, &savepoint).await?;
                match apply_plan(writer, plan, capture_note_versions).await {
                    Ok(applied) => {
                        release_savepoint(writer, &savepoint).await?;
                        if let Some(effect) = applied.effect {
                            post_commit.push(effect);
                        }
                        if let Some(version) = applied.note_version {
                            note_versions.push(version);
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
                        return Err(PreparedAtomicError::Refused {
                            failure: (op_index, failure),
                            message: format!("ADR-099 atomic unit aborted at op {op_index}"),
                        });
                    }
                }
            }
            Ok((note_versions, post_commit))
        })
    });

    match run_prepared_atomic_unit(access, op).await {
        Ok(PreparedAtomicOutcome::Committed { value, post_commit }) => {
            for (_, plan) in finalizers {
                plan.finish_commit();
            }
            Ok((AtomicRunOutcome::Committed { post_commit }, value))
        }
        Ok(PreparedAtomicOutcome::RolledBack((failed_op_index, failure))) => {
            let mut native_source = None;
            for (index, plan) in finalizers {
                if index <= failed_op_index {
                    if let Some(source) = plan.finish_rollback(access).await {
                        native_source = Some(source);
                    }
                }
            }
            if let Some(source) = native_source {
                return Err(AtomicRunnerError(StorageError::WriterTaskRequestFailed {
                    request_state: khive_storage::WriterTaskRequestState::TransactionRolledBack,
                    source: Box::new(source),
                }));
            }
            Ok((
                AtomicRunOutcome::RolledBack {
                    failed_op_index,
                    failure,
                },
                Vec::new(),
            ))
        }
        Err(error) => {
            // A lost acknowledgement is not proof of rollback. Only the
            // storage owner's explicit settled state permits failure audits.
            if matches!(
                &error,
                StorageError::WriterTaskRequestFailed {
                    request_state: khive_storage::WriterTaskRequestState::TransactionRolledBack,
                    ..
                }
            ) {
                for (_, plan) in finalizers {
                    let _ = plan.finish_rollback(access).await;
                }
            }
            Err(AtomicRunnerError(error))
        }
    }
}

pub(crate) enum PreparedAtomicError<E> {
    Refused { failure: E, message: String },
    Storage(StorageError),
}

impl<E> From<StorageError> for PreparedAtomicError<E> {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

pub(crate) enum PreparedAtomicOutcome<T, E> {
    Committed {
        value: T,
        post_commit: CommittedPostCommitEffects,
    },
    RolledBack(E),
}

pub(crate) type PreparedAtomicOp<T, E> = Box<
    dyn for<'w> FnOnce(
            &'w mut dyn SqlWriter,
        )
            -> BoxFuture<'w, Result<(T, Vec<PostCommitEffect>), PreparedAtomicError<E>>>
        + Send,
>;

/// Recover a recorded refusal only after the storage owner proves rollback;
/// effects become executable only after that same owner confirms commit.
pub(crate) async fn run_prepared_atomic_unit<T: Send + 'static, E: Send + 'static>(
    access: &dyn SqlAccess,
    prepared: PreparedAtomicOp<T, E>,
) -> Result<PreparedAtomicOutcome<T, E>, StorageError> {
    let failure_slot = Arc::new(Mutex::new(None));
    let recorded = Arc::clone(&failure_slot);
    let op: AtomicUnitOp = Box::new(move |writer| {
        Box::pin(async move {
            match prepared(writer).await {
                Ok(value) => Ok(Box::new(value) as Box<dyn Any + Send>),
                Err(PreparedAtomicError::Storage(error)) => Err(error),
                Err(PreparedAtomicError::Refused { failure, message }) => {
                    *recorded
                        .lock()
                        .expect("atomic runner failure slot poisoned") = Some(failure);
                    Err(StorageError::Internal(message))
                }
            }
        })
    });
    match access.atomic_unit(op).await {
        Ok(boxed) => {
            let (value, effects) = *boxed
                .downcast::<(T, Vec<PostCommitEffect>)>()
                .map_err(|_| StorageError::Internal("invalid prepared atomic outcome".into()))?;
            Ok(PreparedAtomicOutcome::Committed {
                value,
                post_commit: CommittedPostCommitEffects::new(effects),
            })
        }
        Err(storage_err) => {
            // A recorded op failure does not prove the outer transaction rolled back.
            if !atomic_unit_error_allows_recorded_refusal(&storage_err) {
                return Err(storage_err);
            }
            let recorded = failure_slot
                .lock()
                .expect("atomic runner failure slot poisoned")
                .take();
            match recorded {
                Some(failure) => Ok(PreparedAtomicOutcome::RolledBack(failure)),
                None => Err(storage_err),
            }
        }
    }
}

pub(crate) fn atomic_unit_error_allows_recorded_refusal(error: &StorageError) -> bool {
    match error.without_sqlite_write_stage() {
        StorageError::WriterTaskTerminated { .. } => false,
        StorageError::WriterTaskRequestFailed { request_state, .. } => {
            *request_state == khive_storage::WriterTaskRequestState::TransactionRolledBack
        }
        _ => true,
    }
}

#[cfg(test)]
#[path = "atomic_runner_tests.rs"]
mod tests;
