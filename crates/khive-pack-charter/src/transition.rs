//! Revision-and-state compare-and-set primitives for a caller-owned transaction.
//!
//! These helpers are not charter lifecycle or authorization gates. Callers must
//! use the writer supplied by [`khive_storage::SqlAccess::atomic_unit`], read and
//! validate subject, definition and authority state there, enforce the allowed
//! lifecycle transition, and append the corresponding evidence and command
//! receipt in that same atomic unit. In particular, advancing a run revision
//! here does not by itself publish its evidence sequence head. No helper opens
//! or commits a transaction, acquires authority, or admits an external effect.

use khive_storage::{SqlStatement, SqlValue, SqlWriter, StorageCapability, StorageError};
use thiserror::Error;

/// The closed run-state vocabulary in ADR-193.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunState {
    Open,
    Completed,
    Cancelled,
    Superseded,
    Invalidated,
    Failed,
}

impl RunState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Superseded => "superseded",
            Self::Invalidated => "invalidated",
            Self::Failed => "failed",
        }
    }
}

/// The closed phase-state vocabulary in ADR-193.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhaseState {
    Dormant,
    WaitingTrigger,
    WaitingGate,
    WaitingActor,
    Ready,
    Executing,
    Uncertain,
    Completed,
}

impl PhaseState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dormant => "dormant",
            Self::WaitingTrigger => "waiting_trigger",
            Self::WaitingGate => "waiting_gate",
            Self::WaitingActor => "waiting_actor",
            Self::Ready => "ready",
            Self::Executing => "executing",
            Self::Uncertain => "uncertain",
            Self::Completed => "completed",
        }
    }
}

/// The snapshot guard and next state for one run.
#[derive(Clone, Copy, Debug)]
pub struct RunTransition<'a> {
    pub policy_domain: &'a str,
    pub run_id: &'a str,
    pub expected_revision: i64,
    pub expected_state: RunState,
    pub next_state: RunState,
    pub at_us: i64,
}

/// The snapshot guard and next state for one phase of a run.
#[derive(Clone, Copy, Debug)]
pub struct PhaseTransition<'a> {
    pub policy_domain: &'a str,
    pub run_id: &'a str,
    pub phase_id: &'a str,
    pub expected_revision: i64,
    pub expected_state: PhaseState,
    pub next_state: PhaseState,
    pub at_us: i64,
}

/// A compare-and-set refusal or an unchanged underlying storage failure.
#[derive(Debug, Error)]
pub enum TransitionError {
    #[error("RevisionConflict: {record} {id} did not match revision {expected_revision} and state {expected_state}")]
    RevisionConflict {
        record: &'static str,
        id: String,
        expected_revision: i64,
        expected_state: &'static str,
    },
    #[error("InvalidRevision: {value} cannot be advanced")]
    InvalidRevision { value: i64 },
    #[error("charter transition affected {affected} rows; expected exactly one")]
    UnexpectedAffectedRows { affected: u64 },
    #[error(transparent)]
    Storage(#[from] StorageError),
}

impl From<TransitionError> for StorageError {
    fn from(error: TransitionError) -> Self {
        match error {
            TransitionError::Storage(inner) => inner,
            conflict @ TransitionError::RevisionConflict { .. } => Self::Conflict {
                capability: StorageCapability::Sql,
                operation: "charter.transition".into(),
                message: conflict.to_string(),
            },
            invalid @ TransitionError::InvalidRevision { .. } => Self::InvalidInput {
                capability: StorageCapability::Sql,
                operation: "charter.transition".into(),
                message: invalid.to_string(),
            },
            unexpected @ TransitionError::UnexpectedAffectedRows { .. } => {
                Self::Internal(unexpected.to_string())
            }
        }
    }
}

/// Advance exactly one guarded run row and return its new revision.
///
/// The caller must propagate any error out of its enclosing atomic unit so all
/// accompanying writes roll back. See the module-level transaction obligations;
/// this primitive does not decide which state changes the caller may perform.
pub async fn transition_run_in_transaction(
    writer: &mut dyn SqlWriter,
    transition: RunTransition<'_>,
) -> Result<i64, TransitionError> {
    let next_revision = next_revision(transition.expected_revision)?;
    let affected = writer
        .execute(
            SqlStatement::new(
                include_str!("../sql/transition-run.sql"),
                vec![
                    SqlValue::Text(transition.policy_domain.to_owned()),
                    SqlValue::Text(transition.run_id.to_owned()),
                    SqlValue::Integer(transition.expected_revision),
                    SqlValue::Text(transition.expected_state.as_str().to_owned()),
                    SqlValue::Text(transition.next_state.as_str().to_owned()),
                    SqlValue::Integer(transition.at_us),
                ],
            )
            .labelled("charter.transition_run"),
        )
        .await?;
    require_one_row(
        affected,
        "run",
        transition.run_id,
        transition.expected_revision,
        transition.expected_state.as_str(),
    )?;
    Ok(next_revision)
}

/// Advance exactly one guarded phase row and return its new revision.
///
/// The caller must also guard its run and current-phase relationship inside the
/// same atomic unit. Any error must escape that unit so preceding writes roll
/// back; this primitive does not decide which lifecycle transition is allowed.
pub async fn transition_phase_in_transaction(
    writer: &mut dyn SqlWriter,
    transition: PhaseTransition<'_>,
) -> Result<i64, TransitionError> {
    let next_revision = next_revision(transition.expected_revision)?;
    let affected = writer
        .execute(
            SqlStatement::new(
                include_str!("../sql/transition-phase.sql"),
                vec![
                    SqlValue::Text(transition.policy_domain.to_owned()),
                    SqlValue::Text(transition.run_id.to_owned()),
                    SqlValue::Text(transition.phase_id.to_owned()),
                    SqlValue::Integer(transition.expected_revision),
                    SqlValue::Text(transition.expected_state.as_str().to_owned()),
                    SqlValue::Text(transition.next_state.as_str().to_owned()),
                    SqlValue::Integer(transition.at_us),
                ],
            )
            .labelled("charter.transition_phase"),
        )
        .await?;
    require_one_row(
        affected,
        "phase",
        transition.phase_id,
        transition.expected_revision,
        transition.expected_state.as_str(),
    )?;
    Ok(next_revision)
}

fn next_revision(value: i64) -> Result<i64, TransitionError> {
    if value < 0 {
        return Err(TransitionError::InvalidRevision { value });
    }
    value
        .checked_add(1)
        .ok_or(TransitionError::InvalidRevision { value })
}

fn require_one_row(
    affected: u64,
    record: &'static str,
    id: &str,
    expected_revision: i64,
    expected_state: &'static str,
) -> Result<(), TransitionError> {
    match affected {
        1 => Ok(()),
        0 => Err(TransitionError::RevisionConflict {
            record,
            id: id.to_owned(),
            expected_revision,
            expected_state,
        }),
        _ => Err(TransitionError::UnexpectedAffectedRows { affected }),
    }
}
