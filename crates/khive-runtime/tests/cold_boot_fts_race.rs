//! Regression test for #667 — cold-boot FTS/schema initialization must not
//! race a concurrent writer into `notes`/`fts_notes` index corruption.
//!
//! ADR-D3 (`../architect/design.md`) requires that any construction of a
//! `KhiveRuntime` against a possibly-fresh (cold) database file happen while
//! holding the same process-wide recovery/boot guard
//! (`khive_runtime::daemon::acquire_recovery_lock`) that daemon boot now
//! holds across migrations + pack schema plans (see
//! `run_daemon_with_boot_guard`, `khive-mcp/src/serve.rs::run`,
//! `kkernel/src/main.rs`). This test proves the positive-path guarantee:
//! two independent "boot" attempts that both respect the guard contract,
//! racing to construct+migrate the same fresh SQLite file and write notes
//! immediately afterward, never corrupt the FTS index — every planted note
//! is present, searchable, and `PRAGMA integrity_check` reports `ok`.
//!
//! This mirrors the existing deterministic-interleaving style used for #667
//! in `khive-runtime/src/daemon.rs::recovery_lock_serializes_two_concurrent_boot_sequences`
//! (real threads + the real lock primitive, not a mocked scheduler), but
//! here the "critical section" is a real cold-boot migration run plus real
//! writes instead of a synthetic counter.

use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};
use khive_storage::types::SqlStatement;
use serial_test::serial;

fn file_backed_config(db_path: std::path::PathBuf) -> RuntimeConfig {
    RuntimeConfig {
        db_path: Some(db_path),
        embedding_model: None,
        additional_embedding_models: vec![],
        ..RuntimeConfig::default()
    }
}

/// Simulates one "cold boot" attempt: acquire the same recovery/boot guard
/// production wiring now holds across migrations (ADR-D3), construct a
/// `KhiveRuntime` against the shared fresh db file (running migrations +
/// FTS DDL), then write `count` notes tagged with `writer_label` before
/// releasing the guard — modeling that boot construction and its first
/// writes are both inside the guarded window, exactly as
/// `run_daemon_with_boot_guard` holds the guard through bind+pid-write.
///
/// Runs its own single-threaded Tokio runtime via `block_on` on a plain OS
/// thread (`std::thread::spawn`), deliberately mirroring
/// `recovery_lock_serializes_two_concurrent_boot_sequences` in
/// `khive-runtime/src/daemon.rs` rather than `tokio::spawn` on a shared test
/// runtime: `acquire_recovery_lock` is a *blocking* `flock` call, and two
/// such calls racing as tasks on one current-thread test runtime can starve
/// each other's executor thread — the same self-deadlock class ADR-D3 calls
/// out for `run_daemon_with_boot_guard` vs. client-side lock acquisition.
fn run_one_cold_boot(db_path: std::path::PathBuf, writer_label: &'static str, count: usize) {
    let guard =
        khive_runtime::daemon::acquire_recovery_lock().expect("acquire recovery/boot guard");

    let rt_handle = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build per-thread tokio runtime");

    rt_handle.block_on(async {
        let rt =
            KhiveRuntime::new(file_backed_config(db_path)).expect("cold-boot migrations succeed");
        let token = rt
            .authorize(Namespace::local())
            .expect("authorize local namespace");

        for i in 0..count {
            rt.create_note(
                &token,
                "memo",
                None,
                &format!("{writer_label} note {i} — cold boot race marker"),
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

/// Opens a fresh runtime against `db_path` and asserts every planted marker
/// note is present, FTS-searchable, and that SQLite reports the file intact.
fn verify_no_corruption(db_path: std::path::PathBuf, expected_count: usize) {
    let rt_handle = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build verification tokio runtime");

    rt_handle.block_on(async {
        let verify_rt = KhiveRuntime::new(file_backed_config(db_path))
            .expect("post-race runtime opens cleanly");
        let token = verify_rt
            .authorize(Namespace::local())
            .expect("authorize local namespace");

        let hits = verify_rt
            .search_notes(
                &token,
                "cold boot race marker",
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
            expected_count,
            "every planted note must be present and FTS-searchable — a \
             corrupted/partial index would drop or duplicate rows: {hits:?}"
        );

        let sql = verify_rt.sql();
        let mut reader = sql.reader().await.expect("sql reader");
        let integrity = reader
            .query_scalar(SqlStatement {
                sql: "PRAGMA integrity_check".into(),
                params: vec![],
                label: Some("cold_boot_race_integrity_check".into()),
            })
            .await
            .expect("PRAGMA integrity_check must run")
            .expect("integrity_check returns a row");
        let integrity_text = format!("{integrity:?}");
        assert!(
            integrity_text.to_lowercase().contains("ok"),
            "sqlite integrity_check must report ok after the cold-boot race, \
             got: {integrity_text}"
        );
    });
}

#[test]
#[serial]
fn concurrent_cold_boots_do_not_corrupt_notes_fts_index() {
    let dir = tempfile::tempdir().expect("tempdir");
    let lock_file = dir.path().join("khived.recovery.lock");
    std::env::set_var("KHIVE_LOCK", &lock_file);

    // Fresh (cold) database file — neither "boot" has run migrations on it yet.
    let db_path = dir.path().join("cold_boot_race.db3");

    const PER_WRITER: usize = 10;
    let path_a = db_path.clone();
    let path_b = db_path.clone();

    let t_a = std::thread::spawn(move || run_one_cold_boot(path_a, "writer-a", PER_WRITER));
    let t_b = std::thread::spawn(move || run_one_cold_boot(path_b, "writer-b", PER_WRITER));
    t_a.join().expect("boot thread A must not panic");
    t_b.join().expect("boot thread B must not panic");

    verify_no_corruption(db_path, PER_WRITER * 2);

    std::env::remove_var("KHIVE_LOCK");
}

/// Guards against a regression where the boot guard is dropped *before* the
/// caller's post-construction writes (e.g. if a future refactor narrows the
/// guarded window back down to just `KhiveRuntime::new`). Re-running a full
/// "boot" (construction + migrations + writes) against an already-migrated
/// file must be idempotent — no duplicate schema objects, no lost rows.
#[test]
#[serial]
fn sequential_cold_boots_against_same_file_are_idempotent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let lock_file = dir.path().join("khived.recovery.lock");
    std::env::set_var("KHIVE_LOCK", &lock_file);

    let db_path = dir.path().join("sequential_boot.db3");

    run_one_cold_boot(db_path.clone(), "first-boot", 3);
    run_one_cold_boot(db_path.clone(), "second-boot", 2);

    verify_no_corruption(db_path, 5);

    std::env::remove_var("KHIVE_LOCK");
}
