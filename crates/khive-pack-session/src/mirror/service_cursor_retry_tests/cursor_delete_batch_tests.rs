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
