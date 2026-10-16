use super::{
    delete_cursors, drain_pending_cursor_deletes, finalize_dispatch_stats, load_cursors,
    queue_cursor_deletes, reconcile_cursor_identity, tally_dispatch_errors, CandidateDispatch,
    CursorState, DiscoveredKind, DiscoveryIndex, CURSOR_DELETE_RETRY_LIMIT,
    FILE_ERROR_POLLS_BEFORE_COLD,
};
use crate::mirror::ingest::{file_identity, mirror_file, LineTailSource, MirrorStats};
use crate::vocab::{SESSION_SCHEMA_COLUMN_ADDITIONS, SESSION_SCHEMA_PLAN_STMTS};
use khive_runtime::{
    AllowAllGate, BackendId, KhiveRuntime, Namespace, RuntimeConfig, RuntimeError,
};
use khive_storage::types::{SqlStatement, SqlValue};
use std::collections::HashMap;
use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::{NamedTempFile, TempDir};

/// File-backed runtime WITHOUT the session schema applied — cursor DML
/// fails with "no such table", which doubles as the fault injection for
/// the retry path. Caller keeps the `TempDir` alive.
fn runtime_without_schema() -> (KhiveRuntime, TempDir) {
    let dir = TempDir::new().expect("tempdir");
    let db_path = dir.path().join("test.db");
    let rt = KhiveRuntime::new(RuntimeConfig {
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
        db_path: Some(db_path),
        blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
        exec: Default::default(),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .expect("file-backed runtime");
    (rt, dir)
}

async fn apply_session_schema(rt: &KhiveRuntime) {
    let sql = rt.sql();
    let mut writer = sql.writer().await.expect("writer");
    for stmt in &SESSION_SCHEMA_PLAN_STMTS {
        writer
            .execute_script(stmt.to_string())
            .await
            .expect("schema stmt");
    }
}

async fn insert_cursor_row(rt: &KhiveRuntime, path: &std::path::Path, offset: i64) {
    let mut writer = rt.sql().writer().await.expect("writer");
    writer
        .execute(SqlStatement {
            sql:
                "INSERT INTO session_mirror_cursor(file_path, session_id, byte_offset, updated_at) \
                      VALUES(?1, NULL, ?2, 0)"
                    .into(),
            params: vec![
                SqlValue::Text(path.to_string_lossy().into_owned()),
                SqlValue::Integer(offset),
            ],
            label: None,
        })
        .await
        .expect("cursor insert");
}

async fn cursor_row_exists(rt: &KhiveRuntime, path: &std::path::Path) -> bool {
    let mut reader = rt.sql().reader().await.expect("reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT byte_offset FROM session_mirror_cursor WHERE file_path = ?1".into(),
            params: vec![SqlValue::Text(path.to_string_lossy().into_owned())],
            label: None,
        })
        .await
        .expect("cursor query");
    !rows.is_empty()
}

async fn cursor_row_offset(rt: &KhiveRuntime, path: &std::path::Path) -> Option<i64> {
    let mut reader = rt.sql().reader().await.expect("reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT byte_offset FROM session_mirror_cursor WHERE file_path = ?1".into(),
            params: vec![SqlValue::Text(path.to_string_lossy().into_owned())],
            label: None,
        })
        .await
        .expect("cursor query");
    rows.first().and_then(|row| match row.get("byte_offset") {
        Some(SqlValue::Integer(n)) => Some(*n),
        _ => None,
    })
}

#[test]
fn cursor_preserves_unchanged_and_append_offsets() {
    let mut cursor = CursorState {
        byte_offset: 100,
        file_identity: Some("old-file".into()),
    };
    assert!(!cursor.reset_if_replaced("old-file", 100, true));
    assert!(!cursor.reset_if_replaced("old-file", 150, true));
    assert_eq!(cursor.byte_offset, 100, "an append keeps its offset");
}

#[test]
fn cursor_truncation_rewinds_from_previous_offset() {
    let mut cursor = CursorState {
        byte_offset: 100,
        file_identity: Some("old-file".into()),
    };
    assert!(cursor.reset_if_replaced("old-file", 50, true));
    assert_eq!(cursor.byte_offset, 0, "truncation restarts from zero");
}

#[test]
fn cursor_identity_change_rewinds_without_truncation() {
    for backfill in [true, false] {
        let mut cursor = CursorState {
            byte_offset: 100,
            file_identity: Some("old-file".into()),
        };
        assert!(cursor.reset_if_replaced("new-file", 150, backfill));
        assert_eq!(
            cursor.byte_offset, 0,
            "a replacement at least as long as the offset restarts from zero"
        );
        assert_eq!(cursor.file_identity.as_deref(), Some("new-file"));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn same_size_atomic_replacement_is_reingested_after_cursor_reload() {
    let (rt, dir) = runtime_without_schema();
    apply_session_schema(&rt).await;
    let path = dir.path().join("transcript.jsonl");
    let original = concat!(
        r#"{"uuid":"event-a","sessionId":"sess-replace","type":"user","timestamp":"2026-08-05T10:00:00Z","message":{"role":"user","content":"first"}}"#,
        "\n"
    );
    let replacement = original.replace("event-a", "event-b");
    assert_eq!(original.len(), replacement.len());
    std::fs::write(&path, original).expect("original transcript");
    let first = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
        .await
        .expect("first ingest");
    assert_eq!(first.inserted, 1);

    let mut cursor = load_cursors(&rt)
        .await
        .expect("reload cursor")
        .remove(&path)
        .expect("stored cursor");
    assert_eq!(cursor.byte_offset, original.len() as u64);
    assert_eq!(
        cursor.file_identity.as_deref(),
        Some(
            file_identity(&std::fs::File::open(&path).expect("open original"))
                .expect("original identity")
                .as_str()
        )
    );
    assert!(!cursor.reset_if_replaced(
        &file_identity(&std::fs::File::open(&path).expect("open unchanged"))
            .expect("unchanged identity"),
        original.len() as u64,
        true,
    ));

    let staged = dir.path().join("replacement.jsonl");
    std::fs::write(&staged, replacement).expect("replacement transcript");
    std::fs::rename(&staged, &path).expect("atomic replacement");
    let new_identity = file_identity(&std::fs::File::open(&path).expect("open replacement"))
        .expect("replacement identity");
    assert!(cursor.reset_if_replaced(&new_identity, original.len() as u64, true));
    assert_eq!(cursor.byte_offset, 0);
    let second = mirror_file(
        &rt,
        &path,
        cursor.byte_offset,
        LineTailSource::ClaudeCode,
        None,
    )
    .await
    .expect("replacement ingest");
    assert_eq!(second.inserted, 1);
    let saved = load_cursors(&rt)
        .await
        .expect("reload replacement cursor")
        .remove(&path)
        .expect("replacement cursor");
    assert_eq!(saved.byte_offset, original.len() as u64);
    assert_eq!(saved.file_identity.as_deref(), Some(new_identity.as_str()));
}

#[tokio::test]
async fn legacy_cursor_schema_adds_identity_without_losing_offset() {
    let (rt, _dir) = runtime_without_schema();
    let mut writer = rt.sql().writer().await.expect("writer");
    writer
            .execute_script(
                "CREATE TABLE session_mirror_cursor (file_path TEXT PRIMARY KEY, session_id TEXT, byte_offset INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL)"
                    .to_string(),
            )
            .await
            .expect("legacy cursor table");
    drop(writer);
    let path = PathBuf::from("/projects/legacy.jsonl");
    insert_cursor_row(&rt, &path, 123).await;

    rt.backend()
        .apply_pack_ddl_statements_with_columns(
            &SESSION_SCHEMA_PLAN_STMTS,
            &SESSION_SCHEMA_COLUMN_ADDITIONS,
        )
        .expect("pack column upgrade");
    let mut cursor = load_cursors(&rt)
        .await
        .expect("load upgraded cursor")
        .remove(&path)
        .expect("legacy row retained");
    assert_eq!(cursor.byte_offset, 123);
    assert_eq!(cursor.file_identity, None);
    assert!(cursor.reset_if_replaced("current-file", 123, true));
    assert_eq!(cursor.byte_offset, 0, "legacy row must replay once");
}

#[tokio::test]
async fn legacy_cursor_without_backfill_keeps_skipped_prefix_and_persists_identity() {
    let (rt, dir) = runtime_without_schema();
    let mut writer = rt.sql().writer().await.expect("writer");
    writer
            .execute_script(
                "CREATE TABLE session_mirror_cursor (file_path TEXT PRIMARY KEY, session_id TEXT, byte_offset INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL)"
                    .to_string(),
            )
            .await
            .expect("legacy cursor table");
    drop(writer);

    let path = dir.path().join("legacy.jsonl");
    let skipped = concat!(
        r#"{"uuid":"event-before-opt-out","sessionId":"sess-legacy","type":"user","timestamp":"2026-08-05T10:00:00Z","message":{"role":"user","content":"skipped"}}"#,
        "\n"
    );
    let appended = concat!(
        r#"{"uuid":"event-after-opt-out","sessionId":"sess-legacy","type":"user","timestamp":"2026-08-05T10:01:00Z","message":{"role":"user","content":"mirrored"}}"#,
        "\n"
    );
    std::fs::write(&path, format!("{skipped}{appended}")).expect("transcript");
    insert_cursor_row(&rt, &path, skipped.len() as i64).await;
    rt.backend()
        .apply_pack_ddl_statements_with_columns(
            &SESSION_SCHEMA_PLAN_STMTS,
            &SESSION_SCHEMA_COLUMN_ADDITIONS,
        )
        .expect("pack column upgrade");

    let mut cursor = load_cursors(&rt)
        .await
        .expect("load upgraded cursor")
        .remove(&path)
        .expect("legacy row retained");
    let file_len = std::fs::metadata(&path).expect("transcript metadata").len();
    let identity = file_identity(&std::fs::File::open(&path).expect("open transcript"))
        .expect("transcript identity");
    assert!(
        !reconcile_cursor_identity(&rt, &path, &mut cursor, &identity, file_len, false)
            .await
            .expect("adopt legacy identity")
    );
    assert_eq!(cursor.byte_offset, skipped.len() as u64);
    assert_eq!(cursor.file_identity.as_deref(), Some(identity.as_str()));

    let saved = load_cursors(&rt)
        .await
        .expect("reload adopted cursor")
        .remove(&path)
        .expect("adopted row");
    assert_eq!(saved, cursor);
    let mirrored = mirror_file(
        &rt,
        &path,
        cursor.byte_offset,
        LineTailSource::ClaudeCode,
        None,
    )
    .await
    .expect("tail after skipped prefix");
    assert_eq!(mirrored.inserted, 1, "only the appended event is imported");

    let mut truncated_legacy = CursorState {
        byte_offset: skipped.len() as u64,
        file_identity: None,
    };
    assert!(truncated_legacy.reset_if_replaced(&identity, 1, false));
    assert_eq!(truncated_legacy.byte_offset, 0);
}

#[tokio::test]
async fn failed_cursor_delete_stays_pending_and_is_retried_next_tick() {
    let (rt, _dir) = runtime_without_schema();
    let discovery = DiscoveryIndex::default();
    let path = PathBuf::from("/projects/gone.jsonl");

    let mut pending = VecDeque::new();
    queue_cursor_deletes(&mut pending, std::slice::from_ref(&path));
    assert_eq!(pending.len(), 1);
    let mut offsets = HashMap::new();

    // No cursor table yet: the delete fails and the entry must stay
    // pending — dropping it would let a stale row survive a daemon
    // restart and skip bytes on a recreated file.
    drain_pending_cursor_deletes(&rt, &discovery, &mut offsets, &mut pending).await;
    assert_eq!(
        pending.len(),
        1,
        "failed delete must remain in the retry set"
    );

    // The table appears (schema applied) and a row exists for the path:
    // the next tick's retry deletes it and drains the set.
    apply_session_schema(&rt).await;
    insert_cursor_row(&rt, &path, 4096).await;
    drain_pending_cursor_deletes(&rt, &discovery, &mut offsets, &mut pending).await;
    assert!(pending.is_empty(), "successful retry drains the set");
    assert!(
        !cursor_row_exists(&rt, &path).await,
        "stale row deleted on retry"
    );
}

#[tokio::test]
async fn retracked_path_cancels_its_pending_cursor_delete() {
    let (rt, _dir) = runtime_without_schema();
    apply_session_schema(&rt).await;

    let path = PathBuf::from("/projects/recreated.jsonl");
    insert_cursor_row(&rt, &path, 512).await;

    let mut discovery = DiscoveryIndex::default();
    let mut pending = VecDeque::new();
    queue_cursor_deletes(&mut pending, std::slice::from_ref(&path));
    let mut offsets = HashMap::new();

    // The file is re-discovered before the delete drains: its fresh
    // cursor row must not be deleted out from under it.
    discovery.add_file(path.clone(), DiscoveredKind::ChatGptExport, false);
    drain_pending_cursor_deletes(&rt, &discovery, &mut offsets, &mut pending).await;
    assert!(pending.is_empty(), "re-tracked path cancels its delete");
    assert!(cursor_row_exists(&rt, &path).await, "fresh row preserved");
    assert_eq!(
        offsets.get(&path).map(|cursor| cursor.byte_offset),
        Some(512),
        "canceling the delete restores the in-memory offset from the preserved row"
    );
}

/// Item 3 regression: remove + re-add in the same pass. The removal
/// drops the in-memory offset and queues the cursor delete; the re-add
/// cancels the delete. The cancel must restore the offset from the
/// preserved cursor row — otherwise the next seed falls back to
/// `file_len` (`backfill=false`) and skips bytes the preserved row
/// proves were already mirrored.
#[tokio::test]
async fn same_pass_remove_and_readd_restores_offset_from_preserved_cursor_row() {
    let (rt, _dir) = runtime_without_schema();
    apply_session_schema(&rt).await;

    let path = PathBuf::from("/projects/flap.jsonl");
    insert_cursor_row(&rt, &path, 4096).await;

    let mut discovery = DiscoveryIndex::default();
    discovery.add_file(path.clone(), DiscoveredKind::ChatGptExport, false);

    // Same-pass removal: discovery reports the file gone, the service
    // loop drops the in-memory offset and queues the cursor delete.
    discovery.remove_file(&path, false);
    let removed = discovery.take_removed_files();
    assert_eq!(removed, vec![path.clone()]);
    let mut offsets = HashMap::from([(
        path.clone(),
        CursorState {
            byte_offset: 4096,
            file_identity: None,
        },
    )]);
    for removed_path in &removed {
        offsets.remove(removed_path);
    }
    let mut pending = VecDeque::new();
    queue_cursor_deletes(&mut pending, &removed);
    assert!(offsets.is_empty(), "removal dropped the in-memory offset");

    // Same-pass re-discovery: the file is back before the delete drains.
    discovery.add_file(path.clone(), DiscoveredKind::ChatGptExport, false);
    drain_pending_cursor_deletes(&rt, &discovery, &mut offsets, &mut pending).await;

    assert!(
        pending.is_empty(),
        "re-added path cancels its pending delete"
    );
    assert!(
        cursor_row_exists(&rt, &path).await,
        "the preserved cursor row survives the cancel"
    );
    assert_eq!(
        offsets.get(&path).map(|cursor| cursor.byte_offset),
        Some(4096),
        "the cancel restores the in-memory offset from the preserved row, \
             so backfill=false seeding never falls back to EOF and skips bytes"
    );
}

#[tokio::test]
async fn failed_cancelled_cursor_restore_blocks_eof_seed_until_retry() {
    let (rt, _dir) = runtime_without_schema();
    let mut file = NamedTempFile::new().expect("tmpfile");
    let prefix = b"already mirrored\n";
    let new_line = br#"{"uuid":"uuid-restored","sessionId":"sess-restored","type":"user","timestamp":"2026-08-05T10:00:00Z","message":{"role":"user","content":"restored"}}"#;
    file.write_all(prefix).expect("prefix");
    file.write_all(new_line).expect("new line");
    file.write_all(b"\n").expect("line terminator");
    let path = file.path().to_path_buf();
    let true_offset = prefix.len() as u64;
    let file_len = std::fs::metadata(&path).expect("file metadata").len();

    let mut discovery = DiscoveryIndex::default();
    discovery.add_file(path.clone(), DiscoveredKind::ChatGptExport, false);
    let gone = path.with_file_name("gone.jsonl");
    let mut pending = VecDeque::new();
    queue_cursor_deletes(&mut pending, &[path.clone(), gone.clone()]);
    let mut offsets = HashMap::new();

    // The missing schema injects a cursor-read failure. The cancellation
    // remains pending and the tracked path is explicitly blocked from the
    // scheduling pass, so backfill=false cannot seed it at EOF.
    let blocked = drain_pending_cursor_deletes(&rt, &discovery, &mut offsets, &mut pending).await;
    assert!(blocked.contains(path.as_path()));
    assert!(pending.contains(&path));
    assert!(
        pending.contains(&gone),
        "an unrelated failed delete remains pending"
    );
    let scheduled = discovery
        .schedule_files()
        .into_iter()
        .filter(|file| !blocked.contains(file.path.as_path()))
        .collect::<Vec<_>>();
    assert!(scheduled.is_empty(), "failed restore must block this tick");
    assert!(
        !offsets.contains_key(&path),
        "failed restore must not insert an EOF seed before retry"
    );

    // Once the schema and preserved row are available, the next drain
    // restores the true offset and removes the pending cancellation.
    apply_session_schema(&rt).await;
    insert_cursor_row(&rt, &path, true_offset as i64).await;
    let blocked = drain_pending_cursor_deletes(&rt, &discovery, &mut offsets, &mut pending).await;
    assert!(blocked.is_empty());
    assert!(pending.is_empty());
    assert_eq!(
        offsets.get(&path).map(|cursor| cursor.byte_offset),
        Some(true_offset)
    );

    // The restored offset points at the new line, proving the retry does
    // not skip bytes that followed the already mirrored prefix.
    let restored_offset = offsets
        .entry(path.clone())
        .or_insert(CursorState {
            byte_offset: file_len,
            file_identity: None,
        })
        .byte_offset;
    assert_eq!(restored_offset, true_offset);
    let stats = mirror_file(
        &rt,
        &path,
        restored_offset,
        LineTailSource::ClaudeCode,
        None,
    )
    .await
    .expect("mirror bytes after restored cursor");
    assert_eq!(
        stats.inserted, 1,
        "bytes after the preserved cursor are mirrored"
    );
}

#[tokio::test]
async fn failed_empty_advance_commit_counts_as_error_for_cold_demotion() {
    let (rt, _dir) = runtime_without_schema();
    let path = PathBuf::from("/projects/blank.jsonl");
    let mut discovery = DiscoveryIndex::default();
    discovery.add_file(path.clone(), DiscoveredKind::ChatGptExport, false);

    let (stats, had_errors) = finalize_dispatch_stats(
        &rt,
        &path,
        0,
        false,
        Some(MirrorStats {
            inserted: 0,
            scanned: 0,
            new_offset: 64,
            ..Default::default()
        }),
        false,
    )
    .await;
    assert!(stats.is_none(), "failed commit must hold back the offset");
    assert!(had_errors, "failed commit must become an error poll");

    for _ in 1..FILE_ERROR_POLLS_BEFORE_COLD {
        assert!(!tally_dispatch_errors(
            &mut discovery,
            &path,
            false,
            had_errors
        ));
    }
    assert!(tally_dispatch_errors(
        &mut discovery,
        &path,
        false,
        had_errors
    ));
    assert!(discovery.files[&path].cold);
}

/// ADR-080 regression: under overlapping-root dispatch, one candidate
/// reports a genuine skipped-oversized-line advance while a sibling
/// candidate errors on the same span. The skip is source-independent —
/// the line was too large for any parser before one ever ran — so it
/// must commit despite the sibling's error, instead of being discarded
/// and re-read (bounded but forever) on every later poll.
#[tokio::test]
async fn oversized_skip_advance_commits_despite_a_sibling_candidate_error() {
    let (rt, _dir) = runtime_without_schema();
    apply_session_schema(&rt).await;
    let path = PathBuf::from("/projects/oversized.jsonl");
    let start_offset = 0u64;

    let mut dispatch = CandidateDispatch::default();

    // Candidate A: the oversized-unterminated-line skip. Bytes consumed,
    // nothing inserted, but source-independent.
    assert!(!dispatch.record(
        Ok(MirrorStats {
            inserted: 0,
            scanned: 0,
            new_offset: 4096,
            skipped_oversized_bytes: true,
            replay_mismatches: 0,
            file_identity: None,
        }),
        start_offset,
    ));
    // Candidate B: an unrelated provider candidate errors on this pass.
    assert!(!dispatch.record(Err(RuntimeError::Internal("boom".into())), start_offset,));

    let CandidateDispatch { stats, errors } = dispatch;
    let had_errors = !errors.is_empty();
    assert!(had_errors, "the sibling error must be recorded");

    let (stats, had_errors) =
        finalize_dispatch_stats(&rt, &path, start_offset, false, stats, had_errors).await;

    let stats = stats.expect(
        "a skipped-oversized-line advance must commit even when a sibling \
             candidate errored on the same span",
    );
    assert_eq!(stats.new_offset, 4096);
    assert!(
        had_errors,
        "the sibling's error is still reported for cold-demotion accounting"
    );
    assert_eq!(
        cursor_row_offset(&rt, &path).await,
        Some(4096),
        "the cursor for the oversized file must have durably advanced, so the \
             next pass starts past the discarded prefix instead of re-reading it"
    );
}

/// A pass that skipped an oversized line but also scanned an ordinary line
/// is not source-independent: the ordinary line is exactly what the
/// erroring sibling might have parsed, so the veto still holds and the
/// bounded span is re-read on a later pass.
#[tokio::test]
async fn mixed_oversized_skip_and_scanned_line_keeps_the_error_veto() {
    let (rt, _dir) = runtime_without_schema();
    apply_session_schema(&rt).await;
    let path = PathBuf::from("/projects/mixed.jsonl");
    let start_offset = 0u64;

    let mut dispatch = CandidateDispatch::default();
    assert!(!dispatch.record(
        Ok(MirrorStats {
            inserted: 0,
            scanned: 1,
            new_offset: 4200,
            skipped_oversized_bytes: true,
            replay_mismatches: 0,
            file_identity: None,
        }),
        start_offset,
    ));
    assert!(!dispatch.record(Err(RuntimeError::Internal("boom".into())), start_offset,));

    let CandidateDispatch { stats, errors } = dispatch;
    let (stats, had_errors) =
        finalize_dispatch_stats(&rt, &path, start_offset, false, stats, !errors.is_empty()).await;

    assert!(
        stats.is_none(),
        "an advance that also scanned an ordinary line stays vetoed while a sibling errored"
    );
    assert!(had_errors);
    assert_eq!(
        cursor_row_offset(&rt, &path).await,
        None,
        "no cursor row may be written for a contested span"
    );
}

#[tokio::test]
async fn queue_cursor_deletes_deduplicates_and_is_bounded() {
    let mut pending = VecDeque::new();
    let path = PathBuf::from("/projects/gone.jsonl");
    queue_cursor_deletes(&mut pending, &[path.clone(), path.clone()]);
    assert_eq!(pending.len(), 1, "duplicate removals collapse to one entry");

    for index in 0..(CURSOR_DELETE_RETRY_LIMIT + 8) {
        let extra = PathBuf::from(format!("/projects/extra-{index}.jsonl"));
        queue_cursor_deletes(&mut pending, &[extra]);
    }
    assert_eq!(
        pending.len(),
        CURSOR_DELETE_RETRY_LIMIT,
        "the retry set is bounded; oldest entries are evicted"
    );
    assert!(
        !pending.contains(&path),
        "the oldest entry is the one evicted under the bound"
    );
}

#[tokio::test]
async fn delete_cursors_reports_per_path_failures_without_aborting_the_batch() {
    let (rt, _dir) = runtime_without_schema();
    let path = PathBuf::from("/projects/gone.jsonl");
    let failed = delete_cursors(&rt, std::slice::from_ref(&path))
        .await
        .expect("writer acquisition succeeds even when the table is missing");
    assert_eq!(
        failed,
        vec![path],
        "the failing path is reported for retry instead of aborting the batch"
    );
}

/// Item 7 regression: a failing DELETE must not block later paths in the
/// batch (head-of-line blocking). With the cursor table missing, every
/// path fails — and every path is still attempted and reported, rather
/// than the batch aborting at the first failure.
#[tokio::test]
async fn head_of_line_cursor_delete_failure_does_not_block_the_batch() {
    let (rt, _dir) = runtime_without_schema();
    let first = PathBuf::from("/projects/first.jsonl");
    let second = PathBuf::from("/projects/second.jsonl");
    let third = PathBuf::from("/projects/third.jsonl");
    let failed = delete_cursors(&rt, &[first.clone(), second.clone(), third.clone()])
        .await
        .expect("writer acquisition succeeds even when the table is missing");
    assert_eq!(
        failed,
        vec![first, second, third],
        "every path is attempted past the first failure; the failed paths \
             are retained for retry"
    );
}

/// The happy path still drains the whole batch and reports nothing.
#[tokio::test]
async fn delete_cursors_drains_the_batch_when_every_delete_succeeds() {
    let (rt, _dir) = runtime_without_schema();
    apply_session_schema(&rt).await;
    let first = PathBuf::from("/projects/a.jsonl");
    let second = PathBuf::from("/projects/b.jsonl");
    insert_cursor_row(&rt, &first, 10).await;
    insert_cursor_row(&rt, &second, 20).await;

    let failed = delete_cursors(&rt, &[first.clone(), second.clone()])
        .await
        .expect("delete batch");
    assert!(failed.is_empty(), "no per-path failures on the happy path");
    assert!(!cursor_row_exists(&rt, &first).await);
    assert!(!cursor_row_exists(&rt, &second).await);
}
