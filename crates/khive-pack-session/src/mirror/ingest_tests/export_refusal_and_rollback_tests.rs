#[tokio::test]
async fn test_claude_ai_export_over_max_bytes_leaves_cursor_untouched() {
    let (rt, _dir) = setup().await;
    let (_file, path) = write_export_file("[]");

    let stats = mirror_claude_ai_export_file_with_max_bytes(&rt, &path, 0, 1)
        .await
        .expect("oversized export is skipped");
    // The pass reports the identity it observed (the in-memory poll state
    // needs it), but persists nothing: no cursor row is written.
    assert_eq!(
        stats,
        MirrorStats {
            file_identity: Some(
                file_identity(&std::fs::File::open(&path).expect("open export"))
                    .expect("export identity")
            ),
            ..MirrorStats::default()
        }
    );
    assert_eq!(cursor_offset(&rt, &path.to_string_lossy()).await, None);
}

/// SS6 invariants #4/#5 (error never advances cursor; one transaction per
/// pass) — see
/// `crates/khive-pack-session/docs/api/mirror-ingest.md#test_mid_transaction_db_error_leaves_no_partial_state_and_cursor_unadvanced`
/// for why this drives `atomic_unit` directly instead of through crafted event data.
#[tokio::test]
async fn test_mid_transaction_db_error_leaves_no_partial_state_and_cursor_unadvanced() {
    let (rt, _dir) = setup().await;
    let sql = rt.sql();
    let path = std::path::Path::new("/synthetic/mid-tx-probe.json");
    let path_owned = path.to_path_buf();

    let op: khive_storage::AtomicUnitOp = Box::new(move |writer: &mut dyn SqlWriter| {
        Box::pin(async move {
            // First write succeeds — mirrors event 1's session row in a
            // multi-event file pass.
            writer
                    .execute(SqlStatement {
                        sql: "INSERT INTO sessions \
                              (id, provider_session_id, source, message_count, first_seen_at, last_seen_at, namespace) \
                              VALUES('mid-tx-session', 'mid-tx-session', 'chatgpt_export', 0, 1, 1, 'local')"
                            .into(),
                        params: vec![],
                        label: None,
                    })
                    .await?;

            // Cursor advance succeeds too — mirrors `upsert_cursor_on_writer`
            // running near the end of `write_events_and_cursor_on_writer`.
            upsert_cursor_on_writer(writer, &path_owned, Some("mid-tx-session"), 999, 1, None)
                .await?;

            // Third write fails with a genuine (non-suppressed) SQL error —
            // mirrors a mid-loop DB failure on a later event in the same file.
            writer
                .execute(SqlStatement {
                    sql: "INSERT INTO no_such_table_mid_tx_probe(a) VALUES(1)".into(),
                    params: vec![],
                    label: None,
                })
                .await?;

            Ok(Box::new(()) as Box<dyn std::any::Any + Send>)
        })
    });

    // `atomic_unit` itself must surface the error and roll back the
    // whole unit — no explicit `commit()`/`drop()` orchestration is the
    // caller's job anymore; the seam owns it.
    let result = sql.atomic_unit(op).await;
    assert!(
        result.is_err(),
        "atomic_unit must propagate the forced third-write failure"
    );

    assert_eq!(
        count_rows(&rt, "sessions").await,
        0,
        "session write must not survive a later error in the same atomic unit"
    );
    assert_eq!(
        cursor_offset(&rt, &path.to_string_lossy()).await,
        None,
        "cursor must not advance when a later write in the same atomic unit fails"
    );
}
