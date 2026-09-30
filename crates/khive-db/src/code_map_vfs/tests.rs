use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use rusqlite::Connection;

use super::{callbacks, vfs, CodeMapHandleGuard, GuardError, Mode, Role};
#[cfg(unix)]
use super::{OpenAccess, ProductionBase, ProductionKind};
use crate::StorageBackend;

const CHILD_TEST: &str = "KHIVE_CODE_MAP_VFS_TEST_CHILD";
#[cfg(unix)]
const PRODUCTION_LOCK_PROBE: &str = "KHIVE_CODE_MAP_VFS_PRODUCTION_LOCK_PROBE";

fn run_in_child() -> bool {
    let thread = std::thread::current();
    let name = thread.name().expect("libtest names its test threads");
    if std::env::var(CHILD_TEST).ok().as_deref() == Some(name) {
        return false;
    }
    let output = Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(CHILD_TEST, name)
        .output()
        .expect("spawn isolated VFS test");
    assert!(
        output.status.success()
            && String::from_utf8_lossy(&output.stdout)
                .contains("test result: ok. 1 passed; 0 failed;"),
        "isolated VFS test failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

fn fixture() -> tempfile::TempDir {
    let plain_temp_root = std::env::temp_dir()
        .canonicalize()
        .expect("plain absolute temp root");
    tempfile::Builder::new()
        .prefix("kh-code-map-vfs-")
        .tempdir_in(plain_temp_root)
        .expect("private VFS fixture")
}

#[cfg(unix)]
#[test]
fn configured_production_parent_alias_is_sampled() {
    if run_in_child() {
        return;
    }
    let dir = fixture();
    let physical_ancestor = dir.path().join("physical-ancestor");
    let alias_ancestor = dir.path().join("alias-ancestor");
    let physical_root = physical_ancestor.join("configured-root");
    let configured_root = alias_ancestor.join("configured-root");
    std::fs::create_dir_all(&physical_root).unwrap();
    std::os::unix::fs::symlink(&physical_ancestor, &alias_ancestor).unwrap();
    let production = physical_root.join("production.db");
    seed_rollback(&production);
    let configured_production = configured_root.join("production.db");

    let target = dir.path().join("dedicated-map.db");
    let backend =
        StorageBackend::sqlite_code_map(&target, std::slice::from_ref(&configured_production), &[])
            .expect("configured parent alias resolves before protected leaf sampling");
    drop(backend);

    let alias = dir.path().join("production-hardlink.db");
    std::fs::hard_link(&production, &alias).unwrap();
    let error =
        StorageBackend::sqlite_code_map(&alias, std::slice::from_ref(&configured_production), &[])
            .err()
            .expect("physical production identity remains protected");
    assert!(
        error.to_string().contains("protected production identity"),
        "{error}"
    );
}

#[cfg(unix)]
#[test]
fn planted_target_parent_symlink_below_fixture_root_refuses() {
    if run_in_child() {
        return;
    }
    let dir = fixture();
    let physical_root = dir.path().join("target-root");
    let planted = dir.path().join("planted-link");
    std::fs::create_dir(&physical_root).unwrap();
    std::os::unix::fs::symlink(&physical_root, &planted).unwrap();
    let target = planted.join("code-map.db");
    let error = StorageBackend::sqlite_code_map(&target, &[], &[])
        .err()
        .expect("target parent symlink must refuse before opening a database");
    assert!(
        error
            .to_string()
            .contains("symlinked code-map parent component"),
        "{error}"
    );
    assert!(!physical_root.join("code-map.db").exists());
}

fn seed_rollback(path: &Path) {
    let conn = Connection::open(path).expect("seed SQLite database");
    conn.execute_batch(
        "CREATE TABLE witness(value INTEGER NOT NULL); INSERT INTO witness VALUES(7)",
    )
    .expect("seed witness row");
}

#[cfg(unix)]
fn main_base(path: &Path) -> ProductionBase {
    ProductionBase {
        path: path.to_path_buf(),
        kind: ProductionKind::Main,
    }
}

fn companion(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

#[test]
fn fresh_first_open_stays_in_delete_and_never_uses_shm() {
    if run_in_child() {
        return;
    }
    let dir = fixture();
    let target = dir.path().join("code-map.db");
    let registrations_before = vfs::registration_count();
    let shm_before = callbacks::shm_violation_count();
    let backend = StorageBackend::sqlite_code_map(&target, &[], &[]).expect("first guarded open");
    {
        let writer = backend.pool().writer().expect("guarded writer");
        let mode: String = writer
            .conn()
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("journal mode");
        assert_eq!(mode.to_ascii_lowercase(), "delete");
        writer
            .conn()
            .execute_batch(
                "CREATE TABLE witness(value INTEGER NOT NULL); INSERT INTO witness VALUES(1)",
            )
            .expect("first write through guarded VFS");
    }
    assert!(target.exists());
    assert!(!companion(&target, "-wal").exists());
    assert!(!companion(&target, "-shm").exists());
    assert_eq!(callbacks::shm_violation_count(), shm_before);
    assert_eq!(vfs::registration_count(), registrations_before + 1);
    drop(backend);

    let reopened = StorageBackend::sqlite_code_map(&target, &[], &[]).expect("reuse guarded VFS");
    assert_eq!(vfs::registration_count(), registrations_before + 1);
    let count: i64 = reopened
        .pool()
        .writer()
        .expect("reopened writer")
        .conn()
        .query_row("SELECT COUNT(*) FROM witness", [], |row| row.get(0))
        .expect("persisted first write");
    assert_eq!(count, 1);
}

#[test]
fn live_rollback_pool_reopens_without_quiescent_wal_transition() {
    if run_in_child() {
        return;
    }
    let dir = fixture();
    let target = dir.path().join("live-rollback-code-map.db");
    let first = StorageBackend::sqlite_code_map(&target, &[], &[]).expect("first guarded pool");
    first
        .pool()
        .writer()
        .expect("first writer")
        .conn()
        .execute_batch("CREATE TABLE witness(value INTEGER); INSERT INTO witness VALUES(7)")
        .expect("seed a committed DELETE-mode row");

    let second = StorageBackend::sqlite_code_map(&target, &[], &[])
        .expect("a live rollback pool must not trigger a WAL transition");
    let writer = second.pool().writer().expect("second guarded writer");
    let mode: String = writer
        .conn()
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("second journal mode");
    let count: i64 = writer
        .conn()
        .query_row("SELECT COUNT(*) FROM witness", [], |row| row.get(0))
        .expect("first pool's row remains visible");
    assert_eq!(mode.to_ascii_lowercase(), "delete");
    assert_eq!(count, 1);
    assert!(!companion(&target, "-wal").exists());
    assert!(!companion(&target, "-shm").exists());
}

#[test]
fn prior_wal_is_converted_to_delete_without_losing_rows_or_using_shm() {
    if run_in_child() {
        return;
    }
    let dir = fixture();
    let target = dir.path().join("prior-wal-code-map.db");
    let seed = Connection::open(&target).expect("seed prior WAL with ordinary SQLite");
    let seeded_mode: String = seed
        .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
        .expect("enable WAL on prior map");
    assert_eq!(seeded_mode.to_ascii_lowercase(), "wal");
    seed.execute_batch(
        "CREATE TABLE witness(value INTEGER NOT NULL); INSERT INTO witness VALUES(7)",
    )
    .expect("seed prior WAL row");
    drop(seed);

    let shm_before = callbacks::shm_violation_count();
    let backend = StorageBackend::sqlite_code_map(&target, &[], &[])
        .expect("guarded quiescent WAL transition");
    let writer = backend.pool().writer().expect("post-transition writer");
    let mode: String = writer
        .conn()
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("post-transition mode");
    let value: i64 = writer
        .conn()
        .query_row("SELECT value FROM witness", [], |row| row.get(0))
        .expect("prior WAL row survived");
    assert_eq!(mode.to_ascii_lowercase(), "delete");
    assert_eq!(value, 7);
    assert!(!companion(&target, "-wal").exists());
    assert!(!companion(&target, "-shm").exists());
    assert_eq!(callbacks::shm_violation_count(), shm_before);
}

#[test]
fn transition_rejects_new_wal_and_shm_names_before_cleanup() {
    if run_in_child() {
        return;
    }
    let dir = fixture();
    for (index, (suffix, role)) in [("-wal", Role::TransitionWal), ("-shm", Role::Shm)]
        .into_iter()
        .enumerate()
    {
        let target = dir.path().join(format!("prior-wal-{index}.db"));
        let seed = Connection::open(&target).unwrap();
        let mode: String = seed
            .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");
        seed.execute_batch("CREATE TABLE witness(value INTEGER)")
            .unwrap();
        drop(seed);
        for old_suffix in ["-wal", "-shm"] {
            let old = companion(&target, old_suffix);
            if old.exists() {
                std::fs::remove_file(old).unwrap();
            }
        }
        let guard = CodeMapHandleGuard::new(target.clone(), Mode::QuiescentWalTransition, vec![])
            .expect("attest original WAL target");
        let initial = guard.preflight().unwrap();
        let unexpected = companion(&target, suffix);
        std::fs::write(&unexpected, b"unattested-companion").unwrap();
        let error = guard
            .verify_transition_companions(&initial)
            .expect_err("new companion must fail attestation");
        assert!(matches!(&error, GuardError::ProtectedChanged), "{error}");
        let error = guard
            .delete(role, true)
            .expect_err("pre-switch companion deletion must be forbidden");
        assert!(matches!(&error, GuardError::Unsafe { .. }), "{error}");
        assert_eq!(
            std::fs::read(&unexpected).unwrap(),
            b"unattested-companion".to_vec()
        );
    }
}

#[test]
fn prior_wal_with_rollback_journal_refuses_incomplete_and_preserves_sidecars() {
    if run_in_child() {
        return;
    }
    let dir = fixture();
    let target = dir.path().join("prior-wal.db");
    let seed = Connection::open(&target).unwrap();
    let mode: String = seed
        .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode.to_ascii_lowercase(), "wal");
    seed.execute_batch("CREATE TABLE witness(value INTEGER)")
        .unwrap();
    drop(seed);
    let main_before = std::fs::read(&target).unwrap();
    let sidecars = [
        (companion(&target, "-wal"), b"prior-wal".to_vec()),
        (companion(&target, "-shm"), b"prior-shm".to_vec()),
        (companion(&target, "-journal"), b"prior-journal".to_vec()),
    ];
    for (path, bytes) in &sidecars {
        std::fs::write(path, bytes).unwrap();
    }
    let shm_before = callbacks::shm_violation_count();
    let error = super::prepare_rollback_target(target.clone(), vec![])
        .expect_err("mixed WAL and journal must refuse before DELETE setter");
    assert!(
        matches!(
            &error,
            super::transition::TransitionError::Incomplete {
                stage: "WAL admission",
                ..
            }
        ),
        "{error}"
    );
    assert_eq!(std::fs::read(&target).unwrap(), main_before);
    for (path, bytes) in &sidecars {
        assert_eq!(std::fs::read(path).unwrap().as_slice(), bytes.as_slice());
    }
    assert_eq!(callbacks::shm_violation_count(), shm_before);
}

#[test]
fn byte_copy_of_production_is_not_an_identity_alias() {
    if run_in_child() {
        return;
    }
    let dir = fixture();
    let production = dir.path().join("production.db");
    let target = dir.path().join("code-map.db");
    seed_rollback(&production);
    std::fs::copy(&production, &target).expect("copy equal bytes to a different inode");
    let backend = StorageBackend::sqlite_code_map(&target, std::slice::from_ref(&production), &[])
        .expect("independent byte copy must be admissible");
    backend
        .pool()
        .writer()
        .expect("guarded writer")
        .conn()
        .execute("INSERT INTO witness VALUES(8)", [])
        .expect("write independent code map");
    let original_count: i64 = Connection::open(&production)
        .expect("production remains openable")
        .query_row("SELECT COUNT(*) FROM witness", [], |row| row.get(0))
        .expect("production witness");
    assert_eq!(original_count, 1);
    assert_eq!(super::quarantine_occupancy(), 0);
}

#[cfg(unix)]
#[test]
fn main_hardlink_swap_is_quarantined_without_poisoning_production() {
    if let Some(path) = std::env::var_os(PRODUCTION_LOCK_PROBE) {
        let conn = Connection::open(PathBuf::from(path)).expect("open production from lock probe");
        conn.busy_timeout(std::time::Duration::ZERO).unwrap();
        let error = conn
            .execute_batch("BEGIN IMMEDIATE")
            .expect_err("other process must not acquire the production writer lock");
        assert!(matches!(
            &error,
            rusqlite::Error::SqliteFailure(sqlite, _)
                if matches!(
                    sqlite.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                )
        ));
        return;
    }
    if run_in_child() {
        return;
    }
    let dir = fixture();
    let production = dir.path().join("production.db");
    let target = dir.path().join("code-map.db");
    seed_rollback(&production);
    std::fs::copy(&production, &target).unwrap();
    let guard =
        CodeMapHandleGuard::new(target.clone(), Mode::Rollback, vec![main_base(&production)])
            .unwrap();
    let production_before = std::fs::read(&production).unwrap();
    let quarantine_before = super::quarantine_occupancy();
    let production_conn = Connection::open(&production).unwrap();
    production_conn
        .execute_batch("BEGIN IMMEDIATE")
        .expect("hold production writer lock before swapped open");
    let swap_target = target.clone();
    let swap_production = production.clone();
    super::set_before_os_open(move || {
        std::fs::remove_file(&swap_target).unwrap();
        std::fs::hard_link(&swap_production, &swap_target).unwrap();
    });
    let error = guard
        .open(Role::Main, OpenAccess::ReadWrite)
        .expect_err("swapped production identity must be refused");
    assert!(
        matches!(&error, GuardError::ProtectedAlias { .. }),
        "{error}"
    );
    assert_eq!(super::quarantine_occupancy(), quarantine_before + 1);

    // Closing another descriptor for this inode would release the writer's
    // process-wide POSIX record lock before the child can probe it.
    let name = std::thread::current().name().unwrap().to_owned();
    let probe = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name.as_str(), "--nocapture", "--test-threads=1"])
        .env(CHILD_TEST, &name)
        .env(PRODUCTION_LOCK_PROBE, &production)
        .output()
        .expect("probe production writer lock from another process");
    assert!(
        probe.status.success()
            && String::from_utf8_lossy(&probe.stdout)
                .contains("test result: ok. 1 passed; 0 failed;"),
        "production lock was lost after alias refusal:\n{}\n{}",
        String::from_utf8_lossy(&probe.stdout),
        String::from_utf8_lossy(&probe.stderr)
    );
    production_conn
        .execute_batch("INSERT INTO witness VALUES(9); COMMIT")
        .expect("same production connection must finish its write after refusal");
    let count: i64 = production_conn
        .query_row("SELECT COUNT(*) FROM witness", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 2);
    let production_after = std::fs::read(&production).unwrap();
    assert!(production_after.starts_with(b"SQLite format 3\0"));
    assert_ne!(production_after, production_before);
}

#[cfg(unix)]
#[test]
fn main_symlink_swap_is_refused_without_an_opened_handle() {
    if run_in_child() {
        return;
    }
    let dir = fixture();
    let production = dir.path().join("production.db");
    let target = dir.path().join("code-map.db");
    seed_rollback(&production);
    std::fs::copy(&production, &target).unwrap();
    let guard =
        CodeMapHandleGuard::new(target.clone(), Mode::Rollback, vec![main_base(&production)])
            .unwrap();
    let production_before = std::fs::read(&production).unwrap();
    let quarantine_before = super::quarantine_occupancy();
    let swap_target = target.clone();
    let swap_production = production.clone();
    super::set_before_os_open(move || {
        std::fs::remove_file(&swap_target).unwrap();
        std::os::unix::fs::symlink(&swap_production, &swap_target).unwrap();
    });
    let error = guard
        .open(Role::Main, OpenAccess::ReadWrite)
        .expect_err("symlink swap must be refused");
    assert!(matches!(&error, GuardError::Io { .. }), "{error}");
    assert_eq!(super::quarantine_occupancy(), quarantine_before);
    assert_eq!(std::fs::read(&production).unwrap(), production_before);
}

#[test]
fn multiply_linked_journal_is_refused_before_any_open() {
    if run_in_child() {
        return;
    }
    let dir = fixture();
    let target = dir.path().join("code-map.db");
    let other = dir.path().join("other-journal");
    seed_rollback(&target);
    std::fs::write(&other, b"journal-canary").unwrap();
    let journal = companion(&target, "-journal");
    std::fs::hard_link(&other, &journal).unwrap();
    let original = std::fs::read(&other).unwrap();
    let Err(error) = CodeMapHandleGuard::new(target.clone(), Mode::Rollback, vec![]) else {
        panic!("multiply linked journal must be refused");
    };
    assert!(matches!(&error, GuardError::Unsafe { .. }), "{error}");
    assert_eq!(std::fs::read(&other).unwrap(), original);
    assert_eq!(std::fs::read(&journal).unwrap(), original);
    assert_eq!(super::quarantine_occupancy(), 0);
}

#[cfg(unix)]
#[test]
fn quarantine_cap_refuses_before_another_native_open() {
    use std::sync::atomic::{AtomicBool, Ordering};

    if run_in_child() {
        return;
    }
    let dir = fixture();
    let production = dir.path().join("production.db");
    let target = dir.path().join("code-map.db");
    seed_rollback(&production);
    std::fs::copy(&production, &target).unwrap();
    let guard =
        CodeMapHandleGuard::new(target.clone(), Mode::Rollback, vec![main_base(&production)])
            .unwrap();
    assert_eq!(super::quarantine_occupancy(), 0);
    for count in 1..=super::QUARANTINE_CAP {
        if count > 1 {
            std::fs::remove_file(&target).unwrap();
            std::fs::copy(&production, &target).unwrap();
        }
        let swap_target = target.clone();
        let swap_production = production.clone();
        super::set_before_os_open(move || {
            std::fs::remove_file(&swap_target).unwrap();
            std::fs::hard_link(&swap_production, &swap_target).unwrap();
        });
        let error = guard
            .open(Role::Main, OpenAccess::ReadWrite)
            .expect_err("swapped identity must be quarantined");
        assert!(
            matches!(&error, GuardError::ProtectedAlias { .. }),
            "{error}"
        );
        assert_eq!(super::quarantine_occupancy(), count);
    }
    let opened = Arc::new(AtomicBool::new(false));
    let hook_opened = Arc::clone(&opened);
    super::set_before_os_open(move || hook_opened.store(true, Ordering::SeqCst));
    let error = guard
        .open(Role::Main, OpenAccess::ReadWrite)
        .expect_err("quarantine cap must refuse further opens");
    assert!(matches!(&error, GuardError::QuarantineFull), "{error}");
    assert!(!opened.load(Ordering::SeqCst));
    let Err(error) = CodeMapHandleGuard::new(target, Mode::Rollback, vec![]) else {
        panic!("new guards must also refuse at the cap");
    };
    assert!(matches!(&error, GuardError::QuarantineFull), "{error}");
    assert!(error.to_string().contains("restart"));
}

#[test]
fn rollback_registration_reuses_contract_then_caps_distinct_targets() {
    if run_in_child() {
        return;
    }
    let dir = fixture();
    let target = dir.path().join("code-map-0.db");
    let before = vfs::registration_count();
    let first = Arc::new(CodeMapHandleGuard::new(target.clone(), Mode::Rollback, vec![]).unwrap());
    let first_name = vfs::register(first).expect("first rollback VFS registration");
    let again = Arc::new(CodeMapHandleGuard::new(target, Mode::Rollback, vec![]).unwrap());
    assert_eq!(vfs::register(again).unwrap(), first_name);
    assert_eq!(vfs::registration_count(), before + 1);

    for index in 1..512 {
        let target = dir.path().join(format!("code-map-{index}.db"));
        let guard = Arc::new(CodeMapHandleGuard::new(target, Mode::Rollback, vec![]).unwrap());
        vfs::register(guard).expect("distinct target below registration cap");
    }
    assert_eq!(vfs::registration_count(), before + 512);
    let overflow = dir.path().join("code-map-overflow.db");
    let guard = Arc::new(CodeMapHandleGuard::new(overflow, Mode::Rollback, vec![]).unwrap());
    let error = vfs::register(guard).expect_err("distinct target beyond the cap must be refused");
    assert!(matches!(&error, GuardError::RegistrationFull), "{error}");
    assert!(error.to_string().contains("restart"));
    assert_eq!(vfs::registration_count(), before + 512);
}
