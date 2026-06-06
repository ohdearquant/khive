//! Proposals projection worker — maintains the `proposals_open` table.

mod helpers;

use helpers::build_conditional_event_insert;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::{
    event::Event,
    types::{SqlStatement, SqlValue},
};
use khive_types::{ProposalDecision, ProposalReviewedPayload};
use uuid::Uuid;

/// Worker that maintains the `proposals_open` projection table from proposal events.
pub struct ProposalsProjectionWorker {
    runtime: KhiveRuntime,
}

impl ProposalsProjectionWorker {
    /// Create a new projection worker backed by the given runtime.
    pub fn new(runtime: KhiveRuntime) -> Self {
        Self { runtime }
    }

    /// Called after a `ProposalCreated` event is emitted.
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
    pub async fn on_proposal_reviewed(
        &self,
        token: &NamespaceToken,
        payload: &ProposalReviewedPayload,
    ) -> Result<bool, RuntimeError> {
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

        let last_decision_str = payload.decision.as_str();

        let sql = self.runtime.sql();
        let mut writer = sql.writer().await.map_err(RuntimeError::Storage)?;

        let rows = if let Some(new_status) = new_status_opt {
            writer
                .execute(SqlStatement {
                    sql: "UPDATE proposals_open \
                          SET status = ?1, updated_at = ?2, last_decision = ?3, \
                              review_count = review_count + 1, \
                              approve_count = approve_count + ?4, \
                              reject_count = reject_count + ?5 \
                          WHERE proposal_id = ?6 AND namespace = ?7 \
                            AND status NOT IN ('applied', 'withdrawn', 'rejected', 'approved')"
                        .to_string(),
                    params: vec![
                        SqlValue::Text(new_status.to_string()),
                        SqlValue::Integer(now),
                        SqlValue::Text(last_decision_str.to_string()),
                        SqlValue::Integer(approve_delta),
                        SqlValue::Integer(reject_delta),
                        SqlValue::Text(proposal_id.to_string()),
                        SqlValue::Text(ns),
                    ],
                    label: Some("projection_worker.proposals_open.update_review_status".into()),
                })
                .await
                .map_err(RuntimeError::Storage)?
        } else {
            writer
                .execute(SqlStatement {
                    sql: "UPDATE proposals_open \
                          SET updated_at = ?1, last_decision = ?2, \
                              review_count = review_count + 1 \
                          WHERE proposal_id = ?3 AND namespace = ?4"
                        .to_string(),
                    params: vec![
                        SqlValue::Integer(now),
                        SqlValue::Text(last_decision_str.to_string()),
                        SqlValue::Text(proposal_id.to_string()),
                        SqlValue::Text(ns),
                    ],
                    label: Some("projection_worker.proposals_open.update_review_comment".into()),
                })
                .await
                .map_err(RuntimeError::Storage)?
        };

        Ok(rows == 1)
    }

    /// Called after a `ProposalApplied` event is emitted.
    pub async fn on_proposal_applied(
        &self,
        token: &NamespaceToken,
        proposal_id: Uuid,
    ) -> Result<bool, RuntimeError> {
        let now = chrono::Utc::now().timestamp_micros();
        let ns = token.namespace().as_str().to_owned();
        let sql = self.runtime.sql();
        let mut writer = sql.writer().await.map_err(RuntimeError::Storage)?;
        let rows = writer
            .execute(SqlStatement {
                sql: "UPDATE proposals_open \
                      SET status = 'applied', updated_at = ?1 \
                      WHERE proposal_id = ?2 AND namespace = ?3 \
                        AND status = 'applying'"
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
        Ok(rows == 1)
    }

    /// Atomically move status `approved` → `applying` before KG mutation.
    pub async fn pre_apply_cas(
        &self,
        token: &NamespaceToken,
        proposal_id: Uuid,
    ) -> Result<bool, RuntimeError> {
        let now = chrono::Utc::now().timestamp_micros();
        let ns = token.namespace().as_str().to_owned();
        let sql = self.runtime.sql();
        let mut writer = sql.writer().await.map_err(RuntimeError::Storage)?;
        let rows = writer
            .execute(SqlStatement {
                sql: "UPDATE proposals_open \
                      SET status = 'applying', updated_at = ?1 \
                      WHERE proposal_id = ?2 AND namespace = ?3 \
                        AND status = 'approved'"
                    .to_string(),
                params: vec![
                    SqlValue::Integer(now),
                    SqlValue::Text(proposal_id.to_string()),
                    SqlValue::Text(ns),
                ],
                label: Some("projection_worker.proposals_open.pre_apply_cas".into()),
            })
            .await
            .map_err(RuntimeError::Storage)?;
        Ok(rows == 1)
    }

    /// Called after a `ProposalWithdrawn` event is emitted.
    pub async fn on_proposal_withdrawn(
        &self,
        token: &NamespaceToken,
        proposal_id: Uuid,
    ) -> Result<bool, RuntimeError> {
        let now = chrono::Utc::now().timestamp_micros();
        let ns = token.namespace().as_str().to_owned();
        let sql = self.runtime.sql();
        let mut writer = sql.writer().await.map_err(RuntimeError::Storage)?;
        let rows = writer
            .execute(SqlStatement {
                sql: "UPDATE proposals_open \
                      SET status = 'withdrawn', updated_at = ?1 \
                      WHERE proposal_id = ?2 AND namespace = ?3 \
                        AND status NOT IN ('applied', 'applying', 'withdrawn', 'rejected')"
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
        Ok(rows == 1)
    }

    /// Revert status from `applying` back to `approved` on KG failure.
    pub async fn revert_applying_to_approved(
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
                      SET status = 'approved', updated_at = ?1 \
                      WHERE proposal_id = ?2 AND namespace = ?3 \
                        AND status = 'applying'"
                    .to_string(),
                params: vec![
                    SqlValue::Integer(now),
                    SqlValue::Text(proposal_id.to_string()),
                    SqlValue::Text(ns),
                ],
                label: Some("projection_worker.proposals_open.revert_applying".into()),
            })
            .await
            .map_err(RuntimeError::Storage)?;
        Ok(())
    }

    /// Atomically run the reviewed CAS UPDATE and `ProposalReviewed` event INSERT.
    pub async fn reviewed_and_emit(
        &self,
        token: &NamespaceToken,
        payload: &ProposalReviewedPayload,
        event: Event,
        decision_changes_state: bool,
    ) -> Result<(bool, Uuid), RuntimeError> {
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
        let last_decision_str = payload.decision.as_str();

        let (projection_stmt, guard_sql, guard_params) = if let Some(new_status) = new_status_opt {
            let stmt = SqlStatement {
                    sql: "UPDATE proposals_open \
                          SET status = ?1, updated_at = ?2, last_decision = ?3, \
                              review_count = review_count + 1, \
                              approve_count = approve_count + ?4, \
                              reject_count = reject_count + ?5 \
                          WHERE proposal_id = ?6 AND namespace = ?7 \
                            AND status NOT IN ('applied', 'applying', 'withdrawn', 'rejected', 'approved')"
                        .to_string(),
                    params: vec![
                        SqlValue::Text(new_status.to_string()),
                        SqlValue::Integer(now),
                        SqlValue::Text(last_decision_str.to_string()),
                        SqlValue::Integer(approve_delta),
                        SqlValue::Integer(reject_delta),
                        SqlValue::Text(proposal_id.to_string()),
                        SqlValue::Text(ns.clone()),
                    ],
                    label: Some("projection_worker.reviewed_and_emit.cas".into()),
                };
            let guard = "changes() = 1";
            let gp: Vec<SqlValue> = vec![];
            (stmt, guard, gp)
        } else {
            let stmt = SqlStatement {
                sql: "UPDATE proposals_open \
                          SET updated_at = ?1, last_decision = ?2, \
                              review_count = review_count + 1 \
                          WHERE proposal_id = ?3 AND namespace = ?4"
                    .to_string(),
                params: vec![
                    SqlValue::Integer(now),
                    SqlValue::Text(last_decision_str.to_string()),
                    SqlValue::Text(proposal_id.to_string()),
                    SqlValue::Text(ns.clone()),
                ],
                label: Some("projection_worker.reviewed_and_emit.comment".into()),
            };
            let guard = "changes() = 1";
            let gp: Vec<SqlValue> = vec![];
            (stmt, guard, gp)
        };

        let event_id = event.id;
        let event_stmt = build_conditional_event_insert(&event, guard_sql, guard_params);

        let sql = self.runtime.sql();
        let mut writer = sql.writer().await.map_err(RuntimeError::Storage)?;
        let total_rows = writer
            .execute_batch(vec![projection_stmt, event_stmt])
            .await
            .map_err(RuntimeError::Storage)?;

        let cas_hit = if decision_changes_state {
            total_rows == 2
        } else {
            true
        };

        Ok((cas_hit, event_id))
    }

    /// Atomically run the withdrawn CAS UPDATE + `ProposalWithdrawn` event INSERT.
    pub async fn withdrawn_and_emit(
        &self,
        token: &NamespaceToken,
        proposal_id: Uuid,
        event: Event,
    ) -> Result<(bool, Uuid), RuntimeError> {
        let now = chrono::Utc::now().timestamp_micros();
        let ns = token.namespace().as_str().to_owned();

        let projection_stmt = SqlStatement {
            sql: "UPDATE proposals_open \
                  SET status = 'withdrawn', updated_at = ?1 \
                  WHERE proposal_id = ?2 AND namespace = ?3 \
                    AND status NOT IN ('applied', 'applying', 'withdrawn', 'rejected')"
                .to_string(),
            params: vec![
                SqlValue::Integer(now),
                SqlValue::Text(proposal_id.to_string()),
                SqlValue::Text(ns),
            ],
            label: Some("projection_worker.withdrawn_and_emit.cas".into()),
        };

        let guard_sql = "changes() = 1";
        let guard_params: Vec<SqlValue> = vec![];

        let event_id = event.id;
        let event_stmt = build_conditional_event_insert(&event, guard_sql, guard_params);

        let sql = self.runtime.sql();
        let mut writer = sql.writer().await.map_err(RuntimeError::Storage)?;
        let total_rows = writer
            .execute_batch(vec![projection_stmt, event_stmt])
            .await
            .map_err(RuntimeError::Storage)?;

        let cas_hit = total_rows == 2;
        Ok((cas_hit, event_id))
    }

    /// Read the current row from `proposals_open` for a given proposal_id.
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
    /// UUID of the proposal as a string.
    pub proposal_id: String,
    /// Identity of the agent that submitted the proposal.
    pub proposer: String,
    /// Current lifecycle status.
    pub status: String,
    /// Number of approve decisions recorded.
    pub approve_count: i64,
    /// Number of reject decisions recorded.
    pub reject_count: i64,
}

#[cfg(test)]
mod tests;
