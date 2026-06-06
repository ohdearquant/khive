//! ProposalsProjectionWorker -- maintains the `proposals_open` projection table.
//!
//! Handles the four proposal event kinds (`Created`, `Reviewed`, `Applied`, `Withdrawn`)
//! and is the sole writer to the projection table. Handlers MUST NOT update it directly.

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
    /// Create a new projection worker backed by the given runtime.
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

    /// Atomically run the reviewed CAS UPDATE and `ProposalReviewed` event INSERT.
    ///
    /// Both statements execute in a single `BEGIN IMMEDIATE` / `COMMIT` transaction
    /// via `execute_batch`. The CAS guard uses `changes() = 1` — see
    /// `docs/design.md §"Proposal Projection CAS"` for the full atomicity proof.
    ///
    /// Returns `Ok((cas_hit, event_id))`:
    /// - `cas_hit = true`: projection row was updated and event was inserted.
    /// - `cas_hit = false`: no projection update, no event written (CAS lost).
    /// - For `Comment` decisions (no state change), `cas_hit` is always `true`.
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
    /// UUID of the proposal as a string.
    pub proposal_id: String,
    /// Identity of the agent that submitted the proposal.
    pub proposer: String,
    /// Current lifecycle status: `open`, `approved`, `rejected`, `applying`, `applied`, or `withdrawn`.
    pub status: String,
    /// Number of approve decisions recorded.
    pub approve_count: i64,
    /// Number of reject decisions recorded.
    pub reject_count: i64,
}

#[cfg(test)]
mod tests;
