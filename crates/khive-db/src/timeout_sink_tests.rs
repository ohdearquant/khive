use super::*;
use std::thread;

#[test]
fn store_direct_route_sites_have_stable_names() {
    assert_eq!(Site::DirectRouteEntity.as_str(), "direct_route:entity");
    assert_eq!(Site::DirectRouteNote.as_str(), "direct_route:note");
    assert_eq!(
        Site::DirectRouteGraphGeneralWrite.as_str(),
        "direct_route:graph_general_write"
    );
    assert_eq!(
        Site::DirectRouteEventGeneralWrite.as_str(),
        "direct_route:event_general_write"
    );
    assert_eq!(
        Site::DirectRouteSparseGeneralWrite.as_str(),
        "direct_route:sparse_general_write"
    );
    assert_eq!(
        Site::DirectRouteAgentGeneralWrite.as_str(),
        "direct_route:agent_general_write"
    );
    assert_eq!(
        Site::DirectRouteRuntimeMergeEntity.as_str(),
        "direct_route:runtime_merge_entity"
    );
    assert_eq!(
        Site::DirectRouteRuntimeMergeNote.as_str(),
        "direct_route:runtime_merge_note"
    );
    assert_eq!(
        Site::DirectRouteRuntimeUpdateSymmetricEdge.as_str(),
        "direct_route:runtime_update_symmetric_edge"
    );
}

#[test]
fn resolve_log_dir_prefers_explicit_override() {
    if crate::test_process::run_in_child(|_| {}) {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    crate::test_process::set_var(SINK_DIR_OVERRIDE_ENV, dir.path());
    let resolved = resolve_log_dir(None);
    crate::test_process::remove_var(SINK_DIR_OVERRIDE_ENV);
    assert_eq!(resolved, Some(dir.path().to_path_buf()));
}

#[test]
fn resolve_log_dir_falls_back_to_db_parent_under_test_harness() {
    if crate::test_process::run_in_child(|command| {
        command.env_remove(SINK_DIR_OVERRIDE_ENV);
    }) {
        return;
    }
    // KHIVE_TEST_HARNESS=1 is force-set by .cargo/config.toml for every
    // cargo test/binary in this workspace — assert the behavior this
    // module relies on to keep tests off the real ~/.khive/logs.
    assert_eq!(std::env::var(TEST_HARNESS_ENV).as_deref(), Ok("1"));
    let parent = Path::new("/tmp/does-not-need-to-exist");
    let resolved = resolve_log_dir(Some(parent));
    assert_eq!(resolved, Some(parent.join(FALLBACK_LOG_SUBDIR)));
}

#[test]
fn resolve_log_dir_none_without_db_parent_under_test_harness() {
    if crate::test_process::run_in_child(|command| {
        command.env_remove(SINK_DIR_OVERRIDE_ENV);
    }) {
        return;
    }
    assert_eq!(resolve_log_dir(None), None);
}

#[test]
fn heartbeat_and_write_delay_keep_distinct_zero_defaults() {
    if crate::test_process::run_in_child(|command| {
        command
            .env_remove(HEARTBEAT_MS_OVERRIDE_ENV)
            .env_remove(WRITE_DELAY_MS_OVERRIDE_ENV);
    }) {
        return;
    }
    for (key, read, fallback) in [
        (
            HEARTBEAT_MS_OVERRIDE_ENV,
            heartbeat_interval_from_env as fn() -> Duration,
            Duration::from_secs(300),
        ),
        (
            WRITE_DELAY_MS_OVERRIDE_ENV,
            write_delay_from_env as fn() -> Duration,
            Duration::ZERO,
        ),
    ] {
        assert_eq!(read(), fallback);
        for value in ["0", "invalid", " 25 ", "18446744073709551616"] {
            crate::test_process::set_var(key, value);
            assert_eq!(read(), fallback, "{key}={value:?}");
        }
        crate::test_process::set_var(key, "25");
        assert_eq!(read(), Duration::from_millis(25));
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            crate::test_process::set_var(key, std::ffi::OsStr::from_bytes(b"\xff"));
            assert_eq!(read(), fallback);
        }
        crate::test_process::remove_var(key);
        assert_eq!(read(), fallback);
    }
}

#[test]
fn slow_write_threshold_env_override_and_zero_disable() {
    if crate::test_process::run_in_child(|_| {}) {
        return;
    }
    crate::test_process::remove_var(SLOW_WRITE_THRESHOLD_MS_OVERRIDE_ENV);
    assert_eq!(slow_write_threshold(), Some(SLOW_WRITE_THRESHOLD));

    crate::test_process::set_var(SLOW_WRITE_THRESHOLD_MS_OVERRIDE_ENV, "250");
    assert_eq!(slow_write_threshold(), Some(Duration::from_millis(250)));

    // 0 = disabled entirely, never "emit every write".
    crate::test_process::set_var(SLOW_WRITE_THRESHOLD_MS_OVERRIDE_ENV, "0");
    assert_eq!(slow_write_threshold(), None);

    // Unparseable falls back to the default, never to disabled — a typo
    // in the override must not silently turn the instrument off.
    crate::test_process::set_var(SLOW_WRITE_THRESHOLD_MS_OVERRIDE_ENV, "not-a-number");
    assert_eq!(slow_write_threshold(), Some(SLOW_WRITE_THRESHOLD));
    crate::test_process::remove_var(SLOW_WRITE_THRESHOLD_MS_OVERRIDE_ENV);
}

#[test]
fn slow_write_line_carries_elapsed_and_depth_and_omits_inapplicable_fields() {
    let stages = WriterStageFields {
        queue_wait_micros: 11,
        transaction_acquire_micros: 22,
        body_micros: 1_111_000,
        commit_micros: 44,
        total_micros: 1_234_000,
    };
    let line = build_line(
        now_rfc3339(),
        "slow_write",
        "test-db",
        None,
        None,
        None,
        None,
        Some(1234),
        Some(7),
        Some(stages),
        Some(SlowWriteLabels {
            labels: vec!["resolve_prefix".to_string()].into_boxed_slice(),
            labels_truncated: Some(2),
        }),
        None,
        None,
    );
    let parsed: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(parsed["kind"], "slow_write");
    assert_eq!(parsed["elapsed_ms"], 1234);
    assert_eq!(parsed["queue_depth"], 7);
    assert_eq!(parsed["queue_wait_micros"], 11);
    assert_eq!(parsed["transaction_acquire_micros"], 22);
    assert_eq!(parsed["body_micros"], 1_111_000);
    assert_eq!(parsed["commit_micros"], 44);
    assert_eq!(parsed["total_micros"], 1_234_000);
    // Inapplicable fields are omitted, not null (existing sink contract).
    assert!(parsed.get("site").is_none());
    assert!(parsed.get("error").is_none());
    assert!(parsed.get("timeout_ms").is_none());
    // A slow-write row names the labels the writer ran.
    assert_eq!(parsed["labels"], serde_json::json!(["resolve_prefix"]));
    assert_eq!(parsed["labels_truncated"], 2);
}

#[test]
fn truncate_error_leaves_short_strings_untouched() {
    let short = "database is locked";
    assert_eq!(truncate_error(short, MAX_ERROR_BYTES), short);
}

#[test]
fn truncate_error_bounds_long_strings() {
    let long = "x".repeat(MAX_ERROR_BYTES * 4);
    let truncated = truncate_error(&long, MAX_ERROR_BYTES);
    assert!(
        truncated.len() <= MAX_ERROR_BYTES + "...(truncated)".len(),
        "truncated length {} exceeds bound",
        truncated.len()
    );
    assert!(truncated.ends_with("...(truncated)"));
}

#[test]
fn truncate_error_respects_utf8_char_boundaries() {
    // A multi-byte character straddling the byte cutoff must not panic
    // and must produce valid UTF-8.
    let s = "a".repeat(MAX_ERROR_BYTES - 1) + "€€€€";
    let truncated = truncate_error(&s, MAX_ERROR_BYTES);
    assert!(truncated.is_char_boundary(truncated.len() - "...(truncated)".len()));
}

/// `enqueue` (the whole of what a caller-path emission does) never
/// blocks on a full channel — it drops the event and counts it. No
/// `SINK`/`OnceLock`/thread involved, so this is deterministic.
#[test]
fn enqueue_drops_and_counts_on_full_channel() {
    let (sender, _receiver) = mpsc::sync_channel::<QueuedEvent>(1);
    let dropped = AtomicU64::new(0);
    let make_event = || QueuedEvent {
        ts_utc: now_rfc3339(),
        kind: "timeout",
        db: "test-db".to_string(),
        site: Some(Site::PoolAdmission.as_str()),
        phase: None,
        error: Some("boom".to_string()),
        timeout_ms: Some(5),
        elapsed_ms: None,
        queue_depth: None,
        writer_stages: None,
        labels: None,
    };

    enqueue(&sender, &dropped, make_event());
    assert_eq!(dropped.load(Ordering::Relaxed), 0);

    // Channel capacity is 1 and nothing has drained it yet, so this
    // second enqueue must be dropped rather than block.
    enqueue(&sender, &dropped, make_event());
    assert_eq!(dropped.load(Ordering::Relaxed), 1);
}

/// A short write (partial acceptance) is TERMINAL for the sink instance
/// — no later line may land after an unterminated fragment. Uses an
/// in-memory mock `Write` so the failure mode is exact and
/// reproducible, with no reliance on OS-level file-descriptor tricks.
struct ShortWriteOnceThenOk {
    calls: usize,
    written: Vec<u8>,
}

impl Write for ShortWriteOnceThenOk {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.calls += 1;
        if self.calls == 1 && buf.len() > 1 {
            // Accept only part of the buffer once.
            let n = buf.len() - 1;
            self.written.extend_from_slice(&buf[..n]);
            Ok(n)
        } else {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn append_sink_short_write_poisons_permanently() {
    let mut sink = AppendSink::<ShortWriteOnceThenOk>::new();
    sink.set_target(ShortWriteOnceThenOk {
        calls: 0,
        written: Vec::new(),
    });

    assert!(
        !sink.write_line("first line\n"),
        "a short write must be reported as failed"
    );
    assert!(sink.is_poisoned());
    assert!(!sink.is_open(), "the target must be dropped on poisoning");

    // Even a fresh, healthy target must never be accepted again.
    sink.set_target(ShortWriteOnceThenOk {
        calls: 0,
        written: Vec::new(),
    });
    assert!(
        !sink.is_open(),
        "set_target must refuse to arm a poisoned sink"
    );
    assert!(
        !sink.write_line("second line\n"),
        "a poisoned sink must never write again"
    );
}

/// Distinguishes "hard I/O error" (recoverable — the target is
/// dropped, but a later `set_target` heals it) from "short write"
/// (terminal — see the test above).
/// Shares a "has failed once, globally" flag across separately
/// constructed instances — mirrors the real scenario: the writer
/// thread's *first* target (this drain's `File`) fails, gets dropped,
/// and a *later, distinct* target (the next `ensure_open`'s freshly
/// reopened `File`) succeeds. Resetting a per-instance counter instead
/// would make the second instance fail too, which is not what "retry
/// heals" means.
struct ErrorOnceThenOk {
    already_failed: Arc<std::sync::atomic::AtomicBool>,
    written: Vec<u8>,
}

impl Write for ErrorOnceThenOk {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if !self.already_failed.swap(true, Ordering::Relaxed) {
            return Err(std::io::Error::other("injected failure"));
        }
        self.written.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn append_sink_hard_error_is_recoverable_not_poisoned() {
    let already_failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut sink = AppendSink::<ErrorOnceThenOk>::new();
    sink.set_target(ErrorOnceThenOk {
        already_failed: Arc::clone(&already_failed),
        written: Vec::new(),
    });

    assert!(!sink.write_line("first line\n"));
    assert!(
        !sink.is_poisoned(),
        "an ordinary write error must not be terminal"
    );
    assert!(!sink.is_open(), "the failed target is dropped");

    // A later retry (mirroring the writer thread's next `ensure_open`
    // reopening a fresh `File`) must be able to arm and write
    // successfully.
    sink.set_target(ErrorOnceThenOk {
        already_failed: Arc::clone(&already_failed),
        written: Vec::new(),
    });
    assert!(sink.write_line("second line\n"));
}

/// A temporarily unwritable log directory must heal once it becomes
/// writable — a permanent silent disable after one failed open is not
/// acceptable behavior.
#[test]
fn append_sink_recovers_after_transient_directory_failure() {
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("blocker");
    std::fs::write(&blocker, b"not a directory").unwrap();
    let bogus_dir = blocker.join("logs");

    let mut sink = AppendSink::<File>::new();
    sink.ensure_open(&bogus_dir);
    assert!(!sink.is_open(), "open against a blocked path must fail");
    assert!(!sink.is_poisoned(), "a failed open must not be terminal");

    // "Fix" the directory: remove the blocking file, so the same path
    // (now backed by a real directory) can be created and opened.
    std::fs::remove_file(&blocker).unwrap();
    sink.ensure_open(&bogus_dir);
    assert!(
        sink.is_open(),
        "a later ensure_open against a now-writable directory must succeed"
    );
    assert!(sink.write_line("recovered\n"));

    let contents = std::fs::read_to_string(bogus_dir.join(ndjson_file_name())).unwrap();
    assert!(contents.contains("recovered"));
}

/// Coverage-contract concurrency test: many threads enqueuing
/// simultaneously must never panic or corrupt the channel; every
/// accepted event, once drained by a single-threaded writer, produces
/// an intact JSON line — no interleaving is even structurally possible
/// with one writer thread, but this proves the producer side is sound
/// under real concurrent load.
#[test]
fn concurrent_enqueue_then_single_threaded_drain_produces_intact_lines() {
    let (sender, receiver) = mpsc::sync_channel::<QueuedEvent>(QUEUE_CAPACITY);
    let dropped = Arc::new(AtomicU64::new(0));

    const THREADS: usize = 64;
    let handles: Vec<_> = (0..THREADS)
        .map(|i| {
            let sender = sender.clone();
            let dropped = Arc::clone(&dropped);
            thread::spawn(move || {
                let event = QueuedEvent {
                    ts_utc: now_rfc3339(),
                    kind: "timeout",
                    db: "concurrent-test".to_string(),
                    site: Some(Site::StandaloneGraph.as_str()),
                    phase: None,
                    error: Some(format!("contention-{i}")),
                    timeout_ms: Some(i as u64),
                    elapsed_ms: None,
                    queue_depth: None,
                    writer_stages: None,
                    labels: None,
                };
                enqueue(&sender, &dropped, event);
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    drop(sender);

    let mut sink = AppendSink::<Vec<u8>>::new();
    sink.set_target(Vec::new());
    let mut written_lines = 0usize;
    while let Ok(event) = receiver.try_recv() {
        let line = build_line(
            event.ts_utc,
            event.kind,
            &event.db,
            event.site,
            event.phase,
            event.error.as_deref(),
            event.timeout_ms,
            event.elapsed_ms,
            event.queue_depth,
            event.writer_stages,
            event.labels,
            None,
            None,
        );
        if sink.write_line(&line) {
            written_lines += 1;
        }
    }

    assert_eq!(
        written_lines + dropped.load(Ordering::Relaxed) as usize,
        THREADS,
        "every enqueue must be either written or counted as dropped"
    );
    assert_eq!(
        dropped.load(Ordering::Relaxed),
        0,
        "queue capacity exceeds THREADS"
    );

    let bytes = sink.into_target().expect("target must still be armed");
    let contents = String::from_utf8(bytes).unwrap();
    let lines: Vec<&str> = contents.lines().collect();
    assert_eq!(lines.len(), THREADS);
    for line in &lines {
        let parsed: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("corrupt line {line:?}: {e}"));
        assert_eq!(parsed["kind"], "timeout");
        assert_eq!(parsed["site"], "standalone:graph");
    }
}

#[test]
fn is_busy_or_locked_classifies_sqlite_error_codes() {
    let busy = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
        Some("database is locked".to_string()),
    );
    assert!(is_busy_or_locked(&busy));

    let locked = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_LOCKED),
        Some("table is locked".to_string()),
    );
    assert!(is_busy_or_locked(&locked));

    let other = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CONSTRAINT),
        Some("constraint failed".to_string()),
    );
    assert!(!is_busy_or_locked(&other));

    assert!(!is_busy_or_locked(&rusqlite::Error::QueryReturnedNoRows));
}

#[test]
fn sqlite_full_escalation_has_its_own_event_kind() {
    let full = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL),
        Some("disk I/O write stopped".to_string()),
    );
    let wrapped = khive_storage::StorageError::driver(
        khive_storage::StorageCapability::Sql,
        "test_write",
        full,
    );
    assert!(is_sqlite_full(&wrapped));
    let event = sqlite_full_event("test.db", &wrapped);
    assert_eq!(event.kind, "sqlite_full");
    assert_eq!(event.site, None);
    assert_eq!(event.timeout_ms, None);

    let busy =
        rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY), None);
    assert!(!is_sqlite_full(&busy));
    assert!(is_busy_or_locked(&busy));
}
