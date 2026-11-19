// ── expires_at recall filter ──────────────────────────────────────────────────

/// Expired memories (expires_at <= now) must be excluded from recall results.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_expires_at_excluded_from_recall() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    // Store a memory.
    let result = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "this memory should expire and vanish from recall",
                "memory_type": "semantic",
                "salience": 0.9,
            }),
        )
        .await
        .expect("memory.remember must succeed");

    let note_id_str = result["id"].as_str().expect("note id present").to_owned();
    let note_id: Uuid = note_id_str.parse().expect("valid UUID");

    // Confirm it is recalled while fresh.
    let fresh_recall = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "memory should expire and vanish from recall" }),
        )
        .await
        .expect("recall must succeed");
    let fresh_hits = fresh_recall.as_array().expect("array");
    let fresh_ids: Vec<&str> = fresh_hits
        .iter()
        .map(|h| h["id"].as_str().unwrap_or(""))
        .collect();
    assert!(
        fresh_ids.contains(&note_id_str.as_str()),
        "memory must appear in recall before expiry; got: {fresh_ids:?}"
    );

    // Backdate expires_at to a point in the past.
    let past_micros = chrono::Utc::now().timestamp_micros() - 1_000_000; // 1 second ago
    let tok = rt.authorize(Namespace::local()).expect("authorize local");
    let note_store = rt.notes(&tok).expect("notes store");
    let mut note = note_store
        .get_note(note_id)
        .await
        .expect("get must succeed")
        .expect("note must exist");
    note.expires_at = Some(past_micros);
    note_store
        .upsert_note(note)
        .await
        .expect("upsert must succeed");

    // Now recall must NOT include the expired memory.
    let expired_recall = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "memory should expire and vanish from recall" }),
        )
        .await
        .expect("recall after expiry must succeed");
    let expired_hits = expired_recall.as_array().expect("array");
    let expired_ids: Vec<&str> = expired_hits
        .iter()
        .map(|h| h["id"].as_str().unwrap_or(""))
        .collect();
    assert!(
        !expired_ids.contains(&note_id_str.as_str()),
        "expired memory (expires_at <= now) must be excluded from recall; got: {expired_ids:?}"
    );
}

// ── memory.prune ─────────────────────────────────────────────────────────────

/// memory.prune with min_salience soft-deletes low-salience memories.
#[tokio::test]
async fn test_prune_by_salience() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    // High-salience memory — must survive prune.
    let high = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "high salience memory that should survive prune",
                "memory_type": "semantic",
                "salience": 0.9,
            }),
        )
        .await
        .expect("remember high");
    let high_id = high["id"].as_str().expect("id").to_owned();

    // Low-salience memory — must be pruned.
    let low = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "low salience memory that should be pruned",
                "memory_type": "semantic",
                "salience": 0.1,
            }),
        )
        .await
        .expect("remember low");
    let low_id = low["id"].as_str().expect("id").to_owned();

    // Prune min_salience=0.5: low must go, high must remain.
    let prune_result = registry
        .dispatch("memory.prune", json!({ "min_salience": 0.5, "before": 0 }))
        .await
        .expect("memory.prune must succeed");

    assert_eq!(
        prune_result["dry_run"].as_bool(),
        Some(false),
        "dry_run must be false"
    );
    assert!(
        prune_result["pruned"].as_u64().unwrap_or(0) >= 1,
        "at least 1 memory pruned; got: {prune_result:?}"
    );

    // Verify via NoteStore that low-salience note is soft-deleted.
    let tok = rt.authorize(Namespace::local()).expect("authorize local");
    let low_uuid: Uuid = low_id.parse().expect("valid uuid");
    let low_note = rt
        .get_note_including_deleted(&tok, low_uuid)
        .await
        .expect("get must succeed")
        .expect("row must exist");
    assert!(
        low_note.deleted_at.is_some(),
        "low-salience memory must be soft-deleted after prune; deleted_at={:?}",
        low_note.deleted_at
    );

    // High-salience note must NOT be deleted.
    let high_uuid: Uuid = high_id.parse().expect("valid uuid");
    let high_note = rt
        .get_note_including_deleted(&tok, high_uuid)
        .await
        .expect("get must succeed")
        .expect("row must exist");
    assert!(
        high_note.deleted_at.is_none(),
        "high-salience memory must NOT be deleted by prune; deleted_at={:?}",
        high_note.deleted_at
    );
}

/// memory.prune with no explicit `before` value (defaults to now) prunes expired memories.
#[tokio::test]
async fn test_prune_expired_by_default_cutoff() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    // Store a memory.
    let result = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "will be expired and then pruned",
                "memory_type": "semantic",
                "salience": 0.8,
            }),
        )
        .await
        .expect("remember");
    let note_id_str = result["id"].as_str().expect("id").to_owned();
    let note_id: Uuid = note_id_str.parse().expect("valid uuid");

    // Set expires_at to the past.
    let past_micros = chrono::Utc::now().timestamp_micros() - 1_000_000;
    let tok = rt.authorize(Namespace::local()).expect("authorize local");
    let note_store = rt.notes(&tok).expect("notes");
    let mut note = note_store
        .get_note(note_id)
        .await
        .expect("get")
        .expect("exists");
    note.expires_at = Some(past_micros);
    note_store.upsert_note(note).await.expect("upsert");

    // Prune with no salience filter, default cutoff = now.
    let prune_result = registry
        .dispatch("memory.prune", json!({}))
        .await
        .expect("memory.prune must succeed");

    assert!(
        prune_result["pruned"].as_u64().unwrap_or(0) >= 1,
        "expired memory must be pruned; got: {prune_result:?}"
    );

    let pruned_note = rt
        .get_note_including_deleted(&tok, note_id)
        .await
        .expect("get")
        .expect("exists");
    assert!(
        pruned_note.deleted_at.is_some(),
        "expired memory must be soft-deleted after prune"
    );
}

/// memory.prune dry_run=true counts without deleting.
#[tokio::test]
async fn test_prune_dry_run() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    let result = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "dry run target memory",
                "memory_type": "semantic",
                "salience": 0.05,
            }),
        )
        .await
        .expect("remember");
    let note_id_str = result["id"].as_str().expect("id").to_owned();
    let note_id: Uuid = note_id_str.parse().expect("valid uuid");

    // Dry run — should report would_prune >= 1 but not delete.
    let dry_result = registry
        .dispatch(
            "memory.prune",
            json!({ "min_salience": 0.5, "before": 0, "dry_run": true }),
        )
        .await
        .expect("memory.prune dry_run must succeed");

    assert_eq!(
        dry_result["dry_run"].as_bool(),
        Some(true),
        "dry_run field must be true"
    );
    assert!(
        dry_result["would_prune"].as_u64().unwrap_or(0) >= 1,
        "dry_run must report >= 1 would_prune; got: {dry_result:?}"
    );
    assert_eq!(
        dry_result["pruned"].as_u64(),
        Some(0),
        "dry_run must not delete (pruned must be 0)"
    );

    // Verify note is NOT deleted.
    let tok = rt.authorize(Namespace::local()).expect("authorize local");
    let note = rt
        .get_note_including_deleted(&tok, note_id)
        .await
        .expect("get")
        .expect("exists");
    assert!(
        note.deleted_at.is_none(),
        "dry_run must not soft-delete notes; deleted_at={:?}",
        note.deleted_at
    );
}

// ── memory.vacuum ─────────────────────────────────────────────────────────────

/// memory.vacuum must succeed and return ok=true.
#[tokio::test]
async fn test_vacuum_succeeds() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    let result = registry
        .dispatch("memory.vacuum", json!({}))
        .await
        .expect("memory.vacuum must succeed");

    assert_eq!(
        result["ok"].as_bool(),
        Some(true),
        "vacuum must return ok=true; got: {result:?}"
    );
}
