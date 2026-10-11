use super::*;

/// Revalidate and commit every feedback side effect against one rebased snapshot.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn persist_feedback_state_mutation(
    sql: &dyn SqlAccess,
    token: &NamespaceToken,
    tracker: &Mutex<PersistenceTracker>,
    state: &Mutex<BrainState>,
    profile_id: String,
    event_write: FeedbackEventWrite,
    entity_capacity: usize,
    apply: impl FnOnce(&mut BrainState, &BrainSignal) + Send + 'static,
) -> Result<Option<Event>, RuntimeError> {
    let namespace = token.namespace().as_str().to_string();
    let now_us = chrono::Utc::now().timestamp_micros();
    let (local_version, local_snapshot) = {
        let tracker = tracker.lock().unwrap();
        let version = tracker.snapshot_version(&namespace);
        let snapshot = state.lock().unwrap().to_snapshot();
        (version, snapshot)
    };

    let namespace_for_op = namespace.clone();
    let attribution_for_op = EventAttribution::from_token(token);
    let profile_id_for_op = profile_id;
    let op: khive_storage::AtomicUnitOp = Box::new(move |writer| {
        Box::pin(async move {
            let previous_updated_at = load_snapshot_version_on_reader(writer, &namespace_for_op)
                .await
                .map_err(|e| {
                    khive_storage::StorageError::driver(
                        khive_storage::StorageCapability::Sql,
                        "brain_feedback_load_snapshot_version",
                        e,
                    )
                })?;
            let base_snapshot = match previous_updated_at {
                Some(updated_at) if local_version == Some(updated_at) => local_snapshot,
                Some(updated_at) => {
                    let (durable_snapshot, reloaded_updated_at) =
                        load_latest_snapshot_on_reader(writer, &namespace_for_op, entity_capacity)
                            .await
                            .map_err(|e| {
                                khive_storage::StorageError::driver(
                                    khive_storage::StorageCapability::Sql,
                                    "brain_feedback_load_snapshot",
                                    e,
                                )
                            })?
                            .ok_or_else(|| {
                                khive_storage::StorageError::driver(
                                    khive_storage::StorageCapability::Sql,
                                    "brain_feedback_load_snapshot",
                                    std::io::Error::other(
                                        "snapshot disappeared inside the write transaction",
                                    ),
                                )
                            })?;
                    if reloaded_updated_at != updated_at {
                        return Err(khive_storage::StorageError::driver(
                            khive_storage::StorageCapability::Sql,
                            "brain_feedback_load_snapshot",
                            std::io::Error::other(format!(
                                "snapshot generation changed inside the write transaction: \
                                 expected {updated_at}, got {reloaded_updated_at}"
                            )),
                        ));
                    }
                    durable_snapshot
                }
                None => local_snapshot,
            };
            let mut proposed = BrainState::from_snapshot(base_snapshot, entity_capacity);

            match proposed.profiles.get(&profile_id_for_op) {
                None => {
                    return Ok(
                        Box::new(FeedbackMutationOutcome::Rejected(RuntimeError::NotFound(
                            format!(
                                "serving profile {:?} not found in profile registry",
                                profile_id_for_op
                            ),
                        ))) as Box<dyn std::any::Any + Send>,
                    );
                }
                Some(record)
                    if record.lifecycle == khive_brain_core::ProfileLifecycle::Archived =>
                {
                    return Ok(Box::new(FeedbackMutationOutcome::Rejected(
                        RuntimeError::InvalidInput(format!(
                            "serving profile {:?} is archived; feedback cannot credit archived profiles",
                            profile_id_for_op
                        )),
                    )) as Box<dyn std::any::Any + Send>);
                }
                Some(_) => {}
            }

            let commit_at_us = previous_updated_at
                .map(|updated_at| now_us.max(updated_at.saturating_add(1)))
                .unwrap_or(now_us);
            let event = match event_write {
                FeedbackEventWrite::Direct(event) => {
                    let mut event = attribution_for_op.stamp(event);
                    event.created_at = commit_at_us;
                    khive_db::stores::event::append_event_on_writer(writer, &event)
                        .await
                        .map_err(|e| {
                            khive_storage::StorageError::driver(
                                khive_storage::StorageCapability::Sql,
                                "brain_feedback_append_public_event",
                                e,
                            )
                        })?;
                    event
                }
                FeedbackEventWrite::Graded {
                    event,
                    scorer_run_id,
                    serve_ledger_id,
                    grade,
                } => {
                    let claimed = crate::fold_gate::claim_dedup_within_tx(
                        writer,
                        &scorer_run_id,
                        &serve_ledger_id,
                        commit_at_us,
                    )
                    .await
                    .map_err(|e| {
                        khive_storage::StorageError::driver(
                            khive_storage::StorageCapability::Sql,
                            "brain_feedback_claim_scorer_dedup",
                            e,
                        )
                    })?;
                    if !claimed {
                        return Ok(Box::new(FeedbackMutationOutcome::Deduped)
                            as Box<dyn std::any::Any + Send>);
                    }
                    let mut event = attribution_for_op.stamp(event);
                    event.created_at = commit_at_us;
                    khive_db::stores::event::append_event_on_writer(writer, &event)
                        .await
                        .map_err(|e| {
                            khive_storage::StorageError::driver(
                                khive_storage::StorageCapability::Sql,
                                "brain_feedback_append_public_event",
                                e,
                            )
                        })?;
                    crate::serve_ledger::backfill_grade_on_writer(
                        writer,
                        &serve_ledger_id,
                        &grade,
                        commit_at_us,
                        &scorer_run_id,
                    )
                    .await
                    .map_err(|e| {
                        khive_storage::StorageError::driver(
                            khive_storage::StorageCapability::Sql,
                            "brain_feedback_backfill_grade",
                            e,
                        )
                    })?;
                    event
                }
                FeedbackEventWrite::Gated {
                    mut event,
                    target_id,
                    gate_mode,
                    gate_now_us,
                    dedup_key,
                    grade,
                } => {
                    event.created_at = commit_at_us;
                    let dedup_ref = dedup_key
                        .as_ref()
                        .map(|(scorer, ledger)| (scorer.as_str(), ledger.as_str()));
                    let outcome = crate::fold_gate::apply_gate_and_append_within_tx(
                        writer,
                        &namespace_for_op,
                        &attribution_for_op,
                        &profile_id_for_op,
                        &target_id,
                        gate_mode,
                        gate_now_us,
                        dedup_ref,
                        grade.as_deref(),
                        move |fold_outcome, forced_zero| {
                            let (effective_weight, mass_before, mass_after) = match fold_outcome {
                                Some(outcome) => (
                                    outcome.effective_weight,
                                    outcome.mass_before,
                                    outcome.mass_after,
                                ),
                                None => (0.0, 0.0, 0.0),
                            };
                            event.payload["gate"] = serde_json::json!({
                                "effective_weight": effective_weight,
                                "mass_before": mass_before,
                                "mass_after": mass_after,
                                "forced_zero_weight": forced_zero,
                            });
                            event
                        },
                    )
                    .await
                    .map_err(|e| {
                        khive_storage::StorageError::driver(
                            khive_storage::StorageCapability::Sql,
                            "brain_feedback_gate_and_append",
                            e,
                        )
                    })?;
                    match outcome {
                        crate::fold_gate::GateAndAppendOutcome::Deduped => {
                            return Ok(Box::new(FeedbackMutationOutcome::Deduped)
                                as Box<dyn std::any::Any + Send>);
                        }
                        crate::fold_gate::GateAndAppendOutcome::Applied(result) => result.event,
                    }
                }
            };

            let signal = interpret(&event);
            apply(&mut proposed, &signal);
            let payload = serde_json::to_value(&event).map_err(|e| {
                khive_storage::StorageError::driver(
                    khive_storage::StorageCapability::Sql,
                    "brain_feedback_serialize_private_event",
                    e,
                )
            })?;
            append_brain_event_on_writer(
                writer,
                &namespace_for_op,
                &profile_id_for_op,
                &event.verb,
                &payload,
                commit_at_us,
            )
            .await
            .map_err(|e| {
                khive_storage::StorageError::driver(
                    khive_storage::StorageCapability::Sql,
                    "brain_feedback_append_private_event",
                    e,
                )
            })?;
            let snapshot_json = proposed.to_snapshot_json().map_err(|e| {
                khive_storage::StorageError::driver(
                    khive_storage::StorageCapability::Sql,
                    "brain_feedback_serialize_snapshot",
                    e,
                )
            })?;
            upsert_snapshot_json_on_writer(writer, &namespace_for_op, snapshot_json, commit_at_us)
                .await
                .map_err(|e| {
                    khive_storage::StorageError::driver(
                        khive_storage::StorageCapability::Sql,
                        "brain_feedback_upsert_snapshot",
                        e,
                    )
                })?;

            Ok(Box::new(FeedbackMutationOutcome::Committed {
                event,
                state: Box::new(proposed),
                snapshot_version: commit_at_us,
            }) as Box<dyn std::any::Any + Send>)
        })
    });

    let boxed = sql.atomic_unit(op).await?;
    let outcome = *boxed
        .downcast::<FeedbackMutationOutcome>()
        .expect("persist_feedback_state_mutation must return FeedbackMutationOutcome");

    match outcome {
        FeedbackMutationOutcome::Rejected(error) => Err(error),
        FeedbackMutationOutcome::Deduped => Ok(None),
        FeedbackMutationOutcome::Committed {
            event,
            state: proposed,
            snapshot_version,
        } => {
            let mut tracker = tracker.lock().unwrap();
            if !matches!(
                tracker.snapshot_version(&namespace),
                Some(current) if current > snapshot_version
            ) {
                *state.lock().unwrap() = *proposed;
                tracker.reset_dirty(&namespace);
                tracker.mark_loaded_at(namespace, Some(snapshot_version));
            }
            Ok(Some(event))
        }
    }
}
