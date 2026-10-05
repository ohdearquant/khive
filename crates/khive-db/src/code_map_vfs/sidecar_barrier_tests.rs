use std::time::SystemTime;

use super::*;
use crate::code_map_vfs::{quarantine_occupancy, set_before_os_open};

/// How a target member is replaced between the guard's preflight and its open.
#[derive(Clone, Copy)]
enum Swap {
    /// A hard link to the production main database.
    ProtectedHardLink,
    /// A symlink to the production main database.
    Symlink,
}

/// A production main database and a dedicated map that is a byte copy of it.
fn production_and_target() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = fixture();
    let production = dir.path().join("production.db");
    let target = dir.path().join("code-map.db");
    seed_rollback(&production);
    std::fs::copy(&production, &target).unwrap();
    (dir, production, target)
}

/// Presence, bytes and modification time of a production main database and
/// each of its companions.
fn production_members(production: &Path) -> Vec<Option<(Vec<u8>, SystemTime)>> {
    ["", "-journal", "-wal", "-shm"]
        .into_iter()
        .map(|suffix| {
            let path = companion(production, suffix);
            let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
            Some((std::fs::read(&path).ok()?, modified))
        })
        .collect()
}

/// The second process of a lock probe: it must not be able to take the
/// production writer lock while the first process holds it.
fn run_as_lock_probe() -> bool {
    let Some(path) = std::env::var_os(PRODUCTION_LOCK_PROBE) else {
        return false;
    };
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
    true
}

/// Closing another descriptor for the production inode would release the
/// writer's process-wide POSIX record lock, so probe it from another process.
fn assert_production_writer_lock_is_held(production: &Path) {
    let name = std::thread::current().name().unwrap().to_owned();
    let probe = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name.as_str(), "--nocapture", "--test-threads=1"])
        .env(CHILD_TEST, &name)
        .env(PRODUCTION_LOCK_PROBE, production)
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
}

/// Swap the target's `suffix` member for `swap` between the guard's preflight
/// and its native open of `role`, and prove the refusal leaves production
/// intact: its files are unchanged, its writer lock survives, and the same
/// process can still finish a production write.
fn assert_swap_refused(mode: Mode, role: Role, suffix: &str, swap: Swap) {
    let (_dir, production, target) = production_and_target();
    let member = companion(&target, suffix);
    if !member.exists() {
        std::fs::write(&member, b"harmless member").unwrap();
    }
    let guard = CodeMapHandleGuard::new(target, mode, vec![main_base(&production)]).unwrap();
    // Reading a production member opens and closes a descriptor for its inode,
    // which releases this process's POSIX record locks on it. Snapshot before
    // taking the writer lock, and compare only after the lock probe below.
    let members_before = production_members(&production);
    let production_conn = Connection::open(&production).unwrap();
    production_conn
        .execute_batch("BEGIN IMMEDIATE")
        .expect("hold the production writer lock before the swapped open");
    let quarantine_before = quarantine_occupancy();
    let source = production.clone();
    set_before_os_open(move || {
        std::fs::remove_file(&member).unwrap();
        match swap {
            Swap::ProtectedHardLink => std::fs::hard_link(&source, &member).unwrap(),
            Swap::Symlink => std::os::unix::fs::symlink(&source, &member).unwrap(),
        }
    });
    let error = guard
        .open(role, OpenAccess::ReadWrite)
        .expect_err("a swapped member must be refused");
    match swap {
        Swap::ProtectedHardLink => {
            assert!(
                matches!(&error, GuardError::ProtectedAlias { .. }),
                "{error}"
            );
            assert_eq!(quarantine_occupancy(), quarantine_before + 1);
        }
        Swap::Symlink => {
            assert!(matches!(&error, GuardError::Io { .. }), "{error}");
            assert_eq!(quarantine_occupancy(), quarantine_before);
        }
    }
    if matches!(swap, Swap::ProtectedHardLink) {
        assert_production_writer_lock_is_held(&production);
    }
    assert_eq!(production_members(&production), members_before);
    production_conn
        .execute_batch("INSERT INTO witness VALUES(9); COMMIT")
        .expect("the production connection must finish its write after the refusal");
    let count: i64 = production_conn
        .query_row("SELECT COUNT(*) FROM witness", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 2);
}

#[test]
fn journal_swapped_to_a_protected_hardlink_is_quarantined_without_poisoning_production() {
    if run_as_lock_probe() {
        return;
    }
    if run_in_child() {
        return;
    }
    assert_swap_refused(
        Mode::Rollback,
        Role::Journal,
        "-journal",
        Swap::ProtectedHardLink,
    );
}

#[test]
fn journal_swapped_to_a_symlink_is_refused_without_an_opened_handle() {
    if run_in_child() {
        return;
    }
    assert_swap_refused(Mode::Rollback, Role::Journal, "-journal", Swap::Symlink);
}

#[test]
fn wal_swapped_to_a_protected_hardlink_is_quarantined_without_poisoning_production() {
    if run_as_lock_probe() {
        return;
    }
    if run_in_child() {
        return;
    }
    assert_swap_refused(
        Mode::QuiescentWalTransition,
        Role::TransitionWal,
        "-wal",
        Swap::ProtectedHardLink,
    );
}

#[test]
fn wal_swapped_to_a_symlink_is_refused_without_an_opened_handle() {
    if run_in_child() {
        return;
    }
    assert_swap_refused(
        Mode::QuiescentWalTransition,
        Role::TransitionWal,
        "-wal",
        Swap::Symlink,
    );
}

#[test]
fn main_symlink_swap_leaves_production_writable() {
    if run_in_child() {
        return;
    }
    assert_swap_refused(Mode::Rollback, Role::Main, "", Swap::Symlink);
}

#[test]
fn transition_sidecar_hardlinked_to_a_protected_member_refuses_at_admission() {
    if run_in_child() {
        return;
    }
    let dir = fixture();
    let production = dir.path().join("production.db");
    seed_rollback(&production);
    for suffix in ["-journal", "-wal", "-shm"] {
        let path = companion(&production, suffix);
        std::fs::write(path, suffix).unwrap();
    }
    let production_before = production_members(&production);
    let mut case = 0;
    for sidecar in ["-wal", "-shm"] {
        for member in ["", "-journal", "-wal", "-shm"] {
            case += 1;
            let target = dir.path().join(format!("code-map-{case}.db"));
            std::fs::copy(&production, &target).unwrap();
            let planted = companion(&target, sidecar);
            std::fs::hard_link(companion(&production, member), &planted).unwrap();
            let protected = vec![main_base(&production)];
            let guard = CodeMapHandleGuard::new(target, Mode::QuiescentWalTransition, protected);
            let Err(error) = guard else {
                panic!("{sidecar} hard-linked to production{member} must be refused");
            };
            assert!(
                matches!(&error, GuardError::ProtectedAlias { .. }),
                "{sidecar} -> production{member}: {error}"
            );
            assert!(planted.exists());
        }
    }
    assert_eq!(production_members(&production), production_before);
    assert_eq!(quarantine_occupancy(), 0);
}

#[test]
fn prior_wal_target_with_a_protected_hardlinked_sidecar_refuses_before_any_mutation() {
    if run_in_child() {
        return;
    }
    let dir = fixture();
    let production = dir.path().join("production.db");
    seed_rollback(&production);
    let production_before = production_members(&production);
    for (index, suffix) in ["-wal", "-shm"].into_iter().enumerate() {
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
        let planted = companion(&target, suffix);
        std::fs::hard_link(&production, &planted).unwrap();
        let main_before = std::fs::read(&target).unwrap();
        let protected = vec![main_base(&production)];
        let result = crate::code_map_vfs::prepare_rollback_target(target.clone(), protected);
        let error = result.expect_err("a protected sidecar must refuse the transition");
        let text = error.to_string();
        assert!(text.contains("incomplete at admission"), "{text}");
        assert!(text.contains("protected production identity"), "{text}");
        assert_eq!(std::fs::read(&target).unwrap(), main_before);
        assert!(planted.exists());
    }
    assert_eq!(production_members(&production), production_before);
    assert_eq!(quarantine_occupancy(), 0);
}

#[test]
fn multiply_linked_transition_sidecars_are_refused_before_any_open() {
    if run_in_child() {
        return;
    }
    let dir = fixture();
    for suffix in ["-wal", "-shm"] {
        let target = dir.path().join(format!("map{suffix}.db"));
        let other = dir.path().join(format!("other{suffix}"));
        seed_rollback(&target);
        std::fs::write(&other, b"sidecar-canary").unwrap();
        let sidecar = companion(&target, suffix);
        std::fs::hard_link(&other, &sidecar).unwrap();
        let original = std::fs::read(&other).unwrap();
        let guard = CodeMapHandleGuard::new(target, Mode::QuiescentWalTransition, vec![]);
        let Err(error) = guard else {
            panic!("a multiply linked {suffix} sidecar must be refused");
        };
        assert!(matches!(&error, GuardError::Unsafe { .. }), "{error}");
        assert_eq!(std::fs::read(&other).unwrap(), original);
        assert_eq!(std::fs::read(&sidecar).unwrap(), original);
    }
    assert_eq!(quarantine_occupancy(), 0);
}

/// Refuse one open of `guard` for a reason unrelated to the protected set: the
/// target's rollback journal is a hard link of another file.
fn refuse_once(dir: &Path, guard: &CodeMapHandleGuard, target: &Path) {
    let journal = companion(target, "-journal");
    let other = dir.join("other-journal");
    std::fs::write(&other, b"journal-canary").unwrap();
    std::fs::hard_link(&other, &journal).unwrap();
    let error = guard
        .open(Role::Main, OpenAccess::ReadWrite)
        .expect_err("a multiply linked journal must refuse the open");
    assert!(matches!(&error, GuardError::Unsafe { .. }), "{error}");
    std::fs::remove_file(&journal).unwrap();
}

// Each open stats the protected set itself. An open that reused the set from
// the refusal would admit the swapped name below, and would refuse the same
// name once the companion is gone.
#[test]
fn protected_companion_created_after_a_refusal_is_sampled_by_the_next_open() {
    if run_in_child() {
        return;
    }
    let (dir, production, target) = production_and_target();
    let guard =
        CodeMapHandleGuard::new(target.clone(), Mode::Rollback, vec![main_base(&production)])
            .unwrap();
    refuse_once(dir.path(), &guard, &target);

    // A production companion now names the target's main file.
    let production_journal = companion(&production, "-journal");
    std::fs::hard_link(&target, &production_journal).unwrap();
    let error = guard
        .open(Role::Main, OpenAccess::ReadWrite)
        .expect_err("a protected companion created after the refusal must be sampled");
    assert!(
        matches!(&error, GuardError::ProtectedAlias { .. }),
        "{error}"
    );

    // With the companion gone the same open is admitted again.
    std::fs::remove_file(&production_journal).unwrap();
    let opened = guard
        .open(Role::Main, OpenAccess::ReadWrite)
        .expect("the next open samples the protected set afresh");
    drop(opened);
    assert_eq!(quarantine_occupancy(), 0);
}

#[test]
fn protected_main_replaced_after_a_refusal_is_sampled_by_the_next_open() {
    if run_in_child() {
        return;
    }
    let (dir, production, target) = production_and_target();
    let guard =
        CodeMapHandleGuard::new(target.clone(), Mode::Rollback, vec![main_base(&production)])
            .unwrap();
    refuse_once(dir.path(), &guard, &target);

    // The production path now names the target's main file.
    let saved = dir.path().join("production-saved.db");
    std::fs::rename(&production, &saved).unwrap();
    std::fs::hard_link(&target, &production).unwrap();
    let error = guard
        .open(Role::Main, OpenAccess::ReadWrite)
        .expect_err("a protected main replaced after the refusal must be sampled");
    assert!(
        matches!(&error, GuardError::ProtectedAlias { .. }),
        "{error}"
    );

    // With the original production file back the same open is admitted again.
    std::fs::remove_file(&production).unwrap();
    std::fs::rename(&saved, &production).unwrap();
    let opened = guard
        .open(Role::Main, OpenAccess::ReadWrite)
        .expect("the next open samples the protected set afresh");
    drop(opened);
    assert_eq!(quarantine_occupancy(), 0);
}
