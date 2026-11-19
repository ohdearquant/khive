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
