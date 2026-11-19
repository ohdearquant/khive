use super::*;

#[test]
fn lifecycle_revision_strictly_advances_a_future_snapshot() {
    let future_snapshot = Utc::now()
        .timestamp_micros()
        .checked_add(86_400_000_000)
        .expect("one-day future timestamp is representable");
    assert_eq!(
        next_lifecycle_updated_at(future_snapshot).expect("advance lifecycle revision"),
        future_snapshot + 1
    );
}

async fn seeded_task() -> (KhiveRuntime, NamespaceToken, khive_storage::note::Note) {
    let runtime = KhiveRuntime::memory().expect("memory runtime");
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .expect("authorize local");
    let mut task = khive_storage::note::Note::new("local", "task", "canonical-lifecycle-test-task");
    task.name = Some("canonical-lifecycle-test-task".to_string());
    task.properties = Some(json!({"status": "inbox", "priority": "p2"}));
    runtime
        .notes(&token)
        .expect("note store")
        .upsert_note(task.clone())
        .await
        .expect("seed task");
    (runtime, token, task)
}

async fn install_concurrent_mirrored_update(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    snapshot: &khive_storage::note::Note,
) -> i64 {
    // This is the canonical generic-note CAS seam after the task hook has
    // synchronized the two body spellings. It starts from the very same
    // snapshot as the lifecycle decision, then commits first.
    let (concurrent, _) = runtime
        .update_note_from_snapshot_with_embedding_report(
            token,
            snapshot.clone(),
            khive_runtime::NotePatch::new(
                None,
                Some("concurrent mirrored body".to_string()),
                None,
                None,
                Some(json!({"description": "concurrent mirrored body"})),
            ),
        )
        .await
        .expect("commit concurrent canonical update");
    concurrent.updated_at
}

async fn assert_concurrent_update_survived(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    task_id: Uuid,
    concurrent_revision: i64,
) {
    let persisted = runtime
        .notes(token)
        .expect("note store")
        .get_note(task_id)
        .await
        .expect("read task")
        .expect("task exists");
    assert_eq!(persisted.updated_at, concurrent_revision);
    assert_eq!(persisted.content, "concurrent mirrored body");
    assert_eq!(
        persisted
            .properties
            .as_ref()
            .and_then(|properties| properties.get("description"))
            .and_then(Value::as_str),
        Some("concurrent mirrored body")
    );
    assert_eq!(task_status(persisted.properties.as_ref()), "inbox");
}

#[tokio::test]
async fn canonical_transition_refuses_stale_decision_snapshot() {
    let (runtime, token, task) = seeded_task().await;
    let decision = prepare_transition(
        &runtime,
        &token,
        &task.id.as_hyphenated().to_string(),
        "next",
        None,
        DependencyOptions::default(),
    )
    .await
    .expect("prepare transition");
    let TransitionDecision::Write {
        note: snapshot,
        current,
        target,
        props,
        updated_at,
        ..
    } = decision
    else {
        panic!("inbox -> next must be a write decision");
    };
    assert!(updated_at > snapshot.updated_at);

    let concurrent_revision = install_concurrent_mirrored_update(&runtime, &token, &snapshot).await;
    let affected = atomic_gtd_transition(
        &runtime, &snapshot, &current, &target, &props, updated_at, None,
    )
    .await
    .expect("execute guarded transition");
    assert_eq!(affected, 0, "stale transition snapshot must lose its CAS");
    assert_concurrent_update_survived(&runtime, &token, task.id, concurrent_revision).await;
}

#[tokio::test]
async fn canonical_transition_guard_closes_soft_delete_without_revision_change() {
    let (runtime, token, task) = seeded_task().await;
    let decision = prepare_transition(
        &runtime,
        &token,
        &task.id.as_hyphenated().to_string(),
        "next",
        None,
        DependencyOptions::default(),
    )
    .await
    .expect("prepare transition");
    let TransitionDecision::Write {
        note: snapshot,
        current,
        target,
        props,
        updated_at,
        ..
    } = decision
    else {
        panic!("inbox -> next must be a write decision");
    };

    assert!(runtime
        .notes(&token)
        .expect("note store")
        .delete_note(task.id, khive_storage::DeleteMode::Soft)
        .await
        .expect("soft delete task"));
    let tombstone = runtime
        .notes(&token)
        .expect("note store")
        .get_note_including_deleted(task.id)
        .await
        .expect("read tombstone")
        .expect("tombstone exists");
    assert_eq!(
        tombstone.updated_at, snapshot.updated_at,
        "legacy soft delete deliberately demonstrates why deletion marker is a separate guard"
    );
    assert!(tombstone.deleted_at.is_some());

    let affected = atomic_gtd_transition(
        &runtime, &snapshot, &current, &target, &props, updated_at, None,
    )
    .await
    .expect("execute guarded transition");
    assert_eq!(affected, 0, "soft-deleted snapshot must lose its CAS");
}

#[tokio::test]
async fn canonical_complete_refuses_stale_decision_snapshot() {
    let (runtime, token, task) = seeded_task().await;
    let decision = prepare_complete(
        &runtime,
        &token,
        &task.id.as_hyphenated().to_string(),
        None,
        Some("shipped"),
        DependencyOptions::default(),
    )
    .await
    .expect("prepare complete");
    assert!(decision.updated_at > decision.note.updated_at);

    let concurrent_revision =
        install_concurrent_mirrored_update(&runtime, &token, &decision.note).await;
    let affected = atomic_gtd_transition(
        &runtime,
        &decision.note,
        &decision.current,
        decision.target,
        &decision.props,
        decision.updated_at,
        None,
    )
    .await
    .expect("execute guarded complete");
    assert_eq!(affected, 0, "stale complete snapshot must lose its CAS");
    assert_concurrent_update_survived(&runtime, &token, task.id, concurrent_revision).await;
}

#[tokio::test]
async fn lost_duplicate_partner_conflict_names_both_causes() {
    let (runtime, token, task) = seeded_task().await;
    let mut kept = khive_storage::note::Note::new("local", "task", "kept");
    kept.name = Some("kept".to_string());
    kept.properties = Some(json!({"status": "inbox", "priority": "p2"}));
    let notes = runtime.notes(&token).expect("note store");
    notes.upsert_note(kept.clone()).await.expect("seed kept");
    let (decision, partner) = duplicate::prepare_transition(
        &runtime,
        &token,
        &task.id.as_hyphenated().to_string(),
        "cancelled",
        None,
        DependencyOptions::default(),
        Some(&kept.id.as_hyphenated().to_string()),
    )
    .await
    .expect("prepare duplicate cancellation");
    let TransitionDecision::Write {
        note: snapshot,
        current,
        target,
        props,
        updated_at,
        ..
    } = decision
    else {
        panic!("inbox -> cancelled must be a write decision");
    };
    assert!(notes
        .delete_note(kept.id, khive_storage::DeleteMode::Soft)
        .await
        .expect("soft delete partner"));

    let affected = atomic_gtd_transition(
        &runtime, &snapshot, &current, &target, &props, updated_at, partner,
    )
    .await
    .expect("execute guarded transition");
    assert_eq!(affected, 0, "a lost partner must stop the write");
    let conflict = lifecycle_write_conflict("transition", task.id, &current, &current, partner);
    let message = conflict.to_string();
    assert!(message.contains("nothing was written"), "{message}");
    assert!(
        message.contains("changed after the lifecycle decision"),
        "{message}"
    );
    let partner_text = format!("duplicate partner {}", short_id(kept.id));
    assert!(message.contains(&partner_text), "{message}");
    assert!(message.contains("is no longer a live task"), "{message}");

    let unchanged = lifecycle_write_conflict("transition", task.id, &current, &current, None);
    assert_eq!(
        unchanged.to_string(),
        format!(
            "invalid input: transition: task {} changed after the lifecycle decision \
                 while remaining in state \"inbox\"; retry with fresh state",
            short_id(task.id)
        ),
        "without duplicate_of the conflict text is unchanged"
    );
}
