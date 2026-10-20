//! Git-owned annotation predicates over the runtime's generic create-only owner.

use khive_runtime::{KhiveRuntime, LinkSpec, NamespaceToken, RuntimeError, RuntimeResult};
use khive_storage::graph::{
    EdgeInsertIfAbsentOutcome, GraphStatementGuard, GraphStatementGuardFailureReason,
};
use khive_storage::{EdgeRelation, SqlStatement, SqlValue, StorageCapability, StorageError};
use uuid::Uuid;

const SOURCE: &str = "git.annotation.source";
const TARGET: &str = "git.annotation.target";
const CURSOR: &str = "git.annotation.cursor";

#[derive(Clone, Debug)]
pub(super) struct CursorValue {
    pub value: Vec<u8>,
    pub updated_at: i64,
}

#[derive(Clone, Debug)]
pub(super) struct AnnotationGuard {
    pub expected_sha: String,
    pub source_identity: String,
    pub commits: CursorValue,
    pub checkpoint: CursorValue,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AnnotationOutcome {
    Created,
    ExistingLive,
    Tombstoned,
    SourceChanged,
    TargetChanged,
    CursorChanged,
}

pub(super) async fn link_annotation_if_absent(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    commit_id: Uuid,
    project_id: Uuid,
    guard: AnnotationGuard,
) -> RuntimeResult<AnnotationOutcome> {
    if !matches!(guard.expected_sha.len(), 40 | 64)
        || !guard
            .expected_sha
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(RuntimeError::InvalidInput(
            "expected full commit SHA".into(),
        ));
    }
    let namespace = token.namespace().as_str().to_owned();
    let guards = vec![
        GraphStatementGuard {
            label: SOURCE.into(),
            statement: SqlStatement::new(
                include_str!("../../sql/commit_annotation_source_guard.sql"),
                vec![
                    SqlValue::Text(commit_id.to_string()),
                    SqlValue::Text(namespace.clone()),
                    SqlValue::Text(guard.expected_sha),
                ],
            ),
        },
        GraphStatementGuard {
            label: TARGET.into(),
            statement: SqlStatement::new(
                include_str!("../../sql/commit_annotation_target_guard.sql"),
                vec![
                    SqlValue::Text(project_id.to_string()),
                    SqlValue::Text(namespace),
                    SqlValue::Text(guard.source_identity),
                ],
            ),
        },
        GraphStatementGuard {
            label: CURSOR.into(),
            statement: SqlStatement::new(
                include_str!("../../sql/commit_annotation_cursor_guard.sql"),
                vec![
                    SqlValue::Text(project_id.to_string()),
                    SqlValue::Blob(guard.commits.value),
                    SqlValue::Integer(guard.commits.updated_at),
                    SqlValue::Blob(guard.checkpoint.value),
                    SqlValue::Integer(guard.checkpoint.updated_at),
                ],
            ),
        },
    ];
    let outcome = runtime
        .link_if_absent_with_guards(
            token,
            LinkSpec {
                namespace: None,
                source_id: commit_id,
                target_id: project_id,
                relation: EdgeRelation::Annotates,
                weight: 1.0,
                metadata: None,
                resurrect: false,
            },
            guards,
        )
        .await?;
    classify_outcome(outcome)
}

pub(super) fn classify_outcome(
    outcome: EdgeInsertIfAbsentOutcome,
) -> RuntimeResult<AnnotationOutcome> {
    match outcome {
        EdgeInsertIfAbsentOutcome::Created(_) => Ok(AnnotationOutcome::Created),
        EdgeInsertIfAbsentOutcome::ExistingLive => Ok(AnnotationOutcome::ExistingLive),
        EdgeInsertIfAbsentOutcome::Tombstoned => Ok(AnnotationOutcome::Tombstoned),
        EdgeInsertIfAbsentOutcome::GuardFailed(failure) => {
            if failure.reason == GraphStatementGuardFailureReason::PredicateFalse {
                match failure.label.as_str() {
                    SOURCE => return Ok(AnnotationOutcome::SourceChanged),
                    TARGET => return Ok(AnnotationOutcome::TargetChanged),
                    CURSOR => return Ok(AnnotationOutcome::CursorChanged),
                    _ => {}
                }
            }
            Err(StorageError::Conflict {
                capability: StorageCapability::Graph,
                operation: "commit_annotation".into(),
                message: failure.to_string(),
            }
            .into())
        }
    }
}
