/// `stream.batch` refuses the memory kind before any sibling mutation commits
/// (atomic mode). It is a preparation failure: every member is prepared before
/// any of them run, so the sibling `append` ordered BEFORE the refused `write`
/// member never commits either.
#[tokio::test]
async fn stream_batch_atomic_refuses_the_memory_kind_before_any_sibling_write_commits() {
    let registry = make_registry(make_runtime());

    // Control, same batch shape minus the refused member: it commits, so the
    // stream read below is an instrument that can be non-zero. A read of a
    // stream nothing ever wrote returns zero entries too, and that reading
    // would pass whether or not the sibling was rolled back.
    registry
        .dispatch(
            "stream.batch",
            json!({
                "atomic": true,
                "ops": [
                    {"op": "append", "stream": "acceptance-2684-atomic", "record": {"control": "a sibling with no refused member beside it"}}
                ]
            }),
        )
        .await
        .expect("the same batch shape without the memory member must commit");
    let seeded = registry
        .dispatch("stream.read", json!({"stream": "acceptance-2684-atomic"}))
        .await
        .expect("stream.read must succeed");
    assert_eq!(
        seeded["entries"].as_array().unwrap().len(),
        1,
        "the control append must be readable, or the assertion after the refusal proves nothing"
    );

    let result = registry
        .dispatch(
            "stream.batch",
            json!({
                "atomic": true,
                "ops": [
                    {"op": "append", "stream": "acceptance-2684-atomic", "record": {"sibling": "before the refused member"}},
                    {"op": "write", "key": "batched memory", "kind": "memory", "doc": {"content": "a memory write inside an atomic batch"}}
                ]
            }),
        )
        .await;
    let err = result.expect_err(
        "a memory-kind batch member must abort the whole atomic batch during preparation",
    );
    assert!(err.to_string().contains("memory.remember"));

    let page = registry
        .dispatch("stream.read", json!({"stream": "acceptance-2684-atomic"}))
        .await
        .expect("stream.read must succeed");
    assert_eq!(
        page["entries"].as_array().unwrap().len(),
        1,
        "the stream must still hold only the control append: the sibling ordered before the \
         refused member must not have committed"
    );
    assert_eq!(page["head_seq"].as_i64(), Some(1));
}

/// Same preparation-failure contract in per-member mode. Per-member `stream.batch`
/// still prepares every member up front before running any of them, so a
/// memory-kind member's refusal aborts the WHOLE call — including a sibling
/// ordered before it that per-member mode would otherwise run independently.
#[tokio::test]
async fn stream_batch_per_member_refuses_the_memory_kind_before_any_sibling_write_commits() {
    let registry = make_registry(make_runtime());

    // Control, same batch shape minus the refused member: it commits, so the
    // stream read below is an instrument that can be non-zero. A read of a
    // stream nothing ever wrote returns zero entries too, and that reading
    // would pass whether or not the sibling was rolled back.
    registry
        .dispatch(
            "stream.batch",
            json!({
                "atomic": false,
                "ops": [
                    {"op": "append", "stream": "acceptance-2684-per-member", "record": {"control": "a sibling with no refused member beside it"}}
                ]
            }),
        )
        .await
        .expect("the same batch shape without the memory member must commit");
    let seeded = registry
        .dispatch(
            "stream.read",
            json!({"stream": "acceptance-2684-per-member"}),
        )
        .await
        .expect("stream.read must succeed");
    assert_eq!(
        seeded["entries"].as_array().unwrap().len(),
        1,
        "the control append must be readable, or the assertion after the refusal proves nothing"
    );

    let result = registry
        .dispatch(
            "stream.batch",
            json!({
                "atomic": false,
                "ops": [
                    {"op": "append", "stream": "acceptance-2684-per-member", "record": {"sibling": "before the refused member"}},
                    {"op": "write", "key": "batched memory two", "kind": "memory", "doc": {"content": "a memory write inside a per-member batch"}}
                ]
            }),
        )
        .await;
    let err = result.expect_err(
        "a memory-kind batch member must abort the whole per-member batch during preparation",
    );
    assert!(err.to_string().contains("memory.remember"));

    let page = registry
        .dispatch(
            "stream.read",
            json!({"stream": "acceptance-2684-per-member"}),
        )
        .await
        .expect("stream.read must succeed");
    assert_eq!(
        page["entries"].as_array().unwrap().len(),
        1,
        "per-member mode still prepares every member before running any, so the stream must \
         still hold only the control append: the sibling ordered before the refused member \
         must not have committed either"
    );
}

/// The specialized writer's row is visible three ways: raw storage, generic
/// `get`, and a nonempty matching `memory.recall` — the nonempty check is what
/// catches a write that stored nothing readable.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn memory_remember_write_is_visible_in_raw_storage_generic_get_and_a_nonempty_recall() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());
    let tok = rt.authorize(Namespace::local()).unwrap();

    let result = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "acceptance amendment visibility across storage get and recall",
                "memory_type": "semantic",
                "salience": 0.65
            }),
        )
        .await
        .expect("memory.remember must succeed");
    let note_id: Uuid = result["id"].as_str().unwrap().parse().unwrap();

    // 1. Visible in raw storage with its fields set.
    let stored = rt
        .notes(&tok)
        .expect("note store")
        .get_note(note_id)
        .await
        .expect("get_note")
        .expect("note exists");
    assert_eq!(stored.salience, Some(0.65));
    assert_eq!(stored.decay_factor, Some(0.005));
    let stored_type = stored
        .properties
        .as_ref()
        .and_then(|p| p.get("memory_type"))
        .and_then(|v| v.as_str());
    assert_eq!(stored_type, Some("semantic"));

    // 2. Visible through generic get.
    let got = registry
        .dispatch("get", json!({"id": note_id.to_string()}))
        .await
        .expect("generic get must find the memory note");
    let got = got.get("record").unwrap_or(&got);
    assert_eq!(got["id"].as_str(), Some(note_id.to_string().as_str()));

    // 3. A nonempty matching recall.
    let recall = registry
        .dispatch(
            "memory.recall",
            json!({"query": "acceptance amendment visibility across storage get and recall"}),
        )
        .await
        .expect("memory.recall must succeed");
    let hits = recall.as_array().expect("recall returns an array");
    assert!(!hits.is_empty(), "recall must be nonempty");
    assert!(hits
        .iter()
        .any(|h| h["id"].as_str() == Some(note_id.to_string().as_str())));
}

/// Identical keyed replay preserves the original record: this amendment does
/// not change `memory.remember`'s key equality, replay result or conflict
/// contract.
#[tokio::test]
async fn memory_remember_identical_keyed_replay_preserves_the_original_record() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());
    let tok = rt.authorize(Namespace::local()).unwrap();

    let args = json!({
        "content": "acceptance amendment keyed replay content",
        "memory_type": "semantic",
        "salience": 0.6,
        "idempotency_key": "acceptance-2684-replay",
    });
    let first = registry
        .dispatch("memory.remember", args.clone())
        .await
        .expect("first remember");
    let second = registry
        .dispatch("memory.remember", args)
        .await
        .expect("identical replay");

    assert_eq!(
        second["id"], first["id"],
        "identical keyed replay must return the original record's id"
    );
    assert_eq!(second["replayed"], json!(true));
    assert_eq!(second["salience"], first["salience"]);
    assert_eq!(second["decay_factor"], first["decay_factor"]);

    let notes = rt
        .notes(&tok)
        .expect("note store")
        .get_live_notes_by_key("local", "acceptance-2684-replay", Some("memory"))
        .await
        .expect("key lookup");
    assert_eq!(
        notes.len(),
        1,
        "a replay must not create a second row under the same key"
    );
}

/// The hook is create-only: an update to an existing memory still works.
#[tokio::test]
async fn update_of_an_existing_memory_still_works_because_the_admission_hook_is_create_only() {
    let registry = make_registry(make_runtime());
    let created = remembered(&registry, json!({})).await;

    let updated = registry
        .dispatch(
            "update",
            json!({"id": created["id"], "content": "updated content for an existing memory"}),
        )
        .await
        .expect(
            "an update to an existing memory must succeed; the admission hook only refuses creation",
        );
    assert_eq!(
        updated["content"],
        json!("updated content for an existing memory")
    );
}

/// Another pack-owned kind with its own `KindHook` remains usable through
/// generic create: `task` (gtd's `TaskHook`) still creates in the same
/// registry that refuses the memory kind — the refusal is scoped to the
/// memory kind, not to "any kind with a hook".
#[tokio::test]
async fn generic_create_of_a_kind_with_its_own_hook_is_unaffected_by_the_memory_refusal() {
    let rt = make_runtime();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(GtdPack::new(rt.clone()));
    builder.register(MemoryPack::new(
        receipt_credentials::with_receipt_credentials(rt),
    ));
    let registry = builder.build().expect("registry builds");

    registry
        .dispatch(
            "create",
            json!({"kind": "note", "note_kind": "task", "title": "a task created through generic create"}),
        )
        .await
        .expect("a kind with its own hook must still create through generic create");

    let refusal = registry
        .dispatch(
            "create",
            json!({"kind": "memory", "content": "still refused alongside a working task hook"}),
        )
        .await
        .expect_err("the memory kind must still refuse in the same registry");
    assert!(refusal.to_string().contains("memory.remember"));
}
