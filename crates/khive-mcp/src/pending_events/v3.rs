//! The generic occurrence-ledger drain. It uses the mapped schedule runtime
//! for accounting and the daemon's existing replay server for effects.

use super::*;
use khive_pack_schedule::occurrence::{
    Admission, DefinitionV3, ObservedResult, PassBudget, ScheduledPayload, TerminalOutcome,
    WorkPurpose,
};
use khive_pack_schedule::occurrence_store::{OccurrenceStore, Recovery};

pub(super) async fn drain(
    rt: &KhiveRuntime,
    server: &KhiveMcpServer,
    verbose: bool,
    summary: &mut DrainSummary,
) -> Result<Option<OccurrenceStore>> {
    drain_inner(rt, server, verbose, summary).await.map_err(|error| {
        let message = format!(
            "accounting_blocked: occurrence accounting or admission failed; no further launches in this pass: {error:#}"
        );
        error.context(message)
    })
}

async fn drain_inner(
    rt: &KhiveRuntime,
    server: &KhiveMcpServer,
    verbose: bool,
    summary: &mut DrainSummary,
) -> Result<Option<OccurrenceStore>> {
    // Optional legacy availability never swallows a partial schema or storage error.
    let Some(store) = OccurrenceStore::for_runtime(rt).await? else {
        return Ok(None);
    };
    let mut budget = PassBudget::default();
    let grace = fire_grace_from_env().num_seconds();
    // Bound discovery separately from materialization. The persisted per-series
    // cursor, rather than this ephemeral keyset, owns every unrecorded instant.
    let work = store
        .series_work_page(
            WorkPurpose::AccountingOrRecovery,
            None,
            200,
            budget.remaining(),
        )
        .await?;
    for item in work.items {
        summary.scanned += 1;
        if item.expired_attempt {
            recover(&store, &item, server, verbose, summary).await?;
        }
        if budget.remaining() > 0 {
            if let Some(snapshot) = store.series(item.series.schedule_id).await? {
                let definition = store.definition(&snapshot).await?;
                let horizon = captured_horizon(&store, snapshot.schedule_id).await?;
                let prepared = store.prepare_page(
                    &snapshot,
                    &definition,
                    horizon,
                    grace,
                    budget.remaining(),
                )?;
                if let Some(page) = store.commit_page(prepared).await? {
                    budget.consume(page.created)?;
                    if page.created > 0 {
                        summary.outcomes_persisted += (page.created
                            - usize::from(page.selected_occurrence_id.is_some()))
                            as u64;
                    }
                }
            }
        }
        if let Some(snapshot) = store.series(item.series.schedule_id).await? {
            if let Some(selected) = snapshot.selected {
                let id = khive_pack_schedule::occurrence::OccurrenceKey {
                    store_id: store.store_id(),
                    schedule_id: snapshot.schedule_id,
                    definition_revision: snapshot.definition_revision,
                    scheduled_for: selected,
                }
                .id();
                run_candidate(&store, snapshot.schedule_id, id, server, verbose, summary).await?;
            }
        }
    }
    // A second, bounded zero-new sweep cannot be hidden behind enumeration-only
    // rows when the global 200 identity allowance is exhausted.
    let ready = store
        .series_work_page(WorkPurpose::AccountingOrRecovery, None, 200, 0)
        .await?;
    for item in ready.items {
        if let (Some(next), Some(cut)) = (item.series.next, item.series.cancellation_cut_us) {
            if next >= khive_pack_schedule::occurrence::OccurrenceInstant::from_micros(cut)? {
                let definition = store.definition(&item.series).await?;
                let page = store.prepare_page(
                    &item.series,
                    &definition,
                    store.clock().await?,
                    grace,
                    0,
                )?;
                if let Some(page) = store.commit_page(page).await? {
                    budget.consume(page.created)?;
                }
            }
        }
        if item.expired_attempt {
            recover(&store, &item, server, verbose, summary).await?;
        }
        if let Some(id) = item.ready_unclaimed {
            run_candidate(
                &store,
                item.series.schedule_id,
                id,
                server,
                verbose,
                summary,
            )
            .await?;
        }
    }
    Ok(Some(store))
}

async fn captured_horizon(store: &OccurrenceStore, _schedule: uuid::Uuid) -> Result<i64> {
    #[cfg(test)]
    if let Ok(Some(horizon)) =
        tests::CAPTURED_HORIZONS.try_with(|values| values.get(&_schedule).copied())
    {
        return Ok(horizon);
    }
    Ok(store.clock().await?)
}

async fn recover(
    store: &OccurrenceStore,
    work: &khive_pack_schedule::occurrence::AuxiliaryWork,
    server: &KhiveMcpServer,
    verbose: bool,
    summary: &mut DrainSummary,
) -> Result<()> {
    match store.recover_expired(work).await? {
        Recovery::None => {}
        Recovery::Settled(_) => {
            summary.reclaimed += 1;
            summary.indeterminate += 1;
            summary.outcomes_persisted += 1;
        }
        Recovery::Reclaimable {
            schedule_id,
            occurrence_id,
        } => {
            run_candidate(store, schedule_id, occurrence_id, server, verbose, summary).await?;
        }
    }
    Ok(())
}

fn replay_identity(definition: &DefinitionV3) -> Result<Option<VerifiedActor>> {
    match (
        definition.creator.kind.as_str(),
        definition.creator.id.as_str(),
    ) {
        ("anonymous", "local") => Ok(None),
        ("actor", id) => VerifiedActor::new(id.to_owned())
            .map(Some)
            .map_err(Into::into),
        _ => Err(anyhow::anyhow!(
            "unsupported immutable schedule creator kind"
        )),
    }
}
fn action(definition: &DefinitionV3) -> String {
    match &definition.payload {
        ScheduledPayload::Action { action } => action.clone(),
        ScheduledPayload::Reminder { content } => {
            super::recurrence::reminder_delivery_action(&definition.recipient.id, content)
        }
    }
}

async fn run_candidate(
    store: &OccurrenceStore,
    schedule: uuid::Uuid,
    occurrence: uuid::Uuid,
    server: &KhiveMcpServer,
    verbose: bool,
    summary: &mut DrainSummary,
) -> Result<()> {
    let Some(snapshot) = store.series(schedule).await? else {
        return Ok(());
    };
    let definition = store.definition(&snapshot).await?;
    let creator = replay_identity(&definition)?;
    let action = action(&definition);
    let is_exec = khive_request::parse_request(&action)
        .is_ok_and(|parsed| parsed.ops.len() == 1 && parsed.ops[0].tool == "exec.run");
    let Some(claim) = store.claim_candidate(schedule, occurrence).await? else {
        summary.skipped_race += 1;
        return Ok(());
    };
    let intent = match store.begin_invocation(&claim).await? {
        Admission::Lost => {
            summary.skipped_race += 1;
            return Ok(());
        }
        Admission::Settled(receipt) => {
            summary.outcomes_persisted += 1;
            summary.finalized += 1;
            if receipt.outcome == "missed" {
                summary.missed.push(schedule);
            }
            return Ok(());
        }
        Admission::Launch(intent) => intent,
    };
    summary.invoked += 1;
    let mut renew = tokio::time::interval_at(
        tokio::time::Instant::now() + std::time::Duration::from_secs(60),
        std::time::Duration::from_secs(60),
    );
    renew.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let dispatch = super::dispatch::dispatch_action_response(
        &action,
        &definition.namespace,
        creator,
        server,
        verbose,
    );
    tokio::pin!(dispatch);
    let result = loop {
        tokio::select! {
            result=&mut dispatch=>break classify_dispatch(result,is_exec),
            _=renew.tick()=>{
                if let Err(error)=store.renew_claim(&claim).await {
                    tracing::warn!(occurrence_id=%occurrence,%error,"schedule claim renewal failed; invocation will require durable settlement");
                }
            },
        }
    };
    // Keep renewal alive through the terminal writer wait. A post-intent
    // recovery can only record uncertainty; it never starts another effect.
    let finish = store.finish(&intent, result.clone());
    tokio::pin!(finish);
    let persisted = loop {
        tokio::select! {
            persisted=&mut finish=>break persisted?,
            _=renew.tick()=>{let _=store.renew_claim(&claim).await;},
        }
    };
    if persisted.is_some() {
        summary.outcomes_persisted += 1;
        summary.finalized += 1;
        match result.outcome {
            TerminalOutcome::Succeeded => summary.fired += 1,
            TerminalOutcome::Indeterminate => summary.indeterminate += 1,
            _ => summary.failed += 1,
        }
    } else {
        // Cancellation/expiry may have won the immutable terminal race while
        // this actual response was in flight. Link it without a second notice.
        store
            .append_verified_reconciliation(&intent, result)
            .await?;
    }
    Ok(())
}

pub(super) fn classify_dispatch(
    result: std::result::Result<Value, DispatchActionError>,
    is_exec: bool,
) -> ObservedResult {
    match result {
        Err(error) => {
            let mut observed = ObservedResult::non_invoked(
                if error.outcome_uncertain {
                    TerminalOutcome::Indeterminate
                } else {
                    TerminalOutcome::Refused
                },
                if error.outcome_uncertain {
                    "dispatch_outcome_unavailable"
                } else {
                    "dispatch_refused"
                },
            );
            observed.evidence =
                json!({"message":error.failure.message,"error":error.failure.payload});
            observed
        }
        Ok(value) => classify_envelope(value, is_exec),
    }
}

fn nullable_integer(value: &Value, key: &str) -> Option<Option<i64>> {
    match value.get(key)? {
        Value::Null => Some(None),
        v => v.as_i64().map(Some),
    }
}
fn nullable_time(value: &Value, key: &str) -> Option<Option<i64>> {
    match value.get(key)? {
        Value::Null => Some(None),
        Value::String(v) => v
            .parse::<DateTime<Utc>>()
            .ok()
            .map(|v| Some(v.timestamp_micros())),
        _ => None,
    }
}

pub(super) fn classify_envelope(value: Value, is_exec: bool) -> ObservedResult {
    let mut observed =
        ObservedResult::non_invoked(TerminalOutcome::Indeterminate, "malformed_domain_result");
    observed.evidence = value.clone();
    let Some(results) = value.get("results").and_then(Value::as_array) else {
        return observed;
    };
    if results.len() != 1 || results[0].get("ok").and_then(Value::as_bool) != Some(true) {
        return observed;
    }
    let Some(domain) = results[0].get("result") else {
        return observed;
    };
    if !is_exec {
        observed.outcome = TerminalOutcome::Succeeded;
        observed.reason_code = "handler_completed".into();
        return observed;
    }
    let (
        Some(id),
        Some(timed_out),
        Some(denied),
        Some(success),
        Some(code),
        Some(signal),
        Some(started),
        Some(completed),
    ) = (
        domain
            .get("id")
            .and_then(Value::as_str)
            .and_then(|s| uuid::Uuid::parse_str(s).ok()),
        domain.get("timed_out").and_then(Value::as_bool),
        domain.get("denied").and_then(Value::as_bool),
        domain.get("success").and_then(Value::as_bool),
        nullable_integer(domain, "exit_code"),
        nullable_integer(domain, "exit_signal"),
        nullable_time(domain, "started_at"),
        nullable_time(domain, "finished_at"),
    )
    else {
        return observed;
    };
    observed.exec_receipt_id = Some(id);
    observed.exit_code = code;
    observed.exit_signal = signal;
    observed.started_at_us = started;
    observed.completed_at_us = completed;
    let (outcome, reason) = if timed_out {
        (TerminalOutcome::TimedOut, "exec_timed_out")
    } else if denied {
        (TerminalOutcome::Refused, "exec_refused")
    } else if !success {
        (TerminalOutcome::Failed, "exec_failed")
    } else if code == Some(0) && signal.is_none() {
        (TerminalOutcome::Succeeded, "exec_succeeded")
    } else {
        (TerminalOutcome::Indeterminate, "contradictory_exec_result")
    };
    observed.outcome = outcome;
    observed.reason_code = reason.into();
    observed
}

#[cfg(test)]
#[path = "v3_tests.rs"]
mod tests;
