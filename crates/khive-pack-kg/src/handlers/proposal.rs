//! `propose`, `review`, `withdraw`, and `list(kind=proposal)` verb handlers.

use std::str::FromStr;

use serde_json::Value;
use uuid::Uuid;

use khive_runtime::{NamespaceToken, RuntimeError, VerbRegistry};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::SubstrateKind;
use khive_types::{
    EventKind, ProposalChangeset, ProposalCreatedPayload, ProposalDecision,
    ProposalReviewedPayload, ProposalWithdrawnPayload,
};

use super::common::{
    deser, to_json, ListProposalsParams, ProposeParams, ReviewParams, WithdrawParams,
};
use crate::KgPack;

use khive_runtime::micros_to_iso;

impl KgPack {
    pub(crate) async fn resolve_proposal_uuid(
        &self,
        token: &NamespaceToken,
        raw: &str,
    ) -> Result<Uuid, RuntimeError> {
        if let Ok(uuid) = Uuid::from_str(raw) {
            return Ok(uuid);
        }
        if raw.len() >= 8 && raw.chars().all(|c| c.is_ascii_hexdigit()) {
            let ns = token.namespace().as_str().to_owned();
            let pattern = format!("{}%", raw);
            let sql = self.runtime.sql();
            let mut reader = sql.reader().await.map_err(RuntimeError::Storage)?;
            let rows = reader
                .query_all(SqlStatement {
                    sql: "SELECT proposal_id FROM proposals_open \
                          WHERE proposal_id LIKE ?1 AND namespace = ?2 LIMIT 2"
                        .to_string(),
                    params: vec![SqlValue::Text(pattern), SqlValue::Text(ns)],
                    label: Some("proposals_open.resolve_prefix".into()),
                })
                .await
                .map_err(RuntimeError::Storage)?;

            let ids: Vec<String> = rows
                .into_iter()
                .filter_map(|row| {
                    row.get("proposal_id").and_then(|v| {
                        if let SqlValue::Text(s) = v {
                            Some(s.clone())
                        } else {
                            None
                        }
                    })
                })
                .collect();

            return match ids.len() {
                0 => Err(RuntimeError::NotFound(format!(
                    "no proposal matches prefix: {raw:?}"
                ))),
                1 => Uuid::from_str(&ids[0]).map_err(|e| {
                    RuntimeError::Internal(format!("stored proposal_id is invalid: {e}"))
                }),
                _ => Err(RuntimeError::InvalidInput(format!(
                    "ambiguous proposal prefix {raw:?}: matches multiple proposals; use full UUID"
                ))),
            };
        }
        Err(RuntimeError::InvalidInput(format!(
            "invalid proposal_id {raw:?}: must be a full UUID or 8-char hex prefix"
        )))
    }

    pub(crate) async fn handle_propose(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: ProposeParams = deser(params)?;
        if p.title.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "propose requires a non-empty 'title'".into(),
            ));
        }
        if p.description.is_empty() {
            return Err(RuntimeError::InvalidInput(
                "propose requires a non-empty 'description'".into(),
            ));
        }

        let _changeset: ProposalChangeset = serde_json::from_value(p.changeset.clone())
            .map_err(|e| RuntimeError::InvalidInput(format!("invalid changeset: {e}")))?;

        let proposal_id = Uuid::new_v4();
        let actor = token.actor().id.clone();
        let ns = token.namespace().as_str().to_owned();

        let validated_parent_id: Option<khive_types::Id128> = p
            .parent_id
            .as_deref()
            .map(|s| -> Result<khive_types::Id128, RuntimeError> {
                let parent_uuid = Uuid::from_str(s).map_err(|e| {
                    RuntimeError::InvalidInput(format!("invalid parent_id {s:?}: {e}"))
                })?;
                Ok(khive_types::Id128::from_u128(parent_uuid.as_u128()))
            })
            .transpose()?;

        if let Some(ref parent_id128) = validated_parent_id {
            let parent_uuid = Uuid::from_u128(parent_id128.to_u128());
            let sql = self.runtime.sql();
            let mut reader = sql.reader().await.map_err(RuntimeError::Storage)?;
            let parent_row = reader
                .query_row(SqlStatement {
                    sql: "SELECT status FROM proposals_open \
                          WHERE proposal_id = ?1 AND namespace = ?2"
                        .to_string(),
                    params: vec![
                        SqlValue::Text(parent_uuid.to_string()),
                        SqlValue::Text(ns.clone()),
                    ],
                    label: Some("proposals_open.validate_parent_id".into()),
                })
                .await
                .map_err(RuntimeError::Storage)?;
            if parent_row.is_none() {
                return Err(RuntimeError::InvalidInput(format!(
                    "parent_id {:?} not found; it must reference an existing proposal",
                    parent_uuid.to_string()
                )));
            }
        }

        let payload = ProposalCreatedPayload {
            proposal_id: khive_types::Id128::from_u128(proposal_id.as_u128()),
            proposer: actor.clone(),
            title: p.title.clone(),
            description: p.description.clone(),
            changeset: _changeset,
            reviewers: p.reviewers.clone(),
            expiry: p
                .expiry
                .map(|v| khive_types::Timestamp::from_micros(v as u64)),
            parent_id: validated_parent_id,
        };

        let event_payload_json = serde_json::to_value(&payload)
            .map_err(|e| RuntimeError::Internal(format!("serialize proposal payload: {e}")))?;

        let mut event = khive_storage::event::Event::new(
            &ns,
            "propose",
            EventKind::ProposalCreated,
            SubstrateKind::Entity,
            &actor,
        );
        event.payload = event_payload_json;
        event.aggregate_kind = Some("proposal".to_string());
        event.aggregate_id = Some(proposal_id);

        let event_store = self.runtime.events(token)?;
        event_store
            .append_event(event)
            .await
            .map_err(RuntimeError::Storage)?;

        crate::projection_worker::ProposalsProjectionWorker::new(self.runtime.clone())
            .on_proposal_created(token, proposal_id, &actor, &p.title, p.expiry)
            .await?;

        to_json(&serde_json::json!({
            "proposal_id": proposal_id.to_string(),
            "status": "open",
            "proposer": actor,
            "title": p.title,
        }))
    }

    pub(crate) async fn handle_review(
        &self,
        token: &NamespaceToken,
        params: Value,
        registry: &VerbRegistry,
    ) -> Result<Value, RuntimeError> {
        let p: ReviewParams = deser(params)?;
        let proposal_id = self.resolve_proposal_uuid(token, &p.proposal_id).await?;
        let actor = token.actor().id.clone();
        let ns = token.namespace().as_str().to_owned();

        let decision: ProposalDecision = match p.decision.trim().to_ascii_lowercase().as_str() {
            "approve" => ProposalDecision::Approve,
            "reject" => ProposalDecision::Reject,
            "comment" => ProposalDecision::Comment,
            "request_changes" | "requestchanges" => ProposalDecision::RequestChanges,
            other => {
                return Err(RuntimeError::InvalidInput(format!(
                    "unknown decision {other:?}; valid: approve | reject | comment | request_changes"
                )));
            }
        };

        let sql = self.runtime.sql();
        let mut reader = sql.reader().await.map_err(RuntimeError::Storage)?;

        let row = reader
            .query_row(SqlStatement {
                sql: "SELECT proposer, status FROM proposals_open \
                      WHERE proposal_id = ?1 AND namespace = ?2"
                    .to_string(),
                params: vec![
                    SqlValue::Text(proposal_id.to_string()),
                    SqlValue::Text(ns.clone()),
                ],
                label: Some("proposals_open.get".into()),
            })
            .await
            .map_err(RuntimeError::Storage)?
            .ok_or_else(|| RuntimeError::NotFound(format!("proposal {}", p.proposal_id)))?;

        let proposer = row
            .get("proposer")
            .and_then(|v| {
                if let SqlValue::Text(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        let current_status = row
            .get("status")
            .and_then(|v| {
                if let SqlValue::Text(s) = v {
                    Some(s.as_str())
                } else {
                    None
                }
            })
            .unwrap_or("open");

        if matches!(
            current_status,
            "applied" | "withdrawn" | "rejected" | "approved"
        ) {
            return Err(RuntimeError::InvalidInput(format!(
                "proposal {} is already {current_status} and cannot be reviewed",
                p.proposal_id
            )));
        }

        if decision == ProposalDecision::Approve && actor == proposer && actor != "local" {
            return Err(RuntimeError::InvalidInput(format!(
                "self-approval is forbidden: proposer {actor:?} cannot approve their own proposal"
            )));
        }

        let payload = ProposalReviewedPayload {
            proposal_id: khive_types::Id128::from_u128(proposal_id.as_u128()),
            reviewer: actor.clone(),
            decision,
            comment: p.comment.clone(),
        };
        let event_payload_json = serde_json::to_value(&payload)
            .map_err(|e| RuntimeError::Internal(format!("serialize review payload: {e}")))?;

        let mut event = khive_storage::event::Event::new(
            &ns,
            "review",
            EventKind::ProposalReviewed,
            SubstrateKind::Entity,
            &actor,
        );
        event.payload = event_payload_json;
        event.aggregate_kind = Some("proposal".to_string());
        event.aggregate_id = Some(proposal_id);

        let new_status = match decision {
            ProposalDecision::Approve => "approved",
            ProposalDecision::Reject => "rejected",
            ProposalDecision::Comment => current_status,
            ProposalDecision::RequestChanges => "changes_requested",
        };

        let decision_changes_state = decision != ProposalDecision::Comment;
        let (projection_updated, _event_id) =
            crate::projection_worker::ProposalsProjectionWorker::new(self.runtime.clone())
                .reviewed_and_emit(token, &payload, event, decision_changes_state)
                .await?;

        if !projection_updated && decision_changes_state {
            return Err(RuntimeError::InvalidInput(format!(
                "proposal {} status changed concurrently; review was not recorded — \
                 the proposal may have been withdrawn or approved by another reviewer \
                 simultaneously",
                p.proposal_id
            )));
        }

        if decision == ProposalDecision::Approve {
            crate::apply_worker::ProposalApplyWorker::new(self.runtime.clone())
                .maybe_apply(token, proposal_id, registry, p.max_new_entries)
                .await?;
        }

        to_json(&serde_json::json!({
            "proposal_id": proposal_id.to_string(),
            "reviewer": actor,
            "decision": p.decision,
            "status": new_status,
        }))
    }

    pub(crate) async fn handle_withdraw(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: WithdrawParams = deser(params)?;
        let proposal_id = self.resolve_proposal_uuid(token, &p.proposal_id).await?;
        let actor = token.actor().id.clone();
        let ns = token.namespace().as_str().to_owned();

        let sql = self.runtime.sql();
        let mut reader = sql.reader().await.map_err(RuntimeError::Storage)?;

        let row = reader
            .query_row(SqlStatement {
                sql: "SELECT proposer, status FROM proposals_open \
                      WHERE proposal_id = ?1 AND namespace = ?2"
                    .to_string(),
                params: vec![
                    SqlValue::Text(proposal_id.to_string()),
                    SqlValue::Text(ns.clone()),
                ],
                label: Some("proposals_open.get_for_withdraw".into()),
            })
            .await
            .map_err(RuntimeError::Storage)?
            .ok_or_else(|| RuntimeError::NotFound(format!("proposal {}", p.proposal_id)))?;

        let proposer = row
            .get("proposer")
            .and_then(|v| {
                if let SqlValue::Text(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        if actor != proposer {
            return Err(RuntimeError::InvalidInput(format!(
                "only the original proposer {proposer:?} may withdraw this proposal"
            )));
        }

        let current_status = row
            .get("status")
            .and_then(|v| {
                if let SqlValue::Text(s) = v {
                    Some(s.as_str())
                } else {
                    None
                }
            })
            .unwrap_or("open");

        if matches!(current_status, "applied" | "withdrawn" | "applying") {
            return Err(RuntimeError::InvalidInput(format!(
                "proposal {} is already {current_status} and cannot be withdrawn",
                p.proposal_id
            )));
        }

        let payload = ProposalWithdrawnPayload {
            proposal_id: khive_types::Id128::from_u128(proposal_id.as_u128()),
            by: actor.clone(),
            reason: p.rationale.clone(),
        };
        let event_payload_json = serde_json::to_value(&payload)
            .map_err(|e| RuntimeError::Internal(format!("serialize withdraw payload: {e}")))?;

        let mut event = khive_storage::event::Event::new(
            &ns,
            "withdraw",
            EventKind::ProposalWithdrawn,
            SubstrateKind::Entity,
            &actor,
        );
        event.payload = event_payload_json;
        event.aggregate_kind = Some("proposal".to_string());
        event.aggregate_id = Some(proposal_id);

        let (updated, _event_id) =
            crate::projection_worker::ProposalsProjectionWorker::new(self.runtime.clone())
                .withdrawn_and_emit(token, proposal_id, event)
                .await?;

        if !updated {
            return Err(RuntimeError::InvalidInput(format!(
                "proposal {} is already in a terminal or in-flight state and cannot be withdrawn",
                p.proposal_id
            )));
        }

        to_json(&serde_json::json!({
            "proposal_id": proposal_id.to_string(),
            "status": "withdrawn",
            "by": actor,
        }))
    }

    pub(crate) async fn handle_list_proposals(
        &self,
        token: &NamespaceToken,
        mut params: Value,
    ) -> Result<Value, RuntimeError> {
        if let Some(obj) = params.as_object_mut() {
            obj.remove("kind");
        }
        let p: ListProposalsParams = serde_json::from_value(params)
            .map_err(|e| RuntimeError::InvalidInput(format!("bad params: {e}")))?;
        let ns = token.namespace().as_str().to_owned();
        let limit = p.limit.unwrap_or(50).min(500) as i64;
        let offset = p.offset.unwrap_or(0) as i64;

        let mut sql_str = "\
            SELECT proposal_id, proposer, title, status, created_at, updated_at, \
                   expiry, last_decision, review_count, approve_count, reject_count \
            FROM proposals_open \
            WHERE namespace = ?1"
            .to_string();
        let mut sql_params: Vec<SqlValue> = vec![SqlValue::Text(ns)];
        let mut param_idx = 2usize;

        if let Some(status) = &p.status {
            sql_str.push_str(&format!(" AND status = ?{param_idx}"));
            sql_params.push(SqlValue::Text(status.clone()));
            param_idx += 1;
        }

        if let Some(proposer) = &p.proposer {
            sql_str.push_str(&format!(" AND proposer = ?{param_idx}"));
            sql_params.push(SqlValue::Text(proposer.clone()));
            param_idx += 1;
        } else {
            let actor_filter = p
                .actor
                .as_deref()
                .unwrap_or_else(|| token.actor().id.as_str());
            if actor_filter != "*" {
                sql_str.push_str(&format!(" AND proposer = ?{param_idx}"));
                sql_params.push(SqlValue::Text(actor_filter.to_owned()));
                param_idx += 1;
            }
        }

        sql_str.push_str(&format!(
            " ORDER BY updated_at DESC LIMIT ?{param_idx} OFFSET ?{}",
            param_idx + 1
        ));
        sql_params.push(SqlValue::Integer(limit));
        sql_params.push(SqlValue::Integer(offset));

        let sql = self.runtime.sql();
        let mut reader = sql.reader().await.map_err(RuntimeError::Storage)?;
        let rows = reader
            .query_all(SqlStatement {
                sql: sql_str,
                params: sql_params,
                label: Some("proposals_open.list".into()),
            })
            .await
            .map_err(RuntimeError::Storage)?;

        let items: Vec<Value> = rows
            .into_iter()
            .map(|row| {
                let get_text = |name: &str| -> String {
                    row.get(name)
                        .and_then(|v| {
                            if let SqlValue::Text(s) = v {
                                Some(s.clone())
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default()
                };
                let get_int = |name: &str| -> Option<i64> {
                    row.get(name).and_then(|v| {
                        if let SqlValue::Integer(i) = v {
                            Some(*i)
                        } else {
                            None
                        }
                    })
                };
                let ts_or_null = |name: &str| -> Value {
                    match get_int(name) {
                        Some(micros) => Value::String(micros_to_iso(micros)),
                        None => Value::Null,
                    }
                };
                serde_json::json!({
                    "proposal_id": get_text("proposal_id"),
                    "proposer": get_text("proposer"),
                    "title": get_text("title"),
                    "status": get_text("status"),
                    "created_at": ts_or_null("created_at"),
                    "updated_at": ts_or_null("updated_at"),
                    "expiry": ts_or_null("expiry"),
                    "last_decision": get_text("last_decision"),
                    "review_count": get_int("review_count").unwrap_or(0),
                    "approve_count": get_int("approve_count").unwrap_or(0),
                    "reject_count": get_int("reject_count").unwrap_or(0),
                })
            })
            .collect();

        to_json(&items)
    }
}
