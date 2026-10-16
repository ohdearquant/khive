// Issue #460 / atomic follow-up: dual_write_message must not leave a live
// outbound note behind when the inbound copy's write fails. Since the
// send-single-txn change, both copies commit under ONE atomic unit, so a
// failure on the inbound copy's FTS statement (armed via
// `arm_fts_fail_scoped(&recipient_ns)`, now consumed by
// `create_notes_atomic` through `consume_fts_fail_fault`) rolls back the
// WHOLE unit — including the outbound copy's already-applied statements
// in sender_ns. Assert absence positively in BOTH namespaces rather than
// inspecting the error string.
#[tokio::test]
async fn dual_write_inbound_failure_rolls_back_whole_unit_including_outbound() {
    use khive_runtime::{arm_fts_fail_scoped, AllowAllGate, BackendId, RuntimeConfig};

    let sender_ns = format!("t460-sender-{}", Uuid::new_v4().simple());
    let recipient_ns = format!("t460-recipient-{}", Uuid::new_v4().simple());

    let runtime = KhiveRuntime::new(RuntimeConfig {
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: Default::default(),
        wal_ceiling_env_raw: None,
        web: Default::default(),
        telemetry: Default::default(),
        mounts: Vec::new(),
        brain: Default::default(),
        git_write: Default::default(),
        display_timezone: khive_runtime::config::resolve_default_display_timezone(),
        events_split: None,
        db_path: None,
        blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::parse(&sender_ns).unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: std::sync::Arc::new(AllowAllGate),
        packs: vec!["kg".to_string(), "comm".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![Namespace::parse(&recipient_ns).unwrap()],
        actor_id: None,
        exec: Default::default(),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime");

    let caller_token = runtime
        .authorize(Namespace::parse(&sender_ns).unwrap())
        .expect("authorize sender");
    let recipient_token = runtime
        .authorize(Namespace::parse(&recipient_ns).unwrap())
        .expect("authorize recipient");

    // Outbound copy targets sender_ns; inbound copy targets recipient_ns
    // and its FTS statement is armed to fail inside the atomic unit.
    let _fts_arm = arm_fts_fail_scoped(&recipient_ns);

    let result = dual_write_message(
        &runtime,
        &caller_token,
        &sender_ns,
        &recipient_ns,
        None,
        "F1 regression content",
        None,
        "2026-07-03T00:00:00Z",
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await;

    assert!(
        result.is_err(),
        "dual_write_message must fail when the inbound copy's write fails; got {result:?}"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("rolled back"),
        "error must report the atomic unit rolled back; got: {err_msg}"
    );
    for (token, ns) in [(&caller_token, "sender"), (&recipient_token, "recipient")] {
        let alive = runtime
            .list_notes(token, Some("message"), 100, 0)
            .await
            .expect("list_notes")
            .into_iter()
            .filter(|n| n.deleted_at.is_none())
            .count();
        assert_eq!(
            alive, 0,
            "no live note may remain in {ns}_ns after the whole unit rolled back; got {alive}"
        );
    }
}
