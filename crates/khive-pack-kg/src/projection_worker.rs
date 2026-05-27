//! ProposalsProjectionWorker — maintains the `proposals_open` projection table.
//!
//! Subscribes to all four proposal EventKinds:
//! - `ProposalCreated`  → INSERT with status='open'
//! - `ProposalReviewed` → UPDATE counts; set status based on decision
//! - `ProposalApplied`  → UPDATE status='applied'  (from 'applying' via pre-apply CAS)
//! - `ProposalWithdrawn`→ UPDATE status='withdrawn'
//!
//! ADR-046 §4: The projection table is the authoritative read surface for
//! `list(kind=proposal)`. Handlers MUST NOT write to it directly; only this
//! worker writes projection rows.
//!
//! ADR-046 §3 amendment (H1): `'applying'` is a transient state used by the
//! apply worker.  The state machine is:
//!   open/changes_requested → approved → applying → applied
//! `withdraw` cannot land while status='applying'.
//!
//! H2 fix: `reviewed_and_emit` / `withdrawn_and_emit` run the projection CAS
//! UPDATE and the lifecycle event INSERT in a single `execute_batch` transaction,
//! preserving the ADR-046 invariant that events are the source of truth (a
//! committed projection state change is always backed by a corresponding event).

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::{
    event::Event,
    types::{SqlStatement, SqlValue},
};
use khive_types::{ProposalDecision, ProposalReviewedPayload};
use uuid::Uuid;

/// Build a conditional event INSERT `SqlStatement` for use in `execute_batch`.
///
/// The INSERT uses a `SELECT ... WHERE (CAS guard subquery)` form so that it
/// inserts exactly one row when `guard_sql` matches at least one row in
/// `proposals_open`, and zero rows otherwise.  Within the same `BEGIN IMMEDIATE`
/// transaction, `guard_sql` sees the post-UPDATE state of the CAS — if the CAS
/// UPDATE in the first batch statement matched 0 rows, `guard_sql` returns 0 rows
/// and the INSERT is skipped.  This ensures both statements are truly atomic:
/// either both commit or neither does.
///
/// Proposal lifecycle events (`ProposalReviewed`, `ProposalWithdrawn`) never
/// produce `event_observations` rows (they fall into the `_ => Ok(vec![])` arm
/// of `decode_event_observations` in `khive-db`), so no observation inserts are
/// needed here.
fn build_conditional_event_insert(
    event: &Event,
    guard_sql: &str,
    guard_params: Vec<SqlValue>,
) -> SqlStatement {
    let substrate_str = event.substrate.name().to_string();
    let kind_str = event.kind.name().to_string();
    let outcome_str = event.outcome.name().to_string();
    let payload_str = event.payload.to_string();

    // Parameter layout:
    //   ?1  – id,  ?2  – namespace,  ?3  – verb,  ?4  – substrate,
    //   ?5  – actor,  ?6  – kind,  ?7  – outcome,  ?8  – payload,
    //   ?9  – payload_schema_version,  ?10 – profile_state_version,
    //   ?11 – duration_us,  ?12 – target_id,  ?13 – session_id,
    //   ?14 – aggregate_kind,  ?15 – aggregate_id,  ?16 – created_at,
    //   ?17+ – guard_params (passed to the WHERE subquery).
    //
    // The guard is inlined as a scalar subquery in the WHERE clause.  SQLite
    // re-parameterises by position, so we append guard params after the event
    // params and substitute the correct ordinal into the guard SQL template.
    let guard_offset = 17usize;
    let remapped_guard = remap_guard_params(guard_sql, guard_offset);

    let mut params = vec![
        SqlValue::Text(event.id.to_string()),
        SqlValue::Text(event.namespace.clone()),
        SqlValue::Text(event.verb.clone()),
        SqlValue::Text(substrate_str),
        SqlValue::Text(event.actor.clone()),
        SqlValue::Text(kind_str),
        SqlValue::Text(outcome_str),
        SqlValue::Text(payload_str),
        SqlValue::Integer(event.payload_schema_version as i64),
        match event.profile_state_version {
            Some(v) => SqlValue::Integer(v as i64),
            None => SqlValue::Null,
        },
        SqlValue::Integer(event.duration_us),
        match event.target_id {
            Some(u) => SqlValue::Text(u.to_string()),
            None => SqlValue::Null,
        },
        match event.session_id {
            Some(u) => SqlValue::Text(u.to_string()),
            None => SqlValue::Null,
        },
        match &event.aggregate_kind {
            Some(s) => SqlValue::Text(s.clone()),
            None => SqlValue::Null,
        },
        match event.aggregate_id {
            Some(u) => SqlValue::Text(u.to_string()),
            None => SqlValue::Null,
        },
        SqlValue::Integer(event.created_at),
    ];
    params.extend(guard_params);

    SqlStatement {
        sql: format!(
            "INSERT INTO events \
             (id, namespace, verb, substrate, actor, kind, outcome, payload, \
              payload_schema_version, profile_state_version, duration_us, target_id, \
              session_id, aggregate_kind, aggregate_id, created_at) \
             SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16 \
             WHERE ({remapped_guard})"
        ),
        params,
        label: Some("projection_worker.conditional_event_insert".into()),
    }
}

/// Remap `?1`, `?2`, ... in `guard_sql` to `?{offset}`, `?{offset+1}`, ... so
/// that guard parameters follow the 16 event-column parameters in the combined
/// parameter list for `execute_batch`.
///
/// Uses character-boundary-aware replacement: `?N` is only replaced when not
/// immediately followed by another digit, preventing `?1` → `?17` from
/// corrupting a pre-existing `?18` token (the naive `.replace("?1", "?17")`
/// would turn `?18` into `?178`).
fn remap_guard_params(guard_sql: &str, offset: usize) -> String {
    // Find the maximum parameter index in the guard.
    let max_idx: usize = (1..=32)
        .rev()
        .find(|&i| {
            let token = format!("?{i}");
            // Match `?N` as a whole token: must not be followed by a digit.
            let mut pos = 0;
            while let Some(found) = guard_sql[pos..].find(&token) {
                let abs = pos + found;
                let after = abs + token.len();
                if guard_sql[after..]
                    .chars()
                    .next()
                    .is_none_or(|c| !c.is_ascii_digit())
                {
                    return true;
                }
                pos = abs + 1;
            }
            false
        })
        .unwrap_or(0);

    if max_idx == 0 {
        return guard_sql.to_string();
    }

    // Replace from highest to lowest to avoid affecting lower-numbered tokens.
    // For each `?N`, scan the string and replace only whole-token occurrences
    // (i.e., `?N` not immediately followed by a digit).
    let mut result = guard_sql.to_string();
    for i in (1..=max_idx).rev() {
        let old = format!("?{i}");
        let new = format!("?{}", i + offset - 1);
        let mut out = String::with_capacity(result.len());
        let mut pos = 0;
        while let Some(found) = result[pos..].find(&old) {
            let abs = pos + found;
            let after = abs + old.len();
            // Only replace when not followed by a digit (whole-token match).
            if result[after..]
                .chars()
                .next()
                .is_none_or(|c| !c.is_ascii_digit())
            {
                out.push_str(&result[pos..abs]);
                out.push_str(&new);
            } else {
                out.push_str(&result[pos..after]);
            }
            pos = after;
        }
        out.push_str(&result[pos..]);
        result = out;
    }
    result
}

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
    ///
    /// Returns `Ok(true)` when the projection row was updated, `Ok(false)` when the
    /// CAS precondition was not met (proposal already in a terminal state or already
    /// approved).
    ///
    /// BUG-4 fix: each status-changing UPDATE includes a WHERE precondition on the
    /// current status so that concurrent calls atomically compete — only one will
    /// find `rows_affected == 1`.  The caller checks the return value and treats
    /// `false` as a race-lost error.
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

        // BUG-3 fix: store the bare variant name ("approve", "reject", …), NOT the
        // JSON-encoded form.  serde_json::to_string of a unit-enum variant produces
        // "\"approve\"" (a JSON string literal with outer quotes), which when stored in
        // a TEXT column and later re-serialised into the wire response yields the
        // double-encoded value "\"approve\"".  Using as_str() avoids the extra layer.
        let last_decision_str = payload.decision.as_str();

        let sql = self.runtime.sql();
        let mut writer = sql.writer().await.map_err(RuntimeError::Storage)?;

        let rows = if let Some(new_status) = new_status_opt {
            // CAS precondition: only act on proposals in an actionable state.
            //   Approve:          status must be 'open' or 'changes_requested'
            //   Reject:           status must not be terminal (any non-terminal)
            //   RequestChanges:   status must be 'open'
            // All of these collapse to: status NOT IN ('applied', 'withdrawn', 'rejected', 'approved')
            // which is the set of states that can still transition.
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
            // Comment: only bump review_count + last_decision, leave status as-is.
            // No CAS needed for comments — they don't change state.
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
    ///
    /// Sets status='applied' using a CAS precondition `WHERE status='applying'`.
    /// H1 fix: the apply worker atomically moves status='approved' → 'applying'
    /// (via `pre_apply_cas`) before touching the KG, so by the time this is
    /// called the status is always 'applying' on the happy path.  If the status
    /// is somehow not 'applying' (e.g. the apply was concurrently cancelled),
    /// the CAS returns false — the apply worker logs a warning.
    ///
    /// Returns `Ok(true)` when the row was updated, `Ok(false)` when the CAS
    /// missed (status was not 'applying').
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

    /// Pre-apply CAS (H1 fix): atomically moves status='approved' → 'applying'.
    ///
    /// Called by the apply worker BEFORE touching the KG.  Returns `Ok(true)`
    /// when the transition succeeded (this worker now exclusively owns the apply
    /// path).  Returns `Ok(false)` when the proposal was not in 'approved' state
    /// (concurrent withdraw won the race, or already applied by another worker).
    /// In both false cases the caller MUST abort without any KG mutation.
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
    ///
    /// Sets status='withdrawn' using a CAS (compare-and-swap) UPDATE: only rows
    /// whose current status is NOT already a terminal state are updated.  Returns
    /// `Ok(true)` when the row was updated, `Ok(false)` when the proposal was
    /// already in a terminal state (concurrent withdraw won the race).
    ///
    /// BUG-4 fix: the WHERE clause acts as a CAS precondition — two concurrent
    /// `withdraw` calls can both pass the application-layer status guard, but only
    /// one will find `rows_affected == 1` here.  The caller MUST check the return
    /// value and return an error for the losing racer rather than emitting a
    /// duplicate `ProposalWithdrawn` event.
    ///
    /// H1 fix: `'applying'` is excluded from the withdrawable set.  Once the
    /// apply worker claims status='applying' via `pre_apply_cas`, withdraw cannot
    /// land — the CAS here returns false, the handler returns an error to the
    /// caller ("in-flight apply — cannot withdraw"), and no `ProposalWithdrawn`
    /// event is written.
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

    /// Revert status from 'applying' back to 'approved' when the KG mutation failed.
    ///
    /// Called by the apply worker after a changeset execution failure so that the
    /// proposal is not permanently stuck in the transient 'applying' state.
    /// Best-effort — the apply worker logs a warning if this itself fails.
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

    /// H2 fix: atomically run the reviewed CAS UPDATE + ProposalReviewed event INSERT.
    ///
    /// Uses `execute_batch` which wraps both SQL statements in a single
    /// `BEGIN IMMEDIATE` / `COMMIT` transaction.  The event INSERT uses a conditional
    /// `INSERT … SELECT … WHERE (guard)` form: if the CAS UPDATE matched 0 rows
    /// (race lost), the INSERT is skipped.
    ///
    /// Codex round-4 guard: `WHERE changes() = 1`.
    /// `changes()` returns the row count from the immediately-preceding statement on
    /// the same connection.  Since `execute_batch` runs both statements on the same
    /// connection with no intervening operations, `changes()` at INSERT time is exactly
    /// the UPDATE's row count.  If the UPDATE hit 1 row (this connection won the CAS),
    /// `changes() = 1` is true and the INSERT runs.  If the UPDATE hit 0 rows (CAS
    /// lost), `changes() = 0` and the INSERT is skipped.
    ///
    /// This replaces the round-3 `updated_at = <now>` subquery guard, which was unsafe
    /// under same-microsecond concurrent calls: two callers can compute identical `now`
    /// values before either holds the writer lock, so the loser's guard could match the
    /// winner's committed `updated_at` and insert a duplicate event.  `changes()` is
    /// connection-local and requires no timestamp uniqueness assumption.
    ///
    /// Returns `Ok((cas_hit, event_id))`.
    /// - `cas_hit` = true when the projection row was updated AND the event was
    ///   inserted (i.e. total_rows == 2 for state-changing decisions).
    /// - `cas_hit` = false → no projection update, no event written.
    /// - For `Comment` decisions (no state change), `cas_hit` is always true.
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

        // For state-changing decisions, the event INSERT guard checks that the
        // projection was successfully updated to `new_status`.  For Comments, the
        // guard checks that the proposal row exists (always true after create).
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
            // Guard: INSERT only when THIS connection's UPDATE just hit 1 row.
            // `changes()` returns the row count from the immediately-preceding
            // statement on the same connection — no timestamp uniqueness needed.
            let guard = "changes() = 1";
            let gp: Vec<SqlValue> = vec![];
            (stmt, guard, gp)
        } else {
            // Comment: no state change; always update review_count, always insert event.
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
            // Guard: INSERT only when the comment UPDATE hit 1 row (proposal exists).
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

        // total_rows == 2 → projection updated + event inserted.
        // total_rows == 1 → only the comment projection updated (guard always true) or
        //                    only one of the two ran (shouldn't happen).
        // total_rows == 0 → CAS missed AND guard rejected insert (both skipped).
        let cas_hit = if decision_changes_state {
            total_rows == 2
        } else {
            true // Comment: no CAS required.
        };

        Ok((cas_hit, event_id))
    }

    /// H2 fix: atomically run the withdrawn CAS UPDATE + ProposalWithdrawn event INSERT.
    ///
    /// The event INSERT guard uses `changes() = 1` (not `updated_at = <now>`) so that
    /// a second concurrent withdraw that loses the CAS cannot insert a duplicate event.
    /// `changes()` is connection-local and requires no timestamp uniqueness assumption —
    /// see `reviewed_and_emit` doc comment for the full reasoning.
    ///
    /// Returns `Ok((cas_hit, event_id))`.
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

        // Guard: INSERT only when THIS connection's UPDATE just hit 1 row.
        // `changes()` returns the row count from the immediately-preceding statement
        // on the same connection — no timestamp uniqueness assumption needed.
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

        // total_rows == 2 → CAS hit + event inserted.
        // total_rows == 0 or 1 → CAS missed (event also skipped by guard).
        let cas_hit = total_rows == 2;
        Ok((cas_hit, event_id))
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

        // Simulate approve path: approved → applying (pre-apply CAS) → applied.
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

        // H1: pre_apply_cas must succeed when status='approved'.
        let claimed = worker
            .pre_apply_cas(&tok, pid)
            .await
            .expect("pre_apply_cas must succeed");
        assert!(
            claimed,
            "pre_apply_cas must return true when status='approved'"
        );

        // H1: on_proposal_applied now uses WHERE status='applying'.
        let applied = worker
            .on_proposal_applied(&tok, pid)
            .await
            .expect("on_proposal_applied must succeed");
        assert!(applied, "CAS must succeed when status='applying'");

        let row = worker
            .get_proposal_row(&tok, pid)
            .await
            .expect("get row")
            .expect("row must exist");

        assert_eq!(row.status, "applied");
    }

    // H1 regression: pre_apply_cas must fail when proposal was already withdrawn.
    #[tokio::test]
    async fn pre_apply_cas_fails_when_already_withdrawn() {
        let (rt, tok) = setup();
        ensure_schema(&rt).await;
        let worker = ProposalsProjectionWorker::new(rt.clone());
        let pid = Uuid::new_v4();

        worker
            .on_proposal_created(&tok, pid, "alice", "Race Test", None)
            .await
            .expect("create");

        let approve_payload = ProposalReviewedPayload {
            proposal_id: Id128::from_u128(pid.as_u128()),
            reviewer: "bob".to_string(),
            decision: ProposalDecision::Approve,
            comment: None,
        };
        worker
            .on_proposal_reviewed(&tok, &approve_payload)
            .await
            .expect("approve");

        worker
            .on_proposal_withdrawn(&tok, pid)
            .await
            .expect("withdraw");

        // pre_apply_cas must fail: status is 'withdrawn', not 'approved'.
        let claimed = worker
            .pre_apply_cas(&tok, pid)
            .await
            .expect("pre_apply_cas must not error");
        assert!(
            !claimed,
            "H1: pre_apply_cas must return false when status='withdrawn'"
        );

        let row = worker
            .get_proposal_row(&tok, pid)
            .await
            .expect("get row")
            .expect("row must exist");
        assert_eq!(
            row.status, "withdrawn",
            "status must remain 'withdrawn' after failed pre_apply_cas"
        );
    }

    // H1 regression: on_proposal_withdrawn must fail when status='applying'.
    #[tokio::test]
    async fn on_proposal_withdrawn_fails_when_status_applying() {
        let (rt, tok) = setup();
        ensure_schema(&rt).await;
        let worker = ProposalsProjectionWorker::new(rt.clone());
        let pid = Uuid::new_v4();

        worker
            .on_proposal_created(&tok, pid, "alice", "Applying Guard", None)
            .await
            .expect("create");

        let approve_payload = ProposalReviewedPayload {
            proposal_id: Id128::from_u128(pid.as_u128()),
            reviewer: "bob".to_string(),
            decision: ProposalDecision::Approve,
            comment: None,
        };
        worker
            .on_proposal_reviewed(&tok, &approve_payload)
            .await
            .expect("approve");

        // Simulate apply worker claiming 'applying'.
        let claimed = worker
            .pre_apply_cas(&tok, pid)
            .await
            .expect("pre_apply_cas");
        assert!(claimed, "pre_apply_cas must succeed");

        // Now withdraw must be blocked (status='applying').
        let withdrew = worker
            .on_proposal_withdrawn(&tok, pid)
            .await
            .expect("on_proposal_withdrawn must not error");
        assert!(
            !withdrew,
            "H1: on_proposal_withdrawn must return false when status='applying'"
        );

        // Status must still be 'applying'.
        let row = worker
            .get_proposal_row(&tok, pid)
            .await
            .expect("get row")
            .expect("row must exist");
        assert_eq!(
            row.status, "applying",
            "status must remain 'applying' after blocked withdraw"
        );
    }

    // BUG-3 regression: on_proposal_reviewed must store the bare variant name in
    // last_decision, not the JSON-quoted form "\"approve\"".
    #[tokio::test]
    async fn on_proposal_reviewed_last_decision_is_bare_string() {
        let (rt, tok) = setup();
        ensure_schema(&rt).await;
        let worker = ProposalsProjectionWorker::new(rt.clone());
        let pid = Uuid::new_v4();

        worker
            .on_proposal_created(&tok, pid, "alice", "Encoding Test", None)
            .await
            .expect("create");

        for (decision, expected_str) in [
            (ProposalDecision::Approve, "approve"),
            (ProposalDecision::Reject, "reject"),
            (ProposalDecision::Comment, "comment"),
            (ProposalDecision::RequestChanges, "request_changes"),
        ] {
            // Reset for each variant.
            let pid2 = Uuid::new_v4();
            worker
                .on_proposal_created(&tok, pid2, "alice", "Encoding Test", None)
                .await
                .expect("create");

            let payload = ProposalReviewedPayload {
                proposal_id: Id128::from_u128(pid2.as_u128()),
                reviewer: "bob".to_string(),
                decision,
                comment: None,
            };
            worker
                .on_proposal_reviewed(&tok, &payload)
                .await
                .expect("on_proposal_reviewed must succeed");

            // Read the raw last_decision column.
            let sql = rt.sql();
            let mut reader = sql.reader().await.expect("reader");
            let row = reader
                .query_row(SqlStatement {
                    sql: "SELECT last_decision FROM proposals_open WHERE proposal_id = ?1"
                        .to_string(),
                    params: vec![SqlValue::Text(pid2.to_string())],
                    label: Some("test.last_decision_encoding".into()),
                })
                .await
                .expect("query_row")
                .expect("row must exist");

            let stored = row
                .get("last_decision")
                .and_then(|v| {
                    if let SqlValue::Text(s) = v {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
                .unwrap_or_default();

            assert_eq!(
                stored, expected_str,
                "BUG-3: last_decision for {decision:?} must be bare {expected_str:?}, not JSON-quoted; got: {stored:?}"
            );
            assert!(
                !stored.starts_with('"'),
                "BUG-3: last_decision must NOT be JSON-quoted; got: {stored:?}"
            );
        }
    }

    // BUG-4 regression: second on_proposal_withdrawn on an already-withdrawn
    // proposal returns false (CAS missed).
    #[tokio::test]
    async fn on_proposal_withdrawn_cas_returns_false_on_second_call() {
        let (rt, tok) = setup();
        ensure_schema(&rt).await;
        let worker = ProposalsProjectionWorker::new(rt.clone());
        let pid = Uuid::new_v4();

        worker
            .on_proposal_created(&tok, pid, "alice", "Withdraw Race", None)
            .await
            .expect("create");

        let first = worker
            .on_proposal_withdrawn(&tok, pid)
            .await
            .expect("first withdraw must not error");
        assert!(first, "BUG-4: first on_proposal_withdrawn must return true");

        let second = worker
            .on_proposal_withdrawn(&tok, pid)
            .await
            .expect("second withdraw must not error");
        assert!(
            !second,
            "BUG-4: second on_proposal_withdrawn must return false (CAS missed)"
        );
    }

    // H1 / Codex-R3 regression: two sequential `withdrawn_and_emit` calls on the same
    // open proposal must produce exactly ONE ProposalWithdrawn event in the events
    // table, and the second call must return cas_hit=false.
    //
    // This test catches the guard bug where checking `WHERE status='withdrawn'` (final
    // state) rather than `WHERE updated_at=<this_tx_now>` (proof of write) would allow
    // a second concurrent/sequential withdraw to insert a duplicate event.
    //
    // SQLite `BEGIN IMMEDIATE` serialises writers, so we simulate the race by calling
    // withdrawn_and_emit twice in sequence — the second call sees the already-committed
    // status='withdrawn' row, which is exactly the condition that triggered the bug.
    #[tokio::test]
    async fn withdrawn_and_emit_second_call_no_duplicate_event() {
        use khive_storage::event::Event;
        use khive_types::{EventKind, SubstrateKind};

        let (rt, tok) = setup();
        ensure_schema(&rt).await;
        let worker = ProposalsProjectionWorker::new(rt.clone());
        let pid = Uuid::new_v4();

        worker
            .on_proposal_created(&tok, pid, "alice", "Duplicate Guard Test", None)
            .await
            .expect("create");

        let make_event = || {
            Event::new(
                tok.namespace().as_str(),
                "withdraw",
                EventKind::ProposalWithdrawn,
                SubstrateKind::Note,
                "alice",
            )
        };

        // First withdraw — must succeed.
        let (cas1, _eid1) = worker
            .withdrawn_and_emit(&tok, pid, make_event())
            .await
            .expect("first withdrawn_and_emit must not error");
        assert!(cas1, "first withdrawn_and_emit must return cas_hit=true");

        // Second withdraw — CAS misses (status already 'withdrawn'), event must NOT be inserted.
        let (cas2, _eid2) = worker
            .withdrawn_and_emit(&tok, pid, make_event())
            .await
            .expect("second withdrawn_and_emit must not error");
        assert!(
            !cas2,
            "H1-R3: second withdrawn_and_emit must return cas_hit=false"
        );

        // Critical: exactly ONE ProposalWithdrawn event in the events table.
        let sql = rt.sql();
        let mut reader = sql.reader().await.expect("reader");
        let rows = reader
            .query_all(SqlStatement {
                sql: "SELECT id FROM events WHERE kind='proposal_withdrawn' AND aggregate_id IS NULL AND target_id IS NULL".to_string(),
                params: vec![],
                label: Some("test.count_withdrawn_events".into()),
            })
            .await
            .expect("query_all");

        // Count all ProposalWithdrawn events — there must be exactly one.
        let withdrawn_count = {
            let sql2 = rt.sql();
            let mut reader2 = sql2.reader().await.expect("reader2");
            reader2
                .query_row(SqlStatement {
                    sql: "SELECT COUNT(*) as cnt FROM events WHERE kind='proposal_withdrawn'"
                        .to_string(),
                    params: vec![],
                    label: Some("test.withdrawn_event_count".into()),
                })
                .await
                .expect("count query")
        };

        let count = withdrawn_count
            .and_then(|row| {
                row.get("cnt").and_then(|v| {
                    if let SqlValue::Integer(n) = v {
                        Some(*n)
                    } else {
                        None
                    }
                })
            })
            .unwrap_or(0);

        assert_eq!(
            count, 1,
            "H1-R3: exactly ONE ProposalWithdrawn event must exist; got {count}. \
             Duplicate events indicate the guard checked final status instead of \
             whether this UPDATE actually ran."
        );
        drop(rows); // silence unused-variable lint
    }

    // Codex round-4 regression: same-microsecond `updated_at` collision.
    //
    // Round-3's guard was `WHERE updated_at = ?` — if two callers sample the same
    // microsecond before either holds the writer lock, the loser's guard sees the
    // winner's committed `updated_at` and inserts a duplicate event.
    //
    // This test proves the new `changes() = 1` guard is immune to that collision by
    // directly executing the SQL with an explicitly shared timestamp: first a real CAS
    // UPDATE + guarded INSERT batch, then a second "loser" batch that supplies the
    // same `now` but whose UPDATE hits 0 rows (status already 'withdrawn').
    //
    // With the round-3 guard the second INSERT would match `updated_at = shared_now`
    // (the winner wrote that value) and produce a duplicate event.  With `changes() = 1`
    // the second INSERT sees `changes() = 0` and is skipped — event count stays at 1.
    #[tokio::test]
    async fn same_microsecond_timestamp_no_duplicate_event_changes_guard() {
        let (rt, tok) = setup();
        ensure_schema(&rt).await;
        let ns = tok.namespace().as_str().to_owned();
        let pid = Uuid::new_v4();
        let pid_str = pid.to_string();

        // Insert an open proposal row directly (bypassing the worker helper so we
        // control the timestamps precisely).
        let shared_now: i64 = 1_700_000_000_000_000; // fixed microsecond — both callers share it
        {
            let sql = rt.sql();
            let mut writer = sql.writer().await.expect("writer");
            writer
                .execute(SqlStatement {
                    sql: "INSERT INTO proposals_open \
                            (proposal_id, namespace, proposer, title, status, \
                             created_at, updated_at) \
                          VALUES (?1, ?2, 'alice', 'Timestamp Race', 'open', ?3, ?3)"
                        .to_string(),
                    params: vec![
                        SqlValue::Text(pid_str.clone()),
                        SqlValue::Text(ns.clone()),
                        SqlValue::Integer(shared_now - 1), // created slightly before
                    ],
                    label: Some("test.insert_open".into()),
                })
                .await
                .expect("insert proposal");
        }

        // Caller A: UPDATE (status open → withdrawn, updated_at = shared_now) +
        //           INSERT guarded by `changes() = 1`.  Should affect 2 rows total.
        {
            let sql = rt.sql();
            let mut writer = sql.writer().await.expect("writer");
            let total = writer
                .execute_batch(vec![
                    SqlStatement {
                        sql: "UPDATE proposals_open \
                              SET status = 'withdrawn', updated_at = ?1 \
                              WHERE proposal_id = ?2 AND namespace = ?3 \
                                AND status NOT IN ('applied', 'applying', 'withdrawn', 'rejected')"
                            .to_string(),
                        params: vec![
                            SqlValue::Integer(shared_now),
                            SqlValue::Text(pid_str.clone()),
                            SqlValue::Text(ns.clone()),
                        ],
                        label: Some("test.caller_a.update".into()),
                    },
                    SqlStatement {
                        sql: "INSERT INTO events \
                               (id, namespace, verb, substrate, actor, kind, outcome, payload, \
                                payload_schema_version, duration_us, created_at) \
                               SELECT ?1, ?2, 'withdraw', 'note', 'alice', \
                                      'proposal_withdrawn', 'ok', '{}', 1, 0, ?3 \
                               WHERE (changes() = 1)"
                            .to_string(),
                        params: vec![
                            SqlValue::Text(Uuid::new_v4().to_string()),
                            SqlValue::Text(ns.clone()),
                            SqlValue::Integer(shared_now),
                        ],
                        label: Some("test.caller_a.insert_event".into()),
                    },
                ])
                .await
                .expect("caller_a execute_batch");
            assert_eq!(
                total, 2,
                "caller A must write 1 UPDATE row + 1 event INSERT row; got {total}"
            );
        }

        // Caller B: same timestamp, but UPDATE hits 0 rows (already 'withdrawn').
        // With the round-3 guard (`updated_at = shared_now`) this INSERT WOULD fire
        // because the committed row has `updated_at = shared_now`.
        // With `changes() = 1` it MUST NOT fire.
        {
            let sql = rt.sql();
            let mut writer = sql.writer().await.expect("writer");
            let total = writer
                .execute_batch(vec![
                    SqlStatement {
                        sql: "UPDATE proposals_open \
                              SET status = 'withdrawn', updated_at = ?1 \
                              WHERE proposal_id = ?2 AND namespace = ?3 \
                                AND status NOT IN ('applied', 'applying', 'withdrawn', 'rejected')"
                            .to_string(),
                        params: vec![
                            SqlValue::Integer(shared_now), // same timestamp as caller A
                            SqlValue::Text(pid_str.clone()),
                            SqlValue::Text(ns.clone()),
                        ],
                        label: Some("test.caller_b.update".into()),
                    },
                    SqlStatement {
                        sql: "INSERT INTO events \
                               (id, namespace, verb, substrate, actor, kind, outcome, payload, \
                                payload_schema_version, duration_us, created_at) \
                               SELECT ?1, ?2, 'withdraw', 'note', 'alice', \
                                      'proposal_withdrawn', 'ok', '{}', 1, 0, ?3 \
                               WHERE (changes() = 1)"
                            .to_string(),
                        params: vec![
                            SqlValue::Text(Uuid::new_v4().to_string()),
                            SqlValue::Text(ns.clone()),
                            SqlValue::Integer(shared_now),
                        ],
                        label: Some("test.caller_b.insert_event".into()),
                    },
                ])
                .await
                .expect("caller_b execute_batch");
            assert_eq!(
                total, 0,
                "caller B's UPDATE hits 0 rows; changes() = 0 so INSERT must be skipped; \
                 got {total} (round-3 guard would have returned 1 here — duplicate event)"
            );
        }

        // Verify: exactly ONE ProposalWithdrawn event.
        let sql = rt.sql();
        let mut reader = sql.reader().await.expect("reader");
        let count_row = reader
            .query_row(SqlStatement {
                sql: "SELECT COUNT(*) as cnt FROM events WHERE kind='proposal_withdrawn'"
                    .to_string(),
                params: vec![],
                label: Some("test.same_micros.event_count".into()),
            })
            .await
            .expect("count query");
        let count = count_row
            .and_then(|row| {
                row.get("cnt").and_then(|v| {
                    if let SqlValue::Integer(n) = v {
                        Some(*n)
                    } else {
                        None
                    }
                })
            })
            .unwrap_or(0);
        assert_eq!(
            count, 1,
            "codex-R4: exactly ONE ProposalWithdrawn event must exist even with identical \
             `updated_at` timestamps; got {count}. \
             A value of 2 means the guard is checking timestamp equality (round-3 bug), \
             not connection-local changes()."
        );
    }

    // H1 regression: on_proposal_applied CAS must fail when status='withdrawn'.
    // on_proposal_applied requires status='applying'; 'withdrawn' ≠ 'applying' → CAS miss.
    #[tokio::test]
    async fn on_proposal_applied_cas_fails_when_already_withdrawn() {
        let (rt, tok) = setup();
        ensure_schema(&rt).await;
        let worker = ProposalsProjectionWorker::new(rt.clone());
        let pid = Uuid::new_v4();

        worker
            .on_proposal_created(&tok, pid, "alice", "Race Test", None)
            .await
            .expect("create");

        // Simulate approve then immediate withdraw.
        let approve_payload = ProposalReviewedPayload {
            proposal_id: Id128::from_u128(pid.as_u128()),
            reviewer: "bob".to_string(),
            decision: ProposalDecision::Approve,
            comment: None,
        };
        worker
            .on_proposal_reviewed(&tok, &approve_payload)
            .await
            .expect("approve");

        worker
            .on_proposal_withdrawn(&tok, pid)
            .await
            .expect("withdraw");

        // on_proposal_applied requires status='applying'; 'withdrawn' fails the CAS.
        let applied = worker
            .on_proposal_applied(&tok, pid)
            .await
            .expect("on_proposal_applied must not error");
        assert!(
            !applied,
            "H1: on_proposal_applied CAS must return false when status='withdrawn' (not 'applying')"
        );

        // Verify the status did not flip back to 'applied'.
        let row = worker
            .get_proposal_row(&tok, pid)
            .await
            .expect("get row")
            .expect("row must exist");
        assert_eq!(
            row.status, "withdrawn",
            "status must remain 'withdrawn' after failed apply CAS"
        );
    }
}
