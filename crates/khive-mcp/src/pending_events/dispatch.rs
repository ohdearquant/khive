use super::*;

/// Resolve the actor bound to a scheduled-event note by the schedule pack's
/// immutable provenance event.
///
/// The note's `properties.created_by_actor` field is intentionally ignored:
/// generic note create can forge it, while schedule-managed rows reject generic
/// update/merge. The `events` substrate is append-only and has no public create
/// verb, so a target-bound event written by `schedule.remind`/`schedule.schedule`
/// is the durable out-of-band proof from which the host constructs a verified
/// replay identity. Zero matching rows means legacy or hand-written intent.
/// More than one is corruption and fails the drain pass rather than choosing an
/// identity nondeterministically.
#[derive(Clone, Debug)]
pub(super) struct VerifiedCreator {
    /// `None` deliberately represents the provenance-verified
    /// `anonymous:local` actor. Request identity resolution must receive
    /// `None`, not `Some("local")`, to preserve the actor kind.
    pub(super) request_actor: Option<VerifiedActor>,
    pub(super) recipient_id: String,
    pub(super) audit_actor: String,
}

pub(super) async fn verified_creator_for_event(
    rt: &KhiveRuntime,
    namespace: &str,
    scheduled_event_id: uuid::Uuid,
    event_type: &str,
) -> Result<Option<VerifiedCreator>> {
    let mut reader = rt
        .sql()
        .reader()
        .await
        .context("pending-events: open SQL reader for creator provenance")?;
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT actor FROM events \
                  WHERE namespace = ?1 \
                    AND verb = ?2 \
                    AND target_id = ?3 \
                    AND outcome = 'success' \
                    AND json_extract(payload, '$.provenance') = ?4 \
                    AND json_extract(payload, '$.event_type') = ?5 \
                  ORDER BY created_at ASC, id ASC LIMIT 2"
                .to_string(),
            params: vec![
                SqlValue::Text(namespace.to_string()),
                SqlValue::Text(khive_pack_schedule::CREATOR_PROVENANCE_VERB.to_string()),
                SqlValue::Text(scheduled_event_id.to_string()),
                SqlValue::Text(khive_pack_schedule::CREATOR_PROVENANCE_MARKER_V1.to_string()),
                SqlValue::Text(event_type.to_string()),
            ],
            label: Some("pending_events_creator_provenance".into()),
        })
        .await
        .context("pending-events: query creator provenance")?;

    match rows.as_slice() {
        [] => Ok(None),
        [row] => {
            let actor = match row.get("actor") {
                Some(SqlValue::Text(actor)) => actor,
                other => {
                    return Err(anyhow::anyhow!(
                        "pending-events: creator provenance for {scheduled_event_id} has invalid \
                         actor column: {other:?}"
                    ));
                }
            };
            if let Some(actor_id) = actor.strip_prefix("actor:") {
                let verified = VerifiedActor::new(actor_id.to_string()).map_err(|e| {
                    anyhow::anyhow!("pending-events: invalid creator provenance: {e}")
                })?;
                Ok(Some(VerifiedCreator {
                    request_actor: Some(verified),
                    recipient_id: actor_id.to_string(),
                    audit_actor: actor.clone(),
                }))
            } else if actor == "anonymous:local" {
                Ok(Some(VerifiedCreator {
                    request_actor: None,
                    recipient_id: "local".to_string(),
                    audit_actor: actor.clone(),
                }))
            } else {
                Err(anyhow::anyhow!(
                    "pending-events: creator provenance for {scheduled_event_id} has \
                     unsupported actor encoding {actor:?}"
                ))
            }
        }
        _ => Err(anyhow::anyhow!(
            "pending-events: scheduled event {scheduled_event_id} has duplicate creator \
             provenance rows"
        )),
    }
}

/// Dispatch a DSL action string in the given namespace while renewing its
/// claim through the claim-bound durable outcome write.
///
/// The action is wrapped as a JSON-form batch with `namespace` injected into
/// each op's args so the VerbRegistry mints a token scoped to the event's
/// namespace. Dispatch also uses the provenance-verified creator as the
/// effective request identity and preserves public-surface visibility, so a
/// delayed action cannot invoke an internal subhandler. Together these
/// preserve the original authority boundary: writes land in the event's
/// namespace and gate/audit decisions never inherit daemon authority. The
/// returned receipt result is already persisted (or carries the persistence
/// error); callers must not perform another outcome write.
pub(super) struct DispatchLeaseTarget<'a> {
    pub(super) rt: &'a KhiveRuntime,
    pub(super) namespace: &'a str,
    pub(super) scheduled_event_id: uuid::Uuid,
    pub(super) claim: &'a DispatchClaim,
}

pub(super) async fn dispatch_with_renewable_lease(
    target: DispatchLeaseTarget<'_>,
    lease: DispatchLeaseConfig,
    action_dsl: &str,
    creator_actor: Option<VerifiedActor>,
    server: &KhiveMcpServer,
    verbose: bool,
) -> (DispatchCompletion, Result<Option<Value>>) {
    let DispatchLeaseTarget {
        rt,
        namespace,
        scheduled_event_id,
        claim,
    } = target;
    let renewal_rt = rt.clone();
    let renewal_namespace = namespace.to_string();
    let renewal_claim = claim.clone();
    let renewal_cancel = tokio_util::sync::CancellationToken::new();
    let _renewal_cancel_on_drop = CancelOnDrop(renewal_cancel.clone());
    let renewal_stop = renewal_cancel.clone();
    let mut renewal = Some(tokio::spawn(async move {
        let mut renewals = tokio::time::interval_at(
            tokio::time::Instant::now() + lease.renew_every,
            lease.renew_every,
        );
        renewals.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = renewal_stop.cancelled() => return None,
                _ = renewals.tick() => {}
            }
            match renew_dispatch_lease(
                &renewal_rt,
                &renewal_namespace,
                scheduled_event_id,
                &renewal_claim,
                lease,
            )
            .await
            {
                Ok(true) => {}
                Ok(false) => {
                    return Some(
                        "dispatch lease ownership was lost before the action returned".to_string(),
                    );
                }
                Err(error) => {
                    return Some(format!(
                        "dispatch lease renewal failed before the action returned: {error}"
                    ));
                }
            }
        }
    }));

    let dispatch_result =
        dispatch_action(action_dsl, namespace, creator_actor, server, verbose).await;
    // If the renewal task already ended before the action did, its failure is
    // part of the action outcome. Otherwise keep it alive while the durable
    // outcome CAS waits for the writer; relinquishing the lease first would
    // reopen the dispatch/finalize crash window under writer contention.
    let early_lease_failure = if renewal.as_ref().is_some_and(|handle| handle.is_finished()) {
        match renewal.take().expect("renewal handle exists").await {
            Ok(failure) => failure,
            Err(error) => Some(format!("dispatch lease renewal task failed: {error}")),
        }
    } else {
        None
    };
    let completion = if let Some(error) = early_lease_failure {
        DispatchCompletion::Indeterminate(DispatchFailure::plain(error))
    } else {
        match dispatch_result {
            Ok(()) => DispatchCompletion::Succeeded,
            Err(error) if error.outcome_uncertain && !error.disposition_only_uncertain => {
                DispatchCompletion::Indeterminate(error.failure)
            }
            Err(error) => DispatchCompletion::Failed(error.failure),
        }
    };

    let persisted =
        persist_dispatch_outcome(rt, namespace, scheduled_event_id, claim, &completion).await;
    let outcome_is_durable = matches!(&persisted, Ok(Some(_)));
    renewal_cancel.cancel();
    if let Some(renewal) = renewal {
        let late_lease_failure = match renewal.await {
            Ok(failure) => failure,
            Err(error) => Some(format!("dispatch lease renewal task failed: {error}")),
        };
        // A renewal already in flight can observe the just-persisted receipt
        // state and report ownership loss. Once the outcome CAS committed,
        // that is expected and harmless; otherwise retain the diagnostic.
        if !outcome_is_durable {
            if let Some(error) = late_lease_failure {
                tracing::error!(
                    scheduled_event_id = %scheduled_event_id,
                    error,
                    "pending-events: lease renewal ended before outcome became durable"
                );
            }
        }
    }
    (completion, persisted)
}

fn action_error_message(error: &Value) -> String {
    error
        .as_str()
        .or_else(|| error.get("message").and_then(Value::as_str))
        .map(str::to_string)
        .unwrap_or_else(|| {
            serde_json::to_string(error)
                .unwrap_or_else(|_| "scheduled action returned an unreadable error".to_string())
        })
}

fn action_error_outcome_is_uncertain(error: &Value) -> bool {
    let message = action_error_message(error).to_ascii_lowercase();
    let kind = error
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let code = error
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let request_state = error
        .get("request_state")
        .or_else(|| error.pointer("/details/request_state"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let has_outbound_id = error
        .pointer("/details/outbound_id")
        .and_then(Value::as_str)
        .is_some_and(|value| uuid::Uuid::parse_str(value).is_ok());

    kind == "ambiguous"
        || code == "side_effects_unknown"
        || code == "ambiguous_outcome"
        || request_state == "side_effects_unknown"
        || message.contains("side_effects_unknown")
        || (has_outbound_id
            && (kind == "conflict"
                || message.contains("outcome is uncertain")
                || message.contains("comm.delivered")))
}

pub(super) fn action_error_disposition_may_have_committed(error: &Value) -> bool {
    match error {
        Value::Array(errors) => errors
            .iter()
            .any(action_error_disposition_may_have_committed),
        Value::Object(fields) => ["domain_disposition", "entry_domain_disposition"]
            .into_iter()
            .filter_map(|key| fields.get(key))
            .any(|disposition| disposition.as_str() != Some("not_committed")),
        _ => false,
    }
}

pub(super) fn action_failures(failures: &[&Value]) -> DispatchActionError {
    let errors: Vec<Value> = failures
        .iter()
        .map(|failure| {
            let mut error = failure
                .get("error")
                .cloned()
                .unwrap_or_else(|| (*failure).clone());
            if let Some(disposition) = failure.get("domain_disposition") {
                if let Some(fields) = error.as_object_mut() {
                    // Preserve both values if the entry and error disagree:
                    // either may say that the mutation already committed.
                    fields.insert("entry_domain_disposition".into(), disposition.clone());
                } else {
                    error = json!({
                        "message": action_error_message(&error),
                        "original_error": error,
                        "entry_domain_disposition": disposition,
                    });
                }
            }
            error
        })
        .collect();
    let heuristic_uncertain = errors.iter().any(action_error_outcome_is_uncertain);
    let disposition_uncertain = errors
        .iter()
        .any(action_error_disposition_may_have_committed);
    let messages = errors
        .iter()
        .map(action_error_message)
        .collect::<Vec<_>>()
        .join("; ");
    let payload = match errors.as_slice() {
        [error] => error.clone(),
        _ => Value::Array(errors),
    };
    let failure = DispatchFailure::with_payload(
        format!(
            "pending-events: action produced {} failure(s): {messages}",
            failures.len()
        ),
        payload,
    );
    if heuristic_uncertain {
        DispatchActionError::uncertain(failure)
    } else if disposition_uncertain {
        DispatchActionError::disposition_uncertain(failure)
    } else {
        DispatchActionError::known(failure)
    }
}

pub(super) fn stored_action_is_non_single(action_dsl: &str) -> bool {
    khive_request::parse_request(action_dsl).is_ok_and(|parsed| {
        parsed.mode != khive_request::ExecutionMode::Single || parsed.ops.len() != 1
    })
}

pub(super) async fn dispatch_action(
    action_dsl: &str,
    namespace: &str,
    creator_actor: Option<VerifiedActor>,
    server: &KhiveMcpServer,
    verbose: bool,
) -> std::result::Result<(), DispatchActionError> {
    let parsed = khive_request::parse_request(action_dsl).map_err(|error| {
        let masked_dsl = khive_runtime::secret_gate::bounded_masked_log_text(action_dsl);
        DispatchActionError::known(DispatchFailure::plain(format!(
            "pending-events: action DSL parse error ({error}): {masked_dsl:?}"
        )))
    })?;

    // `$prev` references are rejected at schedule-creation time, but legacy
    // rows written before that guard may still carry one. Reject rather than
    // silently drop: a dropped arg can dispatch successfully with
    // missing/wrong data, which is worse than a visible replay failure.
    let mut ops_json: Vec<Value> = Vec::with_capacity(parsed.ops.len());
    for op in &parsed.ops {
        let mut args = serde_json::Map::new();
        for (k, v) in &op.args {
            let khive_request::ArgValue::Value(val) = v else {
                let masked_dsl = khive_runtime::secret_gate::bounded_masked_log_text(action_dsl);
                return Err(DispatchActionError::known(DispatchFailure::plain(format!(
                    "pending-events: non-literal scheduled action argument {k:?} is not \
                     replayable: {masked_dsl:?}"
                ))));
            };
            args.insert(k.clone(), val.clone());
        }
        // Inject the event's namespace so the registry writes to it.
        args.insert(
            "namespace".to_string(),
            Value::String(namespace.to_string()),
        );
        ops_json.push(json!({ "tool": op.tool, "args": Value::Object(args) }));
    }

    let ops_str = serde_json::to_string(&ops_json).map_err(|error| {
        DispatchActionError::known(DispatchFailure::plain(format!(
            "pending-events: serialize ops: {error}"
        )))
    })?;

    if verbose {
        eprintln!("[pending-events] dispatch ns={namespace}: {ops_str}");
    }

    let result = server
        .dispatch_request_replay_as(
            RequestParams {
                plan: None,
                ops: ops_str,
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            },
            namespace,
            creator_actor,
        )
        .await
        .map_err(|error| {
            // The replay request was accepted by the in-process host, but no
            // per-op envelope came back. Conservatively retain at-most-once
            // behavior because the action may already have run.
            DispatchActionError::uncertain(DispatchFailure::plain(format!(
                "pending-events: dispatch outcome unavailable: {error}"
            )))
        })?;

    // The MCP response is a JSON string. Check for per-op failures.
    let parsed_result: Value = serde_json::from_str(&result).map_err(|error| {
        DispatchActionError::uncertain(DispatchFailure::with_payload(
            format!("pending-events: dispatch returned invalid JSON: {error}"),
            json!({"raw_response": result.clone()}),
        ))
    })?;
    let results = parsed_result
        .get("results")
        .and_then(Value::as_array)
        .filter(|results| !results.is_empty())
        .ok_or_else(|| {
            DispatchActionError::uncertain(DispatchFailure::with_payload(
                "pending-events: dispatch response omitted per-op results",
                parsed_result.clone(),
            ))
        })?;
    let failures: Vec<_> = results
        .iter()
        .filter(|result| result.get("ok").and_then(Value::as_bool) != Some(true))
        .collect();
    if !failures.is_empty() {
        return Err(action_failures(&failures));
    }

    Ok(())
}
