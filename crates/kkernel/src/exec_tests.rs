use super::*;
use clap::Parser;
use serial_test::serial;
use tempfile::NamedTempFile;
use uuid::Uuid;

include!("exec_tests/failure_reporting_tests.rs");

include!("exec_tests/environment_fixture_tests.rs");

// ── acquire_local_construction_guard: in-memory dbs skip the guard ────────

#[test]
#[serial]
fn acquire_local_construction_guard_is_noop_for_in_memory_db() {
    if crate::test_process::run_in_child() {
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));

    let cfg = RuntimeConfig {
        db_path: None,
        ..RuntimeConfig::default()
    };
    let guard = acquire_local_construction_guard(&cfg).expect("in-memory db needs no guard");
    assert!(
        guard.is_none(),
        "an in-memory database has no shared file to serialize construction against"
    );

    std::env::remove_var("KHIVE_LOCK");
}

// ── acquire_local_construction_guard: file-backed dbs serialize ──────────
//
// Two threads race to acquire the guard for the same file-backed db.
// Both must succeed (the guard is a blocking exclusive lock, not a
// try-and-fail check), but their guarded critical sections must never
// overlap — proven the same way
// `khive_runtime::daemon::tests::recovery_lock_serializes_two_concurrent_boot_sequences`
// proves it for the raw primitive: two threads increment/decrement a
// shared "inside the critical section" counter around a sleep, and a
// third-thread-visible max-observed-concurrency of 1 is the guarantee.

#[cfg(unix)]
#[test]
#[serial]
fn acquire_local_construction_guard_serializes_concurrent_file_backed_callers() {
    if crate::test_process::run_in_child() {
        return;
    }

    acquire_local_construction_guard_serializes_concurrent_file_backed_callers_impl();
}

#[cfg(not(unix))]
#[test]
#[serial]
fn acquire_local_construction_guard_serializes_concurrent_file_backed_callers_nonunix() {
    if crate::test_process::run_in_child() {
        return;
    }

    acquire_local_construction_guard_serializes_concurrent_file_backed_callers_impl();
}

fn acquire_local_construction_guard_serializes_concurrent_file_backed_callers_impl() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));
    let db_path = dir.path().join("cold.db3");

    let concurrent = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let max_observed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let spawn_one = |label: &'static str| {
        let db_path = db_path.clone();
        let concurrent = concurrent.clone();
        let max_observed = max_observed.clone();
        std::thread::spawn(move || {
            let cfg = RuntimeConfig {
                db_path: Some(db_path),
                ..RuntimeConfig::default()
            };
            let guard = acquire_local_construction_guard(&cfg)
                .unwrap_or_else(|e| panic!("{label} must acquire the guard: {e}"));

            let now = concurrent.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            max_observed.fetch_max(now, std::sync::atomic::Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(50));
            concurrent.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);

            drop(guard);
        })
    };

    let t_a = spawn_one("writer-a");
    let t_b = spawn_one("writer-b");
    t_a.join().expect("writer-a thread must not panic");
    t_b.join().expect("writer-b thread must not panic");

    assert_eq!(
        max_observed.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the two guarded critical sections must never overlap — the guard \
             failed to serialize concurrent local-construction callers"
    );

    std::env::remove_var("KHIVE_LOCK");
}

include!("exec_tests/cli_argument_tests.rs");

#[test]
fn serial_requires_ops_file_conflicts_with_inline_and_atomic_and_defaults_off() {
    let serial = ExecArgs::try_parse_from(["exec", "--ops-file", "/tmp/batch.jsonl", "--serial"])
        .expect("--serial must be accepted for a non-atomic ops-file");
    assert!(serial.serial);

    let default_parallel = ExecArgs::parse_from(["exec", "--ops-file", "/tmp/batch.jsonl"]);
    assert!(!default_parallel.serial);

    assert!(
        ExecArgs::try_parse_from(["exec", "stats()", "--serial"]).is_err(),
        "--serial without --ops-file must fail during CLI parsing"
    );
    assert!(
        ExecArgs::try_parse_from([
            "exec",
            "stats()",
            "--ops-file",
            "/tmp/batch.jsonl",
            "--serial",
        ])
        .is_err(),
        "--serial must not compose with inline positional ops"
    );
    assert!(
        ExecArgs::try_parse_from([
            "exec",
            "--ops-file",
            "/tmp/batch.jsonl",
            "--atomic",
            "--serial",
        ])
        .is_err(),
        "--serial and --atomic are distinct execution contracts and must conflict"
    );
}

#[test]
fn atomic_takes_no_inline_batch_of_chains() {
    // The cross-op atomic unit reads an ops file, one JSON op per line, so
    // no inline DSL reaches its admission check: a bracketed batch of
    // chains is refused at the same argument boundary as a flat batch.
    for ops in ["[stats() | stats(), stats()]", "[stats(), stats()]"] {
        let error = ExecArgs::try_parse_from(["exec", ops, "--atomic"])
            .expect_err("--atomic with inline ops must fail during CLI parsing");
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument,
            "{ops}: {error}"
        );
    }
    let atomic = ExecArgs::try_parse_from(["exec", "--ops-file", "/tmp/batch.jsonl", "--atomic"])
        .expect("--atomic must be accepted with an ops file");
    assert!(atomic.atomic);
}

include!("exec_tests/local_server_fixture_tests.rs");

include!("exec_tests/config_identity_tests.rs");

include!("exec_tests/local_backend_routing_tests.rs");

// ── guarded local construction races a guarded boot (#667/#645) ──────────
//
// Mirrors `khive-runtime/tests/cold_boot_fts_race.rs`'s deterministic
// two-thread pattern, but races a `kkernel mcp --daemon`-style guarded
// boot against `build_local_fallback_server` itself — the exact local
// path that, before this fix, constructed `KhiveRuntime`/`KhiveMcpServer`
// without acquiring the boot guard at all. Both "boots" target the SAME
// fresh (cold) db file; if either side ran unguarded, migrations/FTS DDL
// could interleave and corrupt (or lose rows from) the `fts_notes` index.

#[cfg(unix)]
fn run_one_guarded_daemon_boot(
    db_path: std::path::PathBuf,
    writer_label: &'static str,
    count: usize,
) {
    let guard = khive_runtime::daemon::acquire_recovery_lock().expect("acquire daemon boot guard");

    let rt_handle = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build per-thread tokio runtime");

    rt_handle.block_on(async {
        let rt = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(db_path),
            embedding_model: None,
            additional_embedding_models: vec![],
            ..RuntimeConfig::default()
        })
        .expect("cold-boot migrations succeed");
        let token = rt.authorize(Namespace::local()).expect("authorize local");

        for i in 0..count {
            rt.create_note(
                &token,
                "memo",
                None,
                &format!("{writer_label} note {i} — boot race marker"),
                None,
                None,
                vec![],
            )
            .await
            .expect("note write must succeed inside the guarded boot window");
        }
    });

    drop(guard);
}

#[cfg(unix)]
fn run_one_local_exec_construction(
    db_path: std::path::PathBuf,
    writer_label: &'static str,
    count: usize,
) {
    let rt_handle = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build per-thread tokio runtime");

    rt_handle.block_on(async {
        let cfg = RuntimeConfig {
            db_path: Some(db_path),
            embedding_model: None,
            additional_embedding_models: vec![],
            // Pin the pack list explicitly rather than inheriting
            // `KHIVE_PACKS` from the ambient environment (#1276) — this
            // race only exercises `kg` writes.
            packs: vec!["kg".to_string()],
            ..RuntimeConfig::default()
        };
        let khive_cfg = KhiveConfig::default();
        // The exact call site under test: before this fix, this function
        // built `KhiveRuntime`/`KhiveMcpServer` without acquiring any
        // guard, so it could run migrations/FTS DDL concurrently with
        // the guarded boot above against the same file.
        let server = build_local_fallback_server(cfg, &khive_cfg, None, None)
            .await
            .expect("guarded local-exec construction must succeed");

        for i in 0..count {
            let params = RequestParams {
                plan: None,
                ops: format!(
                    r#"create(kind="observation", content="{writer_label} note {i} — boot race marker")"#
                ),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            };
            let raw = server
                .dispatch_request_local(params)
                .await
                .expect("dispatch must succeed inside the guarded construction window");
            let resp: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON");
            assert_eq!(
                resp["results"][0]["ok"],
                serde_json::json!(true),
                "write must succeed: {resp}"
            );
        }
    });
}

// ── deterministic lock-blocking oracle ────────────────────────────────────
//
// The end-to-end race test below proves no corruption results when both
// sides respect the guard, but a mutation-testing pass showed its
// final-row-count oracle does NOT fail if the guard at
// `build_local_fallback_server`'s call site is removed entirely: with no
// second real lock-holder racing it, the row count comes out right either
// way, so the test cannot tell "guarded" from "unguarded". This test
// closes that gap: it holds the SAME recovery lock the guard acquires
// from the test thread itself, then asserts `build_local_fallback_server`
// cannot complete construction while that lock is held (bounded wait) —
// an assertion that is trivially true when the guard is unguarded (it
// never acquires anything, so it isn't blocked by our held lock).
#[cfg(unix)]
#[test]
#[serial]
fn build_local_fallback_server_blocks_while_recovery_lock_is_held() {
    if crate::test_process::run_in_child() {
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let lock_file = dir.path().join("khived.recovery.lock");
    std::env::set_var("KHIVE_LOCK", &lock_file);

    let db_path = dir.path().join("guard_block_test.db3");

    // A separate file descriptor to the SAME lock path — flock's
    // blocking semantics apply per open-file-description, so this
    // blocks a second acquirer even from another thread in this same
    // process (the same pattern `daemon.rs`'s own
    // `recovery_lock_serializes_two_concurrent_boot_sequences` and
    // `cold_boot_fts_race.rs` rely on).
    let held_guard =
        khive_runtime::daemon::acquire_recovery_lock().expect("acquire recovery lock in test");

    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let rt_handle = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build per-thread tokio runtime");
        let cfg = RuntimeConfig {
            db_path: Some(db_path),
            embedding_model: None,
            additional_embedding_models: vec![],
            // Pin the pack list explicitly rather than inheriting
            // `KHIVE_PACKS` from the ambient environment (#1276).
            packs: vec!["kg".to_string()],
            ..RuntimeConfig::default()
        };
        let khive_cfg = KhiveConfig::default();
        // The exact call under test: every non-atomic local-exec path
        // (daemon-unreachable fallback, --save-file, KHIVE_NO_DAEMON=1,
        // non-atomic --ops-file) funnels through this one function.
        let result = rt_handle
            .block_on(async { build_local_fallback_server(cfg, &khive_cfg, None, None).await });
        // Sent only AFTER construction returns — the test observes
        // whether this arrives before or after the lock is released.
        let _ = tx.send(());
        result
    });

    // Bounded wait: construction must NOT complete while the lock is
    // held. If the production guard at `build_local_fallback_server`'s
    // call site is ever removed or no-op'd, nothing blocks this thread
    // and the signal arrives well inside this window — this is the
    // mutation-killing assertion.
    let completed_while_locked = rx
        .recv_timeout(std::time::Duration::from_millis(500))
        .is_ok();
    assert!(
        !completed_while_locked,
        "build_local_fallback_server must NOT complete while the boot/recovery \
             lock is held by another holder — if this fires, the guard at its \
             production call site has been removed or stopped acquiring the shared lock"
    );

    drop(held_guard);

    handle
        .join()
        .expect("construction thread must not panic")
        .expect("construction must succeed once the lock is released");

    std::env::remove_var("KHIVE_LOCK");
}

// Named serial key (not the bare `#[serial]` default): this test only
// touches `KHIVE_LOCK`, not the `KHIVE_REQUIRE_ATTRIBUTED_ACTOR` /
// `KHIVE_NO_DAEMON` / `HOME` vars the default-keyed `#[serial]` tests
// above guard. Sharing their queue would only add wall-clock delay
// (this test spawns two real OS threads doing real `flock` + migrations)
// without protecting anything — and empirically DOES perturb unrelated
// non-serial tests elsewhere in this binary (`pending_events`) that race
// on those other env vars.
#[cfg(unix)]
#[test]
#[serial]
fn local_exec_construction_races_guarded_daemon_boot_without_fts_corruption() {
    if crate::test_process::run_in_child() {
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let lock_file = dir.path().join("khived.recovery.lock");
    std::env::set_var("KHIVE_LOCK", &lock_file);

    // Fresh (cold) database file — neither side has run migrations on it yet.
    let db_path = dir.path().join("local_exec_boot_race.db3");

    const PER_WRITER: usize = 10;
    let path_a = db_path.clone();
    let path_b = db_path.clone();

    let t_a =
        std::thread::spawn(move || run_one_guarded_daemon_boot(path_a, "daemon-boot", PER_WRITER));
    let t_b = std::thread::spawn(move || {
        run_one_local_exec_construction(path_b, "local-exec", PER_WRITER)
    });
    t_a.join().expect("daemon-boot thread must not panic");
    t_b.join().expect("local-exec thread must not panic");

    let rt_handle = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build verification tokio runtime");
    rt_handle.block_on(async {
        let verify_rt = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(db_path.clone()),
            embedding_model: None,
            additional_embedding_models: vec![],
            ..RuntimeConfig::default()
        })
        .expect("post-race runtime opens cleanly");
        let token = verify_rt
            .authorize(Namespace::local())
            .expect("authorize local");

        let hits = verify_rt
            .search_notes(
                &token,
                "boot race marker",
                None,
                100,
                None,
                false,
                &[],
                None,
            )
            .await
            .expect("FTS search over notes must succeed, not error on a corrupted index");
        assert_eq!(
            hits.len(),
            PER_WRITER * 2,
            "every planted note from both writers must be present and \
                 FTS-searchable — a corrupted/partial index would drop or \
                 duplicate rows: {hits:?}"
        );
    });

    std::env::remove_var("KHIVE_LOCK");
}

include!("exec_tests/ops_file_parse_tests.rs");

#[test]
fn atomic_op_limit_is_checked_before_snapshot_materialization() {
    struct PanicOnSnapshotAccess;

    impl std::io::Read for PanicOnSnapshotAccess {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            panic!("over-limit atomic snapshot must not be read or materialized")
        }
    }

    impl std::io::Seek for PanicOnSnapshotAccess {
        fn seek(&mut self, _pos: std::io::SeekFrom) -> std::io::Result<u64> {
            panic!("over-limit atomic snapshot must not be rewound or materialized")
        }
    }

    let error = parse_atomic_validated_snapshot(&mut PanicOnSnapshotAccess, 2, 1)
        .expect_err("the validated op count exceeds the configured atomic ceiling");
    assert!(
        error
            .to_string()
            .contains("op count 2 exceeds the configured maximum 1"),
        "unexpected error: {error:#}"
    );
}

include!("exec_tests/ops_file_limits_and_status_tests.rs");

// ── integration: bulk apply (isolated DB) ─────────────────────────────────

include!("exec_volume_fixture_tests.rs");

#[tokio::test]
async fn serial_ops_file_max_in_flight_is_one_while_default_remains_parallel() {
    let op_count = OPS_FILE_CHUNK_SIZE;
    let (parallel_server, parallel_in_flight, parallel_max) = concurrency_probe_server(false);
    let parallel_summary = apply_ops_file(
        &parallel_server,
        reader_probe_ops(op_count),
        Some("verbose".to_string()),
        Some("json".to_string()),
        None,
        false,
    )
    .await
    .expect("the ordinary bounded-parallel batch must succeed");
    assert_eq!(parallel_summary["succeeded"], op_count);
    let observed_parallel_max = parallel_max.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        observed_parallel_max, 8,
        "default dispatch must retain the server's bounded parallelism"
    );
    assert_eq!(
        parallel_in_flight.load(std::sync::atomic::Ordering::SeqCst),
        0
    );

    let (serial_server, serial_in_flight, serial_max) = concurrency_probe_server(false);
    let serial_summary = apply_ops_file_with_dispatch_mode(
        &serial_server,
        reader_probe_ops(op_count),
        Some("verbose".to_string()),
        Some("json".to_string()),
        None,
        false,
        OpsFileDispatchMode::Serial,
    )
    .await
    .expect("serial dispatch must succeed");
    assert_eq!(serial_summary["succeeded"], op_count);
    assert_eq!(
        serial_max.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "--serial must await every handler before starting the next"
    );
    assert_eq!(
        serial_in_flight.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

#[tokio::test]
async fn serial_ops_file_succeeds_with_a_reader_that_refuses_overlap() {
    let op_count = 4;
    let (parallel_server, _, parallel_max) = concurrency_probe_server(true);
    let parallel_summary = apply_ops_file(
        &parallel_server,
        reader_probe_ops(op_count),
        Some("verbose".to_string()),
        Some("json".to_string()),
        None,
        false,
    )
    .await
    .expect("one parallel probe succeeds, so partial failure remains in-band");
    assert_eq!(parallel_summary["succeeded"], 1);
    assert_eq!(parallel_summary["failed"], op_count - 1);
    assert_eq!(
        parallel_max.load(std::sync::atomic::Ordering::SeqCst),
        op_count
    );
    assert!(parallel_summary["failures"]
        .as_array()
        .expect("failure rows")
        .iter()
        .all(|failure| failure["error"]["message"]
            .as_str()
            .expect("error.message is text")
            .contains("sql_bridge.reader_open")));

    let (serial_server, _, serial_max) = concurrency_probe_server(true);
    let serial_summary = apply_ops_file_with_dispatch_mode(
        &serial_server,
        reader_probe_ops(op_count),
        Some("verbose".to_string()),
        Some("json".to_string()),
        None,
        false,
        OpsFileDispatchMode::Serial,
    )
    .await
    .expect("serial dispatch must avoid constrained-reader overlap");
    assert_eq!(serial_summary["succeeded"], op_count);
    assert_eq!(serial_summary["failed"], 0);
    assert_eq!(serial_max.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn serial_ops_file_preserves_order_save_rows_and_strict_failure() {
    let (server, _, max_in_flight) = concurrency_probe_server(false);
    let mut ops = reader_probe_ops(3);
    ops[1].args["fail"] = serde_json::json!(true);
    let output_dir = tempfile::tempdir().expect("output dir");
    let save_path = output_dir.path().join("serial-ordered.jsonl");

    let error = apply_ops_file_with_dispatch_mode(
        &server,
        ops,
        Some("verbose".to_string()),
        Some("json".to_string()),
        Some(save_path.to_string_lossy().into_owned()),
        true,
        OpsFileDispatchMode::Serial,
    )
    .await
    .expect_err("strict mode must report the middle handler failure");
    assert!(error.to_string().contains("--strict"), "{error:#}");
    assert_eq!(max_in_flight.load(std::sync::atomic::Ordering::SeqCst), 1);

    let rows: Vec<serde_json::Value> = std::fs::read_to_string(save_path)
        .expect("strict failure still publishes all confirmed ordered rows")
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSON result row"))
        .collect();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["tool"], "reader_probe");
    assert_eq!(rows[0]["ok"], true);
    assert_eq!(rows[0]["result"]["sequence"], 0);
    assert_eq!(rows[1]["tool"], "reader_probe");
    assert_eq!(rows[1]["ok"], false);
    assert_eq!(rows[1]["reason"], "strict-op-failure");
    assert_eq!(rows[2]["tool"], "reader_probe");
    assert_eq!(rows[2]["ok"], true);
    assert_eq!(rows[2]["result"]["sequence"], 2);
}

#[tokio::test]
async fn serial_dispatch_consumes_the_preflighted_stable_snapshot() {
    use std::io::{Seek as _, Write as _};

    let mut source = NamedTempFile::new().expect("ops source");
    for sequence in 0..2 {
        serde_json::to_writer(
            source.as_file_mut(),
            &serde_json::json!({
                "tool": "reader_probe",
                "args": {"sequence": sequence},
            }),
        )
        .expect("write source op");
        source.write_all(b"\n").expect("write newline");
    }
    let mut validated = validate_ops_file(source.path()).expect("preflight source");

    source.as_file_mut().set_len(0).expect("truncate source");
    source
        .as_file_mut()
        .rewind()
        .expect("rewind replacement source");
    source
        .write_all(b"{malformed replacement\n")
        .expect("replace source after preflight");
    source.flush().expect("flush replacement");

    let (server, _, max_in_flight) = concurrency_probe_server(false);
    let summary = apply_ops_file_reader_with_dispatch_mode(
        &server,
        std::io::BufReader::new(&mut validated.snapshot),
        validated.total,
        Some("verbose".to_string()),
        Some("json".to_string()),
        None,
        false,
        OpsFileDispatchMode::Serial,
    )
    .await
    .expect("dispatch must consume the immutable validated snapshot");
    assert_eq!(summary["total"], 2);
    assert_eq!(summary["succeeded"], 2);
    assert_eq!(max_in_flight.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn serial_typed_preflight_rejects_later_prev_before_first_write() {
    if crate::test_process::run_in_child() {
        return;
    }

    use std::io::Write as _;

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let mut source = NamedTempFile::new().expect("ops source");
    source
        .write_all(
            b"{\"tool\":\"create\",\"args\":{\"kind\":\"concept\",\"name\":\"must-not-land\"}}\n",
        )
        .expect("write valid first op");
    source
        .write_all(b"{\"tool\":\"stats\",\"args\":{\"probe\":\"$prev.id\"}}\n")
        .expect("write typed-invalid later op");

    let config_dir = tempfile::tempdir().expect("config dir");
    let config_path = config_dir.path().join("khive.toml");
    std::fs::write(&config_path, "").expect("write empty config");
    let cfg = RuntimeConfig {
        db_path: Some(PathBuf::from(&db_path)),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::default()
    };
    let error = run_exec_ops_file(
        source.path().to_path_buf(),
        cfg,
        Some("verbose".to_string()),
        Some("json".to_string()),
        None,
        false,
        ExecDbContext {
            raw: Some(db_path.clone()),
            anchor: Some(PathBuf::from(&db_path)),
            config: Some(config_path),
        },
        true,
        false,
        None,
        false,
    )
    .await
    .expect_err("later $prev must fail before the first serial handler starts");
    assert!(error.to_string().contains("$prev"), "{error:#}");

    let server = isolated_server(&db_path);
    let response = dispatch_json(&server, r#"list(kind="concept")"#).await;
    assert_eq!(
        response["results"][0]["result"]["items"],
        serde_json::json!([]),
        "whole-snapshot typed preflight must prevent the valid first write"
    );
}

#[tokio::test]
async fn serial_whole_snapshot_preflight_does_not_change_default_chunk_commit_parity() {
    if crate::test_process::run_in_child() {
        return;
    }

    use std::io::Write as _;

    let mut source = NamedTempFile::new().expect("ops source");
    for sequence in 0..OPS_FILE_CHUNK_SIZE {
        serde_json::to_writer(
            source.as_file_mut(),
            &serde_json::json!({
                "tool": "create",
                "args": {
                    "kind": "concept",
                    "name": format!("default-first-chunk-{sequence}"),
                },
            }),
        )
        .expect("write valid first-chunk op");
        source.write_all(b"\n").expect("write newline");
    }
    source
        .write_all(b"{\"tool\":\"stats\",\"args\":{\"probe\":\"$prev.id\"}}\n")
        .expect("write typed-invalid second-chunk op");

    let config_dir = tempfile::tempdir().expect("config dir");
    let config_path = config_dir.path().join("khive.toml");
    std::fs::write(&config_path, "").expect("write empty config");

    let default_db = NamedTempFile::new().expect("default db");
    let default_db_path = default_db.path().to_str().expect("utf8").to_string();
    let default_error = run_exec_ops_file(
        source.path().to_path_buf(),
        RuntimeConfig {
            db_path: Some(PathBuf::from(&default_db_path)),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string()],
            ..RuntimeConfig::default()
        },
        Some("verbose".to_string()),
        Some("json".to_string()),
        None,
        false,
        ExecDbContext {
            raw: Some(default_db_path.clone()),
            anchor: Some(PathBuf::from(&default_db_path)),
            config: Some(config_path.clone()),
        },
        false,
        false,
        None,
        false,
    )
    .await
    .expect_err("default mode must reject the typed-invalid second chunk");
    assert!(
        default_error.to_string().contains("$prev"),
        "{default_error:#}"
    );
    let default_server = isolated_server(&default_db_path);
    let default_response =
        dispatch_json(&default_server, r#"list(kind="concept", limit=200)"#).await;
    assert_eq!(
        default_response["results"][0]["result"]["items"]
            .as_array()
            .expect("default concept rows")
            .len(),
        OPS_FILE_CHUNK_SIZE,
        "backward-compatible default mode must commit its first logical chunk"
    );

    let serial_db = NamedTempFile::new().expect("serial db");
    let serial_db_path = serial_db.path().to_str().expect("utf8").to_string();
    let serial_error = run_exec_ops_file(
        source.path().to_path_buf(),
        RuntimeConfig {
            db_path: Some(PathBuf::from(&serial_db_path)),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string()],
            ..RuntimeConfig::default()
        },
        Some("verbose".to_string()),
        Some("json".to_string()),
        None,
        false,
        ExecDbContext {
            raw: Some(serial_db_path.clone()),
            anchor: Some(PathBuf::from(&serial_db_path)),
            config: Some(config_path),
        },
        true,
        false,
        None,
        false,
    )
    .await
    .expect_err("serial mode must reject the later invalid op before dispatch");
    assert!(
        serial_error.to_string().contains("$prev"),
        "{serial_error:#}"
    );
    let serial_server = isolated_server(&serial_db_path);
    let serial_response = dispatch_json(&serial_server, r#"list(kind="concept", limit=200)"#).await;
    assert_eq!(
        serial_response["results"][0]["result"]["items"],
        serde_json::json!([]),
        "serial whole-snapshot preflight must reject before its first write"
    );
}

/// #3089 end-to-end witness: a daemonless write dispatched through
/// `run_exec_ops_file` must leave no `-wal`/`-shm` sidecar once it
/// returns. `run_exec_ops_file` never attempts daemon forwarding (see
/// its own doc comment), so this drives the real in-process dispatch
/// path deterministically without a spy or `KHIVE_NO_DAEMON`, and it
/// exercises the same `settle_exec_storage_before_return` helper that
/// `run_exec_inline_with_forward` calls at its own tail, including the
/// ADR-133 audit-batch drain (`with_mounted_packs`,
/// `khive-mcp/src/server.rs`, wires an `EventStore` for every
/// non-read-only runtime this test's writable config builds) and the
/// writer-task join. `pool_drop_never_leaves_a_reader_as_the_last_connection_closed`
/// (`khive-db/src/pool.rs`) already covers `ConnectionPool::drop` in
/// isolation; this test covers the caller that owns the process exit
/// path around it.
#[tokio::test]
async fn ops_file_write_leaves_no_wal_sidecar_after_return() {
    if crate::test_process::run_in_child() {
        return;
    }

    use std::io::Write as _;

    let db_dir = tempfile::tempdir().expect("db dir");
    let db_path = db_dir.path().join("close-order.db");
    let db_path_str = db_path.to_str().expect("utf8").to_string();
    let wal = {
        let mut name = db_path.file_name().unwrap().to_os_string();
        name.push("-wal");
        db_path.parent().unwrap().join(name)
    };
    let shm = {
        let mut name = db_path.file_name().unwrap().to_os_string();
        name.push("-shm");
        db_path.parent().unwrap().join(name)
    };

    let config_dir = tempfile::tempdir().expect("config dir");
    let config_path = config_dir.path().join("khive.toml");
    std::fs::write(&config_path, "").expect("write empty config");

    let mut source = NamedTempFile::new().expect("ops source");
    source
        .write_all(
            b"{\"tool\":\"create\",\"args\":{\"kind\":\"concept\",\"name\":\"wal-sidecar-witness\"}}\n",
        )
        .expect("write create op");

    let cfg = RuntimeConfig {
        db_path: Some(db_path.clone()),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::default()
    };

    run_exec_ops_file(
        source.path().to_path_buf(),
        cfg,
        Some("verbose".to_string()),
        Some("json".to_string()),
        None,
        false,
        ExecDbContext {
            raw: Some(db_path_str),
            anchor: Some(db_path),
            config: Some(config_path),
        },
        false,
        false,
        None,
        false,
    )
    .await
    .expect("ops-file write must succeed");

    assert!(
        !wal.exists(),
        "a daemonless write through run_exec_ops_file must not leave a -wal sidecar behind after return"
    );
    assert!(
        !shm.exists(),
        "a daemonless write through run_exec_ops_file must not leave a -shm sidecar behind after return"
    );
}

include!("exec_tests/strict_result_tests.rs");

include!("exec_tests/ops_file_preview_tests.rs");

#[tokio::test]
async fn atomic_dry_run_uses_the_real_read_only_admission() {
    if crate::test_process::run_in_child() {
        return;
    }

    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("dry-run-target.db");
    let config_path = dir.path().join("khive.toml");
    std::fs::write(&config_path, "").expect("empty config");
    let source_path = dir.path().join("ops.jsonl");
    std::fs::write(
        &source_path,
        "{\"tool\":\"create\",\"args\":{\"kind\":\"concept\",\"name\":\"MustNotLand\"}}\n",
    )
    .expect("ops file");
    let db_text = db_path.to_str().expect("utf8 db path").to_string();
    let context = || ExecDbContext {
        raw: Some(db_text.clone()),
        anchor: Some(db_path.clone()),
        config: Some(config_path.clone()),
    };

    for dry_run in [true, false] {
        let error = run_exec_ops_file(
            source_path.clone(),
            atomic_cfg(&db_text),
            None,
            None,
            None,
            dry_run,
            context(),
            false,
            true,
            None,
            false,
        )
        .await
        .expect_err("create is inadmissible in an atomic unit");
        assert!(
            error
                .downcast_ref::<crate::atomic_apply::AtomicExecFailure>()
                .is_some(),
            "dry_run={dry_run}: {error:#}"
        );
        assert!(!db_path.exists(), "preflight must not open the target db");
    }

    let over_limit = run_exec_ops_file(
        source_path,
        atomic_cfg(&db_text),
        None,
        None,
        None,
        true,
        context(),
        false,
        true,
        Some(0),
        false,
    )
    .await
    .expect_err("atomic dry-run must apply the configured op ceiling");
    assert!(over_limit.to_string().contains("op count 1"));
    assert!(
        !db_path.exists(),
        "over-limit preview must not open storage"
    );
}

include!("exec_tests/actor_admission_and_pack_tests.rs");

include!("exec_tests/actor_forwarding_spy_tests.rs");

include!("exec_tests/daemon_spawn_config_tests.rs");

include!("exec_tests/read_only_topology_tests.rs");

include!("exec_tests/daemon_pack_and_database_override_tests.rs");

include!("exec_tests/daemon_config_identity_tests.rs");

include!("exec_tests/inline_database_override_tests.rs");

// ── ADR-099 B3: `--atomic` CLI surface acceptance tests ───────────────────

fn atomic_op(tool: &str, args: serde_json::Value) -> OpsFileEntry {
    OpsFileEntry {
        tool: tool.to_string(),
        args,
    }
}

async fn dispatch_json(server: &KhiveMcpServer, ops: &str) -> serde_json::Value {
    // Verbose presentation: the default Agent mode truncates entity ids
    // to an 8-char short form for readability, which the atomic prepare
    // path (and every KG verb) rejects as "not a full UUID". Tests here
    // need the real id back out so it can feed straight into `update`/
    // `delete`/`link` args.
    let params = RequestParams {
        plan: None,
        ops: ops.to_string(),
        presentation: Some("verbose".to_string()),
        presentation_per_op: None,
        save_to: None,
        format: None,
        format_per_op: None,
        request_id: None,
    };
    let raw = server.dispatch_request_local(params).await.unwrap();
    serde_json::from_str(&raw).unwrap()
}

fn atomic_cfg(db_path: &str) -> RuntimeConfig {
    RuntimeConfig {
        db_path: Some(PathBuf::from(db_path)),
        embedding_model: None,
        additional_embedding_models: vec![],
        // Pin the pack list explicitly rather than inheriting `KHIVE_PACKS`
        // from the ambient environment (#1276). Atomic execution retains
        // the complete discovered validation/lifecycle surface even when
        // the caller configures only the base KG pack.
        packs: vec!["kg".to_string()],
        ..Default::default()
    }
}

async fn replace_fts_entities_with_incompatible_table(db_path: &str) {
    let runtime = KhiveRuntime::new(atomic_cfg(db_path)).expect("runtime for FTS fault setup");
    let sql = runtime.sql();
    let mut writer = sql.writer().await.expect("writer for FTS fault setup");
    writer
        .execute_script(
            "DROP TABLE fts_entities; \
                 CREATE TABLE fts_entities (broken_column TEXT);"
                .to_string(),
        )
        .await
        .expect("replace FTS table to inject post-commit reindex failure");
}

#[tokio::test]
async fn atomic_kg_only_config_keeps_gtd_hook_and_lifecycle_execution() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let khive_cfg = KhiveConfig::default();

    let (hook_task_id, transition_task_id, complete_task_id) = {
        let server = isolated_server(&db_path);
        let response = dispatch_json(
            &server,
            r#"[gtd.assign(title="HookGuard", status="next"), gtd.assign(title="TransitionGuard", status="inbox"), gtd.assign(title="CompleteGuard", status="active")]"#,
        )
        .await;
        let full_id = |index: usize| {
            response["results"][index]["result"]["full_id"]
                .as_str()
                .unwrap_or_else(|| panic!("missing task full_id at index {index}: {response}"))
                .to_string()
        };
        (full_id(0), full_id(1), full_id(2))
    };

    let hook_error = crate::atomic_apply::execute_atomic_ops_file(
        vec![atomic_op(
            "update",
            serde_json::json!({
                "id": hook_task_id.as_str(),
                "properties": {"depends_on": [hook_task_id.as_str()]},
            }),
        )],
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect_err("the GTD task hook must reject a self-dependency");
    assert!(
        format!("{hook_error:#}").contains("cannot depend on itself"),
        "the kg-only atomic registry must enforce the GTD hook: {hook_error:#}"
    );

    let server = isolated_server(&db_path);
    let response = dispatch_json(&server, &format!(r#"get(id="{hook_task_id}")"#)).await;
    assert!(
        response["results"][0]["result"]["properties"]
            .get("depends_on")
            .is_none(),
        "the rejected dependency update must not mutate the task: {response}"
    );

    let envelope = crate::atomic_apply::execute_atomic_ops_file(
        vec![
            atomic_op(
                "gtd.transition",
                serde_json::json!({"id": transition_task_id, "status": "next"}),
            ),
            atomic_op(
                "gtd.complete",
                serde_json::json!({"id": complete_task_id, "result": "verified"}),
            ),
        ],
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("GTD lifecycle adapters must execute with a kg-only config");
    assert_eq!(envelope["atomic"]["committed"], true, "{envelope}");
    assert_eq!(envelope["results"][0]["result"]["to"], "next");
    assert_eq!(envelope["results"][1]["result"]["to"], "done");
}

/// Acceptance test 1a: an all-success atomic ops-file run commits every
/// op as one unit and the results are visible afterward.
#[tokio::test]
async fn atomic_ops_file_success_commits_all_ops() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();

    let (x_id, y_id) = {
        let server = isolated_server(&db_path);
        let resp = dispatch_json(
            &server,
            r#"[create(kind="concept", name="AtomicX"), create(kind="concept", name="AtomicY")]"#,
        )
        .await;
        let x_id = resp["results"][0]["result"]["id"]
            .as_str()
            .expect("x id")
            .to_string();
        let y_id = resp["results"][1]["result"]["id"]
            .as_str()
            .expect("y id")
            .to_string();
        (x_id, y_id)
    };

    let ops = vec![
        atomic_op(
            "update",
            serde_json::json!({"id": x_id, "name": "AtomicX-renamed"}),
        ),
        atomic_op(
            "update",
            serde_json::json!({"id": y_id, "name": "AtomicY-renamed"}),
        ),
    ];

    let khive_cfg = KhiveConfig::default();
    let envelope = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("atomic run must succeed");

    assert_eq!(
        envelope["atomic"]["committed"], true,
        "envelope: {envelope}"
    );

    let server = isolated_server(&db_path);
    let x_resp = dispatch_json(&server, &format!(r#"get(id="{x_id}")"#)).await;
    let y_resp = dispatch_json(&server, &format!(r#"get(id="{y_id}")"#)).await;
    assert_eq!(x_resp["results"][0]["result"]["name"], "AtomicX-renamed");
    assert_eq!(y_resp["results"][0]["result"]["name"], "AtomicY-renamed");
}

#[tokio::test]
async fn atomic_post_commit_reindex_failure_returns_committed_non_retryable_envelope() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let entity_id = {
        let server = isolated_server(&db_path);
        let response = dispatch_json(
            &server,
            r#"create(kind="concept", name="PostCommitReindexBefore")"#,
        )
        .await;
        response["results"][0]["result"]["id"]
            .as_str()
            .expect("entity id")
            .to_string()
    };

    // Migration state remains current, but an incompatible ordinary table
    // occupies the post-commit FTS name. Store initialization cannot
    // recreate the virtual table through IF NOT EXISTS; prepare and base
    // DML do not touch it, so the failure occurs at the real reindex seam.
    replace_fts_entities_with_incompatible_table(&db_path).await;

    let envelope = crate::atomic_apply::execute_atomic_ops_file(
        vec![atomic_op(
            "update",
            serde_json::json!({"id": entity_id, "name": "PostCommitReindexAfter"}),
        )],
        atomic_cfg(&db_path),
        &KhiveConfig::default(),
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("post-commit failure must return a reconciliation envelope");

    assert_eq!(envelope["atomic"]["committed"], true, "{envelope}");
    assert_eq!(
        envelope["atomic"]["status"], "committed_degraded",
        "{envelope}"
    );
    assert_eq!(envelope["atomic"]["retryable"], false, "{envelope}");
    assert_eq!(
        envelope["atomic"]["degradations"][0]["stage"], "post_commit_reindex",
        "{envelope}"
    );
    assert_eq!(envelope["summary"]["succeeded"], 1, "{envelope}");

    let runtime = KhiveRuntime::new(atomic_cfg(&db_path)).expect("runtime for committed-row check");
    let token = runtime.authorize(Namespace::local()).expect("authorize");
    let entity = runtime
        .get_entity(&token, Uuid::parse_str(&entity_id).unwrap())
        .await
        .expect("committed entity row");
    assert_eq!(entity.name, "PostCommitReindexAfter");
}

#[tokio::test]
async fn atomic_result_read_failure_keeps_later_committed_delete_non_retryable() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let entity_id = {
        let server = isolated_server(&db_path);
        let response = dispatch_json(
            &server,
            r#"create(kind="concept", name="RenderThenDelete")"#,
        )
        .await;
        response["results"][0]["result"]["id"]
            .as_str()
            .expect("entity id")
            .to_string()
    };

    // Both plans prepare against the same live row. The unit then updates
    // and hard-deletes it atomically. Rendering op 0 performs its real
    // post-commit read and cannot find the row removed by op 1.
    let envelope = crate::atomic_apply::execute_atomic_ops_file(
        vec![
            atomic_op(
                "update",
                serde_json::json!({"id": entity_id, "name": "NeverRendered"}),
            ),
            atomic_op("delete", serde_json::json!({"id": entity_id, "hard": true})),
        ],
        atomic_cfg(&db_path),
        &KhiveConfig::default(),
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("render failure after commit must be a reconciliation envelope");

    assert_eq!(envelope["atomic"]["committed"], true, "{envelope}");
    assert_eq!(
        envelope["atomic"]["status"], "committed_degraded",
        "{envelope}"
    );
    assert_eq!(envelope["atomic"]["retryable"], false, "{envelope}");
    assert_eq!(
        envelope["atomic"]["degradations"][0]["stage"], "result_rendering",
        "{envelope}"
    );
    assert_eq!(envelope["results"][0]["ok"], true, "{envelope}");
    assert_eq!(
        envelope["results"][0]["status"], "committed_degraded",
        "{envelope}"
    );
    assert_eq!(envelope["results"][0]["retryable"], false, "{envelope}");
    assert_eq!(envelope["results"][1]["result"]["deleted"], true);

    let runtime = KhiveRuntime::new(atomic_cfg(&db_path)).expect("runtime for deleted-row check");
    let token = runtime.authorize(Namespace::local()).expect("authorize");
    let deleted = runtime
        .get_entity(&token, Uuid::parse_str(&entity_id).unwrap())
        .await;
    assert!(deleted.is_err(), "hard delete must remain committed");
}

/// Acceptance test 1b: a mid-unit failure rolls the WHOLE unit back —
/// zero partial state, including the op that "succeeded" before the
/// failing one.
///
/// Shape: `x` and `y` both exist. Op 0 hard-deletes `x`. Op 1 links `y`
/// to `x`. At PREPARE time (before either op runs) `x` still exists, so
/// both plans build successfully. At COMMIT time op 0 removes `x` first,
/// then op 1's guarded `INSERT ... WHERE EXISTS` affects zero rows (the
/// dangling-edge guard, ADR-099 D1 rule 1) — the whole unit rolls back,
/// so `x`'s deletion is undone too.
#[tokio::test]
async fn atomic_ops_file_mid_unit_failure_rolls_back_whole_unit() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();

    let (x_id, y_id) = {
        let server = isolated_server(&db_path);
        let resp = dispatch_json(
            &server,
            r#"[create(kind="concept", name="RollbackX"), create(kind="concept", name="RollbackY")]"#,
        )
        .await;
        let x_id = resp["results"][0]["result"]["id"]
            .as_str()
            .expect("x id")
            .to_string();
        let y_id = resp["results"][1]["result"]["id"]
            .as_str()
            .expect("y id")
            .to_string();
        (x_id, y_id)
    };

    let ops = vec![
        atomic_op("delete", serde_json::json!({"id": x_id, "hard": true})),
        atomic_op(
            "link",
            serde_json::json!({
                "source_id": y_id,
                "target_id": x_id,
                "relation": "extends",
            }),
        ),
    ];

    let khive_cfg = KhiveConfig::default();
    let envelope = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("the seam call itself must not error — the unit rolls back cleanly");

    assert_eq!(
        envelope["atomic"]["rolled_back"], true,
        "envelope: {envelope}"
    );
    assert_eq!(
        envelope["atomic"]["failed_op_index"], 1,
        "envelope: {envelope}"
    );

    let server = isolated_server(&db_path);
    let x_resp = dispatch_json(&server, &format!(r#"get(id="{x_id}")"#)).await;
    assert!(
        x_resp["results"][0]["result"]["deleted_at"].is_null(),
        "x must NOT be deleted — the whole unit must have rolled back: {x_resp}"
    );
}

#[tokio::test]
async fn atomic_rollback_exits_nonzero_with_or_without_save_and_strict() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let (x_id, y_id) = {
        let server = isolated_server(&db_path);
        let response = dispatch_json(
            &server,
            r#"[create(kind="concept", name="AtomicExitX"), create(kind="concept", name="AtomicExitY")]"#,
        )
        .await;
        (
            response["results"][0]["result"]["id"]
                .as_str()
                .unwrap()
                .to_string(),
            response["results"][1]["result"]["id"]
                .as_str()
                .unwrap()
                .to_string(),
        )
    };
    let mut file = NamedTempFile::new().unwrap();
    for op in [
        serde_json::json!({"tool":"delete","args":{"id":x_id.clone(),"hard":true}}),
        serde_json::json!({
            "tool":"link",
            "args":{"source_id":y_id,"target_id":x_id,"relation":"extends"}
        }),
    ] {
        serde_json::to_writer(&mut file, &op).unwrap();
        file.write_all(b"\n").unwrap();
    }
    let config_dir = tempfile::tempdir().unwrap();
    let config_path = config_dir.path().join("khive.toml");
    std::fs::write(&config_path, "").unwrap();
    let db_context = || ExecDbContext {
        raw: Some(db_path.clone()),
        anchor: Some(PathBuf::from(&db_path)),
        config: Some(config_path.clone()),
    };

    let error = run_exec_ops_file(
        file.path().to_path_buf(),
        atomic_cfg(&db_path),
        None,
        None,
        None,
        false,
        db_context(),
        false,
        true,
        None,
        false,
    )
    .await
    .expect_err("atomic rollback must exit non-zero without --strict");
    assert!(error.to_string().contains("rolled back"), "{error:#}");

    let output_dir = tempfile::tempdir().unwrap();
    let save_path = output_dir.path().join("atomic-rollback.jsonl");
    let error = run_exec_ops_file(
        file.path().to_path_buf(),
        atomic_cfg(&db_path),
        None,
        Some("json".to_string()),
        Some(save_path.to_string_lossy().into_owned()),
        false,
        db_context(),
        false,
        true,
        None,
        true,
    )
    .await
    .expect_err("atomic rollback must exit non-zero with --strict and --save-file");
    assert!(error.to_string().contains("rolled back"), "{error:#}");
    assert_eq!(
        std::fs::read_to_string(save_path).unwrap().lines().count(),
        2
    );
}

#[test]
fn atomic_save_persist_failure_returns_committed_reconciliation_stdout() {
    let output_dir = tempfile::tempdir().unwrap();
    let save_path = output_dir.path().join("publish-race.jsonl");
    let sink =
        khive_mcp::save_sink::JsonlSaveSink::new(&save_path, false).expect("preflight save sink");
    // Deterministic post-preflight publication failure: a directory wins
    // the destination path after the sibling temp file has been created,
    // so row writes and flush succeed but the final atomic rename fails.
    std::fs::create_dir(&save_path).expect("occupy destination with a directory");
    let mut envelope = serde_json::json!({
        "results": [{
            "ok": true,
            "tool": "update",
            "op_index": 0,
            "result": {"id": Uuid::new_v4()}
        }],
        "summary": {"total": 1, "succeeded": 1, "failed": 0},
        "atomic": {
            "committed": true,
            "rolled_back": false,
            "failed_op_index": null,
            "error": null
        }
    });

    let failure = render_atomic_output(&mut envelope, Some(sink))
        .expect_err("persist failure must remain a non-zero CLI outcome");
    let stdout: serde_json::Value =
        serde_json::from_str(&failure.stdout).expect("structured stdout envelope");

    assert_eq!(stdout["atomic"]["committed"], true, "{stdout}");
    assert_eq!(stdout["atomic"]["status"], "committed_degraded", "{stdout}");
    assert_eq!(stdout["atomic"]["retryable"], false, "{stdout}");
    assert_eq!(
        stdout["atomic"]["degradations"][0]["stage"], "save_file_publish",
        "{stdout}"
    );
    assert!(
        stdout["atomic"]["degradations"][0]["error"]
            .as_str()
            .is_some_and(|error| error.contains("persist temp file")),
        "{stdout}"
    );
    assert!(
        format!("{:#}", failure.error).contains("do not replay the mutation"),
        "terminal error must point automation at reconciliation"
    );
}

#[tokio::test]
async fn atomic_invalid_save_directory_is_rejected_before_commit() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let mut file = NamedTempFile::new().unwrap();
    serde_json::to_writer(
        &mut file,
        &serde_json::json!({
            "tool":"create",
            "args":{"kind":"concept","name":"atomic-must-not-exist"}
        }),
    )
    .unwrap();
    file.write_all(b"\n").unwrap();

    let config_dir = tempfile::tempdir().unwrap();
    let config_path = config_dir.path().join("khive.toml");
    std::fs::write(&config_path, "").unwrap();
    let save_directory = tempfile::tempdir().unwrap();
    let error = run_exec_ops_file(
        file.path().to_path_buf(),
        atomic_cfg(&db_path),
        None,
        Some("json".to_string()),
        Some(save_directory.path().to_string_lossy().into_owned()),
        false,
        ExecDbContext {
            raw: Some(db_path.clone()),
            anchor: Some(PathBuf::from(&db_path)),
            config: Some(config_path),
        },
        false,
        true,
        None,
        true,
    )
    .await
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("absent or an existing regular file"));

    let server = isolated_server(&db_path);
    let response = dispatch_json(&server, r#"list(kind="concept")"#).await;
    assert_eq!(
        response["results"][0]["result"]["items"],
        serde_json::json!([])
    );
}

#[tokio::test]
async fn atomic_preflighted_save_keeps_prior_file_on_execution_error() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let mut file = NamedTempFile::new().unwrap();
    serde_json::to_writer(
        &mut file,
        &serde_json::json!({
            "tool":"create",
            "args":{"kind":"concept","name":"atomic-rejected"}
        }),
    )
    .unwrap();
    file.write_all(b"\n").unwrap();

    let config_dir = tempfile::tempdir().unwrap();
    let config_path = config_dir.path().join("khive.toml");
    std::fs::write(&config_path, "").unwrap();
    let output_dir = tempfile::tempdir().unwrap();
    let save_path = output_dir.path().join("prior.jsonl");
    std::fs::write(&save_path, b"prior-complete-output\n").unwrap();

    let error = run_exec_ops_file(
        file.path().to_path_buf(),
        atomic_cfg(&db_path),
        None,
        Some("json".to_string()),
        Some(save_path.to_string_lossy().into_owned()),
        false,
        ExecDbContext {
            raw: Some(db_path),
            anchor: Some(PathBuf::from(db_file.path())),
            config: Some(config_path),
        },
        false,
        true,
        Some(1),
        true,
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("--atomic rejected"),
        "unexpected atomic execution error: {error:#}"
    );
    assert_eq!(
        std::fs::read(&save_path).unwrap(),
        b"prior-complete-output\n"
    );
}

/// #1474: the user-facing `--atomic` executor prepares every operation
/// before its commit pass. Two individually acyclic task writes can
/// therefore form a cycle only inside the unit. The V16 commit-time
/// guards must reject the later statement and roll the earlier one back
/// for both authoritative dependency stores.
#[tokio::test]
async fn atomic_ops_file_rejects_same_unit_gtd_dependency_cycles() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();

    let (a_id, b_id) = {
        let server = isolated_server(&db_path);
        let response = dispatch_json(
            &server,
            r#"[gtd.assign(title="AtomicCycleA", status="next"), gtd.assign(title="AtomicCycleB", status="next")]"#,
        )
        .await;
        (
            response["results"][0]["result"]["full_id"]
                .as_str()
                .expect("task A id")
                .to_string(),
            response["results"][1]["result"]["full_id"]
                .as_str()
                .expect("task B id")
                .to_string(),
        )
    };

    let compact_a_id = a_id.replace('-', "");
    let compact_b_id = b_id.replace('-', "");
    let alternate_spelling_error = crate::atomic_apply::execute_atomic_ops_file(
        vec![
            atomic_op(
                "update",
                serde_json::json!({
                    "id": a_id.clone(),
                    "properties": {"depends_on": [compact_b_id]}
                }),
            ),
            atomic_op(
                "update",
                serde_json::json!({
                    "id": b_id.clone(),
                    "properties": {"depends_on": [compact_a_id]}
                }),
            ),
        ],
        atomic_cfg(&db_path),
        &KhiveConfig::default(),
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect_err("atomic preparation must reject an alternate dependency UUID spelling");
    let alternate_spelling_message = format!("{alternate_spelling_error:#}");
    assert!(
        alternate_spelling_message.contains("canonical lowercase hyphenated UUID"),
        "unexpected alternate-spelling error: {alternate_spelling_message}"
    );

    {
        let server = isolated_server(&db_path);
        let response = dispatch_json(&server, &format!(r#"get(id="{a_id}")"#)).await;
        assert!(
            response["results"][0]["result"]["properties"]
                .get("depends_on")
                .is_none(),
            "alternate dependency spelling must not persist: {response}"
        );
    }

    let property_envelope = crate::atomic_apply::execute_atomic_ops_file(
        vec![
            atomic_op(
                "update",
                serde_json::json!({
                    "id": a_id.clone(),
                    "properties": {"depends_on": [b_id.clone()]}
                }),
            ),
            atomic_op(
                "update",
                serde_json::json!({
                    "id": b_id.clone(),
                    "properties": {"depends_on": [a_id.clone()]}
                }),
            ),
        ],
        atomic_cfg(&db_path),
        &KhiveConfig::default(),
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("cycle is a clean atomic rollback, not a seam failure");
    assert_eq!(property_envelope["atomic"]["rolled_back"], true);
    assert_eq!(property_envelope["atomic"]["failed_op_index"], 1);
    assert!(
        property_envelope["atomic"]["error"]
            .as_str()
            .is_some_and(|error| error.contains("dependency cycle")),
        "envelope: {property_envelope}"
    );

    let server = isolated_server(&db_path);
    for task_id in [&a_id, &b_id] {
        let response = dispatch_json(&server, &format!(r#"get(id="{task_id}")"#)).await;
        assert!(
            response["results"][0]["result"]["properties"]
                .get("depends_on")
                .is_none(),
            "the earlier update must roll back too: {response}"
        );
    }

    let edge_envelope = crate::atomic_apply::execute_atomic_ops_file(
        vec![
            atomic_op(
                "link",
                serde_json::json!({
                    "source_id": a_id.clone(),
                    "target_id": b_id.clone(),
                    "relation": "depends_on"
                }),
            ),
            atomic_op(
                "link",
                serde_json::json!({
                    "source_id": b_id.clone(),
                    "target_id": a_id.clone(),
                    "relation": "depends_on"
                }),
            ),
        ],
        atomic_cfg(&db_path),
        &KhiveConfig::default(),
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("edge cycle is a clean atomic rollback, not a seam failure");
    assert_eq!(edge_envelope["atomic"]["rolled_back"], true);
    assert_eq!(edge_envelope["atomic"]["failed_op_index"], 1);
    assert!(
        edge_envelope["atomic"]["error"]
            .as_str()
            .is_some_and(|error| error.contains("dependency cycle")),
        "envelope: {edge_envelope}"
    );

    let server = isolated_server(&db_path);
    let response = dispatch_json(
        &server,
        &format!(r#"neighbors(id="{a_id}", direction="out", relations=["depends_on"])"#),
    )
    .await;
    assert_eq!(
        response["results"][0]["result"],
        serde_json::json!([]),
        "the earlier link must roll back too: {response}"
    );
}

/// ADR-099 B3 (second half): the inverse
/// same-unit race — `[link(A, B, competes_with), update(X
/// extends A-B -> competes_with)]`, where the CANONICAL row the update
/// conflict-absorbs into is created by an EARLIER op in the SAME
/// atomic unit (so it does not exist at either op's prepare time). The
/// commit must both write correctly (X deleted, the just-linked row
/// preserved unchanged per ADR-039 DO NOTHING — X's patch is discarded,
/// not applied) and RENDER the correct surviving id — not X's
/// prepare-time-advisory id, which this fix removed reliance on
/// entirely (`build_op_result` now derives it from a post-commit
/// natural-key lookup).
#[tokio::test]
async fn atomic_symmetric_update_absorbs_into_same_unit_link_and_renders_correct_id() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();

    let (a_id, b_id, x_id) = {
        let server = isolated_server(&db_path);
        let resp = dispatch_json(
            &server,
            r#"[create(kind="concept", name="LinkRaceA"), create(kind="concept", name="LinkRaceB")]"#,
        )
        .await;
        let a_id = resp["results"][0]["result"]["id"]
            .as_str()
            .expect("a id")
            .to_string();
        let b_id = resp["results"][1]["result"]["id"]
            .as_str()
            .expect("b id")
            .to_string();

        let link_resp = dispatch_json(
            &server,
            &format!(
                r#"link(source_id="{a_id}", target_id="{b_id}", relation="extends", weight=0.2)"#
            ),
        )
        .await;
        let x_id = link_resp["results"][0]["result"]["id"]
            .as_str()
            .expect("x id")
            .to_string();
        (a_id, b_id, x_id)
    };

    let ops = vec![
        atomic_op(
            "link",
            serde_json::json!({
                "source_id": a_id,
                "target_id": b_id,
                "relation": "competes_with",
                "weight": 0.6,
            }),
        ),
        atomic_op(
            "update",
            serde_json::json!({"id": x_id, "relation": "competes_with", "weight": 0.9}),
        ),
    ];

    let khive_cfg = KhiveConfig::default();
    let envelope = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("atomic run must succeed");

    assert_eq!(
        envelope["atomic"]["committed"], true,
        "envelope: {envelope}"
    );

    let linked_id = envelope["results"][0]["result"]["id"]
        .as_str()
        .expect("link result id")
        .to_string();
    let rendered_update_id = envelope["results"][1]["result"]["id"]
        .as_str()
        .expect("update result id")
        .to_string();
    assert_ne!(
        rendered_update_id, x_id,
        "the update's rendered result must NOT be X's stale requested id: {envelope}"
    );
    assert_eq!(
        rendered_update_id, linked_id,
        "the update's rendered result must be the surviving (just-linked) row: {envelope}"
    );
    assert_eq!(
        envelope["results"][1]["result"]["weight"], 0.6,
        "ADR-039 DO NOTHING: the surviving row keeps its OWN pre-existing weight (0.6, \
             set by the link above), not the discarded update's patched weight (0.9): {envelope}"
    );

    let server = isolated_server(&db_path);
    let surviving_resp = dispatch_json(&server, &format!(r#"get(id="{linked_id}")"#)).await;
    assert_eq!(
        surviving_resp["results"][0]["result"]["weight"], 0.6,
        "the committed row itself must keep its pre-existing weight, not the discarded \
             update's patch: {surviving_resp}"
    );
}

/// The canonical survivor a
/// symmetric-update op absorbs into can ALREADY be soft-deleted before the
/// atomic unit even runs (not just tombstoned as a side effect of the same
/// unit's own writes, as the sibling test above covers). This exercises
/// `build_op_result`'s `get_edge_by_natural_key_including_deleted` call
/// through the real atomic path with a genuinely pre-existing tombstone: the
/// pre-fix renderer (`KhiveRuntime::list_edges`, which unconditionally
/// filters `deleted_at IS NULL`) would report the committed update as "not
/// found" for exactly this row, turning a successful DO NOTHING absorption
/// into a spurious post-commit error.
#[tokio::test]
async fn atomic_symmetric_update_absorbs_into_pre_existing_tombstoned_survivor_and_renders_it() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();

    let (canonical_id, x_id) = {
        let server = isolated_server(&db_path);
        let resp = dispatch_json(
            &server,
            r#"[create(kind="concept", name="TombA"), create(kind="concept", name="TombB")]"#,
        )
        .await;
        let a_id = resp["results"][0]["result"]["id"]
            .as_str()
            .expect("a id")
            .to_string();
        let b_id = resp["results"][1]["result"]["id"]
            .as_str()
            .expect("b id")
            .to_string();

        // The canonical survivor: created, then soft-deleted, BEFORE the atomic
        // unit ever runs.
        let link_resp = dispatch_json(
            &server,
            &format!(
                r#"link(source_id="{a_id}", target_id="{b_id}", relation="competes_with", weight=0.6)"#
            ),
        )
        .await;
        let canonical_id = link_resp["results"][0]["result"]["id"]
            .as_str()
            .expect("canonical id")
            .to_string();
        dispatch_json(&server, &format!(r#"delete(id="{canonical_id}")"#)).await;

        // A distinct pre-existing edge under a different relation, later
        // converted (by the atomic update below) into the same
        // (a, b, competes_with) natural key — it must absorb into the
        // already-tombstoned canonical row, not resurrect or overwrite it.
        let x_resp = dispatch_json(
            &server,
            &format!(
                r#"link(source_id="{a_id}", target_id="{b_id}", relation="extends", weight=0.2)"#
            ),
        )
        .await;
        let x_id = x_resp["results"][0]["result"]["id"]
            .as_str()
            .expect("x id")
            .to_string();
        (canonical_id, x_id)
    };

    let ops = vec![atomic_op(
        "update",
        serde_json::json!({"id": x_id, "relation": "competes_with", "weight": 0.9}),
    )];

    let khive_cfg = KhiveConfig::default();
    let envelope = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("atomic run must succeed by absorbing into the tombstoned survivor");

    assert_eq!(
        envelope["atomic"]["committed"], true,
        "envelope: {envelope}"
    );

    let rendered_id = envelope["results"][0]["result"]["id"]
        .as_str()
        .expect("update result id")
        .to_string();
    assert_eq!(
        rendered_id, canonical_id,
        "must render the pre-existing tombstoned canonical survivor, not X's stale \
             requested id: {envelope}"
    );
    assert!(
        !envelope["results"][0]["result"]["deleted_at"].is_null(),
        "the rendered survivor must show its OWN tombstoned state (non-null deleted_at) \
             — absorbing a conflicting update must not resurrect it: {envelope}"
    );
    assert_eq!(
        envelope["results"][0]["result"]["weight"], 0.6,
        "ADR-039 DO NOTHING: the survivor keeps its own pre-existing weight, not X's \
             discarded patched weight (0.9): {envelope}"
    );
}

/// Acceptance test 2: every CLI-boundary rejection fires BEFORE any
/// write — each sub-case asserts both the error and that the db stays
/// empty (zero entities created).
#[tokio::test]
async fn atomic_cli_boundary_rejections_happen_before_any_write() {
    if crate::test_process::run_in_child() {
        return;
    }

    let khive_cfg = KhiveConfig::default();

    // (a) embedding-bearing verb.
    {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();
        let ops = vec![atomic_op(
            "create",
            serde_json::json!({"kind": "concept", "name": "ShouldNotLand"}),
        )];
        let err = crate::atomic_apply::execute_atomic_ops_file(
            ops,
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect_err("embedding-bearing verb must be rejected");
        assert!(
            format!("{err:#}").contains("embedding-bearing"),
            "error: {err:#}"
        );
        let server = isolated_server(&db_path);
        let resp = dispatch_json(&server, r#"list(kind="entity")"#).await;
        assert_eq!(
            resp["results"][0]["result"]["items"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }

    // (b) read verb.
    {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();
        let ops = vec![atomic_op("search", serde_json::json!({"query": "x"}))];
        let err = crate::atomic_apply::execute_atomic_ops_file(
            ops,
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect_err("read verbs must be rejected");
        assert!(format!("{err:#}").contains("read"), "error: {err:#}");
    }

    // (c) unlisted verb.
    {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();
        let ops = vec![atomic_op("not_a_real_verb", serde_json::json!({}))];
        let err = crate::atomic_apply::execute_atomic_ops_file(
            ops,
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect_err("unlisted verbs must be rejected");
        assert!(
            format!("{err:#}").contains("not on the v1 atomic-admissible"),
            "error: {err:#}"
        );
    }

    // (d) op-count guard.
    {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();
        let ops = vec![
            atomic_op(
                "update",
                serde_json::json!({"id": uuid::Uuid::new_v4().to_string()}),
            ),
            atomic_op(
                "update",
                serde_json::json!({"id": uuid::Uuid::new_v4().to_string()}),
            ),
            atomic_op(
                "update",
                serde_json::json!({"id": uuid::Uuid::new_v4().to_string()}),
            ),
        ];
        let err =
            crate::atomic_apply::execute_atomic_ops_file(ops, atomic_cfg(&db_path), &khive_cfg, 2)
                .await
                .expect_err("exceeding max_ops must be rejected");
        assert!(
            format!("{err:#}").contains("exceeds the configured maximum"),
            "error: {err:#}"
        );
    }

    // (e) governance verbs (`propose`/`review`/`withdraw`) — ADR-099 B3:
    // these are on the v1 admissible list
    // (ADR-099 D3 intends them to gain a seam) but have no prepare/apply
    // implementation in this slice yet. They must be rejected at this
    // SAME pre-runtime static guard — never reaching `KhiveRuntime::new`
    // or any write — not deferred to fail later inside `prepare_op`.
    for verb in ["propose", "review", "withdraw"] {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();
        let ops = vec![atomic_op(
            verb,
            serde_json::json!({"title": "x", "description": "y", "changeset": {}}),
        )];
        let err = crate::atomic_apply::execute_atomic_ops_file(
            ops,
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect_err(&format!("{verb:?} must be rejected before any write"));
        assert!(
            format!("{err:#}").contains("no --atomic prepare/apply seam"),
            "error for {verb:?}: {err:#}"
        );
        // No runtime/db file activity: the db stays empty (nothing else
        // touched it, so a plain re-open with the same path must show a
        // fresh, unwritten store).
        let server = isolated_server(&db_path);
        let resp = dispatch_json(&server, r#"list(kind="entity")"#).await;
        assert_eq!(
            resp["results"][0]["result"]["items"]
                .as_array()
                .unwrap()
                .len(),
            0,
            "no write must have landed for {verb:?}"
        );
    }

    // (f) `merge` — ADR-099 B3: deferred at this SAME pre-runtime static guard rather than shipped
    // with partial parity. Must name the non-atomic merge verb as the
    // supported route, and must not reach `KhiveRuntime::new`/any write.
    {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();
        let ops = vec![atomic_op(
            "merge",
            serde_json::json!({
                "into_id": uuid::Uuid::new_v4().to_string(),
                "from_id": uuid::Uuid::new_v4().to_string(),
            }),
        )];
        let err = crate::atomic_apply::execute_atomic_ops_file(
            ops,
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect_err("merge must be rejected before any write");
        assert!(
            format!("{err:#}").contains("use the non-atomic merge verb instead"),
            "error: {err:#}"
        );
        let server = isolated_server(&db_path);
        let resp = dispatch_json(&server, r#"list(kind="entity")"#).await;
        assert_eq!(
            resp["results"][0]["result"]["items"]
                .as_array()
                .unwrap()
                .len(),
            0,
            "no write must have landed for merge"
        );
    }
}

// ── ADR-099 B3 fix: `--atomic` deny_unknown_fields parity ────────────────
//
// Canonical `update`/`delete`/`link`/`gtd.transition`/`gtd.complete`
// reject unknown/typo'd arg keys via `#[serde(deny_unknown_fields)]` on
// their param structs. Pre-fix, `--atomic` silently dropped unrecognized
// keys instead of rejecting the op — a typo like `conten` (for
// `content`) would report `ok:true` while quietly discarding the
// caller's intended change. These tests exercise the fix at the same
// `execute_atomic_ops_file` seam as the acceptance tests above, and are
// the end-to-end counterpart to the syntactic-only unit coverage in
// `atomic_apply::validate_atomic_args_tests`.

/// Sharp case called out explicitly: atomic `update(id=X,
/// conten="hello")` (typo of `content`) must be rejected AND must not
/// mutate the row — no `content` change, no `updated_at` bump. Pre-fix,
/// this silently discarded `conten`, reset every other field to its
/// current value, bumped `updated_at`, and reported `ok:true`.
#[tokio::test]
async fn atomic_update_entity_unknown_field_is_rejected_and_does_not_mutate_row() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();

    let (entity_id, updated_at_before) = {
        let server = isolated_server(&db_path);
        let resp = dispatch_json(
            &server,
            r#"create(kind="concept", name="TypoGuardX", description="original")"#,
        )
        .await;
        let id = resp["results"][0]["result"]["id"]
            .as_str()
            .expect("id")
            .to_string();
        let get_resp = dispatch_json(&server, &format!(r#"get(id="{id}")"#)).await;
        let updated_at = get_resp["results"][0]["result"]["updated_at"].clone();
        (id, updated_at)
    };

    let ops = vec![atomic_op(
        "update",
        serde_json::json!({"id": entity_id, "conten": "hello"}),
    )];
    let khive_cfg = KhiveConfig::default();
    let err = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect_err("typo'd `conten` must be rejected, not silently dropped");
    assert!(
        format!("{err:#}").contains("unknown field"),
        "error: {err:#}"
    );

    let server = isolated_server(&db_path);
    let get_resp = dispatch_json(&server, &format!(r#"get(id="{entity_id}")"#)).await;
    assert_eq!(
        get_resp["results"][0]["result"]["description"], "original",
        "a rejected op must not have mutated description: {get_resp}"
    );
    assert_eq!(
        get_resp["results"][0]["result"]["updated_at"], updated_at_before,
        "a rejected op must not bump updated_at (no write happened): {get_resp}"
    );
}

/// update-note variant of the same parity fix: a typo'd key on a note
/// update must be rejected, and a well-formed note update still
/// succeeds (parity boundary — don't over-reject).
#[tokio::test]
async fn atomic_update_note_unknown_field_rejected_well_formed_succeeds() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();

    let note_id = {
        let server = isolated_server(&db_path);
        let resp = dispatch_json(
            &server,
            r#"create(kind="observation", content="original note")"#,
        )
        .await;
        resp["results"][0]["result"]["id"]
            .as_str()
            .expect("id")
            .to_string()
    };

    // (a) unknown field rejected.
    let khive_cfg = KhiveConfig::default();
    let ops = vec![atomic_op(
        "update",
        serde_json::json!({"id": note_id, "conten": "typo'd"}),
    )];
    let err = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect_err("typo'd `conten` on a note update must be rejected");
    assert!(
        format!("{err:#}").contains("unknown field"),
        "error: {err:#}"
    );

    // (b) well-formed update still succeeds.
    let ops = vec![atomic_op(
        "update",
        serde_json::json!({"id": note_id, "content": "updated note"}),
    )];
    let envelope = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("a well-formed note update must succeed");
    assert_eq!(
        envelope["atomic"]["committed"], true,
        "envelope: {envelope}"
    );

    let server = isolated_server(&db_path);
    let get_resp = dispatch_json(&server, &format!(r#"get(id="{note_id}")"#)).await;
    assert_eq!(
        get_resp["results"][0]["result"]["content"], "updated note",
        "the well-formed update must have landed: {get_resp}"
    );
}

#[tokio::test]
async fn atomic_task_note_update_projects_status_like_canonical_handlers() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let task_id = {
        let server = isolated_server(&db_path);
        let response = dispatch_json(
            &server,
            r#"gtd.assign(title="AtomicProjectionTask", status="next")"#,
        )
        .await;
        response["results"][0]["result"]["full_id"]
            .as_str()
            .expect("task id")
            .to_string()
    };
    let khive_cfg = KhiveConfig::default();
    let update = || {
        vec![atomic_op(
            "update",
            serde_json::json!({"id": task_id.clone(), "content": "projected body"}),
        )]
    };

    let changed = crate::atomic_apply::execute_atomic_ops_file(
        update(),
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("atomic note update");
    let changed_result = &changed["results"][0]["result"];
    assert_eq!(changed_result["status"], "next", "{changed}");
    assert_eq!(changed_result["lifecycle"], "active", "{changed}");
    assert_eq!(
        changed_result["display_name"], "AtomicProjectionTask",
        "{changed}"
    );
    assert!(changed_result.get("unchanged").is_none(), "{changed}");

    let server = isolated_server(&db_path);
    let canonical_get = dispatch_json(&server, &format!(r#"get(id="{task_id}")"#)).await;
    assert_eq!(changed_result, &canonical_get["results"][0]["result"]);

    let unchanged = crate::atomic_apply::execute_atomic_ops_file(
        update(),
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("atomic note no-op update");
    let unchanged_result = &unchanged["results"][0]["result"];
    assert_eq!(unchanged_result["unchanged"], true, "{unchanged}");
    let canonical_update = dispatch_json(
        &server,
        &format!(r#"update(id="{task_id}", content="projected body")"#),
    )
    .await;
    assert_eq!(unchanged_result, &canonical_update["results"][0]["result"]);
}

/// `delete`: a typo'd key (`hardd` for `hard`) must be rejected before
/// any write; a well-formed delete still succeeds.
#[tokio::test]
async fn atomic_delete_unknown_field_rejected_well_formed_succeeds() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();

    let entity_id = {
        let server = isolated_server(&db_path);
        let resp =
            dispatch_json(&server, r#"create(kind="concept", name="DeleteTypoGuard")"#).await;
        resp["results"][0]["result"]["id"]
            .as_str()
            .expect("id")
            .to_string()
    };

    // (a) unknown field rejected — entity must survive.
    let khive_cfg = KhiveConfig::default();
    let ops = vec![atomic_op(
        "delete",
        serde_json::json!({"id": entity_id, "hardd": true}),
    )];
    let err = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect_err("typo'd `hardd` must be rejected");
    assert!(
        format!("{err:#}").contains("unknown field"),
        "error: {err:#}"
    );
    let server = isolated_server(&db_path);
    let get_resp = dispatch_json(&server, &format!(r#"get(id="{entity_id}")"#)).await;
    assert!(
        get_resp["results"][0]["result"]["deleted_at"].is_null(),
        "a rejected delete must not have deleted the entity: {get_resp}"
    );

    // (b) well-formed delete still succeeds.
    let ops = vec![atomic_op("delete", serde_json::json!({"id": entity_id}))];
    let envelope = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("a well-formed delete must succeed");
    assert_eq!(
        envelope["atomic"]["committed"], true,
        "envelope: {envelope}"
    );
}

/// `link`: a typo'd key (`relatoin` for `relation`) must be rejected
/// before any write; a well-formed link still succeeds. Validation of the
/// `target_backend` conflict arm remains deferred and is out of scope here.
#[tokio::test]
async fn atomic_link_unknown_field_rejected_well_formed_succeeds() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();

    let (a_id, b_id) = {
        let server = isolated_server(&db_path);
        let resp = dispatch_json(
            &server,
            r#"[create(kind="concept", name="LinkTypoA"), create(kind="concept", name="LinkTypoB")]"#,
        )
        .await;
        let a_id = resp["results"][0]["result"]["id"]
            .as_str()
            .expect("a id")
            .to_string();
        let b_id = resp["results"][1]["result"]["id"]
            .as_str()
            .expect("b id")
            .to_string();
        (a_id, b_id)
    };

    // (a) unknown field rejected.
    let khive_cfg = KhiveConfig::default();
    let ops = vec![atomic_op(
        "link",
        serde_json::json!({
            "source_id": a_id,
            "target_id": b_id,
            "relation": "extends",
            "relatoin": "extends",
        }),
    )];
    let err = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect_err("typo'd `relatoin` must be rejected");
    assert!(
        format!("{err:#}").contains("unknown field"),
        "error: {err:#}"
    );

    // (b) well-formed link still succeeds.
    let ops = vec![atomic_op(
        "link",
        serde_json::json!({"source_id": a_id, "target_id": b_id, "relation": "extends"}),
    )];
    let envelope = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("a well-formed link must succeed");
    assert_eq!(
        envelope["atomic"]["committed"], true,
        "envelope: {envelope}"
    );
}

/// `gtd.transition`: a typo'd key (`notee` for `note`) must be rejected
/// before any write (task status unchanged); a well-formed transition
/// still succeeds.
#[tokio::test]
async fn atomic_gtd_transition_unknown_field_rejected_well_formed_succeeds() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();

    let task_id = {
        let server = isolated_server(&db_path);
        let resp = dispatch_json(
            &server,
            r#"gtd.assign(title="TransitionTypoGuard", status="inbox")"#,
        )
        .await;
        // gtd.assign's `id` field is always the short hex form
        // (handlers.rs:372) regardless of presentation mode — use
        // `full_id`, the real UUID, so it round-trips through the
        // atomic prepare path's UUID parse.
        resp["results"][0]["result"]["full_id"]
            .as_str()
            .expect("full_id")
            .to_string()
    };

    // (a) unknown field rejected — status must stay "inbox".
    let khive_cfg = KhiveConfig::default();
    let ops = vec![atomic_op(
        "gtd.transition",
        serde_json::json!({"id": task_id, "status": "next", "notee": "typo"}),
    )];
    let err = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect_err("typo'd `notee` must be rejected");
    assert!(
        format!("{err:#}").contains("unknown field"),
        "error: {err:#}"
    );

    // (b) well-formed transition still succeeds.
    let ops = vec![atomic_op(
        "gtd.transition",
        serde_json::json!({"id": task_id, "status": "next"}),
    )];
    let envelope = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("a well-formed gtd.transition must succeed");
    assert_eq!(
        envelope["atomic"]["committed"], true,
        "envelope: {envelope}"
    );
}

/// `gtd.complete`: a typo'd key (`resutl` for `result`) must be
/// rejected before any write (task status unchanged); a well-formed
/// complete still succeeds.
#[tokio::test]
async fn atomic_gtd_complete_unknown_field_rejected_well_formed_succeeds() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();

    let task_id = {
        let server = isolated_server(&db_path);
        let resp = dispatch_json(
            &server,
            r#"gtd.assign(title="CompleteTypoGuard", status="next")"#,
        )
        .await;
        // Same `full_id` note as the transition test above.
        resp["results"][0]["result"]["full_id"]
            .as_str()
            .expect("full_id")
            .to_string()
    };

    // (a) unknown field rejected.
    let khive_cfg = KhiveConfig::default();
    let ops = vec![atomic_op(
        "gtd.complete",
        serde_json::json!({"id": task_id, "resutl": "typo"}),
    )];
    let err = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect_err("typo'd `resutl` must be rejected");
    assert!(
        format!("{err:#}").contains("unknown field"),
        "error: {err:#}"
    );

    // (b) well-formed complete still succeeds.
    let ops = vec![atomic_op(
        "gtd.complete",
        serde_json::json!({"id": task_id, "result": "shipped"}),
    )];
    let envelope = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("a well-formed gtd.complete must succeed");
    assert_eq!(
        envelope["atomic"]["committed"], true,
        "envelope: {envelope}"
    );
}

// ── ADR-099 B3: delete kind parity, update
// null/type validation, canonical id resolution, per-op result payloads ──

/// Atomic `delete(id=<entity>, kind="note")` must be
/// REJECTED (no row deleted) — pre-fix, atomic ignored `kind` entirely
/// and deleted the entity anyway (a destructive wrong-substrate action).
/// `delete(id=<entity>, kind="entity")` and `kind` omitted must both
/// still succeed.
#[tokio::test]
async fn atomic_delete_rejects_kind_mismatch_and_accepts_matching_or_omitted_kind() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let khive_cfg = KhiveConfig::default();

    let (mismatch_id, matching_id, omitted_id) = {
        let server = isolated_server(&db_path);
        let resp = dispatch_json(
            &server,
            r#"[create(kind="concept", name="KindMismatch"), create(kind="concept", name="KindMatching"), create(kind="concept", name="KindOmitted")]"#,
        )
        .await;
        let id = |i: usize| {
            resp["results"][i]["result"]["id"]
                .as_str()
                .expect("id")
                .to_string()
        };
        (id(0), id(1), id(2))
    };

    // (a) kind mismatch: entity, caller says "note" — must be rejected,
    // entity must still be present afterward.
    let ops = vec![atomic_op(
        "delete",
        serde_json::json!({"id": mismatch_id, "kind": "note"}),
    )];
    let err = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect_err("delete(kind=\"note\") on an entity must be rejected");
    assert!(
        format!("{err:#}").contains("not found"),
        "expected a NotFound-shaped rejection, error: {err:#}"
    );
    let server = isolated_server(&db_path);
    let resp = dispatch_json(&server, &format!(r#"get(id="{mismatch_id}")"#)).await;
    assert!(
        resp["results"][0]["result"]["deleted_at"].is_null(),
        "entity must NOT be deleted after a kind-mismatch rejection: {resp}"
    );

    // (b) matching kind: succeeds.
    let ops = vec![atomic_op(
        "delete",
        serde_json::json!({"id": matching_id, "kind": "entity"}),
    )];
    let envelope = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("delete(kind=\"entity\") on an entity must succeed");
    assert_eq!(
        envelope["atomic"]["committed"], true,
        "envelope: {envelope}"
    );

    // (c) omitted kind: succeeds.
    let ops = vec![atomic_op("delete", serde_json::json!({"id": omitted_id}))];
    let envelope = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("delete with kind omitted must succeed");
    assert_eq!(
        envelope["atomic"]["committed"], true,
        "envelope: {envelope}"
    );
}

/// Canonical and atomic updates preserve nullable-field presence and applicability.
/// See `crates/kkernel/docs/design.md#execrs-regression-test-notes`.
/// Debug builds approach the default 2 MiB stack even with boxed futures.
/// Reserve headroom only in this isolated child, not production worker stacks.
#[tokio::test]
async fn atomic_update_null_and_type_semantics_match_canonical_behavior() {
    if crate::test_process::run_in_child_with_stack_size(4 * 1024 * 1024) {
        return;
    }

    // The dispatch and atomic pipelines are awaited from many points of this
    // one test future. Boxing them keeps each behind a pointer instead of
    // embedding each pipeline future in the outer test frame.
    async fn call(db_path: &str, ops: &str) -> serde_json::Value {
        let server = isolated_server(db_path);
        let envelope = Box::pin(dispatch_json(&server, ops)).await;
        assert_eq!(envelope["results"][0]["ok"], true, "{envelope}");
        envelope["results"][0]["result"].clone()
    }

    async fn get(db_path: &str, id: &str) -> serde_json::Value {
        call(db_path, &format!(r#"get(id="{id}")"#)).await
    }

    async fn update(db_path: &str, atomic: bool, args: serde_json::Value) -> Result<(), String> {
        let envelope = if atomic {
            Box::pin(crate::atomic_apply::execute_atomic_ops_file(
                vec![atomic_op("update", args)],
                atomic_cfg(db_path),
                &KhiveConfig::default(),
                khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
            ))
            .await
            .map_err(|error| format!("{error:#}"))?
        } else {
            let fields = args
                .as_object()
                .expect("update argument object")
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join(", ");
            let server = isolated_server(db_path);
            Box::pin(dispatch_json(&server, &format!("update({fields})"))).await
        };
        let accepted = if atomic {
            envelope["atomic"]["committed"] == true
        } else {
            envelope["results"][0]["ok"] == true
        };
        if accepted {
            Ok(())
        } else {
            Err(envelope.to_string())
        }
    }

    for atomic in [false, true] {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8");
        let created = call(
            db_path,
            r#"create(kind="concept", name="NullSemantics", description="orig-desc", properties={"k":"v"}, tags=["a","b"], skip_dedup_check=true)"#,
        ).await;
        let id = created["id"].as_str().expect("full entity id");
        let initial = get(db_path, id).await;
        assert_eq!(initial["description"], "orig-desc");

        let error = update(db_path, atomic, serde_json::json!({"id":id,"name":123}))
            .await
            .expect_err("non-string name stays invalid");
        assert!(error.contains("name must be a string"), "{error}");
        assert_eq!(get(db_path, id).await, initial);

        for field in ["salience", "decay_factor"] {
            let mut args = serde_json::json!({"id":id,"name":"must-not-land"});
            args[field] = serde_json::Value::Null;
            let error = update(db_path, atomic, args)
                .await
                .expect_err("nullable note fields are inapplicable to entities");
            assert!(
                error.contains(&format!("field '{field}' is not valid for an entity")),
                "{error}"
            );
            assert_eq!(get(db_path, id).await, initial);
        }

        for malformed in [
            serde_json::json!(123),
            serde_json::json!(true),
            serde_json::json!([]),
            serde_json::json!({}),
        ] {
            let error = update(
                db_path,
                atomic,
                serde_json::json!({
                    "id":id,"description":malformed,"name":"must-not-land"
                }),
            )
            .await
            .expect_err("non-string description stays invalid");
            let expected = if atomic {
                "description must be a string or null"
            } else {
                "description must be null or a string"
            };
            assert!(error.contains(expected), "{error}");
            assert_eq!(get(db_path, id).await, initial);
        }

        update(
            db_path,
            atomic,
            serde_json::json!({"id":id,"properties":{"added":true}}),
        )
        .await
        .expect("omitted description preserves its value");
        let omitted = get(db_path, id).await;
        assert_eq!(omitted["description"], "orig-desc");
        assert_eq!(omitted["properties"]["added"], true);

        update(
            db_path,
            atomic,
            serde_json::json!({
                "id":id,"name":null,"description":null,"properties":null,"tags":null
            }),
        )
        .await
        .expect("description null clears; other null fields keep their behavior");
        let cleared = get(db_path, id).await;
        assert_eq!(cleared.get("description"), Some(&serde_json::Value::Null));
        assert_eq!(cleared["name"], "NullSemantics");
        assert_eq!(cleared["properties"], omitted["properties"]);
        assert_eq!(cleared["tags"], omitted["tags"]);

        update(
            db_path,
            atomic,
            serde_json::json!({"id":id,"name":"after-clear"}),
        )
        .await
        .expect("an unrelated update preserves a cleared description");
        let after_clear = get(db_path, id).await;
        assert_eq!(
            after_clear.get("description"),
            Some(&serde_json::Value::Null)
        );
        assert_eq!(after_clear["name"], "after-clear");

        update(
            db_path,
            atomic,
            serde_json::json!({"id":id,"description":"reset"}),
        )
        .await
        .expect("description may be set again");
        assert_eq!(get(db_path, id).await["description"], "reset");
        update(
            db_path,
            atomic,
            serde_json::json!({"id":id,"name":"after-reset"}),
        )
        .await
        .expect("omission preserves a newly set description");
        assert_eq!(get(db_path, id).await["description"], "reset");
        update(
            db_path,
            atomic,
            serde_json::json!({"id":id,"description":""}),
        )
        .await
        .expect("empty string remains a concrete description");
        assert_eq!(
            get(db_path, id).await.get("description"),
            Some(&serde_json::json!(""))
        );

        let note = call(
            db_path,
            r#"create(kind="observation", name="kept-note", content="before")"#,
        )
        .await;
        let note_id = note["id"].as_str().expect("full note id");
        update(
            db_path,
            atomic,
            serde_json::json!({"id":note_id,"salience":0.6,"decay_factor":0.03}),
        )
        .await
        .expect("nullable fields remain valid on notes");
        let note_before = get(db_path, note_id).await;
        assert_eq!(note_before["salience"], 0.6);
        assert_eq!(note_before["decay_factor"], 0.03);
        let error = update(
            db_path,
            atomic,
            serde_json::json!({
                "id":note_id,"description":null,"content":"must-not-land"
            }),
        )
        .await
        .expect_err("a present description is inapplicable to notes");
        assert!(
            error.contains("field 'description' is not valid for a note"),
            "{error}"
        );
        assert_eq!(get(db_path, note_id).await, note_before);
        update(
            db_path,
            atomic,
            serde_json::json!({
                "id":note_id,"content":"after","name":null,"salience":null,"decay_factor":null
            }),
        )
        .await
        .expect("valid note clears still accept null name as unchanged");
        let note_after = get(db_path, note_id).await;
        assert_eq!(note_after["content"], "after");
        assert_eq!(note_after["name"], note_before["name"]);
        assert_eq!(note_after.get("salience"), Some(&serde_json::Value::Null));
        assert_eq!(
            note_after.get("decay_factor"),
            Some(&serde_json::Value::Null)
        );

        let target = call(
            db_path,
            r#"create(kind="concept", name="DescriptionTarget", skip_dedup_check=true)"#,
        )
        .await;
        let target_id = target["id"].as_str().expect("full target id");
        let edge = call(db_path, &format!(r#"link(source_id="{id}", target_id="{target_id}", relation="supports", weight=0.4)"#)).await;
        let edge_id = edge["id"].as_str().expect("full edge id");
        let edge_before = get(db_path, edge_id).await;
        for field in ["description", "salience", "decay_factor"] {
            let mut args = serde_json::json!({"id":edge_id,"weight":0.8});
            args[field] = serde_json::Value::Null;
            let error = update(db_path, atomic, args)
                .await
                .expect_err("nullable entity/note fields are inapplicable to edges");
            assert!(
                error.contains(&format!("field '{field}' is not valid for an edge")),
                "{error}"
            );
            assert_eq!(get(db_path, edge_id).await, edge_before);
        }
        update(
            db_path,
            atomic,
            serde_json::json!({"id":edge_id,"weight":0.8,"name":null}),
        )
        .await
        .expect("valid edge update preserves null name behavior");
        assert_eq!(get(db_path, edge_id).await["weight"], 0.8);
    }
}
/// An atomic ops-file using an 8-hex-prefix id for
/// `update` AND `gtd.transition` must succeed identically to canonical
/// (which accepts full UUID or an 8+ hex prefix); a non-existent prefix
/// must error with canonical's error shape ("no record matches
/// prefix"). Pre-fix, atomic did a bare `Uuid::parse_str` and rejected
/// any short id outright — the same ops-file that succeeds non-atomically
/// (e.g. against `gtd.assign`'s own short `id` output) would fail before
/// prepare under `--atomic`.
#[tokio::test]
async fn atomic_update_and_gtd_transition_accept_8_hex_prefix_ids() {
    if crate::test_process::run_in_child() {
        return;
    }

    let db_file = NamedTempFile::new().expect("temp db");
    let db_path = db_file.path().to_str().expect("utf8").to_string();
    let khive_cfg = KhiveConfig::default();

    let (entity_full_id, task_full_id) = {
        let server = isolated_server(&db_path);
        let resp = dispatch_json(&server, r#"create(kind="concept", name="PrefixEntity")"#).await;
        let entity_id = resp["results"][0]["result"]["id"]
            .as_str()
            .expect("entity id")
            .to_string();
        let resp = dispatch_json(&server, r#"gtd.assign(title="PrefixTask", status="next")"#).await;
        let task_id = resp["results"][0]["result"]["full_id"]
            .as_str()
            .expect("task full_id")
            .to_string();
        (entity_id, task_id)
    };
    let entity_prefix = &entity_full_id[..8];
    let task_prefix = &task_full_id[..8];

    // (a) 8-hex-prefix update and gtd.transition in the SAME atomic unit
    // both succeed.
    let ops = vec![
        atomic_op(
            "update",
            serde_json::json!({"id": entity_prefix, "name": "PrefixEntity-renamed"}),
        ),
        atomic_op(
            "gtd.transition",
            serde_json::json!({"id": task_prefix, "status": "active"}),
        ),
    ];
    let envelope = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect("8-hex-prefix ids must resolve identically to canonical");
    assert_eq!(
        envelope["atomic"]["committed"], true,
        "envelope: {envelope}"
    );

    let server = isolated_server(&db_path);
    let resp = dispatch_json(&server, &format!(r#"get(id="{entity_full_id}")"#)).await;
    assert_eq!(
        resp["results"][0]["result"]["name"], "PrefixEntity-renamed",
        "prefix-addressed update must have landed: {resp}"
    );

    // (b) a non-existent 8-hex prefix errors with canonical's error
    // shape.
    let ops = vec![atomic_op(
        "update",
        serde_json::json!({"id": "deadbeef", "name": "should not resolve"}),
    )];
    let err = crate::atomic_apply::execute_atomic_ops_file(
        ops,
        atomic_cfg(&db_path),
        &khive_cfg,
        khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
    )
    .await
    .expect_err("a non-existent prefix must be rejected");
    assert!(
        format!("{err:#}").contains("no record matches prefix"),
        "error: {err:#}"
    );
}

include!("exec_atomic_result_shape_tests.rs");
include!("exec_atomic_kind_tests.rs");

#[tokio::test]
async fn disabled_atomic_mutation_refuses_whole_unit_before_prepare_or_write() {
    if crate::test_process::run_in_child() {
        return;
    }
    let db_file = NamedTempFile::new().unwrap();
    let db_path = db_file.path().to_str().unwrap();
    let server = isolated_server(db_path);
    let created = dispatch_json(
        &server,
        r#"create(kind="concept", name="unchanged", skip_dedup_check=true)"#,
    )
    .await;
    assert_eq!(created["results"][0]["ok"], true, "{created}");
    let id = created["results"][0]["result"]["id"].as_str().unwrap();
    let get = format!("get(id=\"{id}\")");
    let before = dispatch_json(&server, &get).await["results"][0]["result"].clone();
    let policy: KhiveConfig = toml::from_str("[packs.kg]\nverbs_disabled = ['delete']\n").unwrap();
    let ops = vec![
        atomic_op(
            "update",
            serde_json::json!({"id": id, "name": "would have changed"}),
        ),
        atomic_op("delete", serde_json::json!({"id": id})),
    ];
    let cfg = atomic_cfg(db_path);
    let preview =
        crate::atomic_apply::preflight_atomic_ops_file(&ops, &cfg, &policy, 10).unwrap_err();
    assert!(
        preview.to_string().contains("unknown or disabled"),
        "{preview:#}"
    );
    let error = crate::atomic_apply::execute_atomic_ops_file(ops, cfg, &policy, 10)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("unknown or disabled"),
        "{error:#}"
    );
    let after = dispatch_json(&server, &get).await;
    assert_eq!(after["results"][0]["result"], before);
    let enabled = crate::atomic_apply::execute_atomic_ops_file(
        vec![atomic_op(
            "update",
            serde_json::json!({"id": id, "name": "enabled update"}),
        )],
        atomic_cfg(db_path),
        &policy,
        10,
    )
    .await
    .unwrap();
    assert_eq!(enabled["results"][0]["ok"], true, "{enabled}");
    let after = dispatch_json(&server, &get).await;
    assert_eq!(after["results"][0]["result"]["name"], "enabled update");
    // The ordinary exec fallback must use the same policy as the daemon and
    // atomic runner, including a config with only an implicit main backend.
    let local = build_local_fallback_server(atomic_cfg(db_path), &policy, None, None)
        .await
        .unwrap();
    let refused = dispatch_json(&local, &format!("delete(id=\"{id}\")")).await;
    assert_eq!(refused["results"][0]["ok"], false, "{refused}");
    assert_eq!(refused["results"][0]["reason"], "verb-refused", "{refused}");
    assert_eq!(
        dispatch_json(&server, &get).await["results"][0]["result"],
        after["results"][0]["result"]
    );
}

#[tokio::test]
async fn disabled_atomic_admissible_verbs_never_open_the_target_database() {
    if crate::test_process::run_in_child() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    for (pack, verb) in [
        ("kg", "update"),
        ("kg", "delete"),
        ("kg", "link"),
        ("gtd", "gtd.transition"),
    ] {
        let path = root.path().join(format!("{verb}.db"));
        let mut cfg = atomic_cfg(path.to_str().unwrap());
        cfg.packs.push("gtd".into());
        let policy: KhiveConfig =
            toml::from_str(&format!("[packs.{pack}]\nverbs_disabled = ['{verb}']\n")).unwrap();
        let error = crate::atomic_apply::execute_atomic_ops_file(
            vec![atomic_op(verb, serde_json::json!({}))],
            cfg,
            &policy,
            10,
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("unknown or disabled"),
            "{error:#}"
        );
        assert!(!path.exists(), "disabled {verb} opened the target database");
    }
}
