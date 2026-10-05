    #[tokio::test]
    async fn ordinary_store_accessors_ignore_request_read_cancellation() {
        let backend = StorageBackend::memory().unwrap();
        let (_sender, receiver) = tokio::sync::watch::channel(true);
        khive_storage::scope_request_read_cancellation(receiver, async {
            backend.notes().expect("ordinary notes accessor");
            backend.events().expect("ordinary events accessor");
            #[cfg(feature = "vectors")]
            backend
                .vectors("ordinary_store", "ordinary-store", 8)
                .expect("ordinary vectors accessor");
        })
        .await;
    }

    #[tokio::test]
    async fn admitted_store_constructor_finishes_ddl_after_cancellation() {
        let backend = StorageBackend::memory().unwrap();
        let (sender, receiver) = tokio::sync::watch::channel(false);
        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let writer = backend.pool.writer().unwrap();
            let fired = fired.clone();
            writer
                .conn()
                .authorizer(Some(move |_: rusqlite::hooks::AuthContext<'_>| {
                    fired.store(true, Ordering::SeqCst);
                    sender.send_replace(true);
                    rusqlite::hooks::Authorization::Allow
                }))
                .unwrap();
        }
        let result = khive_storage::scope_request_read_cancellation(receiver, async {
            khive_storage::capture_request_read_context()
                .scope_store_acquisition("admitted_notes_store", || backend.notes())
        })
        .await;
        let writer = backend.pool.writer().unwrap();
        writer
            .conn()
            .authorizer(
                None::<fn(rusqlite::hooks::AuthContext<'_>) -> rusqlite::hooks::Authorization>,
            )
            .unwrap();
        result.expect("request cancellation must not interrupt admitted constructor DDL");
        assert!(
            fired.load(Ordering::SeqCst),
            "cancellation must fire inside actual SQLite work"
        );
        assert_eq!(
            backend.notes_seq_repair_run_count(),
            1,
            "constructor must finish schema repair"
        );
        assert!(sqlite_table_exists(writer.conn(), "notes_seq").unwrap());
    }

    #[cfg(unix)]
    use khive_storage::test_support::freeze_snapshot_sidecars;

    #[tokio::test]
    async fn hot_path_guard_g2_file_backed_read_suite_uses_only_pooled_readers() {
        let dir = tempfile::tempdir().unwrap();
        let backend = StorageBackend::sqlite_for_test(dir.path().join("hot_path_g2.db")).unwrap();
        backend.prepare_core_schema().unwrap();

        // Construct every store named by ADR-165 Slice 2 before the counter
        // baseline. Accessor-time DDL/validation is not request read traffic.
        let entities = backend.entities().unwrap();
        let notes = backend.notes().unwrap();
        let graph = backend.graph().unwrap();
        let events = backend.events().unwrap();
        let text = backend
            .text_with_tokenizer("hot_path_g2", "unicode61")
            .unwrap();
        let agents = backend.agents().unwrap();
        let attachments = backend.attachments().unwrap();
        let sparse = backend.sparse("hot_path_g2").unwrap();
        #[cfg(feature = "vectors")]
        let vectors = backend.vectors("hot_path_g2", "test-model", 2).unwrap();
        let sql = backend.sql();

        let before = backend.pool().reader_acquisition_snapshot();
        assert_eq!(
            entities
                .count_entities("local", EntityFilter::default())
                .await
                .unwrap(),
            0
        );
        assert_eq!(notes.count_notes("local", None).await.unwrap(), 0);
        assert_eq!(graph.count_edges(EdgeFilter::default()).await.unwrap(), 0);
        assert_eq!(
            events.count_events(EventFilter::default()).await.unwrap(),
            0
        );
        assert!(text
            .get_document("local", uuid::Uuid::new_v4())
            .await
            .unwrap()
            .is_none());
        assert!(agents.get("no-such-agent").await.unwrap().is_none());
        assert!(attachments
            .get_attachment(uuid::Uuid::new_v4(), "primary")
            .await
            .unwrap()
            .is_none());
        assert_eq!(sparse.count().await.unwrap(), 0);
        #[cfg(feature = "vectors")]
        assert_eq!(vectors.count().await.unwrap(), 0);

        let mut raw = sql.reader().await.unwrap();
        assert!(matches!(
            raw.query_scalar(SqlStatement {
                sql: "SELECT 1".into(),
                params: Vec::new(),
                label: None,
            })
            .await
            .unwrap(),
            Some(SqlValue::Integer(1))
        ));

        let after = backend.pool().reader_acquisition_snapshot();
        let expected_pooled_delta = 9 + u64::from(cfg!(feature = "vectors"));
        assert_eq!(
            after.pooled_checkouts - before.pooled_checkouts,
            expected_pooled_delta,
            "each ordinary file-backed read must check out exactly one pooled reader"
        );
        assert_eq!(
            after.standalone_opens, before.standalone_opens,
            "ADR-166 G2: ordinary file-backed read verbs must not open standalone readers"
        );
        assert_eq!(after.active_pooled_checkouts, 0);
        assert_eq!(
            after.completed_pooled_checkouts - before.completed_pooled_checkouts,
            expected_pooled_delta
        );
    }
