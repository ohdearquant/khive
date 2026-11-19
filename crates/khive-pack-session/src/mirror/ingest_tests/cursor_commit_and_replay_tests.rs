/// Regression for the multi-candidate atomicity finding: the deferred
/// dispatch variant must NOT commit the cursor on an empty advance —
/// the commit is the dispatch loop's final step, vetoable until then.
#[tokio::test]
async fn deferred_empty_advance_leaves_cursor_uncommitted_until_commit_empty_advance() {
    let (rt, _dir) = setup().await;

    let mut file = NamedTempFile::new().expect("tmpfile");
    for _ in 0..16 {
        writeln!(file).unwrap(); // blank line: just "\n"
    }
    let path = file.path().to_path_buf();
    let file_len = std::fs::metadata(&path).unwrap().len();

    let stats = mirror_file_deferred(&rt, &path, 0, LineTailSource::ClaudeCode, None)
        .await
        .expect("deferred blank-line pass");
    assert_eq!(stats.inserted, 0);
    assert_eq!(
        stats.new_offset, file_len,
        "bytes were consumed off the file"
    );
    assert_eq!(
        cursor_offset(&rt, &path.to_string_lossy()).await,
        None,
        "the deferred pass must not commit the cursor itself"
    );

    // Dispatch ends without an inserting candidate: the loop commits.
    commit_empty_advance(&rt, &path, &stats)
        .await
        .expect("end-of-dispatch commit");
    assert_eq!(
        cursor_offset(&rt, &path.to_string_lossy()).await,
        Some(file_len as i64),
        "the commit lands exactly where the deferred pass consumed"
    );
}

/// A deferred pass that DOES parse events commits rows and cursor in one
/// transaction, exactly like the immediate variant.
#[tokio::test]
async fn deferred_pass_with_events_commits_rows_and_cursor_together() {
    let (rt, _dir) = setup().await;

    let mut file = NamedTempFile::new().expect("tmpfile");
    let session_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    for index in 0..3 {
        writeln!(
            file,
            "{}",
            user_line(&format!("u{index}"), session_id, "hello")
        )
        .unwrap();
    }
    let path = file.path().to_path_buf();
    let file_len = std::fs::metadata(&path).unwrap().len();

    let stats = mirror_file_deferred(&rt, &path, 0, LineTailSource::ClaudeCode, None)
        .await
        .expect("deferred event pass");
    assert_eq!(stats.inserted, 3);
    assert_eq!(stats.new_offset, file_len);
    assert_eq!(
        cursor_offset(&rt, &path.to_string_lossy()).await,
        Some(file_len as i64),
        "an inserting pass commits its cursor inside the same atomic unit"
    );
    assert_eq!(count_rows(&rt, "session_messages").await, 3);
}

#[tokio::test]
async fn test_partial_trailing_line_not_consumed() {
    let (rt, _dir) = setup().await;

    let line1 = user_line("uuid-p1", "sess-B", "Complete");
    // Write one complete line + a partial line without trailing '\n'.
    let partial = r#"{"uuid":"uuid-p2","sessionId":"sess-B","type":"user""#;

    let mut file = NamedTempFile::new().expect("tmpfile");
    writeln!(file, "{line1}").unwrap(); // complete line (has \n)
    write!(file, "{partial}").unwrap(); // partial — NO trailing \n

    let path = file.path().to_path_buf();
    let full_len = std::fs::metadata(&path).unwrap().len();

    let stats = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
        .await
        .expect("mirror_file partial");

    // Only the complete line should be consumed.
    assert_eq!(stats.inserted, 1, "only 1 complete line inserted");
    assert!(
        stats.new_offset < full_len,
        "new_offset {new} must be less than file_len {full_len}",
        new = stats.new_offset
    );

    // The partial bytes remain; calling again from new_offset finds no complete lines.
    let stats2 = mirror_file(
        &rt,
        &path,
        stats.new_offset,
        LineTailSource::ClaudeCode,
        None,
    )
    .await
    .expect("second call");
    assert_eq!(
        stats2.inserted, 0,
        "partial line must not be consumed on re-poll"
    );
    assert_eq!(
        stats2.new_offset, stats.new_offset,
        "offset must not advance on partial-only content"
    );
}

#[tokio::test]
async fn test_duplicate_uuid_across_two_calls() {
    let (rt, _dir) = setup().await;

    let line = user_line("uuid-dup", "sess-C", "First");

    let mut file = NamedTempFile::new().expect("tmpfile");
    writeln!(file, "{line}").unwrap();

    let path = file.path().to_path_buf();

    // First call inserts.
    let s1 = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
        .await
        .unwrap();
    assert_eq!(s1.inserted, 1);

    // Append same uuid again.
    writeln!(file, "{line}").unwrap();

    // Second call from offset 0 should see both lines but insert 0 new rows.
    let s2 = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
        .await
        .unwrap();
    assert_eq!(s2.inserted, 0, "duplicate uuid must not be re-inserted");
    assert_eq!(count_rows(&rt, "session_messages").await, 1);

    // Incremental: call from first call's new_offset; the second line is the dup.
    let s3 = mirror_file(&rt, &path, s1.new_offset, LineTailSource::ClaudeCode, None)
        .await
        .unwrap();
    assert_eq!(s3.inserted, 0, "incremental dup must also insert 0");
}

#[tokio::test]
async fn test_replay_does_not_mutate_session_metadata() {
    // Replay-idempotency regression (see docs guide): a pure replay must
    // not advance last_seen_at.
    let (rt, _dir) = setup().await;

    let line = user_line_no_ts("uuid-nts", "sess-NTS", "no timestamp here");
    let mut file = NamedTempFile::new().expect("tmpfile");
    writeln!(file, "{line}").unwrap();
    let path = file.path().to_path_buf();

    let s1 = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
        .await
        .unwrap();
    assert_eq!(s1.inserted, 1);
    let seen_after_first = last_seen_at(&rt, "sess-NTS")
        .await
        .expect("session row exists");

    // Replay from offset 0: re-scans the same line, inserts 0, and must
    // leave last_seen_at byte-identical even though now_us has advanced.
    let s2 = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
        .await
        .unwrap();
    assert_eq!(s2.inserted, 0, "replay must insert 0 rows");
    let seen_after_replay = last_seen_at(&rt, "sess-NTS").await.unwrap();
    assert_eq!(
        seen_after_first, seen_after_replay,
        "replay must not advance last_seen_at for a timestamp-missing event"
    );
}

#[tokio::test]
async fn test_empty_file_is_a_no_op() {
    let (rt, _dir) = setup().await;

    let file = NamedTempFile::new().expect("tmpfile");
    let path = file.path().to_path_buf();

    let stats = mirror_file(&rt, &path, 0, LineTailSource::ClaudeCode, None)
        .await
        .unwrap();
    assert_eq!(stats.inserted, 0);
    assert_eq!(stats.scanned, 0);
    assert_eq!(stats.new_offset, 0);
}

#[tokio::test]
async fn test_missing_file_returns_error() {
    let (rt, _dir) = setup().await;
    let bad_path = std::path::PathBuf::from("/nonexistent/path/session.jsonl");
    let result = mirror_file(&rt, &bad_path, 0, LineTailSource::ClaudeCode, None).await;
    assert!(
        matches!(result, Err(RuntimeError::Internal(_))),
        "missing file should return Internal error"
    );
}
