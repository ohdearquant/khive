use super::*;

#[path = "blob_tests/volume_floor_tests.rs"]
mod volume_floor_tests;

fn store(floor_bytes: u64) -> (tempfile::TempDir, FsBlobStore) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("blobs");
    // Zero orphan-sweep grace period: these tests exercise immediate
    // orphan deletion, not the publish-grace window (covered by the
    // `orphan_sweep_grace` tests below).
    let store = FsBlobStore::new(root, floor_bytes)
        .unwrap()
        .with_orphan_sweep_grace(Duration::ZERO);
    (dir, store)
}

include!("blob_publication_barrier_tests.rs");

#[cfg(unix)]
#[tokio::test]
async fn publication_barriers_retry_after_each_directory_failure() {
    use std::os::unix::fs::MetadataExt;

    for operation in ["put_sync_shard", "put_sync_parent", "put_sync_root"] {
        let (_dir, store) = store(0);
        let bytes = b"retry a visible but unacknowledged blob".to_vec();
        let content_ref = ContentRef::from_digest_bytes(blake3::hash(&bytes).as_bytes());
        sync_hook::install_publication(store.root(), Some(operation));
        let error = store.put(bytes.clone()).await.unwrap_err();
        assert!(error.to_string().contains(operation), "{error}");
        let path = shard_path(store.root(), &content_ref);
        let inode = fs::metadata(&path).unwrap().ino();
        assert_eq!(fs::read(&path).unwrap(), bytes);

        let reopened = FsBlobStore::open_existing(store.root().to_path_buf(), 0).unwrap();
        let failed_retry = sync_hook::install_publication(reopened.root(), Some(operation));
        let error = reopened.put(bytes.clone()).await.unwrap_err();
        assert!(error.to_string().contains(operation), "{error}");
        assert!(!failed_retry
            .completed
            .lock()
            .unwrap()
            .contains(&"put_persist"));
        let repaired = sync_hook::install_publication(reopened.root(), None);
        assert_eq!(reopened.put(bytes.clone()).await.unwrap(), content_ref);
        assert_eq!(
            *repaired.completed.lock().unwrap(),
            [
                "put_fsync",
                "put_sync_shard",
                "put_sync_parent",
                "put_sync_root"
            ]
        );
        assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn publication_barriers_fault_is_scoped_to_one_root_and_put() {
    let (_a, first) = store(0);
    let (_b, second) = store(0);
    sync_hook::install_publication(first.root(), Some("put_sync_shard"));
    second.put(b"second".to_vec()).await.unwrap();
    assert!(first.put(b"first".to_vec()).await.is_err());
    first.put(b"first".to_vec()).await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn publication_barriers_keep_open_handles_when_root_path_changes() {
    let (dir, store) = store(0);
    let root = store.root().to_path_buf();
    let moved = dir.path().join("moved-root");
    let hook = sync_hook::install_publication(&root, None);
    let moved_for_hook = moved.clone();
    hook.on_step("put_sync_shard", move || {
        fs::rename(&root, moved_for_hook).unwrap();
        // Reopening this spelling must fail; retained directory handles
        // must still complete the publication against the original tree.
        std::os::unix::fs::symlink(&root, &root).unwrap();
    });
    let bytes = b"pinned publication".to_vec();
    let content_ref = store.put(bytes.clone()).await.unwrap();
    assert_eq!(fs::read(shard_path(&moved, &content_ref)).unwrap(), bytes);
    assert_eq!(
        *hook.completed.lock().unwrap(),
        [
            "put_fsync",
            "put_persist",
            "put_sync_shard",
            "put_sync_parent",
            "put_sync_root"
        ]
    );
    assert!(store.put(bytes).await.is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn publication_barriers_reopen_in_a_fresh_process() {
    let (_dir, store) = store(0);
    let bytes = b"fresh process publication".to_vec();
    let content_ref = store.put(bytes).await.unwrap();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "stores::blob::tests::publication_barriers_process_reader",
            "--ignored",
            "--nocapture",
        ])
        .env("KHIVE_TEST_BLOB_PUBLICATION_ROOT", store.root())
        .env("KHIVE_TEST_BLOB_PUBLICATION_REF", content_ref.as_str())
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    assert!(String::from_utf8_lossy(&result.stdout).contains("verified published object"));
}

#[cfg(unix)]
#[test]
#[ignore = "subprocess helper for publication_barriers_reopen_in_a_fresh_process"]
fn publication_barriers_process_reader() {
    let root = PathBuf::from(std::env::var_os("KHIVE_TEST_BLOB_PUBLICATION_ROOT").unwrap());
    let content_ref =
        ContentRef::from_hex(std::env::var("KHIVE_TEST_BLOB_PUBLICATION_REF").unwrap()).unwrap();
    let store = FsBlobStore::open_existing(root, 0).unwrap();
    let bytes = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(store.get_bounded_verified(&content_ref, 1024))
        .unwrap();
    assert_eq!(bytes, b"fresh process publication");
    println!("verified published object");
}

/// Build the exact historical V20 prefix without invoking the V21
/// zero-reference fast path in [`crate::run_migrations`].
fn prepare_v20_gc_fixture(conn: &mut rusqlite::Connection) {
    conn.execute_batch(include_str!("../../sql/schema-migrations-table.sql"))
        .expect("create migration ledger");
    for migration in crate::MIGRATIONS
        .iter()
        .filter(|migration| migration.version <= 20)
    {
        let tx = conn.transaction().expect("begin historical migration");
        tx.execute_batch(migration.up)
            .expect("apply historical migration body");
        tx.execute(
            "INSERT INTO _schema_migrations (version, name, applied_at) \
                 VALUES (?1, ?2, 0)",
            rusqlite::params![migration.version, migration.name],
        )
        .expect("record historical migration");
        tx.commit().expect("commit historical migration");
    }
}

/// Build the canonical completed V21 schema used by transactional-GC
/// tests. Phase 4b owns the real cutover now, so tests exercise its schema
/// instead of retaining Phase 4a's synthetic future-schema fixture.
///
/// This stages through V21 explicitly rather than calling `run_migrations`,
/// because the GC gate admits exactly V21: running the whole chain would
/// hand these tests whatever the latest version happens to be, which the
/// gate then rejects. The assert below is what catches a drift here.
fn prepare_completed_v21_gc_fixture(conn: &mut rusqlite::Connection) {
    prepare_v20_gc_fixture(conn);
    let admission = crate::pool::WriteAdmission::for_migration_policy(
        conn.path()
            .filter(|path| !path.is_empty())
            .map(std::path::PathBuf::from),
        &crate::migrations::migration_test_policy(),
    )
    .expect("fixture admission policy");
    crate::migrations::stage_attachment_cutover_with_admission(conn, &admission)
        .expect("stage canonical completed V21");
    crate::migrations::finalize_attachment_cutover_with_admission(conn, &admission)
        .expect("finalize canonical completed V21");
    let version =
        crate::migrations::read_schema_version(conn).expect("read canonical completed V21 ledger");
    assert_eq!(
        version,
        crate::migrations::ATTACHMENT_CUTOVER_VERSION,
        "GC-gate fixtures need a completed-V21 ledger; a later migration chain \
             must provide a pinned through-V21 fixture builder for these tests"
    );
}

#[tokio::test]
async fn completed_v21_gc_gate_requires_new_indexes_and_absent_legacy_column() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    let backend = crate::StorageBackend::sqlite_for_test(&db_path).unwrap();
    {
        let mut writer = backend.pool().writer().unwrap();
        prepare_completed_v21_gc_fixture(writer.conn_mut());
    }
    assert!(blob_gc_fencing_complete(backend.sql().as_ref())
        .await
        .unwrap());

    {
        let writer = backend.pool().writer().unwrap();
        writer
            .conn()
            .execute_batch("DROP INDEX idx_attachments_content_ref")
            .unwrap();
    }
    assert!(!blob_gc_fencing_complete(backend.sql().as_ref())
        .await
        .unwrap());

    {
        let writer = backend.pool().writer().unwrap();
        writer
            .conn()
            .execute_batch(
                "CREATE INDEX idx_attachments_content_ref \
                         ON attachments(content_ref); \
                     ALTER TABLE entities ADD COLUMN content_ref TEXT",
            )
            .unwrap();
    }
    assert!(!blob_gc_fencing_complete(backend.sql().as_ref())
        .await
        .unwrap());
}

/// Table-driven acceptance matrix: starting from a fixture that passes
/// the gate, remove exactly one required table/index/trigger/marker/
/// ledger fact at a time and prove the gate rejects it, no probe or
/// abandoned-claim residue survives, and every candidate file and
/// pre-existing claim is untouched. A wrong implementation that stops
/// requiring any single one of these facts (e.g. the claims content_ref
/// index, the V21 ledger row/name/max, the marker row/`completed_at`, or
/// the INSERT fence alone) would still pass the narrower pre-existing
/// tests but fails here.
#[tokio::test]
async fn completed_v21_gate_acceptance_matrix_rejects_each_removed_fact_independently() {
    let cases: &[(&str, &str)] = &[
        (
            "blob_gc_claims_content_ref_index_dropped",
            "DROP INDEX idx_blob_gc_claims_content_ref",
        ),
        (
            "v21_ledger_row_deleted",
            "DELETE FROM _schema_migrations WHERE version = 21",
        ),
        (
            "v21_ledger_row_renamed",
            "UPDATE _schema_migrations SET name = 'not_attachments_first_class' \
                 WHERE version = 21",
        ),
        // NOTE: the ledger predicate requires the exact contiguous
        // canonical ledger {1..21} — the named V21 row, COUNT(*) = 21,
        // MIN(version) = 1, MAX(version) = 21 (version is the PRIMARY
        // KEY, so together these pin the set exactly). Anything else —
        // rows missing below V21, or any row above it — is a schema
        // epoch this gate never validated and fails closed. The
        // ahead-of-V21 arm ADDS a fact rather than removing one and so
        // lives outside this matrix, in
        // `gate_rejects_ledger_ahead_of_binary_latest`.
        (
            "below_v21_ledger_rows_deleted",
            "DELETE FROM _schema_migrations WHERE version < 21",
        ),
        (
            "marker_row_deleted",
            "DELETE FROM attachment_cutover_state WHERE singleton = 1",
        ),
        (
            // The schema's own compound CHECK constraint already forbids
            // `state = 'complete' AND completed_at IS NULL` via a plain
            // UPDATE, so this rebuilds the table without that constraint
            // to construct the row directly -- proving the gate's own
            // `completed_at IS NOT NULL` predicate rejects it too,
            // independent of the CHECK constraint's own enforcement.
            "marker_completed_at_null_while_state_complete",
            "DROP TABLE attachment_cutover_state; \
                 CREATE TABLE attachment_cutover_state ( \
                     singleton    INTEGER PRIMARY KEY CHECK (singleton = 1), \
                     state        TEXT NOT NULL CHECK (state IN ('incomplete', 'complete')), \
                     started_at   INTEGER NOT NULL, \
                     completed_at INTEGER \
                 ) STRICT; \
                 INSERT INTO attachment_cutover_state \
                     (singleton, state, started_at, completed_at) \
                 VALUES (1, 'complete', 21, NULL)",
        ),
        (
            "insert_fence_dropped_alone",
            "DROP TRIGGER attachments_reject_claimed_blob_insert",
        ),
    ];

    for (case, mutation_sql) in cases {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = crate::StorageBackend::sqlite_for_test(&db_path).unwrap();
        {
            let mut writer = backend.pool().writer().unwrap();
            prepare_completed_v21_gc_fixture(writer.conn_mut());
            writer
                .conn_mut()
                .execute_batch(mutation_sql)
                .unwrap_or_else(|e| panic!("case {case}: failed to apply mutation: {e}"));
        }

        assert!(
            !blob_gc_fencing_complete(backend.sql().as_ref())
                .await
                .unwrap(),
            "case {case}: gate must reject with this fact removed"
        );

        let store = Arc::new(
            FsBlobStore::new(dir.path().join("blobs"), 0)
                .unwrap()
                .with_orphan_sweep_grace(Duration::ZERO),
        );
        let orphan = store
            .put(format!("gate matrix orphan for {case}").into_bytes())
            .await
            .unwrap();
        let abandoned_ref = "c".repeat(64);
        {
            let writer = backend.pool().writer().unwrap();
            writer
                .conn()
                .execute(
                    "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) \
                         VALUES ('gate-matrix-abandoned', ?1, 1)",
                    [abandoned_ref.as_str()],
                )
                .unwrap();
        }

        let _root_guard = store.write_lock.clone().lock_owned().await;
        for dry_run in [true, false] {
            let outcome = tokio::time::timeout(
                Duration::from_secs(1),
                store.transactional_orphan_sweep(backend.sql().as_ref(), dry_run),
            )
            .await
            .unwrap_or_else(|_| panic!("case {case}: refusal must precede the root wait"));
            assert!(
                matches!(outcome, Err(StorageError::Unsupported { .. })),
                "case {case} dry_run={dry_run}: expected Unsupported, got {outcome:?}"
            );
        }

        assert!(
            store.exists(&orphan).await.unwrap(),
            "case {case}: a refused sweep must not delete anything"
        );
        let reader = backend.pool().reader().unwrap();
        let remaining: i64 = reader
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM blob_gc_claims WHERE root_key = 'gate-matrix-abandoned'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 1, "case {case}: refusal must not recover claims");

        // The gate refuses before `blob_gc_fence_probe` ever runs in
        // every one of these cases, so no probe-shaped row (the fence
        // probe's own claim/attachment id patterns) should exist in
        // either table for either dry_run mode.
        let probe_claims: i64 = reader
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM blob_gc_claims WHERE root_key GLOB '__fence_probe-*'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            probe_claims, 0,
            "case {case}: refusal must leave no fence-probe claim residue"
        );
        let probe_attachments: i64 = reader
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM attachments \
                     WHERE record_uuid GLOB '__blob-gc-fence-probe-*'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            probe_attachments, 0,
            "case {case}: refusal must leave no fence-probe attachment residue"
        );
    }
}

/// A ledger whose MAX(version) is above the V21 epoch belongs to a
/// schema epoch this gate never validated; ADR-160 admits destructive GC
/// only for the EXACT completed V21 epoch, so anything ahead fails
/// closed. This is the arm the removal matrix above cannot host (it ADDS
/// a ledger fact), and it separates the exact-epoch predicate from the
/// fail-open `>= 21`: under `>= 21` this fixture passes the gate. The
/// appended row is shaped exactly like a row `run_migrations` records —
/// canonical name style, real timestamp — because "a migration this same
/// binary applied on top" is the case the exact-epoch rule exists for.
/// While the binary's terminal version IS 21, this fixture cannot
/// distinguish `= 21` from the former `BETWEEN 21 AND terminal` — both
/// refuse a V22 row — so the exact-epoch property is enforced by the
/// predicate's text and by the contiguity clause the removal matrix
/// binds (`below_v21_ledger_rows_deleted`); the first appended migration
/// makes the ahead distinction observable.
#[tokio::test]
async fn gate_rejects_ledger_ahead_of_binary_latest() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    let backend = crate::StorageBackend::sqlite_for_test(&db_path).unwrap();
    {
        let mut writer = backend.pool().writer().unwrap();
        prepare_completed_v21_gc_fixture(writer.conn_mut());
        // Positive control: the untouched fixture passes.
    }
    assert!(
        blob_gc_fencing_complete(backend.sql().as_ref())
            .await
            .unwrap(),
        "control: completed fixture at the binary's latest version must pass"
    );

    {
        let writer = backend.pool().writer().unwrap();
        writer
            .conn()
            .execute(
                "INSERT INTO _schema_migrations (version, name, applied_at) \
                     VALUES (?1, 'post_cutover_feature', unixepoch())",
                [i64::from(crate::migrations::latest_schema_version()) + 1],
            )
            .unwrap();
    }
    assert!(
        !blob_gc_fencing_complete(backend.sql().as_ref())
            .await
            .unwrap(),
        "a ledger ahead of the binary's latest schema version must fail closed"
    );
}

/// An incomplete migration history must fail closed even when its
/// terminal row looks right: delete every ledger row below V21 while
/// keeping the V21 row, so the named-row check AND `MAX(version) = 21`
/// both still hold. A predicate reading only those two facts admits this
/// ledger; only the contiguity clause (COUNT/MIN/MAX over the
/// PRIMARY-KEY `version` column) rejects it, so removing that clause
/// turns this test red.
#[tokio::test]
async fn gate_rejects_incomplete_ledger_behind_v21() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    let backend = crate::StorageBackend::sqlite_for_test(&db_path).unwrap();
    {
        let mut writer = backend.pool().writer().unwrap();
        prepare_completed_v21_gc_fixture(writer.conn_mut());
    }
    assert!(
        blob_gc_fencing_complete(backend.sql().as_ref())
            .await
            .unwrap(),
        "control: the untouched completed fixture must pass"
    );

    {
        let writer = backend.pool().writer().unwrap();
        writer
            .conn()
            .execute_batch("DELETE FROM _schema_migrations WHERE version < 21")
            .unwrap();
        // The facts the old predicate read are still intact.
        let (v21_named, max_version): (i64, i64) = writer
            .conn()
            .query_row(
                "SELECT (SELECT COUNT(*) FROM _schema_migrations \
                             WHERE version = 21 AND name = 'attachments_first_class'), \
                            (SELECT MAX(version) FROM _schema_migrations)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (v21_named, max_version),
            (1, 21),
            "fixture must keep the named V21 row and MAX(version) = 21 so the \
                 rejection can only come from the contiguity clause"
        );
    }
    assert!(
        !blob_gc_fencing_complete(backend.sql().as_ref())
            .await
            .unwrap(),
        "an incomplete ledger behind V21 must fail closed despite a valid terminal row"
    );
}

/// A read failure while evaluating the completed-marker/ledger predicate
/// (a malformed or partially-migrated `attachment_cutover_state`) must
/// propagate as an error, not be silently treated as "not complete" via
/// some default-to-false path that could theoretically be confused with
/// a permissive read elsewhere. The gate's three `query_scalar` calls all
/// use `?`, so this pins that direction rather than leaving it assumed.
#[tokio::test]
async fn completed_v21_gate_fails_closed_when_marker_read_errors() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    let backend = crate::StorageBackend::sqlite_for_test(&db_path).unwrap();
    {
        let mut writer = backend.pool().writer().unwrap();
        prepare_completed_v21_gc_fixture(writer.conn_mut());
        // `ALTER TABLE ... DROP COLUMN` refuses outright here because the
        // table's own compound CHECK constraint still references
        // `completed_at`; rebuild the table without the column instead
        // to get a genuine "no such column" read failure.
        writer
            .conn_mut()
            .execute_batch(
                "DROP TABLE attachment_cutover_state; \
                     CREATE TABLE attachment_cutover_state ( \
                         singleton  INTEGER PRIMARY KEY CHECK (singleton = 1), \
                         state      TEXT NOT NULL CHECK (state IN ('incomplete', 'complete')), \
                         started_at INTEGER NOT NULL \
                     ) STRICT; \
                     INSERT INTO attachment_cutover_state (singleton, state, started_at) \
                     VALUES (1, 'complete', 21)",
            )
            .unwrap();
    }

    let gate_error = blob_gc_fencing_complete(backend.sql().as_ref())
        .await
        .expect_err("a marker read error must propagate, not silently resolve to false");
    assert!(
        !matches!(gate_error, StorageError::Unsupported { .. }),
        "a read error is a distinct failure from the typed epoch refusal: {gate_error:?}"
    );

    let store = Arc::new(
        FsBlobStore::new(dir.path().join("blobs"), 0)
            .unwrap()
            .with_orphan_sweep_grace(Duration::ZERO),
    );
    let orphan = store
        .put(b"marker read error orphan".to_vec())
        .await
        .unwrap();
    let _root_guard = store.write_lock.clone().lock_owned().await;
    for dry_run in [true, false] {
        let outcome = tokio::time::timeout(
            Duration::from_secs(1),
            store.transactional_orphan_sweep(backend.sql().as_ref(), dry_run),
        )
        .await
        .expect("a marker read error must fail before waiting on the root lock");
        assert!(
            outcome.is_err(),
            "dry_run={dry_run}: expected the sweep to fail closed, got {outcome:?}"
        );
    }
    assert!(store.exists(&orphan).await.unwrap());
}

#[test]
fn database_sweep_owner_is_keyed_by_database_not_blob_root() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("khive.db");
    let same_database_a = sweep_lock_for_database(Some(&database));
    let same_database_b = sweep_lock_for_database(Some(&database));
    let other_database = sweep_lock_for_database(Some(&dir.path().join("other.db")));
    let mut expected_lock_path = database.as_os_str().to_os_string();
    expected_lock_path.push(DATABASE_GC_LOCK_SUFFIX);

    assert!(Arc::ptr_eq(&same_database_a, &same_database_b));
    assert!(!Arc::ptr_eq(&same_database_a, &other_database));
    assert_eq!(
        database_gc_lock_path(&database),
        PathBuf::from(expected_lock_path)
    );
}

#[tokio::test]
async fn database_gc_owner_holds_process_and_advisory_fences_until_drop() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("owner.db");
    let backend = crate::StorageBackend::sqlite_for_test(&database).unwrap();
    let owner = acquire_database_gc_owner(backend.sql().as_ref())
        .await
        .unwrap();
    let canonical_database = owner
        .database_path()
        .expect("file-backed owner path")
        .to_path_buf();

    assert!(
        sweep_lock_for_database(Some(&canonical_database))
            .try_acquire()
            .is_none(),
        "boot and sweep must share one process-local database owner"
    );
    let external = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(database_gc_lock_path(&canonical_database))
        .unwrap();
    assert!(
        matches!(
            fs4::FileExt::try_lock(&external),
            Err(fs4::TryLockError::WouldBlock)
        ),
        "the reusable owner must also retain the cross-process advisory fence"
    );

    drop(owner);
    fs4::FileExt::try_lock(&external).expect("owner drop releases advisory fence");
}

#[cfg(unix)]
#[test]
fn database_gc_lock_path_preserves_non_utf8_identity() {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let database = PathBuf::from(std::ffi::OsString::from_vec(
        b"khive-non-utf8-\xff.db".to_vec(),
    ));
    let lock_path = database_gc_lock_path(&database);
    let mut expected = database.as_os_str().as_bytes().to_vec();
    expected.extend_from_slice(DATABASE_GC_LOCK_SUFFIX.as_bytes());
    assert_eq!(lock_path.as_os_str().as_bytes(), expected);
}

/// A shard directory replaced by a symlink (an attacker with write access
/// to the blob root, a misconfigured shared parent, or a race between
/// this sweep's directory walk and its physical delete) must be refused,
/// never followed, and an unrelated real shard must keep sweeping
/// normally. This is the fix for the shard-directory symlink-replacement
/// hazard: a plain `fs::remove_file(shard_path(root, content_ref))`
/// would resolve straight through the symlink and unlink whatever file
/// its target names.
#[cfg(unix)]
#[tokio::test]
async fn unlink_blob_shard_refuses_symlinked_shard_dir_and_still_sweeps_real_shard() {
    let (dir, store) = store(0);
    let root = dir.path().join("blobs");

    // Real, non-attacked blob: written through the normal `put` path and
    // must still sweep after the fix.
    let real = store.put(b"real blob content".to_vec()).await.unwrap();

    // Attack setup: a `content_ref` whose first shard directory does not
    // exist yet is replaced by a symlink to a directory entirely outside
    // the blob root, with a file planted at the exact name a path-based
    // delete would target.
    let outside = tempfile::tempdir().unwrap();
    let victim = outside.path().join("victim.txt");
    fs::write(&victim, b"do not delete me").unwrap();

    let real_prefix = &real.as_str()[0..2];
    let attack_prefix = if real_prefix == "aa" { "bb" } else { "aa" };
    let fake_ref = ContentRef::from_hex(format!("{attack_prefix}{}", "0".repeat(62))).unwrap();
    let fake_hex = fake_ref.as_str().to_string();

    let shard1 = root.join(attack_prefix);
    std::os::unix::fs::symlink(outside.path(), &shard1).unwrap();
    // Plant the file a naive path-based delete would actually resolve
    // to through the symlink: `<outside>/<shard2>/<full-hex>` is never
    // reached because the fix refuses at the `shard1` open itself, but
    // planting it this deep proves the refusal isn't accidental — there
    // was a real target for the naive path to have deleted.
    fs::create_dir_all(outside.path().join(&fake_hex[2..4])).unwrap();
    fs::write(
        outside.path().join(&fake_hex[2..4]).join(&fake_hex),
        b"decoy",
    )
    .unwrap();

    let root_handle = open_blob_root_handle(&root).unwrap();
    let error = unlink_blob_shard_file_no_follow(&root, &root_handle, &fake_ref).unwrap_err();
    // `O_DIRECTORY | O_NOFOLLOW` against a symlink refuses to follow it,
    // but the exact errno is platform-dependent: Linux reports `ELOOP`,
    // Darwin reports `ENOTDIR` (the symlink itself is not a directory
    // once `O_NOFOLLOW` stops it from being resolved). Either way it
    // must be an outright refusal, not a successful open/unlink.
    assert!(
        matches!(
            error.raw_os_error(),
            Some(libc::ELOOP) | Some(libc::ENOTDIR)
        ),
        "opening a symlinked shard directory must be refused, not followed; got: {error}"
    );
    assert!(
        victim.exists(),
        "the file outside the blob root must never be touched by a refused shard-dir open"
    );

    // The unrelated real shard, never touched by the attack, must still
    // unlink normally after the fix. `orphan_sweep` is disabled in this
    // compatibility release (it cannot prove a completed V21 epoch), so
    // this exercises the same underlying primitive directly rather than
    // going through that disabled API.
    unlink_blob_shard_file_no_follow(&root, &root_handle, &real).unwrap();
    assert!(!store.exists(&real).await.unwrap());
}

/// Block on `rx.recv()` on a dedicated thread so a `#[tokio::test]`
/// (current-thread runtime) doesn't stall other spawned tasks while
/// waiting on a `sync_hook` signal: the deterministic,
/// event-driven replacement for fixed-sleep / fixed-duration-poll
/// assertions.
async fn recv_blocking(rx: std::sync::mpsc::Receiver<()>) -> bool {
    tokio::task::spawn_blocking(move || rx.recv().is_ok())
        .await
        .expect("recv_blocking thread panicked")
}

#[tokio::test]
async fn put_bounded_get_roundtrip() {
    let (_dir, store) = store(0);
    let bytes = b"hello blob store".to_vec();
    let content_ref = store.put(bytes.clone()).await.unwrap();
    let fetched = store
        .get_bounded_verified(&content_ref, bytes.len() as u64)
        .await
        .unwrap();
    assert_eq!(fetched, bytes);
}

#[tokio::test]
async fn bounded_verified_get_accepts_exact_and_portable_maximum_limits() {
    let (_dir, store) = store(0);
    let bytes = b"bounded fs blob".to_vec();
    let content_ref = store.put(bytes.clone()).await.unwrap();

    assert_eq!(
        store
            .get_bounded_verified(&content_ref, bytes.len() as u64)
            .await
            .unwrap(),
        bytes
    );
    assert_eq!(
        store
            .get_bounded_verified(&content_ref, MAX_BLOB_WHOLE_BYTES)
            .await
            .unwrap(),
        bytes
    );
}

#[cfg(unix)]
#[tokio::test]
async fn bounded_verified_get_resolves_a_configured_symlink_root_once() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("blob-target");
    fs::create_dir(&target).unwrap();
    let configured = dir.path().join("blob-configured");
    symlink(&target, &configured).unwrap();

    let store = FsBlobStore::new(configured, 0).unwrap();
    assert_eq!(store.root(), target.canonicalize().unwrap());
    let bytes = b"symlink-configured root".to_vec();
    let content_ref = store.put(bytes.clone()).await.unwrap();
    assert_eq!(
        store
            .get_bounded_verified(&content_ref, bytes.len() as u64)
            .await
            .unwrap(),
        bytes
    );
}

#[cfg(unix)]
#[tokio::test]
async fn fs_blob_store_refuses_root_replacement_before_put() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("blobs");
    let store = FsBlobStore::new(root.clone(), 0).unwrap();
    let original_root = dir.path().join("blobs-original");
    fs::rename(&root, &original_root).unwrap();

    let redirected_root = tempfile::tempdir().unwrap();
    symlink(redirected_root.path(), &root).unwrap();

    let bytes = b"must not land in a replaced root".to_vec();
    let content_ref = ContentRef::from_digest_bytes(blake3::hash(&bytes).as_bytes());
    store
        .put(bytes)
        .await
        .expect_err("a root replaced after construction must be refused");

    assert!(
        !shard_path(redirected_root.path(), &content_ref).exists(),
        "put must not publish into the tree selected by the replacement symlink"
    );
    assert!(
        !shard_path(&original_root, &content_ref).exists(),
        "a refused put must not mutate the initialization-time root either"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn fs_blob_store_refuses_ancestor_replacement_for_existing_blob_operations() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let ancestor = dir.path().join("store-parent");
    let root = ancestor.join("blobs");
    fs::create_dir(&ancestor).unwrap();
    let store = FsBlobStore::new(root.clone(), 0).unwrap();
    let bytes = b"same bytes in both trees".to_vec();
    let content_ref = store.put(bytes.clone()).await.unwrap();

    let original_ancestor = dir.path().join("store-parent-original");
    fs::rename(&ancestor, &original_ancestor).unwrap();
    let original_blob = shard_path(&original_ancestor.join("blobs"), &content_ref);

    let redirected_ancestor = tempfile::tempdir().unwrap();
    let redirected_blob = shard_path(&redirected_ancestor.path().join("blobs"), &content_ref);
    fs::create_dir_all(redirected_blob.parent().unwrap()).unwrap();
    fs::write(&redirected_blob, &bytes).unwrap();
    symlink(redirected_ancestor.path(), &ancestor).unwrap();

    store
        .get_bounded_verified(&content_ref, bytes.len() as u64)
        .await
        .expect_err("a read through a replaced root ancestor must be refused");
    store
        .exists(&content_ref)
        .await
        .expect_err("exists through a replaced root ancestor must be refused");
    store
        .size(&content_ref)
        .await
        .expect_err("size through a replaced root ancestor must be refused");
    store
        .delete(&content_ref)
        .await
        .expect_err("delete through a replaced root ancestor must be refused");

    assert!(
        original_blob.exists(),
        "refusal must preserve the initialization-time blob"
    );
    assert!(
        redirected_blob.exists(),
        "refusal must not read as authority or delete the redirected blob"
    );
}

#[tokio::test]
async fn bounded_verified_get_rejects_same_size_digest_corruption() {
    let (_dir, store) = store(0);
    let expected_bytes = b"expected".to_vec();
    let actual_bytes = b"mutated!".to_vec();
    let expected = store.put(expected_bytes).await.unwrap();
    let actual = ContentRef::from_digest_bytes(blake3::hash(&actual_bytes).as_bytes());
    fs::write(shard_path(store.root(), &expected), actual_bytes).unwrap();

    let err = store.get_bounded_verified(&expected, 8).await.unwrap_err();
    assert!(matches!(
        err,
        StorageError::BlobDigestMismatch {
            expected: ref got_expected,
            actual: ref got_actual,
        } if got_expected == &expected && got_actual == &actual
    ));
}

#[tokio::test]
async fn bounded_verified_get_stops_at_max_plus_one_after_file_growth() {
    let (_dir, store) = store(0);
    let store = Arc::new(store);
    let content_ref = store.put(b"abcd".to_vec()).await.unwrap();
    let path = shard_path(store.root(), &content_ref);
    let (reached, release) = bounded_read_sync_hook::install(store.root());

    let read_store = Arc::clone(&store);
    let read_ref = content_ref.clone();
    let read = tokio::spawn(async move { read_store.get_bounded_verified(&read_ref, 4).await });
    assert!(
        recv_blocking(reached).await,
        "read must reach the metadata seam"
    );
    let mut writer = fs::OpenOptions::new().append(true).open(&path).unwrap();
    writer.write_all(b"efgh-poison-tail").unwrap();
    writer.flush().unwrap();
    release.send(()).unwrap();

    let err = read.await.unwrap().unwrap_err();
    assert!(matches!(
        err,
        StorageError::BlobTooLarge {
            content_ref: ref got,
            max_bytes: 4,
            observed_at_least: 5,
        } if got == &content_ref
    ));
}

#[tokio::test]
async fn bounded_verified_get_reports_growth_within_limit_as_size_mismatch() {
    let (_dir, store) = store(0);
    let store = Arc::new(store);
    let content_ref = store.put(b"abcd".to_vec()).await.unwrap();
    let path = shard_path(store.root(), &content_ref);
    let (reached, release) = bounded_read_sync_hook::install(store.root());

    let read_store = Arc::clone(&store);
    let read_ref = content_ref.clone();
    let read = tokio::spawn(async move { read_store.get_bounded_verified(&read_ref, 8).await });
    assert!(
        recv_blocking(reached).await,
        "read must reach the metadata seam"
    );
    let mut writer = fs::OpenOptions::new().append(true).open(&path).unwrap();
    writer.write_all(b"ef").unwrap();
    writer.flush().unwrap();
    release.send(()).unwrap();

    let err = read.await.unwrap().unwrap_err();
    assert!(matches!(
        err,
        StorageError::BlobSizeMismatch {
            content_ref: ref got,
            metadata_bytes: 4,
            actual_bytes: 6,
        } if got == &content_ref
    ));
}

#[tokio::test]
async fn bounded_verified_get_reports_truncation_as_size_mismatch() {
    let (_dir, store) = store(0);
    let store = Arc::new(store);
    let content_ref = store.put(b"abcd".to_vec()).await.unwrap();
    let path = shard_path(store.root(), &content_ref);
    let (reached, release) = bounded_read_sync_hook::install(store.root());

    let read_store = Arc::clone(&store);
    let read_ref = content_ref.clone();
    let read = tokio::spawn(async move { read_store.get_bounded_verified(&read_ref, 4).await });
    assert!(
        recv_blocking(reached).await,
        "read must reach the metadata seam"
    );
    let mut writer = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&path)
        .unwrap();
    writer.write_all(b"abc").unwrap();
    writer.flush().unwrap();
    release.send(()).unwrap();

    let err = read.await.unwrap().unwrap_err();
    assert!(matches!(
        err,
        StorageError::BlobSizeMismatch {
            content_ref: ref got,
            metadata_bytes: 4,
            actual_bytes: 3,
        } if got == &content_ref
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn bounded_verified_get_keeps_the_opened_inode_when_the_path_is_replaced() {
    let (_dir, store) = store(0);
    let store = Arc::new(store);
    let original = b"original".to_vec();
    let replacement = b"replaced".to_vec();
    let content_ref = store.put(original.clone()).await.unwrap();
    let path = shard_path(store.root(), &content_ref);
    let moved_path = path.with_extension("opened-inode");
    let max_bytes = original.len() as u64;
    let (reached, release) = bounded_read_sync_hook::install(store.root());

    let read_store = Arc::clone(&store);
    let read_ref = content_ref.clone();
    let read =
        tokio::spawn(async move { read_store.get_bounded_verified(&read_ref, max_bytes).await });
    assert!(
        recv_blocking(reached).await,
        "read must reach the metadata seam"
    );
    fs::rename(&path, &moved_path).unwrap();
    fs::write(&path, replacement).unwrap();
    release.send(()).unwrap();

    assert_eq!(read.await.unwrap().unwrap(), original);
}

#[cfg(unix)]
#[tokio::test]
async fn bounded_verified_get_refuses_a_symlink_leaf() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let store = FsBlobStore::new(dir.path().join("blobs"), 0).unwrap();
    let outside_bytes = b"outside but digest matching".to_vec();
    let content_ref = ContentRef::from_digest_bytes(blake3::hash(&outside_bytes).as_bytes());
    let outside = dir.path().join("outside");
    fs::write(&outside, &outside_bytes).unwrap();
    let leaf = shard_path(store.root(), &content_ref);
    fs::create_dir_all(leaf.parent().unwrap()).unwrap();
    symlink(&outside, &leaf).unwrap();

    let err = store
        .get_bounded_verified(&content_ref, outside_bytes.len() as u64)
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::Driver { .. }), "got {err:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn bounded_verified_get_refuses_a_symlinked_shard_component() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let store = FsBlobStore::new(dir.path().join("blobs"), 0).unwrap();
    let outside_bytes = b"outside through shard link".to_vec();
    let content_ref = ContentRef::from_digest_bytes(blake3::hash(&outside_bytes).as_bytes());
    let hex = content_ref.as_str();
    let outside_shard1 = dir.path().join("outside-shard1");
    let outside_shard2 = outside_shard1.join(&hex[2..4]);
    fs::create_dir_all(&outside_shard2).unwrap();
    fs::write(outside_shard2.join(hex), &outside_bytes).unwrap();
    symlink(&outside_shard1, store.root().join(&hex[0..2])).unwrap();

    let err = store
        .get_bounded_verified(&content_ref, outside_bytes.len() as u64)
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::Driver { .. }), "got {err:?}");
}

#[tokio::test]
async fn put_content_ref_matches_blake3_digest() {
    let (_dir, store) = store(0);
    let bytes = b"digest check".to_vec();
    let content_ref = store.put(bytes.clone()).await.unwrap();
    let expected = ContentRef::from_digest_bytes(blake3::hash(&bytes).as_bytes());
    assert_eq!(content_ref, expected);
}

#[tokio::test]
async fn put_dedups_identical_content() {
    let (_dir, store) = store(0);
    let bytes = b"same bytes twice".to_vec();
    let first = store.put(bytes.clone()).await.unwrap();
    let second = store.put(bytes.clone()).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(
        store
            .get_bounded_verified(&first, bytes.len() as u64)
            .await
            .unwrap(),
        bytes
    );
}

#[tokio::test]
async fn exists_reflects_put_and_delete() {
    let (_dir, store) = store(0);
    let bytes = b"exists check".to_vec();
    let content_ref = store.put(bytes).await.unwrap();
    assert!(store.exists(&content_ref).await.unwrap());

    assert!(store.delete(&content_ref).await.unwrap());
    assert!(!store.exists(&content_ref).await.unwrap());
}

#[tokio::test]
async fn delete_missing_content_ref_returns_false() {
    let (_dir, store) = store(0);
    let missing = ContentRef::from_hex("f".repeat(64)).unwrap();
    assert!(!store.delete(&missing).await.unwrap());
}

#[tokio::test]
async fn size_reports_byte_length_for_a_present_object() {
    let (_dir, store) = store(0);
    let bytes = b"size check".to_vec();
    let content_ref = store.put(bytes.clone()).await.unwrap();
    assert_eq!(
        store.size(&content_ref).await.unwrap(),
        Some(bytes.len() as u64)
    );
}

#[tokio::test]
async fn size_returns_none_for_an_absent_object() {
    let (_dir, store) = store(0);
    let missing = ContentRef::from_hex("9".repeat(64)).unwrap();
    assert_eq!(store.size(&missing).await.unwrap(), None);
}

#[tokio::test]
async fn bounded_get_missing_content_ref_returns_not_found() {
    let (_dir, store) = store(0);
    let missing = ContentRef::from_hex("e".repeat(64)).unwrap();
    let err = store
        .get_bounded_verified(&missing, MAX_BLOB_WHOLE_BYTES)
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::NotFound { .. }));
}

#[tokio::test]
async fn put_refuses_below_free_space_floor() {
    // A floor no real disk clears -> put must fail closed, not silently
    // degrade or spill elsewhere (khive#292 SPEC-gate ruling).
    let (_dir, store) = store(u64::MAX);
    let err = store.put(b"too big a floor".to_vec()).await.unwrap_err();
    match err {
        StorageError::CapacityFloor {
            floor_bytes,
            available_bytes,
            ..
        } => {
            assert_eq!(floor_bytes, u64::MAX);
            assert!(available_bytes < u64::MAX);
        }
        other => panic!("expected CapacityFloor, got {other:?}"),
    }
}

#[tokio::test]
async fn capacity_floor_error_names_the_floor_and_volume() {
    let (_dir, store) = store(u64::MAX);
    let err = store.put(b"x".to_vec()).await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains(&u64::MAX.to_string()),
        "must name the floor: {msg}"
    );
    assert!(msg.contains("Blob"), "must name the capability: {msg}");
}

#[test]
fn put_refuses_a_write_that_would_cross_the_floor_even_though_available_alone_clears_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("blobs");
    fs::create_dir_all(&root).unwrap();

    // Use the same blocking path as `FsBlobStore::put`, with a fixed
    // capacity snapshot: 101 bytes clears a 100-byte floor by itself,
    // but a pending two-byte write would leave only 99 bytes. Sampling
    // the host-wide APFS free-space gauge here made the old test flaky:
    // unrelated cleanup could legitimately replenish more than its
    // 64 MiB cushion between the test's sample and the put's sample.
    let err = put_blocking_with_space_probe(&root, 100, vec![7u8; 2], |_| Ok(101)).unwrap_err();
    assert!(
        matches!(err, StorageError::CapacityFloor { .. }),
        "a write-size-aware floor check must reject a write that pushes the volume \
             below the floor even though available space alone still clears it: {err:?}"
    );
}

#[test]
fn a_later_put_checks_a_fresh_capacity_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("blobs");
    fs::create_dir_all(&root).unwrap();

    // Model the two snapshots observed by serialized puts without tying
    // the assertion to a host-wide free-space gauge. The first two-byte
    // write may land exactly on the 100-byte floor from a 102-byte
    // snapshot. A later, different write sees 101 bytes and must refuse.
    // Mutual exclusion itself is covered deterministically below by the
    // shared-root lock test; together the tests prove the stale-snapshot
    // race is closed without relying on unrelated filesystem activity.
    let first = put_blocking_with_space_probe(&root, 100, vec![1u8; 2], |_| Ok(102));
    let second = put_blocking_with_space_probe(&root, 100, vec![2u8; 2], |_| Ok(101));

    assert!(
        first.is_ok(),
        "the first put may land on the floor: {first:?}"
    );
    assert!(
        matches!(second, Err(StorageError::CapacityFloor { .. })),
        "the later put must use its lower capacity snapshot: {second:?}"
    );
}

#[tokio::test]
async fn concurrent_puts_from_two_independently_constructed_stores_share_the_root_lock() {
    // The actual gap in the prior fix: the
    // test above uses ONE `FsBlobStore` behind a shared `Arc`, so it
    // exercises only the per-instance mutex and cannot catch a missing
    // cross-instance guarantee. `StorageBackend::blob_store` constructs
    // a FRESH `FsBlobStore` on every call, even for the same root -- so
    // the real regression is two SEPARATELY CONSTRUCTED stores for the
    // same root. Before the shared canonical-root registry, each
    // store's `write_lock` was its own independent `Mutex`, and this
    // exact scenario would have let both puts pass the same free-space
    // snapshot.
    //
    // The earlier version of this test let
    // two real `tokio::spawn`ed puts race with no control over
    // interleaving -- it could PASS on the prior per-instance-mutex
    // bug purely because the blocking thread pool happened to run them
    // sequentially, which is not a deterministic regression guard.
    //
    // The first `sync_hook`-driven attempt kept proving exclusion
    // INDIRECTLY, through a free-space floor sized to admit exactly one
    // `payload_len` write -- but this dev box's real
    // `fs4::available_space` swings by many tens to hundreds of MB in
    // either direction over the several-second window the hook
    // orchestration takes (concurrent fleet `cargo clean`/build
    // activity), and no floor margin proved robust: it was observed to
    // both under-shoot (store_a's own write refused; available_bytes
    // 25521500160 vs floor_bytes 25517096960, a ~60 MiB drop) and
    // over-shoot (store_b's write unexpectedly SUCCEEDED after
    // store_a's landed) in back-to-back runs.
    //
    // Lock sharing is orthogonal to floor arithmetic -- the same
    // `crosses_floor`/`put_blocking` path runs regardless of which
    // `FsBlobStore` instance calls it, and that arithmetic is already
    // covered deterministically by `a_later_put_checks_a_fresh_capacity_snapshot`
    // and the pure `crosses_floor` unit tests above.
    //
    // The prior fix's negative proof (B
    // must not reach its own checkpoint) still leaned on a 200ms
    // `recv_timeout` as the CORRECTNESS decision -- under sufficiently
    // delayed scheduling, old per-instance-mutex code's B could simply
    // arrive after the window and every assertion would still pass,
    // silently defeating the regression guard. Fix: assert directly
    // and immediately (no timeout, no second hook, no second
    // `tokio::spawn` racing at all) that `store_b.write_lock` -- a
    // private field, reachable here because `tests` is a child module
    // of the module that declares it -- is ALREADY held the instant
    // store_a's put holds ITS guard. Under the fixed canonical-root
    // registry this is the exact same `Arc` store_a's own `write_lock`
    // resolves to, so `try_lock()` fails with zero timing dependence;
    // under the old per-instance-mutex code, `store_b.write_lock` is a
    // completely independent, unheld `Mutex`, so `try_lock()` would
    // succeed immediately, pinning the defect on the spot regardless
    // of scheduling.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("blobs");
    fs::create_dir_all(&root).unwrap();
    let canonical_root = root.canonicalize().unwrap();

    // Two INDEPENDENT `FsBlobStore::new` calls for the identical root --
    // exactly what `StorageBackend::blob_store` does on repeat calls.
    let store_a = std::sync::Arc::new(FsBlobStore::new(root.clone(), 0).unwrap());
    let store_b = std::sync::Arc::new(FsBlobStore::new(root, 0).unwrap());

    let (a_reached, a_release, _a_done) = sync_hook::install(&canonical_root);
    let a = {
        let store_a = store_a.clone();
        tokio::spawn(async move { store_a.put(b"store_a payload".to_vec()).await })
    };
    assert!(
        recv_blocking(a_reached).await,
        "store_a's put must reach the sync_hook checkpoint"
    );

    // The deterministic proof: store_b's OWN write_lock field must
    // already be unavailable while store_a holds its guard -- true
    // only if the two independently constructed stores share one
    // Arc<Mutex<()>>. No timeout, no scheduling dependence.
    assert!(
        store_b.write_lock.try_lock().is_err(),
        "store_b's write_lock was NOT held while store_a's put held its guard -- the two \
             independently constructed stores do NOT share one lock"
    );

    // Release A and let it finish. Awaiting A's outer task
    // deterministically waits for the guard to be dropped too (see
    // `put`'s inner-block scoping).
    a_release.send(()).unwrap();
    let result_a = a.await.unwrap();
    assert!(result_a.is_ok(), "store_a's put must succeed: {result_a:?}");

    // Liveness coverage: an ordinary put on store_b succeeds once
    // store_a has released the (shared) lock.
    let result_b = store_b.put(b"store_b payload".to_vec()).await;
    assert!(result_b.is_ok(), "store_b's put must succeed: {result_b:?}");
}

#[tokio::test]
async fn aborting_the_outer_put_future_does_not_release_the_guard_before_persist_completes() {
    // The prior fix held the write guard only
    // in `put`'s own async stack frame (`let _write_guard = ...
    // .lock().await`) while the `spawn_blocking` closure captured just
    // root/floor_bytes/bytes. Cancelling/dropping the outer `put`
    // future released that borrowed guard immediately, even though an
    // already-started blocking write kept running on its own thread --
    // a second put could then pass its floor check while the first
    // write was still landing.
    //
    // The earlier version of this test
    // proved the fix with a fixed 10ms sleep before abort and a fixed
    // 500x10ms poll loop waiting for the lock to free -- and the poll
    // loop actually FAILED once in a required-suite run (a
    // flaky gate, not a regression). This version uses the `sync_hook`
    // seam instead: `reached` fires only once execution is genuinely
    // inside the guarded closure (owned guard already moved in) and
    // blocks there until released; `done` fires only after the guard
    // has actually been dropped (see `put`'s inner-block scoping) --
    // both edges event-driven, no sleeps, no polling.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("blobs");
    fs::create_dir_all(&root).unwrap();
    let canonical_root = root.canonicalize().unwrap();

    let store = std::sync::Arc::new(FsBlobStore::new(root, 0).unwrap());
    let (reached, release, done) = sync_hook::install(&canonical_root);
    let handle = {
        let store = store.clone();
        tokio::spawn(async move { store.put(b"cancellation race payload".to_vec()).await })
    };

    assert!(
        recv_blocking(reached).await,
        "put must reach the sync_hook checkpoint -- owned guard already moved into the \
             closure -- before this test can mean anything"
    );

    handle.abort();
    let abort_result = handle.await;
    match &abort_result {
        Err(e) if e.is_cancelled() => {}
        other => panic!(
            "the outer task must actually have been cancelled for this test to be \
                 meaningful: {other:?}"
        ),
    }

    let shared_lock = write_lock_for_root(&canonical_root).unwrap();
    assert!(
        shared_lock.try_lock().is_err(),
        "the guard must still be held by the detached blocking write immediately after \
             the outer future was cancelled -- if this is free, the guard was released with \
             the aborted frame instead of moving into the spawn_blocking closure"
    );

    // Let the detached write proceed and finish, then wait for its
    // explicit completion signal -- no polling, no fixed durations.
    // `done` only fires after the guard is actually dropped (see
    // `put`), so the very next check is race-free.
    release.send(()).unwrap();
    assert!(
        recv_blocking(done).await,
        "the detached write must signal completion once it actually persists"
    );
    assert!(
        shared_lock.try_lock().is_ok(),
        "the guard must be free once the detached write's completion was observed"
    );
}

/// `orphan_sweep` is disabled in this compatibility release: it has no
/// `SqlAccess` capability with which to prove a completed V21 epoch, so
/// every call refuses regardless of `live_refs` contents or `dry_run`,
/// and nothing on disk is ever touched. This replaces the prior
/// `orphan_sweep_race_demonstrates_the_documented_quiescence_requirement`
/// regression, which pinned the now-eliminated caller-snapshot deletion
/// hazard this disablement closes.
#[tokio::test]
async fn orphan_sweep_is_disabled_in_both_modes_regardless_of_live_refs() {
    let (_dir, store) = store(0);
    let blob = store
        .put(b"never swept by this API".to_vec())
        .await
        .unwrap();
    let mut live_refs = std::collections::HashSet::new();
    live_refs.insert(blob.clone());

    for dry_run in [true, false] {
        let error = store
            .orphan_sweep(&BlobOrphanSweepConfig {
                live_refs: live_refs.clone(),
                dry_run,
            })
            .await
            .expect_err("caller-snapshot orphan_sweep must be disabled");
        assert!(
            matches!(error, StorageError::Unsupported { .. }),
            "expected typed Unsupported, got {error:?}"
        );
    }
    assert!(store.exists(&blob).await.unwrap());
}

/// Rollout compatibility fence: the Phase-3 binary's V20 schema cannot
/// represent a moodboard model's nested FANN network as SQL liveness.
/// Both report-only and destructive transactional sweeps must therefore
/// refuse before taking the root lock or mutating abandoned claims.
#[tokio::test]
async fn transactional_orphan_sweep_refuses_v20_before_root_or_claim_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    let backend = crate::StorageBackend::sqlite_for_test(&db_path).unwrap();
    {
        let mut writer = backend.pool().writer().unwrap();
        prepare_v20_gc_fixture(writer.conn_mut());
    }

    let root = dir.path().join("blobs");
    let store = Arc::new(
        FsBlobStore::new(root, 0)
            .unwrap()
            .with_orphan_sweep_grace(Duration::ZERO),
    );
    let bundle = store.put(b"legacy model bundle".to_vec()).await.unwrap();
    let network = store.put(b"legacy FANN network".to_vec()).await.unwrap();
    let orphan = store.put(b"ordinary old orphan".to_vec()).await.unwrap();
    let abandoned_ref = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
    {
        let writer = backend.pool().writer().unwrap();
        writer
            .conn()
            .execute(
                "INSERT INTO entities \
                     (id, namespace, kind, entity_type, name, tags, created_at, updated_at, \
                      content_ref) \
                     VALUES ('legacy-model', 'local', 'artifact', 'moodboard_model', \
                             'legacy model', '[]', 1, 1, ?1)",
                [bundle.as_str()],
            )
            .unwrap();
        writer
            .conn()
            .execute(
                "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) \
                     VALUES ('abandoned-before-compat', ?1, 1)",
                [abandoned_ref],
            )
            .unwrap();
    }

    // If the compatibility gate is below the root lock, the call parks
    // here and the timeout fails. A correct V20 refusal never waits for it.
    let _root_guard = store.write_lock.clone().lock_owned().await;
    for dry_run in [true, false] {
        let outcome = tokio::time::timeout(
            Duration::from_secs(1),
            store.transactional_orphan_sweep(backend.sql().as_ref(), dry_run),
        )
        .await
        .expect("V20 refusal must happen before waiting for the held root lock");
        let error = outcome.expect_err("V20 transactional sweep must be disabled");
        match error {
            StorageError::Unsupported {
                capability: StorageCapability::Blob,
                operation,
                message,
            } => {
                assert_eq!(operation, "transactional_orphan_sweep");
                assert!(
                    message.contains("complete V21 attachment cutover"),
                    "unexpected compatibility diagnostic: {message}"
                );
            }
            other => panic!("expected typed Unsupported refusal, got {other:?}"),
        }
    }

    let reader = backend.pool().reader().unwrap();
    let abandoned: i64 = reader
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM blob_gc_claims \
                 WHERE root_key = 'abandoned-before-compat' AND content_ref = ?1",
            [abandoned_ref],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(abandoned, 1, "V20 refusal must not clean abandoned claims");
    drop(reader);
    assert!(store.exists(&bundle).await.unwrap());
    assert!(store.exists(&network).await.unwrap());
    assert!(store.exists(&orphan).await.unwrap());
}

/// The durable marker is authoritative, not the mere presence of V21
/// tables, triggers, or even a ledger row. An interrupted/inconsistent
/// cutover remains non-sweepable for both modes and is rejected before
/// the root wait or abandoned-claim recovery.
#[tokio::test]
async fn transactional_orphan_sweep_refuses_incomplete_v21_marker_without_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    let backend = crate::StorageBackend::sqlite_for_test(&db_path).unwrap();
    let abandoned_ref = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    {
        let mut writer = backend.pool().writer().unwrap();
        prepare_completed_v21_gc_fixture(writer.conn_mut());
        writer
            .conn_mut()
            .execute(
                "UPDATE attachment_cutover_state \
                     SET state = 'incomplete', completed_at = NULL \
                     WHERE singleton = 1",
                [],
            )
            .unwrap();
        writer
            .conn_mut()
            .execute(
                "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) \
                     VALUES ('abandoned-incomplete-v21', ?1, 1)",
                [abandoned_ref],
            )
            .unwrap();
    }

    let store = Arc::new(
        FsBlobStore::new(dir.path().join("blobs"), 0)
            .unwrap()
            .with_orphan_sweep_grace(Duration::ZERO),
    );
    let orphan = store.put(b"incomplete V21 orphan".to_vec()).await.unwrap();
    let _root_guard = store.write_lock.clone().lock_owned().await;

    for dry_run in [true, false] {
        let outcome = tokio::time::timeout(
            Duration::from_secs(1),
            store.transactional_orphan_sweep(backend.sql().as_ref(), dry_run),
        )
        .await
        .expect("incomplete V21 must refuse before waiting for the root lock");
        assert!(
            matches!(outcome, Err(StorageError::Unsupported { .. })),
            "incomplete V21 must return typed Unsupported: {outcome:?}"
        );
    }

    let remaining: i64 = backend
        .pool()
        .reader()
        .unwrap()
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM blob_gc_claims \
                 WHERE root_key = 'abandoned-incomplete-v21' AND content_ref = ?1",
            [abandoned_ref],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(remaining, 1, "refusal must not recover abandoned claims");
    assert!(store.exists(&orphan).await.unwrap());
}

/// Both public sweep APIs must refuse under the same non-completed
/// epochs, in both `dry_run` modes, without touching files or claims.
/// `orphan_sweep` ignores the fixture's `SqlAccess` entirely (it has no
/// epoch capability of its own -- that is exactly why it is disabled),
/// so this proves the disablement holds even when a caller has a real,
/// otherwise-plausible database sitting right next to it.
#[tokio::test]
async fn both_sweep_apis_refuse_v20_and_incomplete_v21_epochs_in_both_modes() {
    for incomplete_v21 in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("khive.db");
        let backend = crate::StorageBackend::sqlite_for_test(&db_path).unwrap();
        {
            let mut writer = backend.pool().writer().unwrap();
            if incomplete_v21 {
                prepare_completed_v21_gc_fixture(writer.conn_mut());
                writer
                    .conn_mut()
                    .execute(
                        "UPDATE attachment_cutover_state \
                             SET state = 'incomplete', completed_at = NULL \
                             WHERE singleton = 1",
                        [],
                    )
                    .unwrap();
            } else {
                prepare_v20_gc_fixture(writer.conn_mut());
            }
        }

        let store = FsBlobStore::new(dir.path().join("blobs"), 0)
            .unwrap()
            .with_orphan_sweep_grace(Duration::ZERO);
        let orphan = store
            .put(format!("both-apis orphan (incomplete_v21={incomplete_v21})").into_bytes())
            .await
            .unwrap();

        // A known claim, seeded once per epoch case, must survive every
        // refused arm below untouched -- refusal must never recover or
        // otherwise mutate an existing claim.
        let known_claim_ref = "f".repeat(64);
        {
            let writer = backend.pool().writer().unwrap();
            writer
                .conn()
                .execute(
                    "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) \
                         VALUES ('both-api-known-claim', ?1, 1)",
                    [known_claim_ref.as_str()],
                )
                .unwrap();
        }
        let assert_known_claim_unchanged = |arm: &str| {
            let remaining: i64 = backend
                .pool()
                .reader()
                .unwrap()
                .conn()
                .query_row(
                    "SELECT COUNT(*) FROM blob_gc_claims \
                         WHERE root_key = 'both-api-known-claim' AND content_ref = ?1",
                    [known_claim_ref.as_str()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                remaining, 1,
                "incomplete_v21={incomplete_v21} arm={arm}: refusal must not mutate \
                     an existing claim"
            );
        };

        for dry_run in [true, false] {
            let snapshot_error = store
                .orphan_sweep(&BlobOrphanSweepConfig {
                    live_refs: std::collections::HashSet::new(),
                    dry_run,
                })
                .await
                .expect_err("orphan_sweep must refuse regardless of epoch");
            assert!(
                matches!(snapshot_error, StorageError::Unsupported { .. }),
                "incomplete_v21={incomplete_v21} dry_run={dry_run}: expected Unsupported \
                     from orphan_sweep, got {snapshot_error:?}"
            );
            assert_known_claim_unchanged(&format!("orphan_sweep dry_run={dry_run}"));

            let transactional_error = store
                .transactional_orphan_sweep(backend.sql().as_ref(), dry_run)
                .await
                .expect_err("transactional_orphan_sweep must refuse this epoch");
            assert!(
                matches!(transactional_error, StorageError::Unsupported { .. }),
                "incomplete_v21={incomplete_v21} dry_run={dry_run}: expected Unsupported \
                     from transactional_orphan_sweep, got {transactional_error:?}"
            );
            assert_known_claim_unchanged(&format!("transactional_orphan_sweep dry_run={dry_run}"));
        }

        assert!(
            store.exists(&orphan).await.unwrap(),
            "incomplete_v21={incomplete_v21}: a refused sweep must not delete anything"
        );
    }
}

/// The epoch recheck taken under database ownership must run before the
/// sweep ever waits on the root guard/lock, even when the epoch was
/// still valid at the read-only preflight and only regressed while the
/// call was waiting for database ownership. Without the fix this recheck
/// used to run after the root guard, filesystem root lock, and directory
/// walk -- this pins the corrected ordering with a real cross-task race
/// instead of trusting the comment above it.
///
/// The externally held blocking point is the OS-level root advisory lock
/// (`acquire_root_write_lock`), not the process-local `write_lock`
/// mutex. The old (pre-fix) ordering acquired the process-local mutex
/// before ever reaching database ownership -- holding that mutex here
/// would have starved the sweep task before it ever reached the hook
/// below, hanging this test instead of failing it. The OS-level root
/// lock is only ever acquired after database ownership under both the
/// old and the new ordering, so both orderings reach the hook; only the
/// old ordering then blocks trying to acquire the lock held here,
/// because its (mis-placed) recheck runs after that acquisition instead
/// of before it.
#[tokio::test]
async fn transactional_orphan_sweep_recheck_refuses_before_root_lock_when_epoch_regresses_after_db_ownership(
) {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    let backend = Arc::new(crate::StorageBackend::sqlite_for_test(&db_path).unwrap());
    {
        let mut writer = backend.pool().writer().unwrap();
        prepare_completed_v21_gc_fixture(writer.conn_mut());
    }
    assert!(blob_gc_fencing_complete(backend.sql().as_ref())
        .await
        .unwrap());

    let blob_root = dir.path().join("blobs");
    let store = Arc::new(
        FsBlobStore::new(blob_root.clone(), 0)
            .unwrap()
            .with_orphan_sweep_grace(Duration::ZERO),
    );
    let orphan = store
        .put(b"epoch regressed after database ownership".to_vec())
        .await
        .unwrap();

    // Hold the same OS-level root lock the real sweep acquires, on the
    // same canonicalized path it canonicalizes to.
    let canonical_root = blob_root.canonicalize().unwrap();
    let _root_write_guard = acquire_root_write_lock(&canonical_root).unwrap();

    // Key the hook by the same canonicalized path `SqlAccess::database_path`
    // returns -- not the raw `db_path` above -- since that's what the
    // implementation looks the hook up by.
    let canonical_db_path = backend.sql().database_path();
    let (reached, release) = db_ownership_sync_hook::install(canonical_db_path.as_deref());
    let sweep_store = store.clone();
    let sweep_backend = backend.clone();
    let handle = tokio::spawn(async move {
        sweep_store
            .transactional_orphan_sweep(sweep_backend.sql().as_ref(), false)
            .await
    });

    // The hook fires only once database ownership (process-local guard
    // plus the cross-process advisory lock) is held, strictly after the
    // read-only preflight already observed a valid epoch. Bounded so a
    // regression that drops the hook call (or blocks ahead of it) fails
    // this test instead of hanging it.
    let reached_signal = tokio::time::timeout(Duration::from_secs(1), recv_blocking(reached))
        .await
        .expect("the sweep must reach database ownership before this test's timeout");
    assert!(reached_signal, "hook sender was dropped before signaling");
    {
        let writer = backend.pool().writer().unwrap();
        writer
            .conn()
            .execute("DELETE FROM _schema_migrations WHERE version = 21", [])
            .unwrap();
    }
    release.send(()).unwrap();

    let outcome = tokio::time::timeout(Duration::from_secs(1), handle)
        .await
        .expect(
            "the recheck must refuse before ever waiting on the externally held root lock -- \
                 under the old (pre-fix) ordering this join times out instead, because the \
                 sweep blocks acquiring the OS-level root lock held above",
        )
        .unwrap();
    assert!(
        matches!(outcome, Err(StorageError::Unsupported { .. })),
        "expected the regressed epoch to be caught immediately after database ownership: \
             {outcome:?}"
    );
    assert!(store.exists(&orphan).await.unwrap());
}

/// A Phase-4a binary can remain in a mixed fleet after a newer binary has
/// atomically completed V21. It must then use every attachment role as
/// liveness, including a moodboard FANN network, and delete only the true
/// orphan.
#[tokio::test]
async fn transactional_orphan_sweep_accepts_completed_v21_attachment_liveness() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    let backend = crate::StorageBackend::sqlite_for_test(&db_path).unwrap();
    {
        let mut writer = backend.pool().writer().unwrap();
        prepare_completed_v21_gc_fixture(writer.conn_mut());
    }

    let store = FsBlobStore::new(dir.path().join("blobs"), 0)
        .unwrap()
        .with_orphan_sweep_grace(Duration::ZERO);
    let bundle = store.put(b"V21 model bundle".to_vec()).await.unwrap();
    let network = store.put(b"V21 FANN network".to_vec()).await.unwrap();
    let orphan = store.put(b"V21 true orphan".to_vec()).await.unwrap();
    {
        let writer = backend.pool().writer().unwrap();
        writer
            .conn()
            .execute(
                "INSERT INTO entities \
                     (id, namespace, kind, entity_type, name, tags, created_at, updated_at) \
                     VALUES ('model', 'local', 'artifact', 'moodboard_model', \
                             'model', '[]', 1, 1)",
                [],
            )
            .unwrap();
        writer
            .conn()
            .execute(
                "INSERT INTO attachments \
                     (record_uuid, substrate, role, content_ref, created_at) \
                     VALUES ('model', 'entity', 'content', ?1, 1), \
                            ('model', 'entity', 'fann-network', ?2, 1)",
                rusqlite::params![bundle.as_str(), network.as_str()],
            )
            .unwrap();
    }

    let dry_run = store
        .transactional_orphan_sweep(backend.sql().as_ref(), true)
        .await
        .expect("completed V21 dry run must be supported");
    assert_eq!(dry_run.would_delete, 1);
    assert_eq!(dry_run.deleted, 0);

    let result = store
        .transactional_orphan_sweep(backend.sql().as_ref(), false)
        .await
        .expect("completed V21 destructive sweep must be supported");
    assert_eq!(result.deleted, 1);
    assert!(store.exists(&bundle).await.unwrap());
    assert!(store.exists(&network).await.unwrap());
    assert!(!store.exists(&orphan).await.unwrap());
}

/// A `StorageBackend` constructed directly and never run through the
/// versioned migration ledger (`run_migrations`/`prepare_core_schema`) —
/// only the ad hoc, idempotent `entities` DDL a plain `entities()` call
/// applies — has no completed V21 marker, attachment liveness table, or
/// attachment fencing triggers. Without that set a reference committed between liveness
/// selection and physical deletion would dangle, so the trait contract
/// requires `StorageError::Unsupported` here rather than an unfenced
/// sweep, and every candidate must survive.
#[tokio::test]
async fn transactional_orphan_sweep_refuses_without_the_blob_gc_claims_migration() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    let backend = std::sync::Arc::new(crate::StorageBackend::sqlite_for_test(&db_path).unwrap());
    backend.entities().unwrap();
    {
        let reader = backend.pool().reader().unwrap();
        let present: bool = reader
            .conn()
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' \
                     AND name = 'blob_gc_claims'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            !present,
            "this test's premise requires blob_gc_claims to be absent"
        );
    }

    let root = dir.path().join("blobs");
    let store = std::sync::Arc::new(
        FsBlobStore::new(root.clone(), 0)
            .unwrap()
            .with_orphan_sweep_grace(Duration::ZERO),
    );
    let orphan = store.put(b"direct-backend orphan".to_vec()).await.unwrap();

    let sql = backend.sql();
    let error = store
        .transactional_orphan_sweep(sql.as_ref(), false)
        .await
        .expect_err("sweep must refuse a backend without the blob_gc_claims fencing set");
    assert!(
        matches!(error, StorageError::Unsupported { .. }),
        "expected StorageError::Unsupported, got {error:?}"
    );
    assert!(
        store.exists(&orphan).await.unwrap(),
        "a refused sweep must not have deleted anything"
    );
}

#[tokio::test]
async fn transactional_orphan_sweep_refuses_an_incomplete_cutover_marker() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    let backend = std::sync::Arc::new(crate::StorageBackend::sqlite_for_test(&db_path).unwrap());
    {
        let mut writer = backend.pool().writer().unwrap();
        prepare_completed_v21_gc_fixture(writer.conn_mut());
        writer
            .conn_mut()
            .execute_batch(
                "UPDATE attachment_cutover_state \
                     SET state = 'incomplete', completed_at = NULL WHERE singleton = 1; \
                     DELETE FROM _schema_migrations WHERE version = 21;",
            )
            .unwrap();
    }

    let store = FsBlobStore::new(dir.path().join("blobs"), 0)
        .unwrap()
        .with_orphan_sweep_grace(Duration::ZERO);
    let orphan = store
        .put(b"incomplete-cutover orphan".to_vec())
        .await
        .unwrap();
    let error = store
        .transactional_orphan_sweep(backend.sql().as_ref(), false)
        .await
        .expect_err("sweep must refuse every durable incomplete marker");
    assert!(matches!(error, StorageError::Unsupported { .. }));
    assert!(
        store.exists(&orphan).await.unwrap(),
        "refused incomplete-state sweep must preserve every blob"
    );
}

/// The fencing gate must demand the complete V21 set, not just the
/// claims table: with a fencing trigger dropped, a claim no longer
/// blocks a concurrent attachment write from resurrecting the digest, so
/// the sweep must refuse exactly as it does with no migration at all.
#[tokio::test]
async fn transactional_orphan_sweep_refuses_with_incomplete_fencing_triggers() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    let backend = std::sync::Arc::new(crate::StorageBackend::sqlite_for_test(&db_path).unwrap());
    {
        let mut writer = backend.pool().writer().unwrap();
        prepare_completed_v21_gc_fixture(writer.conn_mut());
        writer
            .conn_mut()
            .execute_batch("DROP TRIGGER attachments_reject_claimed_blob_update")
            .unwrap();
        writer
            .conn_mut()
            .execute(
                "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) \
                     VALUES ('abandoned-partial-fence', \
                             'dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd', \
                             1)",
                [],
            )
            .unwrap();
    }

    let root = dir.path().join("blobs");
    let store = std::sync::Arc::new(
        FsBlobStore::new(root.clone(), 0)
            .unwrap()
            .with_orphan_sweep_grace(Duration::ZERO),
    );
    let orphan = store.put(b"partial-fence orphan".to_vec()).await.unwrap();

    let sql = backend.sql();
    let _root_guard = store.write_lock.clone().lock_owned().await;
    let error = tokio::time::timeout(
        Duration::from_secs(1),
        store.transactional_orphan_sweep(sql.as_ref(), false),
    )
    .await
    .expect("an incomplete V21 fence must refuse before the root wait")
    .expect_err("sweep must refuse when any V21 fencing trigger is missing");
    assert!(
        matches!(error, StorageError::Unsupported { .. }),
        "expected StorageError::Unsupported, got {error:?}"
    );
    assert!(
        store.exists(&orphan).await.unwrap(),
        "a refused sweep must not have deleted anything"
    );
    let remaining: i64 = backend
        .pool()
        .reader()
        .unwrap()
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM blob_gc_claims \
                 WHERE root_key = 'abandoned-partial-fence'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(remaining, 1, "a refused sweep must not recover claims");
}

/// The gate must verify the fence FUNCTIONS, not that three names exist
/// in `sqlite_master`: triggers with the right names but no-op bodies
/// pass any name census while letting a claimed `content_ref` become
/// live during the released-writer deletion window. The fence probe must
/// catch them and refuse, deleting nothing and leaving no probe residue.
#[tokio::test]
async fn transactional_orphan_sweep_refuses_same_named_noop_fencing_triggers() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    let backend = std::sync::Arc::new(crate::StorageBackend::sqlite_for_test(&db_path).unwrap());
    {
        let mut writer = backend.pool().writer().unwrap();
        prepare_completed_v21_gc_fixture(writer.conn_mut());
        writer
            .conn_mut()
            .execute_batch(
                "DROP TRIGGER attachments_reject_claimed_blob_insert; \
                     DROP TRIGGER attachments_reject_claimed_blob_update; \
                     CREATE TRIGGER attachments_reject_claimed_blob_insert \
                     BEFORE INSERT ON attachments BEGIN SELECT 0; END; \
                     CREATE TRIGGER attachments_reject_claimed_blob_update \
                     BEFORE UPDATE OF content_ref ON attachments \
                     BEGIN SELECT 0; END;",
            )
            .unwrap();
    }

    let root = dir.path().join("blobs");
    let store = std::sync::Arc::new(
        FsBlobStore::new(root.clone(), 0)
            .unwrap()
            .with_orphan_sweep_grace(Duration::ZERO),
    );
    let orphan = store.put(b"noop-trigger orphan".to_vec()).await.unwrap();

    let sql = backend.sql();
    let error = store
        .transactional_orphan_sweep(sql.as_ref(), false)
        .await
        .expect_err("sweep must refuse when the fencing triggers are same-named no-ops");
    assert!(
        matches!(error, StorageError::Unsupported { .. }),
        "expected StorageError::Unsupported, got {error:?}"
    );
    assert!(
        store.exists(&orphan).await.unwrap(),
        "a refused sweep must not have deleted anything"
    );

    // The probe must not leave residue behind either.
    let reader = backend.pool().reader().unwrap();
    let leftovers: i64 = reader
        .conn()
        .query_row(
            "SELECT (SELECT COUNT(*) FROM blob_gc_claims \
                         WHERE root_key GLOB '__fence_probe-*') \
                      + (SELECT COUNT(*) FROM attachments \
                         WHERE record_uuid GLOB '__blob-gc-fence-probe-*')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(leftovers, 0, "fence probe rows must not survive the probe");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fence_probe_refuses_id_collision_and_preserves_the_colliding_attachment() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    let backend = std::sync::Arc::new(crate::StorageBackend::sqlite_for_test(&db_path).unwrap());
    {
        let mut writer = backend.pool().writer().unwrap();
        prepare_completed_v21_gc_fixture(writer.conn_mut());
        writer
            .conn_mut()
            .execute(
                "INSERT INTO attachments \
                     (record_uuid, substrate, role, content_ref, media_type, created_at) \
                     VALUES ('victim-id', 'entity', 'content', \
                             '2222222222222222222222222222222222222222222222222222222222222222', \
                             'application/test', 7)",
                [],
            )
            .unwrap();
    }

    let sql = backend.sql();
    let error = super::blob_gc_fence_probe_with_ids(
        sql.as_ref(),
        "victim-id".to_string(),
        "victim-update-id".to_string(),
        "victim-insert2-id".to_string(),
        "victim-update2-id".to_string(),
        "victim-claim-key".to_string(),
    )
    .await
    .expect_err("the probe must refuse when an id it would delete already names a row");
    assert!(
        matches!(
            &error,
            StorageError::WriterTaskRequestFailed {
                request_state:
                    khive_storage::WriterTaskRequestState::TransactionRolledBack,
                source,
            } if matches!(source.as_ref(), StorageError::Unsupported { .. })
        ),
        "expected a proven-rollback wrapper retaining StorageError::Unsupported, got {error:?}"
    );

    let reader = backend.pool().reader().unwrap();
    let (media_type, created_at): (String, i64) = reader
        .conn()
        .query_row(
            "SELECT media_type, created_at FROM attachments \
                 WHERE record_uuid = 'victim-id' AND role = 'content'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("the colliding attachment must survive the refused probe untouched");
    assert_eq!(media_type, "application/test");
    assert_eq!(created_at, 7);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fence_probe_does_not_touch_an_unrelated_retained_entity_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    let backend = std::sync::Arc::new(crate::StorageBackend::sqlite_for_test(&db_path).unwrap());
    {
        let mut writer = backend.pool().writer().unwrap();
        prepare_completed_v21_gc_fixture(writer.conn_mut());
        // entities_seq rows intentionally survive entity hard deletion, so
        // an id can collide with the ledger alone — no entities row left
        // for the guard to trip on.
        writer
            .conn_mut()
            .execute(
                "INSERT INTO entities \
                     (id, namespace, kind, name, tags, created_at, updated_at) \
                     VALUES ('retained-id', 'local', 'document', 'gone entity', '[]', 7, 7)",
                [],
            )
            .unwrap();
        writer
            .conn_mut()
            .execute("DELETE FROM entities WHERE id = 'retained-id'", [])
            .unwrap();
        let retained: i64 = writer
            .conn_mut()
            .query_row(
                "SELECT COUNT(*) FROM entities_seq WHERE entity_id = 'retained-id'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained, 1, "fixture requires a retained-only ledger row");
    }

    let sql = backend.sql();
    super::blob_gc_fence_probe_with_ids(
        sql.as_ref(),
        "retained-id".to_string(),
        "retained-update-id".to_string(),
        "retained-insert2-id".to_string(),
        "retained-update2-id".to_string(),
        "retained-claim-key".to_string(),
    )
    .await
    .expect("attachment probe has no reason to mutate an entity sequence row");

    let reader = backend.pool().reader().unwrap();
    let survivors: i64 = reader
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM entities_seq WHERE entity_id = 'retained-id'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        survivors, 1,
        "the retained entity ledger row must survive the attachment probe"
    );
}

fn nul_embedded_canonical_ref() -> String {
    let mut polluted = "a".repeat(64);
    polluted.push('\0');
    polluted.push_str("zz");
    polluted
}

/// SQLite's `length()` counts characters before the first NUL and GLOB
/// stops scanning there, so a 64-hex-then-NUL value passes both while the
/// exact-equality liveness anti-join cannot match it. The byte-length arm
/// must refuse the sweep on such a claim row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blob_gc_evidence_rejects_a_nul_embedded_claim_ref() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    let backend = crate::StorageBackend::sqlite_for_test(&db_path).unwrap();
    {
        let mut writer = backend.pool().writer().unwrap();
        prepare_completed_v21_gc_fixture(writer.conn_mut());
        writer
            .conn_mut()
            .execute(
                "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) \
                     VALUES ('nul-claim-key', ?1, 0)",
                rusqlite::params![nul_embedded_canonical_ref()],
            )
            .unwrap();
    }

    let sql = backend.sql();
    let error = super::validate_blob_gc_evidence(sql.as_ref())
        .await
        .expect_err("a NUL-embedded claim ref must refuse the sweep");
    assert!(
        error.to_string().contains("blob_gc_claims"),
        "expected the claims-table refusal, got {error:?}"
    );
}

/// The attachments schema CHECK uses the same NUL-blind `length()`/GLOB
/// pair, so the polluted row INSERTS successfully — this test proves that
/// on purpose — and the evidence validator must then be the backstop that
/// refuses the sweep.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blob_gc_evidence_rejects_a_nul_embedded_attachment_ref() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    let backend = crate::StorageBackend::sqlite_for_test(&db_path).unwrap();
    {
        let mut writer = backend.pool().writer().unwrap();
        prepare_completed_v21_gc_fixture(writer.conn_mut());
        // The table CHECK now rejects a NUL-embedded ref at admission
        // time; bypass it to simulate a row that reached this state some
        // other way (e.g. a pre-fix legacy row) and prove the validator
        // is still defense-in-depth against it.
        writer
            .conn_mut()
            .execute_batch("PRAGMA ignore_check_constraints = ON")
            .unwrap();
        writer
            .conn_mut()
            .execute(
                "INSERT INTO attachments \
                     (record_uuid, substrate, role, content_ref, created_at) \
                     VALUES ('nul-attachment-id', 'entity', 'content', ?1, 0)",
                rusqlite::params![nul_embedded_canonical_ref()],
            )
            .expect("ignore_check_constraints must allow the corrupt row to insert");
        writer
            .conn_mut()
            .execute_batch("PRAGMA ignore_check_constraints = OFF")
            .unwrap();
    }

    let sql = backend.sql();
    let error = super::validate_blob_gc_evidence(sql.as_ref())
        .await
        .expect_err("a NUL-embedded attachment ref must refuse the sweep");
    assert!(
        error.to_string().contains("attachments"),
        "expected the attachments-table refusal, got {error:?}"
    );
}

/// A trigger rewrite that fences only the probe's fixed all-zero sentinel
/// passes the sentinel arms; the second-digest arms must catch it. The
/// healthy fixture is probed first as the positive control.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fence_probe_refuses_a_digest_restricted_trigger_rewrite() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    let backend = crate::StorageBackend::sqlite_for_test(&db_path).unwrap();
    {
        let mut writer = backend.pool().writer().unwrap();
        prepare_completed_v21_gc_fixture(writer.conn_mut());
    }

    let sql = backend.sql();
    super::blob_gc_fence_probe(sql.as_ref())
        .await
        .expect("the healthy fence must pass all four probe arms");

    {
        let mut writer = backend.pool().writer().unwrap();
        writer
            .conn_mut()
            .execute_batch(
                "DROP TRIGGER attachments_reject_claimed_blob_insert; \
                     DROP TRIGGER attachments_reject_claimed_blob_update; \
                     CREATE TRIGGER attachments_reject_claimed_blob_insert \
                     BEFORE INSERT ON attachments \
                     WHEN NEW.content_ref = \
                         '0000000000000000000000000000000000000000000000000000000000000000' \
                       AND EXISTS (SELECT 1 FROM blob_gc_claims \
                                   WHERE content_ref = NEW.content_ref) \
                     BEGIN \
                         SELECT RAISE(ABORT, \
                             'content_ref is reserved by an active blob sweep'); \
                     END; \
                     CREATE TRIGGER attachments_reject_claimed_blob_update \
                     BEFORE UPDATE OF content_ref ON attachments \
                     WHEN NEW.content_ref = \
                         '0000000000000000000000000000000000000000000000000000000000000000' \
                       AND EXISTS (SELECT 1 FROM blob_gc_claims \
                                   WHERE content_ref = NEW.content_ref) \
                     BEGIN \
                         SELECT RAISE(ABORT, \
                             'content_ref is reserved by an active blob sweep'); \
                     END;",
            )
            .unwrap();
    }

    let error = super::blob_gc_fence_probe(sql.as_ref())
        .await
        .expect_err("a sentinel-only fence must fail the second-digest arms");
    assert!(
        matches!(error, StorageError::Unsupported { .. }),
        "expected StorageError::Unsupported, got {error:?}"
    );
    assert!(
        error.to_string().contains("second-digest"),
        "the refusal must name a second-digest arm, got {error}"
    );
}

/// A trigger rewrite restricted to the entity/content shape passes the
/// sentinel arms; the note-shaped second-digest arms must catch it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fence_probe_refuses_a_shape_restricted_trigger_rewrite() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    let backend = crate::StorageBackend::sqlite_for_test(&db_path).unwrap();
    {
        let mut writer = backend.pool().writer().unwrap();
        prepare_completed_v21_gc_fixture(writer.conn_mut());
        writer
            .conn_mut()
            .execute_batch(
                "DROP TRIGGER attachments_reject_claimed_blob_insert; \
                     DROP TRIGGER attachments_reject_claimed_blob_update; \
                     CREATE TRIGGER attachments_reject_claimed_blob_insert \
                     BEFORE INSERT ON attachments \
                     WHEN NEW.substrate = 'entity' \
                       AND EXISTS (SELECT 1 FROM blob_gc_claims \
                                   WHERE content_ref = NEW.content_ref) \
                     BEGIN \
                         SELECT RAISE(ABORT, \
                             'content_ref is reserved by an active blob sweep'); \
                     END; \
                     CREATE TRIGGER attachments_reject_claimed_blob_update \
                     BEFORE UPDATE OF content_ref ON attachments \
                     WHEN NEW.substrate = 'entity' \
                       AND EXISTS (SELECT 1 FROM blob_gc_claims \
                                   WHERE content_ref = NEW.content_ref) \
                     BEGIN \
                         SELECT RAISE(ABORT, \
                             'content_ref is reserved by an active blob sweep'); \
                     END;",
            )
            .unwrap();
    }

    let sql = backend.sql();
    let error = super::blob_gc_fence_probe(sql.as_ref())
        .await
        .expect_err("an entity-shape-only fence must fail the note-shaped arms");
    assert!(
        matches!(error, StorageError::Unsupported { .. }),
        "expected StorageError::Unsupported, got {error:?}"
    );
    assert!(
        error.to_string().contains("second-digest"),
        "the refusal must name a second-digest arm, got {error}"
    );
}

/// SQLite fixes the text encoding when the database file is first
/// initialized; creating (and dropping) a table under the pragma leaves
/// an initialized UTF-16LE database that later writers inherit.
fn initialize_utf16le_database(db_path: &std::path::Path) {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute_batch(
        "PRAGMA encoding = 'UTF-16le'; \
             CREATE TABLE __encoding_pin (x INTEGER); \
             DROP TABLE __encoding_pin;",
    )
    .unwrap();
}

/// CAST(TEXT AS BLOB) yields the database encoding's bytes, so a fixed
/// bytes=64 arm would reject every valid 64-char ref in a UTF-16LE
/// database (128 bytes). The width-derived arm must pass them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blob_gc_evidence_accepts_valid_refs_in_a_utf16le_database() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    initialize_utf16le_database(&db_path);
    let backend = crate::StorageBackend::sqlite_for_test(&db_path).unwrap();
    {
        let mut writer = backend.pool().writer().unwrap();
        let encoding: String = writer
            .conn_mut()
            .query_row("PRAGMA encoding", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            encoding, "UTF-16le",
            "the fixture database must actually be UTF-16le"
        );
        prepare_completed_v21_gc_fixture(writer.conn_mut());
        // The fixture copies attachments from the (empty) entities table,
        // so seed one valid canonical row into each scanned table — with
        // no rows the probes measure nothing and a broken byte arm would
        // still pass this test.
        writer
            .conn_mut()
            .execute(
                "INSERT INTO attachments \
                     (record_uuid, substrate, role, content_ref, created_at) \
                     VALUES ('utf16-valid-attachment', 'entity', 'content', ?1, 0)",
                rusqlite::params!["a".repeat(64)],
            )
            .unwrap();
        writer
            .conn_mut()
            .execute(
                "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) \
                     VALUES ('utf16-valid-claim-key', ?1, 0)",
                rusqlite::params!["b".repeat(64)],
            )
            .unwrap();
    }

    let sql = backend.sql();
    super::validate_blob_gc_evidence(sql.as_ref())
        .await
        .expect("valid canonical refs must pass in a UTF-16LE database");
}

/// The NUL arm must stay red in UTF-16 as well: 64 chars + NUL + tail is
/// 130+ bytes against the expected 128.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blob_gc_evidence_rejects_a_nul_embedded_claim_ref_in_a_utf16le_database() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    initialize_utf16le_database(&db_path);
    let backend = crate::StorageBackend::sqlite_for_test(&db_path).unwrap();
    {
        let mut writer = backend.pool().writer().unwrap();
        let encoding: String = writer
            .conn_mut()
            .query_row("PRAGMA encoding", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            encoding, "UTF-16le",
            "the fixture database must actually be UTF-16le"
        );
        prepare_completed_v21_gc_fixture(writer.conn_mut());
        writer
            .conn_mut()
            .execute(
                "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) \
                     VALUES ('nul-claim-key-utf16', ?1, 0)",
                rusqlite::params![nul_embedded_canonical_ref()],
            )
            .unwrap();
    }

    let sql = backend.sql();
    let error = super::validate_blob_gc_evidence(sql.as_ref())
        .await
        .expect_err("a NUL-embedded claim ref must refuse the sweep in UTF-16LE too");
    assert!(
        error.to_string().contains("blob_gc_claims"),
        "expected the claims-table refusal, got {error:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transactional_orphan_sweep_preserves_put_started_after_liveness_mark() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    let backend = std::sync::Arc::new(crate::StorageBackend::sqlite_for_test(&db_path).unwrap());
    {
        let mut writer = backend.pool().writer().unwrap();
        prepare_completed_v21_gc_fixture(writer.conn_mut());
    }
    let root = dir.path().join("blobs");
    let store = std::sync::Arc::new(
        FsBlobStore::new(root.clone(), 0)
            .unwrap()
            .with_orphan_sweep_grace(Duration::ZERO),
    );
    let orphan = store.put(b"old orphan".to_vec()).await.unwrap();
    let canonical_root = root.canonicalize().unwrap();
    let (marked, release, _done) = sync_hook::install(&canonical_root);

    let sweep = {
        let store = store.clone();
        let sql = backend.sql();
        tokio::spawn(async move { store.transactional_orphan_sweep(sql.as_ref(), false).await })
    };
    assert!(
        recv_blocking(marked).await,
        "sweep must finish its liveness mark"
    );

    assert!(
        store.write_lock.try_lock().is_err(),
        "the sweep must hold the same root lock used by blob writers"
    );
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let new_ref = {
        let root = root.clone();
        tokio::task::spawn_blocking(move || {
            let _ = started_tx.send(());
            put_blocking(&root, 0, b"new concurrent blob".to_vec())
        })
    };
    assert!(recv_blocking(started_rx).await, "blob put must start");

    release.send(()).unwrap();
    let sweep_result = sweep.await.unwrap().unwrap();
    let new_ref = new_ref.await.unwrap().unwrap();

    assert_eq!(sweep_result.deleted, 1);
    assert!(!store.exists(&orphan).await.unwrap());
    assert!(
        store.exists(&new_ref).await.unwrap(),
        "a blob put started between the liveness mark and physical sweep must survive"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transactional_orphan_sweep_releases_sqlite_writer_before_physical_delete() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    let backend = std::sync::Arc::new(crate::StorageBackend::sqlite_for_test(&db_path).unwrap());
    {
        let mut writer = backend.pool().writer().unwrap();
        prepare_completed_v21_gc_fixture(writer.conn_mut());
    }
    let root = dir.path().join("blobs");
    let store = std::sync::Arc::new(
        FsBlobStore::new(root.clone(), 0)
            .unwrap()
            .with_orphan_sweep_grace(Duration::ZERO),
    );
    let orphan = store
        .put(b"claim then delete outside sqlite".to_vec())
        .await
        .unwrap();
    let canonical_root = root.canonicalize().unwrap();
    let (claimed, release_delete, _done) = sync_hook::install(&canonical_root);

    let sweep = {
        let store = store.clone();
        let sql = backend.sql();
        tokio::spawn(async move { store.transactional_orphan_sweep(sql.as_ref(), false).await })
    };
    assert!(
        recv_blocking(claimed).await,
        "sweep must durably claim the orphan before physical deletion"
    );
    assert!(
        store.exists(&orphan).await.unwrap(),
        "the test seam must pause before the physical delete"
    );
    let external_database_lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(database_gc_lock_path(&db_path))
        .unwrap();
    assert!(
        matches!(
            fs4::FileExt::try_lock(&external_database_lock),
            Err(fs4::TryLockError::WouldBlock)
        ),
        "the sweep must retain cross-process database ownership while SQLite's writer is free"
    );

    // The destructive filesystem phase is deliberately paused. An
    // unrelated SQLite writer must nevertheless complete now: this is
    // the hold-time proof that external I/O is no longer inside the
    // sweep's BEGIN IMMEDIATE span.
    let unrelated = rusqlite::Connection::open(&db_path).unwrap();
    unrelated.busy_timeout(Duration::from_millis(100)).unwrap();
    unrelated
        .execute(
            "INSERT INTO entities \
                 (id, namespace, kind, name, tags, created_at, updated_at) \
                 VALUES ('unrelated-writer', 'local', 'concept', 'unrelated', '[]', 1, 1)",
            [],
        )
        .expect("external filesystem work must not retain SQLite's writer lock");

    // The claim trigger is the cross-resource fence: while the file is
    // selected for deletion, a concurrent attachment writer cannot make it
    // newly live in the released-writer window.
    let claimed_err = unrelated
        .execute(
            "INSERT INTO attachments \
                 (record_uuid, substrate, role, content_ref, created_at) \
                 VALUES ('racing-reference', 'entity', 'content', ?1, 1)",
            [orphan.as_str()],
        )
        .expect_err("a claimed content_ref must fail closed before deletion");
    assert!(
        claimed_err.to_string().contains("active blob sweep"),
        "unexpected claim error: {claimed_err}"
    );

    release_delete.send(()).unwrap();
    let result = sweep.await.unwrap().unwrap();
    assert_eq!(result.deleted, 1);
    assert!(!store.exists(&orphan).await.unwrap());

    let remaining_claims: i64 = unrelated
        .query_row(
            "SELECT COUNT(*) FROM blob_gc_claims WHERE content_ref = ?1",
            [orphan.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        remaining_claims, 0,
        "successful deletion releases the claim"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_sweep_during_delete_keeps_owner_locks_until_blocking_work_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    let backend = std::sync::Arc::new(crate::StorageBackend::sqlite_for_test(&db_path).unwrap());
    {
        let mut writer = backend.pool().writer().unwrap();
        prepare_completed_v21_gc_fixture(writer.conn_mut());
    }
    let root = dir.path().join("blobs");
    let store = std::sync::Arc::new(
        FsBlobStore::new(root.clone(), 0)
            .unwrap()
            .with_orphan_sweep_grace(Duration::ZERO),
    );
    let orphan = store.put(b"cancelled sweep orphan".to_vec()).await.unwrap();
    let canonical_root = root.canonicalize().unwrap();
    let (claimed, release_delete, done) = sync_hook::install(&canonical_root);

    let sweep = {
        let store = store.clone();
        let sql = backend.sql();
        tokio::spawn(async move { store.transactional_orphan_sweep(sql.as_ref(), false).await })
    };
    assert!(recv_blocking(claimed).await);
    sweep.abort();
    assert!(sweep.await.unwrap_err().is_cancelled());

    let external_root_lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.join(ROOT_WRITE_LOCK_FILE))
        .unwrap();
    let external_database_lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(database_gc_lock_path(&db_path))
        .unwrap();
    assert!(matches!(
        fs4::FileExt::try_lock(&external_root_lock),
        Err(fs4::TryLockError::WouldBlock)
    ));
    assert!(matches!(
        fs4::FileExt::try_lock(&external_database_lock),
        Err(fs4::TryLockError::WouldBlock)
    ));

    release_delete.send(()).unwrap();
    let done_disconnected = tokio::task::spawn_blocking(move || done.recv().is_err())
        .await
        .unwrap();
    assert!(
        done_disconnected,
        "the cancelled outer task cannot send done"
    );
    assert!(fs4::FileExt::try_lock(&external_root_lock).is_ok());
    assert!(fs4::FileExt::try_lock(&external_database_lock).is_ok());
    drop(external_root_lock);
    drop(external_database_lock);
    assert!(!store.exists(&orphan).await.unwrap());
    let stranded_claims: i64 = rusqlite::Connection::open(&db_path)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM blob_gc_claims", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        stranded_claims, 1,
        "cancellation leaves a fail-closed claim"
    );

    let recovered = store
        .transactional_orphan_sweep(backend.sql().as_ref(), false)
        .await
        .unwrap();
    assert_eq!(recovered.deleted, 0);
    let remaining: i64 = rusqlite::Connection::open(&db_path)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM blob_gc_claims", [], |row| row.get(0))
        .unwrap();
    assert_eq!(remaining, 0, "the next exclusive owner recovers the claim");
}

#[tokio::test]
async fn transactional_orphan_sweep_recovers_stale_claims_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    let backend = crate::StorageBackend::sqlite_for_test(&db_path).unwrap();
    {
        let mut writer = backend.pool().writer().unwrap();
        prepare_completed_v21_gc_fixture(writer.conn_mut());
    }
    let root = dir.path().join("blobs");
    let store = FsBlobStore::new(root.clone(), 0)
        .unwrap()
        .with_orphan_sweep_grace(Duration::from_secs(60));
    let bytes = b"republished after a crashed claim".to_vec();
    let content_ref = store.put(bytes.clone()).await.unwrap();
    let canonical_root = root.canonicalize().unwrap();
    let root_key = blob_root_key(&canonical_root);
    let absent_ref = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    let former_probe_seed = "1111111111111111111111111111111111111111111111111111111111111111";
    {
        let writer = backend.pool().writer().unwrap();
        writer
            .conn()
            .execute(
                "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) \
                     VALUES (?1, ?2, 1), (?1, ?3, 1), (?1, ?4, 1)",
                rusqlite::params![
                    root_key,
                    content_ref.as_str(),
                    absent_ref,
                    former_probe_seed
                ],
            )
            .unwrap();
    }

    // A publisher that recovered after the claiming process crashed
    // refreshes the digest's grace witness before its attachment write. The
    // next sweep must clear this protected claim, the claim whose file was
    // already removed, and the former fixed probe seed without letting a
    // healthy attachment fence block abandoned-claim recovery forever.
    assert_eq!(store.put(bytes).await.unwrap(), content_ref);
    let result = store
        .transactional_orphan_sweep(backend.sql().as_ref(), false)
        .await
        .unwrap();
    assert_eq!(result.deleted, 0);
    assert_eq!(result.grace_period_skipped, 1);
    assert!(store.exists(&content_ref).await.unwrap());

    let remaining: i64 = backend
        .pool()
        .writer()
        .unwrap()
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM blob_gc_claims WHERE root_key = ?1",
            [blob_root_key(&canonical_root)],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(remaining, 0, "the next sweep recovers stale claims");
}

#[tokio::test]
async fn transactional_orphan_sweep_recovers_claims_after_root_relocation() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    let backend = crate::StorageBackend::sqlite_for_test(&db_path).unwrap();
    {
        let mut writer = backend.pool().writer().unwrap();
        prepare_completed_v21_gc_fixture(writer.conn_mut());
    }

    let old_root = dir.path().join("old-blobs");
    let bytes = b"claim must follow a relocated blob root".to_vec();
    let content_ref = {
        let old_store = FsBlobStore::new(old_root.clone(), 0)
            .unwrap()
            .with_orphan_sweep_grace(Duration::from_secs(60));
        old_store.put(bytes).await.unwrap()
    };
    let old_root_key = blob_root_key(&old_root.canonicalize().unwrap());
    backend
        .pool()
        .writer()
        .unwrap()
        .conn()
        .execute(
            "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) \
                 VALUES (?1, ?2, 1)",
            rusqlite::params![old_root_key, content_ref.as_str()],
        )
        .unwrap();

    let new_root = dir.path().join("relocated-blobs");
    std::fs::rename(&old_root, &new_root).unwrap();
    let relocated_store = FsBlobStore::new(new_root, 0)
        .unwrap()
        .with_orphan_sweep_grace(Duration::from_secs(60));
    let result = relocated_store
        .transactional_orphan_sweep(backend.sql().as_ref(), false)
        .await
        .unwrap();

    assert_eq!(
        result.deleted, 0,
        "a fresh relocated blob remains protected"
    );
    assert_eq!(result.grace_period_skipped, 1);
    assert!(relocated_store.exists(&content_ref).await.unwrap());
    let remaining: i64 = backend
        .pool()
        .writer()
        .unwrap()
        .conn()
        .query_row("SELECT COUNT(*) FROM blob_gc_claims", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        remaining, 0,
        "exclusive database sweep ownership makes every pre-existing claim abandoned, \
             even when its old path-derived root key no longer matches"
    );
}

#[tokio::test]
async fn transactional_orphan_sweep_recovers_claims_copied_by_database_restore() {
    let dir = tempfile::tempdir().unwrap();
    let source_path = dir.path().join("source.db");
    let restored_path = dir.path().join("restored.db");
    let bytes = b"claim copied in an online database backup".to_vec();
    let content_ref = ContentRef::from_digest_bytes(blake3::hash(&bytes).as_bytes());
    {
        let source = crate::StorageBackend::sqlite_for_test(&source_path).unwrap();
        let mut writer = source.pool().writer().unwrap();
        prepare_completed_v21_gc_fixture(writer.conn_mut());
        writer
            .conn()
            .execute(
                "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) \
                     VALUES ('source-root-before-backup', ?1, 1)",
                [content_ref.as_str()],
            )
            .unwrap();
        writer
            .conn()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
    }
    std::fs::copy(&source_path, &restored_path).unwrap();

    let restored = crate::StorageBackend::sqlite_for_test(&restored_path).unwrap();
    let restored_root = dir.path().join("restored-blobs");
    let store = FsBlobStore::new(restored_root, 0)
        .unwrap()
        .with_orphan_sweep_grace(Duration::from_secs(60));
    assert_eq!(store.put(bytes).await.unwrap(), content_ref);
    let result = store
        .transactional_orphan_sweep(restored.sql().as_ref(), false)
        .await
        .unwrap();

    assert_eq!(result.deleted, 0);
    assert_eq!(result.grace_period_skipped, 1);
    let remaining: i64 = restored
        .pool()
        .writer()
        .unwrap()
        .conn()
        .query_row("SELECT COUNT(*) FROM blob_gc_claims", [], |row| row.get(0))
        .unwrap();
    assert_eq!(remaining, 0, "restored claims are abandoned ownership");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transactional_orphan_sweep_bounds_each_durable_claim_batch() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    let backend = std::sync::Arc::new(crate::StorageBackend::sqlite_for_test(&db_path).unwrap());
    {
        let mut writer = backend.pool().writer().unwrap();
        prepare_completed_v21_gc_fixture(writer.conn_mut());
    }
    let root = dir.path().join("blobs");
    let store = std::sync::Arc::new(
        FsBlobStore::new(root.clone(), 0)
            .unwrap()
            .with_orphan_sweep_grace(Duration::ZERO),
    );
    let candidate_count = BLOB_GC_CLAIM_BATCH_SIZE * 2 + 1;
    for index in 0..candidate_count {
        store
            .put(format!("bounded claim candidate {index}").into_bytes())
            .await
            .unwrap();
    }
    let canonical_root = root.canonicalize().unwrap();
    let (claimed, release_delete, _done) = sync_hook::install(&canonical_root);

    let sweep = {
        let store = store.clone();
        let sql = backend.sql();
        tokio::spawn(async move { store.transactional_orphan_sweep(sql.as_ref(), false).await })
    };
    assert!(
        recv_blocking(claimed).await,
        "the first bounded claim batch must commit before deletion"
    );
    let active_claims: i64 = rusqlite::Connection::open(&db_path)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM blob_gc_claims", [], |row| row.get(0))
        .unwrap();
    assert!(active_claims > 0);
    assert!(
        active_claims <= BLOB_GC_CLAIM_BATCH_SIZE as i64,
        "one transaction may expose at most {BLOB_GC_CLAIM_BATCH_SIZE} claim rows; \
             observed {active_claims}"
    );

    release_delete.send(()).unwrap();
    let result = sweep.await.unwrap().unwrap();
    assert_eq!(result.deleted, candidate_count as u64);
    let remaining: i64 = rusqlite::Connection::open(&db_path)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM blob_gc_claims", [], |row| row.get(0))
        .unwrap();
    assert_eq!(remaining, 0);
}

#[tokio::test]
async fn abandoned_claim_recovery_deletes_at_most_one_batch_per_writer_hold() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    let backend = crate::StorageBackend::sqlite_for_test(&db_path).unwrap();
    backend.pool().run_migrations().unwrap();
    {
        let mut writer = backend.pool().writer().unwrap();
        let tx = writer.conn_mut().transaction().unwrap();
        for index in 0..(BLOB_GC_CLAIM_BATCH_SIZE + 1) {
            tx.execute(
                "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) \
                     VALUES ('abandoned-root', ?1, 1)",
                [format!("{index:064x}")],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }

    let released = release_abandoned_blob_gc_claim_batch(backend.sql().as_ref())
        .await
        .unwrap();
    assert_eq!(released, BLOB_GC_CLAIM_BATCH_SIZE as u64);
    let remaining: i64 = backend
        .pool()
        .writer()
        .unwrap()
        .conn()
        .query_row("SELECT COUNT(*) FROM blob_gc_claims", [], |row| row.get(0))
        .unwrap();
    assert_eq!(remaining, 1);
}

#[tokio::test]
async fn transactional_orphan_sweep_refuses_corrupt_liveness_and_claim_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    let backend = crate::StorageBackend::sqlite_for_test(&db_path).unwrap();
    {
        let mut writer = backend.pool().writer().unwrap();
        prepare_completed_v21_gc_fixture(writer.conn_mut());
    }
    let root = dir.path().join("blobs");
    let store = FsBlobStore::new(root.clone(), 0)
        .unwrap()
        .with_orphan_sweep_grace(Duration::ZERO);
    let orphan = store
        .put(b"must survive corrupt evidence".to_vec())
        .await
        .unwrap();

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute_batch("PRAGMA ignore_check_constraints = ON")
        .unwrap();
    conn.execute(
        "INSERT INTO attachments \
             (record_uuid, substrate, role, content_ref, created_at) \
             VALUES ('corrupt-live', 'entity', 'content', 'not-a-content-ref', 1)",
        [],
    )
    .unwrap();
    conn.execute_batch("PRAGMA ignore_check_constraints = OFF")
        .unwrap();
    let live_error = store
        .transactional_orphan_sweep(backend.sql().as_ref(), false)
        .await
        .expect_err("corrupt live evidence must fail closed");
    assert!(matches!(live_error, StorageError::InvalidInput { .. }));
    assert!(
        store.exists(&orphan).await.unwrap(),
        "no file may be removed after corrupt live evidence"
    );

    conn.execute(
        "DELETE FROM attachments WHERE record_uuid = 'corrupt-live'",
        [],
    )
    .unwrap();
    let root_key = blob_root_key(&root.canonicalize().unwrap());
    conn.execute(
        "INSERT INTO blob_gc_claims (root_key, content_ref, claimed_at) \
             VALUES (?1, 'also-not-a-content-ref', 1)",
        [root_key.as_str()],
    )
    .unwrap();
    let claim_error = store
        .transactional_orphan_sweep(backend.sql().as_ref(), false)
        .await
        .expect_err("corrupt durable claim evidence must fail closed");
    assert!(matches!(claim_error, StorageError::InvalidInput { .. }));
    assert!(
        store.exists(&orphan).await.unwrap(),
        "no file may be removed after corrupt claim evidence"
    );
    let remaining: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM blob_gc_claims \
                 WHERE root_key = ?1 AND content_ref = 'also-not-a-content-ref'",
            [root_key.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        remaining, 1,
        "corrupt claim evidence is not silently erased"
    );
    let probe_residue: i64 = conn
        .query_row(
            "SELECT (SELECT COUNT(*) FROM blob_gc_claims \
                         WHERE root_key GLOB '__fence_probe-*') \
                      + (SELECT COUNT(*) FROM attachments \
                         WHERE record_uuid GLOB '__blob-gc-fence-probe-*')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        probe_residue, 0,
        "invalid evidence must abort before the functional fence probe"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transactional_orphan_sweep_republishes_deduplicated_external_put() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    let backend = std::sync::Arc::new(crate::StorageBackend::sqlite_for_test(&db_path).unwrap());
    {
        let mut writer = backend.pool().writer().unwrap();
        prepare_completed_v21_gc_fixture(writer.conn_mut());
    }
    let root = dir.path().join("blobs");
    let store = std::sync::Arc::new(
        FsBlobStore::new(root.clone(), 0)
            .unwrap()
            .with_orphan_sweep_grace(Duration::ZERO),
    );
    let payload = b"existing orphan republished during sweep".to_vec();
    let orphan = store.put(payload.clone()).await.unwrap();
    let canonical_root = root.canonicalize().unwrap();
    let (marked, release, _done) = sync_hook::install(&canonical_root);

    let sweep = {
        let store = store.clone();
        let sql = backend.sql();
        tokio::spawn(async move { store.transactional_orphan_sweep(sql.as_ref(), false).await })
    };
    assert!(
        recv_blocking(marked).await,
        "sweep must finish its liveness mark"
    );

    let external_lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.join(ROOT_WRITE_LOCK_FILE))
        .unwrap();
    assert!(
        matches!(
            fs4::FileExt::try_lock(&external_lock),
            Err(fs4::TryLockError::WouldBlock)
        ),
        "the sweep must exclude a publisher using an independently opened root lock"
    );

    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let republished = {
        let root = root.clone();
        tokio::task::spawn_blocking(move || {
            let _ = started_tx.send(());
            put_blocking(&root, 0, payload)
        })
    };
    assert!(recv_blocking(started_rx).await, "blob put must start");

    release.send(()).unwrap();
    let sweep_result = sweep.await.unwrap().unwrap();
    let republished = republished.await.unwrap().unwrap();

    assert_eq!(sweep_result.deleted, 1);
    assert_eq!(republished, orphan);
    assert!(
        store.exists(&republished).await.unwrap(),
        "a deduplicated put concurrent with the sweep must not return a deleted reference"
    );
}

#[tokio::test]
async fn transactional_orphan_sweep_uses_all_attachment_refs_as_live() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    let backend = crate::StorageBackend::sqlite_for_test(&db_path).unwrap();
    {
        let mut writer = backend.pool().writer().unwrap();
        prepare_completed_v21_gc_fixture(writer.conn_mut());
    }
    let store = FsBlobStore::new(dir.path().join("blobs"), 0)
        .unwrap()
        .with_orphan_sweep_grace(Duration::ZERO);
    let live = store.put(b"live".to_vec()).await.unwrap();
    let soft_deleted = store.put(b"soft deleted".to_vec()).await.unwrap();
    let orphan = store.put(b"orphan".to_vec()).await.unwrap();
    {
        let writer = backend.pool().writer().unwrap();
        writer
            .conn()
            .execute_batch(
                "INSERT INTO entities \
                     (id, namespace, kind, name, tags, created_at, updated_at, deleted_at) \
                     VALUES ('live', 'local', 'document', 'live', '[]', 1, 1, NULL), \
                            ('deleted', 'local', 'document', 'deleted', '[]', 1, 1, 2);",
            )
            .unwrap();
        writer
            .conn()
            .execute(
                "INSERT INTO attachments \
                     (record_uuid, substrate, role, content_ref, created_at) \
                     VALUES ('live', 'entity', 'content', ?1, 1), \
                            ('deleted', 'entity', 'content', ?2, 1)",
                rusqlite::params![live.as_str(), soft_deleted.as_str()],
            )
            .unwrap();
    }

    let dry_run = store
        .transactional_orphan_sweep(backend.sql().as_ref(), true)
        .await
        .unwrap();
    assert_eq!(dry_run.would_delete, 1);
    assert_eq!(dry_run.deleted, 0);
    assert!(store.exists(&soft_deleted).await.unwrap());
    assert!(store.exists(&orphan).await.unwrap());

    let result = store
        .transactional_orphan_sweep(backend.sql().as_ref(), false)
        .await
        .unwrap();

    assert_eq!(result.scanned, 3);
    assert_eq!(result.deleted, 1);
    assert!(store.exists(&live).await.unwrap());
    assert!(
        store.exists(&soft_deleted).await.unwrap(),
        "soft delete retains attachment rows and their blobs"
    );
    assert!(!store.exists(&orphan).await.unwrap());
}

#[tokio::test]
async fn transactional_orphan_sweep_protects_a_freshly_published_blob_before_its_reference_commits()
{
    // The exact two-step client protocol hazard: `put` completes and
    // releases its write lock (step 1) while the attachment write that will
    // *later* commit a `content_ref` to this blob (step 2) has not
    // happened yet -- nothing in this store's locking serializes the
    // two, because they are separate calls the client makes with an
    // arbitrary gap in between. A sweep that lands in that gap must not
    // delete the blob: `attachments.content_ref` has no row for it yet
    // purely because the referencing write hasn't landed, not because
    // it is actually orphaned. Without the publish-grace window this
    // reproduces khive#1313's dangling-reference defect: the blob file
    // is deleted here, and the still-pending attachment write below would
    // commit a `content_ref` to nothing.
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    let backend = crate::StorageBackend::sqlite_for_test(&db_path).unwrap();
    {
        let mut writer = backend.pool().writer().unwrap();
        prepare_completed_v21_gc_fixture(writer.conn_mut());
    }
    // Default (non-zero) grace period -- this test exercises exactly
    // what it exists to protect.
    let store = FsBlobStore::new(dir.path().join("blobs"), 0).unwrap();

    // Step 1: put completes, lock released. No attachment anywhere
    // references this blob yet.
    let blob = store
        .put(b"published, reference not yet committed".to_vec())
        .await
        .unwrap();

    // A sweep runs in the gap before step 2 (the attachment write) happens.
    let result = store
        .transactional_orphan_sweep(backend.sql().as_ref(), false)
        .await
        .unwrap();

    assert_eq!(result.deleted, 0, "the blob must survive: {result:?}");
    assert_eq!(
        result.would_delete, 0,
        "not treated as a deletable orphan: {result:?}"
    );
    assert_eq!(
        result.grace_period_skipped, 1,
        "must be reported as grace-protected rather than silently ignored: {result:?}"
    );
    assert!(
        store.exists(&blob).await.unwrap(),
        "a blob still inside its publish grace period must survive the sweep"
    );

    // Step 2 now lands: the record-plus-attachment write commits content_ref to the
    // still-present blob.
    {
        let writer = backend.pool().writer().unwrap();
        writer
            .conn()
            .execute(
                "INSERT INTO entities \
                     (id, namespace, kind, name, tags, created_at, updated_at, deleted_at) \
                     VALUES ('e1', 'local', 'document', 'e1', '[]', 1, 1, NULL)",
                [],
            )
            .unwrap();
        writer
            .conn()
            .execute(
                "INSERT INTO attachments \
                     (record_uuid, substrate, role, content_ref, created_at) \
                     VALUES ('e1', 'entity', 'content', ?1, 1)",
                [blob.as_str()],
            )
            .unwrap();
    }

    // A later sweep now finds it live and keeps it for the ordinary
    // reason, independent of the grace window.
    let result = store
        .transactional_orphan_sweep(backend.sql().as_ref(), false)
        .await
        .unwrap();
    assert_eq!(result.deleted, 0);
    assert!(store.exists(&blob).await.unwrap());
}

#[tokio::test]
async fn put_republishing_an_aged_orphan_restarts_its_grace_clock_before_the_reference_commits() {
    // The dedup fast path (`target.exists()`) used to return without
    // touching the file at all -- so a stale, already-orphaned blob
    // re-published by an identical `put` kept its OLD mtime, bypassed
    // the publish-grace check, and a transactional sweep landing in the
    // gap before the caller's follow-up attachment write could delete it
    // out from under that write (khive#1313). This reproduces the
    // race end to end and proves the mtime refresh closes it.
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    let backend = crate::StorageBackend::sqlite_for_test(&db_path).unwrap();
    {
        let mut writer = backend.pool().writer().unwrap();
        prepare_completed_v21_gc_fixture(writer.conn_mut());
    }
    let store = FsBlobStore::new(dir.path().join("blobs"), 0)
        .unwrap()
        .with_orphan_sweep_grace(Duration::from_secs(60));

    let bytes = b"old orphan re-published".to_vec();
    let first = store.put(bytes.clone()).await.unwrap();

    // Age the blob well past the 60s grace floor -- no sleeps, same
    // backdating pattern as the existing older-than-grace test.
    let path = shard_path(store.root(), &first);
    let old_mtime = SystemTime::now() - Duration::from_secs(3600);
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(old_mtime)
        .unwrap();

    // A deduplicating put republishes the identical bytes. No attachment
    // anywhere references this blob yet.
    let second = store.put(bytes).await.unwrap();
    assert_eq!(first, second);

    // The sweep lands in the gap before the follow-up attachment write --
    // the refreshed mtime must keep it inside the grace window.
    let result = store
        .transactional_orphan_sweep(backend.sql().as_ref(), false)
        .await
        .unwrap();
    assert_eq!(
        result.deleted, 0,
        "a dedup-republished blob must survive a sweep landing before its reference \
             commits: {result:?}"
    );
    assert_eq!(
        result.grace_period_skipped, 1,
        "must be reported as grace-protected, not silently ignored: {result:?}"
    );
    assert!(store.exists(&first).await.unwrap());

    // The caller's follow-up record-plus-attachment write now lands.
    {
        let writer = backend.pool().writer().unwrap();
        writer
            .conn()
            .execute(
                "INSERT INTO entities \
                     (id, namespace, kind, name, tags, created_at, updated_at, deleted_at) \
                     VALUES ('e1', 'local', 'document', 'e1', '[]', 1, 1, NULL)",
                [],
            )
            .unwrap();
        writer
            .conn()
            .execute(
                "INSERT INTO attachments \
                     (record_uuid, substrate, role, content_ref, created_at) \
                     VALUES ('e1', 'entity', 'content', ?1, 1)",
                [first.as_str()],
            )
            .unwrap();
    }

    let result = store
        .transactional_orphan_sweep(backend.sql().as_ref(), false)
        .await
        .unwrap();
    assert_eq!(result.deleted, 0);
    assert!(
        store.exists(&first).await.unwrap(),
        "the blob must stay live once its reference has committed"
    );
}

#[tokio::test]
async fn put_dedup_mtime_refresh_has_no_observable_effect_under_zero_grace_period() {
    // The assumption the fix relies on for every zero-grace test in this
    // file: `within_publish_grace` with `Duration::ZERO` never protects a
    // candidate regardless of its mtime (`age < Duration::ZERO` is always
    // false), so refreshing the mtime on a deduplicated republish must
    // not change zero-grace sweep behavior. Verified directly rather
    // than assumed.
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    let backend = crate::StorageBackend::sqlite_for_test(&db_path).unwrap();
    {
        let mut writer = backend.pool().writer().unwrap();
        prepare_completed_v21_gc_fixture(writer.conn_mut());
    }
    let store = FsBlobStore::new(dir.path().join("blobs"), 0)
        .unwrap()
        .with_orphan_sweep_grace(Duration::ZERO);
    let bytes = b"zero grace dedup refresh".to_vec();
    let first = store.put(bytes.clone()).await.unwrap();
    let second = store.put(bytes.clone()).await.unwrap();
    assert_eq!(first, second);

    let result = store
        .transactional_orphan_sweep(backend.sql().as_ref(), false)
        .await
        .unwrap();
    assert_eq!(
        result.deleted, 1,
        "a zero grace period must still delete an unreferenced blob even after a dedup \
             put refreshed its mtime: {result:?}"
    );
    assert!(!store.exists(&first).await.unwrap());
}

#[tokio::test]
async fn transactional_orphan_sweep_still_removes_orphans_older_than_the_grace_period() {
    // The grace window narrows the publish-vs-sweep race, it does not
    // disable sweeping outright: an object whose age already exceeds a
    // (short, for this test) grace period is removed exactly as before,
    // proving the fix bounds the exposure rather than papering over it.
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    let backend = crate::StorageBackend::sqlite_for_test(&db_path).unwrap();
    {
        let mut writer = backend.pool().writer().unwrap();
        prepare_completed_v21_gc_fixture(writer.conn_mut());
    }
    let store = FsBlobStore::new(dir.path().join("blobs"), 0)
        .unwrap()
        .with_orphan_sweep_grace(Duration::from_secs(60));

    let orphan = store
        .put(b"actually orphaned, published long ago".to_vec())
        .await
        .unwrap();
    // Back-date the file's mtime well past the 60s grace period instead
    // of sleeping in the test.
    let path = shard_path(store.root(), &orphan);
    let old_mtime = SystemTime::now() - Duration::from_secs(3600);
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(old_mtime)
        .unwrap();

    let result = store
        .transactional_orphan_sweep(backend.sql().as_ref(), false)
        .await
        .unwrap();

    assert_eq!(
        result.deleted, 1,
        "an orphan older than the grace period must still be swept: {result:?}"
    );
    assert_eq!(result.grace_period_skipped, 0);
    assert!(!store.exists(&orphan).await.unwrap());
}

include!("blob/environment_tests.rs");

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transactional_orphan_sweep_walk_ignores_a_leaf_swapped_for_an_outside_symlink_mid_scan() {
    // Regression for #2201: the sweep's candidate
    // walk and grace-period mtime read used to be two separate,
    // path-based passes (`walk_blob_files` then `within_publish_grace`
    // via `fs::metadata(path)`), neither pinned to the retained
    // `root_handle`. A concurrent replacement of a leaf entry in the gap
    // between those passes could make the mtime read observe a file
    // OUTSIDE the retained root, letting a stale outside mtime evict the
    // grace period for a freshly published in-root blob. The fix folds
    // discovery and classification into one handle-relative
    // openat(..., O_NOFOLLOW) + fstat in
    // `walk_blob_files_from_root_handle`, so there is no later path
    // re-resolution left to race. This test forces exactly that swap —
    // via `walk_leaf_sync_hook`, between the leaf's hex-name discovery
    // and its classifying open — and proves the outside decoy's stale
    // mtime never reaches the sweep's counts.
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    let backend = std::sync::Arc::new(crate::StorageBackend::sqlite_for_test(&db_path).unwrap());
    {
        let mut writer = backend.pool().writer().unwrap();
        prepare_completed_v21_gc_fixture(writer.conn_mut());
    }

    let root = dir.path().join("blobs");
    let store = std::sync::Arc::new(
        FsBlobStore::new(root.clone(), 0)
            .unwrap()
            .with_orphan_sweep_grace(Duration::from_secs(3600)),
    );

    // The one real, unreferenced candidate the sweep will discover. Its
    // grace period is wide (1h) so correct handle-relative
    // classification always protects it.
    let real = store
        .put(b"real freshly-published blob".to_vec())
        .await
        .unwrap();
    let real_path = shard_path(&root, &real);

    // Outside decoy sharing the SAME leaf name (content-ref hex), aged
    // far past the grace period. If classification ever reads through
    // a swapped symlink, the candidate looks like a stale, deletable
    // orphan instead of a protected fresh publish.
    let outside_dir = dir.path().join("outside");
    fs::create_dir_all(&outside_dir).unwrap();
    let decoy_path = outside_dir.join(real.as_str());
    fs::write(&decoy_path, b"outside decoy, must never be observed").unwrap();
    let ancient = SystemTime::now() - Duration::from_secs(7200);
    fs::OpenOptions::new()
        .write(true)
        .open(&decoy_path)
        .unwrap()
        .set_modified(ancient)
        .unwrap();

    let (reached, release) = walk_leaf_sync_hook::install(&root);
    let sweep = {
        let store = store.clone();
        let sql = backend.sql();
        tokio::spawn(async move { store.transactional_orphan_sweep(sql.as_ref(), true).await })
    };
    assert!(
        recv_blocking(reached).await,
        "sweep walk must reach the leaf classification pause"
    );

    // Swap the real leaf out from under the paused walk: same name, now
    // a symlink resolving outside the retained root.
    fs::remove_file(&real_path).unwrap();
    std::os::unix::fs::symlink(&decoy_path, &real_path).unwrap();

    release.send(()).unwrap();
    let result = sweep.await.unwrap().unwrap();

    assert_eq!(
        result.would_delete, 0,
        "an outside decoy's stale mtime must never make an in-root candidate \
             eligible for deletion: {result:?}"
    );
    assert_eq!(
        result.grace_period_skipped, 0,
        "the swapped leaf is a symlink; `openat(..., O_NOFOLLOW)` refuses it, so it \
             must be dropped from candidates entirely rather than counted (real or \
             outside) at all: {result:?}"
    );
    assert_eq!(
        result.scanned, 0,
        "the symlinked leaf must never be scanned as a candidate: {result:?}"
    );

    // Restore a real leaf and confirm the walk is not permanently wedged
    // by the hook: an un-swapped root still finds and reports a real,
    // grace-protected candidate normally.
    fs::remove_file(&real_path).unwrap();
    fs::write(&real_path, b"real freshly-published blob").unwrap();
    let control = store
        .transactional_orphan_sweep(backend.sql().as_ref(), true)
        .await
        .unwrap();
    assert_eq!(
        control.grace_period_skipped, 1,
        "control: the un-replaced root must still classify the real candidate as \
             grace-protected: {control:?}"
    );
    assert_eq!(control.would_delete, 0);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transactional_orphan_sweep_walk_still_finds_a_real_orphan_past_its_grace_period() {
    // Control for the swap test above, using an independent root (no
    // hook installed): the handle-relative walk must still classify and
    // report a genuine past-grace orphan as deletable, proving the fix
    // narrows the walk to descriptor-relative reads without disabling
    // orphan detection itself.
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("khive.db");
    let backend = crate::StorageBackend::sqlite_for_test(&db_path).unwrap();
    {
        let mut writer = backend.pool().writer().unwrap();
        prepare_completed_v21_gc_fixture(writer.conn_mut());
    }
    let root = dir.path().join("blobs");
    let store = FsBlobStore::new(root.clone(), 0)
        .unwrap()
        .with_orphan_sweep_grace(Duration::from_secs(60));

    let orphan = store.put(b"aged real orphan".to_vec()).await.unwrap();
    let path = shard_path(&root, &orphan);
    let ancient = SystemTime::now() - Duration::from_secs(3600);
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(ancient)
        .unwrap();

    let result = store
        .transactional_orphan_sweep(backend.sql().as_ref(), false)
        .await
        .unwrap();
    assert_eq!(
        result.deleted, 1,
        "a real orphan older than the grace period must still be swept: {result:?}"
    );
    assert!(!store.exists(&orphan).await.unwrap());
}
