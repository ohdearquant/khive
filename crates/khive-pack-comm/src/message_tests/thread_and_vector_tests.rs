/// Build a minimal same-namespace runtime + authorized token for the two
/// tests below. The namespace is still generated fresh per call so
/// `arm_vector_fail_scoped`/`arm_fts_fail_scoped` — process-wide,
/// namespace-keyed statics — never race a concurrently-running test.
fn scratch_runtime_and_token(ns: &str) -> (KhiveRuntime, NamespaceToken) {
    use khive_runtime::{AllowAllGate, BackendId, RuntimeConfig};

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
        default_namespace: Namespace::parse(ns).unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: std::sync::Arc::new(AllowAllGate),
        packs: vec!["kg".to_string(), "comm".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
        exec: Default::default(),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime");
    let token = runtime
        .authorize(Namespace::parse(ns).unwrap())
        .expect("authorize");
    (runtime, token)
}

/// A root send (no `thread_id`) must store the SAME canonical thread_id —
/// the outbound note's own id — on BOTH copies from their first write,
/// not patched in afterward. `created_at == updated_at` on both is the
/// positive signal that no later UPDATE ever touched either row
/// post-creation.
#[tokio::test]
async fn send_root_thread_id_is_canonical_without_patch_write() {
    let ns = format!("thread-id-canonical-{}", Uuid::new_v4().simple());
    let (runtime, token) = scratch_runtime_and_token(&ns);

    let (outbound_note, _) = dual_write_message(
        &runtime,
        &token,
        &ns,
        &ns,
        None,
        "root send thread id",
        None,
        "2026-08-01T00:00:00Z",
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .expect("dual_write_message succeeds");
    let outbound_full_id = outbound_note.id.as_hyphenated().to_string();

    let alive: Vec<_> = runtime
        .list_notes(&token, Some("message"), 100, 0)
        .await
        .expect("list_notes")
        .into_iter()
        .filter(|n| n.deleted_at.is_none())
        .collect();
    assert_eq!(alive.len(), 2, "expected outbound + inbound; got {alive:?}");

    for note in &alive {
        let thread_id = note
            .properties
            .as_ref()
            .and_then(|p| p.get("thread_id"))
            .and_then(|v| v.as_str())
            .expect("thread_id property present");
        assert_eq!(
            thread_id, outbound_full_id,
            "both copies must carry the outbound id as canonical thread_id from \
                 their first write; note {} had {thread_id}",
            note.id
        );
        assert_eq!(
            note.created_at, note.updated_at,
            "no post-creation patch write may have touched note {} \
                 (created_at must equal updated_at)",
            note.id
        );
    }
}

/// Failure injection mid-unit, proven across a CROSS-namespace send: the
/// outbound plan (sender namespace) applies its statements before the
/// inbound plan (recipient namespace) runs, so arming the fault on the
/// recipient namespace only is what actually exercises "an
/// already-applied first plan unwinds" — arming the SAME namespace for
/// both copies (as same-namespace sends do) lets the one-shot fault be
/// consumed by the outbound plan instead, proving nothing about rollback
/// of prior work. Absence is asserted positively — `list_notes`,
/// `fts_notes` row counts, `VectorStore::count`, and `ann_write_log` row
/// counts — in BOTH namespaces, not by inspecting the error.
#[tokio::test]
async fn send_vector_failure_mid_unit_rolls_back_both_notes_and_their_vectors() {
    use async_trait::async_trait;
    use khive_runtime::{
        arm_vector_fail_scoped, AllowAllGate, BackendId, EmbedderProvider, RuntimeConfig,
    };
    use khive_storage::types::SqlValue;
    use khive_storage::SqlStatement;
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};

    const MODEL: &str = "message-vecfail-model";
    const DIMS: usize = 4;

    struct StubService;
    #[async_trait]
    impl EmbeddingService for StubService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts.iter().map(|_| vec![0.5_f32; DIMS]).collect())
        }
        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }
        fn name(&self) -> &'static str {
            MODEL
        }
    }
    struct StubProvider;
    #[async_trait]
    impl EmbedderProvider for StubProvider {
        fn name(&self) -> &str {
            MODEL
        }
        fn dimensions(&self) -> usize {
            DIMS
        }
        async fn build(
            &self,
        ) -> khive_runtime::RuntimeResult<std::sync::Arc<dyn EmbeddingService>> {
            Ok(std::sync::Arc::new(StubService))
        }
    }

    let sender_ns = format!("vecfail-sender-{}", Uuid::new_v4().simple());
    let recipient_ns = format!("vecfail-recipient-{}", Uuid::new_v4().simple());

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
    runtime.register_embedder(StubProvider);

    let sender_token = runtime
        .authorize(Namespace::parse(&sender_ns).unwrap())
        .expect("authorize sender");
    let recipient_token = runtime
        .authorize(Namespace::parse(&recipient_ns).unwrap())
        .expect("authorize recipient");

    // No from_actor: this is a cross-namespace send, so the inbound copy
    // lands in recipient_ns (not the caller's namespace). Arm the fault
    // on recipient_ns — the SECOND plan in send order — so the outbound
    // plan's statements are already applied when the failure hits.
    let _vec_arm = arm_vector_fail_scoped(&recipient_ns);

    let result = dual_write_message(
        &runtime,
        &sender_token,
        &sender_ns,
        &recipient_ns,
        None,
        "vector fail mid-unit",
        None,
        "2026-08-01T00:00:00Z",
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
        "dual_write_message must fail when the vector-insert statement is injected \
             to fail; got {result:?}"
    );

    for (token, ns, label) in [
        (&sender_token, &sender_ns, "sender"),
        (&recipient_token, &recipient_ns, "recipient"),
    ] {
        let alive = runtime
            .list_notes(token, Some("message"), 100, 0)
            .await
            .expect("list_notes")
            .into_iter()
            .filter(|n| n.deleted_at.is_none())
            .count();
        assert_eq!(
            alive, 0,
            "no note may survive a mid-unit vector failure in {label}_ns; got {alive}"
        );

        let mut reader = runtime.sql().reader().await.expect("sql reader");
        let fts_count = reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM fts_notes WHERE namespace = ?1".to_string(),
                params: vec![SqlValue::Text(ns.clone())],
                label: Some("test-fts-count".to_string()),
            })
            .await
            .expect("fts count query");
        assert!(
                matches!(fts_count, Some(SqlValue::Integer(0))),
                "no FTS document may survive a mid-unit vector failure in {label}_ns; got {fts_count:?}"
            );

        let vs = runtime.vectors_for_model(token, MODEL).expect("vec store");
        assert_eq!(
            vs.count().await.expect("count"),
            0,
            "no vector row may survive a mid-unit vector failure in {label}_ns"
        );

        let ann_count = reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM ann_write_log \
                          WHERE namespace = ?1 AND embedding_model = ?2"
                    .to_string(),
                params: vec![
                    SqlValue::Text(ns.clone()),
                    SqlValue::Text(MODEL.to_string()),
                ],
                label: Some("test-ann-log-count".to_string()),
            })
            .await
            .expect("ann_write_log count query");
        assert!(
                matches!(ann_count, Some(SqlValue::Integer(0))),
                "no ann_write_log row may survive a mid-unit vector failure in {label}_ns; got {ann_count:?}"
            );
    }
}
