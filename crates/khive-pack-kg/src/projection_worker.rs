//! ProposalsProjectionWorker — maintains the `proposals_open` projection table.
//!
//! Subscribes to all four proposal EventKinds:
//! - `ProposalCreated`  → INSERT with status='open'
//! - `ProposalReviewed` → UPDATE counts; set status based on decision
//! - `ProposalApplied`  → UPDATE status='applied'
//! - `ProposalWithdrawn`→ UPDATE status='withdrawn'
//!
//! ADR-046 §4: The projection table is the authoritative read surface for
//! `list(kind=proposal)`. Handlers MUST NOT write to it directly; only this
//! worker writes projection rows.

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_types::{ProposalDecision, ProposalReviewedPayload};
use uuid::Uuid;

/// Worker that maintains the `proposals_open` projection table from proposal events.
///
/// Called synchronously from the KG pack handlers after they emit events. This keeps
/// the projection in sync without requiring a background thread or `PackEventConsumer`
/// infrastructure (which is not yet implemented).
pub struct ProposalsProjectionWorker {
    runtime: KhiveRuntime,
}

impl ProposalsProjectionWorker {
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self { runtime }
    }

    /// Called after a `ProposalCreated` event is emitted.
    ///
    /// Inserts a row into `proposals_open` with status='open'.
    pub async fn on_proposal_created(
        &self,
        token: &NamespaceToken,
        proposal_id: Uuid,
        proposer: &str,
        title: &str,
        expiry: Option<i64>,
    ) -> Result<(), RuntimeError> {
        let now = chrono::Utc::now().timestamp_micros();
        let ns = token.namespace().as_str().to_owned();
        let sql = self.runtime.sql();
        let mut writer = sql.writer().await.map_err(RuntimeError::Storage)?;
        writer
            .execute(SqlStatement {
                sql: "INSERT INTO proposals_open \
                        (proposal_id, namespace, proposer, title, status, \
                         created_at, updated_at, expiry) \
                      VALUES (?1, ?2, ?3, ?4, 'open', ?5, ?5, ?6)"
                    .to_string(),
                params: vec![
                    SqlValue::Text(proposal_id.to_string()),
                    SqlValue::Text(ns),
                    SqlValue::Text(proposer.to_string()),
                    SqlValue::Text(title.to_string()),
                    SqlValue::Integer(now),
                    match expiry {
                        Some(v) => SqlValue::Integer(v),
                        None => SqlValue::Null,
                    },
                ],
                label: Some("projection_worker.proposals_open.insert".into()),
            })
            .await
            .map_err(RuntimeError::Storage)?;
        Ok(())
    }

    /// Called after a `ProposalReviewed` event is emitted.
    ///
    /// Updates counts and status in `proposals_open`. Decision semantics:
    /// - `Approve`         → status='approved', approve_count++
    /// - `Reject`          → status='rejected', reject_count++
    /// - `Comment`         → counts unchanged, status unchanged
    /// - `RequestChanges`  → status='changes_requested', counts unchanged
    pub async fn on_proposal_reviewed(
        &self,
        token: &NamespaceToken,
        payload: &ProposalReviewedPayload,
    ) -> Result<(), RuntimeError> {
        let now = chrono::Utc::now().timestamp_micros();
        let ns = token.namespace().as_str().to_owned();
        let proposal_id = Uuid::from_u128(payload.proposal_id.to_u128());

        let (new_status_opt, approve_delta, reject_delta): (Option<&str>, i64, i64) =
            match payload.decision {
                ProposalDecision::Approve => (Some("approved"), 1, 0),
                ProposalDecision::Reject => (Some("rejected"), 0, 1),
                ProposalDecision::Comment => (None, 0, 0),
                ProposalDecision::RequestChanges => (Some("changes_requested"), 0, 0),
            };

        let last_decision_json = serde_json::to_string(&payload.decision)
            .map_err(|e| RuntimeError::Internal(format!("serialize decision: {e}")))?;

        let sql = self.runtime.sql();
        let mut writer = sql.writer().await.map_err(RuntimeError::Storage)?;

        if let Some(new_status) = new_status_opt {
            writer
                .execute(SqlStatement {
                    sql: "UPDATE proposals_open \
                          SET status = ?1, updated_at = ?2, last_decision = ?3, \
                              review_count = review_count + 1, \
                              approve_count = approve_count + ?4, \
                              reject_count = reject_count + ?5 \
                          WHERE proposal_id = ?6 AND namespace = ?7"
                        .to_string(),
                    params: vec![
                        SqlValue::Text(new_status.to_string()),
                        SqlValue::Integer(now),
                        SqlValue::Text(last_decision_json),
                        SqlValue::Integer(approve_delta),
                        SqlValue::Integer(reject_delta),
                        SqlValue::Text(proposal_id.to_string()),
                        SqlValue::Text(ns),
                    ],
                    label: Some("projection_worker.proposals_open.update_review_status".into()),
                })
                .await
                .map_err(RuntimeError::Storage)?;
        } else {
            // Comment: only bump review_count + last_decision, leave status as-is.
            writer
                .execute(SqlStatement {
                    sql: "UPDATE proposals_open \
                          SET updated_at = ?1, last_decision = ?2, \
                              review_count = review_count + 1 \
                          WHERE proposal_id = ?3 AND namespace = ?4"
                        .to_string(),
                    params: vec![
                        SqlValue::Integer(now),
                        SqlValue::Text(last_decision_json),
                        SqlValue::Text(proposal_id.to_string()),
                        SqlValue::Text(ns),
                    ],
                    label: Some("projection_worker.proposals_open.update_review_comment".into()),
                })
                .await
                .map_err(RuntimeError::Storage)?;
        }

        Ok(())
    }

    /// Called after a `ProposalApplied` event is emitted.
    ///
    /// Sets status='applied' unconditionally (even on Failed apply results, the
    /// status is 'approved_unapplied' semantically — but per ADR-046 §9, failed
    /// applies leave status='approved'. This method is only called on success).
    pub async fn on_proposal_applied(
        &self,
        token: &NamespaceToken,
        proposal_id: Uuid,
    ) -> Result<(), RuntimeError> {
        let now = chrono::Utc::now().timestamp_micros();
        let ns = token.namespace().as_str().to_owned();
        let sql = self.runtime.sql();
        let mut writer = sql.writer().await.map_err(RuntimeError::Storage)?;
        writer
            .execute(SqlStatement {
                sql: "UPDATE proposals_open \
                      SET status = 'applied', updated_at = ?1 \
                      WHERE proposal_id = ?2 AND namespace = ?3"
                    .to_string(),
                params: vec![
                    SqlValue::Integer(now),
                    SqlValue::Text(proposal_id.to_string()),
                    SqlValue::Text(ns),
                ],
                label: Some("projection_worker.proposals_open.applied".into()),
            })
            .await
            .map_err(RuntimeError::Storage)?;
        Ok(())
    }

    /// Called after a `ProposalWithdrawn` event is emitted.
    ///
    /// Sets status='withdrawn'.
    pub async fn on_proposal_withdrawn(
        &self,
        token: &NamespaceToken,
        proposal_id: Uuid,
    ) -> Result<(), RuntimeError> {
        let now = chrono::Utc::now().timestamp_micros();
        let ns = token.namespace().as_str().to_owned();
        let sql = self.runtime.sql();
        let mut writer = sql.writer().await.map_err(RuntimeError::Storage)?;
        writer
            .execute(SqlStatement {
                sql: "UPDATE proposals_open \
                      SET status = 'withdrawn', updated_at = ?1 \
                      WHERE proposal_id = ?2 AND namespace = ?3"
                    .to_string(),
                params: vec![
                    SqlValue::Integer(now),
                    SqlValue::Text(proposal_id.to_string()),
                    SqlValue::Text(ns),
                ],
                label: Some("projection_worker.proposals_open.withdrawn".into()),
            })
            .await
            .map_err(RuntimeError::Storage)?;
        Ok(())
    }

    /// Read the current row from `proposals_open` for a given proposal_id.
    ///
    /// Used by the apply worker to check current status before applying.
    pub async fn get_proposal_row(
        &self,
        token: &NamespaceToken,
        proposal_id: Uuid,
    ) -> Result<Option<ProposalRow>, RuntimeError> {
        let ns = token.namespace().as_str().to_owned();
        let sql = self.runtime.sql();
        let mut reader = sql.reader().await.map_err(RuntimeError::Storage)?;
        let row = reader
            .query_row(SqlStatement {
                sql: "SELECT proposal_id, proposer, status, approve_count, reject_count \
                      FROM proposals_open \
                      WHERE proposal_id = ?1 AND namespace = ?2"
                    .to_string(),
                params: vec![SqlValue::Text(proposal_id.to_string()), SqlValue::Text(ns)],
                label: Some("projection_worker.proposals_open.get".into()),
            })
            .await
            .map_err(RuntimeError::Storage)?;

        Ok(row.map(|r| {
            let get_text = |name: &str| -> String {
                r.get(name)
                    .and_then(|v| {
                        if let SqlValue::Text(s) = v {
                            Some(s.clone())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default()
            };
            let get_int = |name: &str| -> i64 {
                r.get(name)
                    .and_then(|v| {
                        if let SqlValue::Integer(i) = v {
                            Some(*i)
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0)
            };
            ProposalRow {
                proposal_id: get_text("proposal_id"),
                proposer: get_text("proposer"),
                status: get_text("status"),
                approve_count: get_int("approve_count"),
                reject_count: get_int("reject_count"),
            }
        }))
    }
}

/// Projection row from `proposals_open`.
#[derive(Debug, Clone)]
pub struct ProposalRow {
    pub proposal_id: String,
    pub proposer: String,
    pub status: String,
    pub approve_count: i64,
    pub reject_count: i64,
}

// ---- Tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use khive_runtime::{KhiveRuntime, Namespace};
    use khive_types::{Id128, ProposalDecision};
    use uuid::Uuid;

    fn setup() -> (KhiveRuntime, NamespaceToken) {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let tok = rt.authorize(Namespace::local());
        (rt, tok)
    }

    async fn ensure_schema(rt: &KhiveRuntime) {
        let sql = rt.sql();
        let mut writer = sql.writer().await.expect("writer");
        writer
            .execute(SqlStatement {
                sql: "\
                CREATE TABLE IF NOT EXISTS proposals_open (\
                    proposal_id TEXT PRIMARY KEY, \
                    namespace TEXT NOT NULL, \
                    proposer TEXT NOT NULL, \
                    title TEXT NOT NULL, \
                    status TEXT NOT NULL, \
                    created_at INTEGER NOT NULL, \
                    updated_at INTEGER NOT NULL, \
                    expiry INTEGER, \
                    last_decision TEXT, \
                    review_count INTEGER NOT NULL DEFAULT 0, \
                    approve_count INTEGER NOT NULL DEFAULT 0, \
                    reject_count INTEGER NOT NULL DEFAULT 0\
                )"
                .to_string(),
                params: vec![],
                label: Some("test.ensure_schema".into()),
            })
            .await
            .expect("create table");
    }

    #[tokio::test]
    async fn on_proposal_created_inserts_open_row() {
        let (rt, tok) = setup();
        ensure_schema(&rt).await;
        let worker = ProposalsProjectionWorker::new(rt.clone());
        let pid = Uuid::new_v4();

        worker
            .on_proposal_created(&tok, pid, "alice", "Add RoPE", None)
            .await
            .expect("on_proposal_created must succeed");

        let row = worker
            .get_proposal_row(&tok, pid)
            .await
            .expect("get_proposal_row must succeed")
            .expect("row must exist");

        assert_eq!(row.status, "open");
        assert_eq!(row.proposer, "alice");
    }

    #[tokio::test]
    async fn on_proposal_reviewed_approve_sets_status_approved() {
        let (rt, tok) = setup();
        ensure_schema(&rt).await;
        let worker = ProposalsProjectionWorker::new(rt.clone());
        let pid = Uuid::new_v4();

        worker
            .on_proposal_created(&tok, pid, "alice", "Test Proposal", None)
            .await
            .expect("create");

        let payload = ProposalReviewedPayload {
            proposal_id: Id128::from_u128(pid.as_u128()),
            reviewer: "bob".to_string(),
            decision: ProposalDecision::Approve,
            comment: None,
        };
        worker
            .on_proposal_reviewed(&tok, &payload)
            .await
            .expect("on_proposal_reviewed must succeed");

        let row = worker
            .get_proposal_row(&tok, pid)
            .await
            .expect("get row")
            .expect("row must exist");

        assert_eq!(row.status, "approved");
        assert_eq!(row.approve_count, 1);
        assert_eq!(row.reject_count, 0);
    }

    #[tokio::test]
    async fn on_proposal_withdrawn_sets_status_withdrawn() {
        let (rt, tok) = setup();
        ensure_schema(&rt).await;
        let worker = ProposalsProjectionWorker::new(rt.clone());
        let pid = Uuid::new_v4();

        worker
            .on_proposal_created(&tok, pid, "alice", "Withdraw Me", None)
            .await
            .expect("create");

        worker
            .on_proposal_withdrawn(&tok, pid)
            .await
            .expect("on_proposal_withdrawn must succeed");

        let row = worker
            .get_proposal_row(&tok, pid)
            .await
            .expect("get row")
            .expect("row must exist");

        assert_eq!(row.status, "withdrawn");
    }

    #[tokio::test]
    async fn on_proposal_applied_sets_status_applied() {
        let (rt, tok) = setup();
        ensure_schema(&rt).await;
        let worker = ProposalsProjectionWorker::new(rt.clone());
        let pid = Uuid::new_v4();

        worker
            .on_proposal_created(&tok, pid, "alice", "Apply Me", None)
            .await
            .expect("create");

        // Simulate approve path first.
        let approve_payload = ProposalReviewedPayload {
            proposal_id: Id128::from_u128(pid.as_u128()),
            reviewer: "alice".to_string(),
            decision: ProposalDecision::Approve,
            comment: None,
        };
        worker
            .on_proposal_reviewed(&tok, &approve_payload)
            .await
            .expect("review");

        worker
            .on_proposal_applied(&tok, pid)
            .await
            .expect("on_proposal_applied must succeed");

        let row = worker
            .get_proposal_row(&tok, pid)
            .await
            .expect("get row")
            .expect("row must exist");

        assert_eq!(row.status, "applied");
    }
}
