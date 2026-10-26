//! Process-isolated quota and descriptor-lifetime witnesses for code-map admission.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use rusqlite::Connection;

use super::*;

const CASE_ENV: &str = "KHIVE_CODE_MAP_BUDGET_CASE";
const ROLE_ENV: &str = "KHIVE_CODE_MAP_BUDGET_ROLE";
const PROBE_ENV: &str = "KHIVE_CODE_MAP_BUDGET_PRODUCTION";
const WORKER_LIMIT: Duration = Duration::from_secs(90);
const PROBE_LIMIT: Duration = Duration::from_secs(15);

fn fixture() -> tempfile::TempDir {
    let root = std::env::temp_dir()
        .canonicalize()
        .expect("setup: canonical temp root");
    tempfile::Builder::new()
        .prefix("kh-code-map-budget-")
        .tempdir_in(root)
        .expect("setup: private quota fixture")
}

fn run_child(fqn: &str, role: &str, production: Option<&Path>) -> Output {
    let capture = fixture();
    let stdout = capture.path().join("stdout");
    let stderr = capture.path().join("stderr");
    let mut command = Command::new(std::env::current_exe().expect("setup: test executable"));
    command
        .args(["--exact", fqn, "--nocapture", "--test-threads=1"])
        .env(CASE_ENV, fqn)
        .env(ROLE_ENV, role)
        .env_remove(PROBE_ENV)
        .stdout(Stdio::from(
            fs::File::create(&stdout).expect("setup: stdout capture"),
        ))
        .stderr(Stdio::from(
            fs::File::create(&stderr).expect("setup: stderr capture"),
        ));
    if let Some(path) = production {
        command.env(PROBE_ENV, path);
    }
    #[cfg(unix)]
    if role == "worker" {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().expect("setup: exact-case child spawn");
    let limit = if role == "probe" {
        PROBE_LIMIT
    } else {
        WORKER_LIMIT
    };
    let deadline = Instant::now() + limit;
    let status = loop {
        if let Some(status) = child.try_wait().expect("setup: poll child") {
            break status;
        }
        if Instant::now() >= deadline {
            #[cfg(unix)]
            if role == "worker" {
                // The worker owns this process group; its nested lock probe inherits it.
                // SAFETY: the positive child PID identifies the group created above.
                unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
            }
            let _ = child.kill();
            let _ = child.wait();
            panic!("WATCHDOG: exact-case child exceeded {limit:?}; timeout is invalid evidence");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    Output {
        status,
        stdout: fs::read(stdout).expect("setup: read child stdout"),
        stderr: fs::read(stderr).expect("setup: read child stderr"),
    }
}

fn require_child_pass(output: &Output, marker: &str) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success()
            && stdout.contains("running 1 test")
            && stdout.contains("test result: ok. 1 passed; 0 failed; 0 ignored;")
            && stdout.contains(marker),
        "child case did not complete its exact witness:\n{stdout}\n{stderr}"
    );
}

fn isolated(fqn: &str, body: impl FnOnce()) {
    assert_eq!(
        std::thread::current().name(),
        Some(fqn),
        "setup: exact libtest FQN"
    );
    if std::env::var(CASE_ENV).ok().as_deref() == Some(fqn) {
        match std::env::var(ROLE_ENV).ok().as_deref() {
            Some("worker") => {
                body();
                println!("QUOTA_WORKER_OK {fqn}");
                return;
            }
            Some("probe") => {
                production_lock_probe();
                println!("QUOTA_PROBE_OK {fqn}");
                return;
            }
            _ => panic!("setup: unknown isolated child role"),
        }
    }
    let output = run_child(fqn, "worker", None);
    require_child_pass(&output, &format!("QUOTA_WORKER_OK {fqn}"));
}

fn seeded_bytes(path: &Path) -> Vec<u8> {
    let connection = Connection::open(path).expect("setup: seed production fixture");
    connection
        .execute_batch(
            "CREATE TABLE witness(value INTEGER NOT NULL); INSERT INTO witness VALUES(7)",
        )
        .expect("setup: seed witness");
    drop(connection);
    fs::read(path).expect("setup: capture seed bytes before any production lock")
}

fn protected(path: &Path) -> ProductionBase {
    ProductionBase {
        path: path.to_path_buf(),
        kind: ProductionKind::Main,
    }
}

fn suffix(path: &Path, value: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(value);
    PathBuf::from(name)
}

fn clear_open_hook() {
    // A pre-open quota refusal leaves its thread-local hook unconsumed.
    set_before_os_open(|| {});
}

fn drift_open(
    guard: &CodeMapHandleGuard,
    production: &Path,
    access: OpenAccess,
    native_opens: &Arc<AtomicUsize>,
) -> GuardError {
    let companion = suffix(production, "-journal");
    let count = Arc::clone(native_opens);
    set_before_os_open(move || {
        count.fetch_add(1, Ordering::SeqCst);
        if companion.exists() {
            fs::remove_file(companion).expect("setup: remove protected companion");
        } else {
            fs::write(companion, b"protected snapshot drift")
                .expect("setup: add protected companion");
        }
        // This hook holds the production ledger mutex: never query counters here.
    });
    let error = guard
        .open(Role::Main, access)
        .expect_err("setup: drift must refuse opened handle");
    clear_open_hook();
    error
}

fn new_guard(target: &Path, production: &Path, mode: Mode) -> CodeMapHandleGuard {
    CodeMapHandleGuard::new(target.to_path_buf(), mode, vec![protected(production)])
        .expect("setup: construct quota guard")
}

fn require_drift(error: GuardError) {
    assert!(
        matches!(error, GuardError::ProtectedChanged),
        "setup: wrong drift refusal: {error:?}"
    );
}

#[test]
fn noisy_target_preserves_other_maps_and_global_limit() {
    const FQN: &str =
        "code_map_vfs::quarantine_budget_tests::noisy_target_preserves_other_maps_and_global_limit";
    isolated(FQN, || {
        let dir = fixture();
        let seed = seeded_bytes(&dir.path().join("seed.db"));
        let mut productions = Vec::new();
        let mut guards = Vec::new();
        for index in 0..4 {
            let target = dir.path().join(format!("map-{index}.db"));
            let production = dir.path().join(format!("protected-{index}.db"));
            fs::write(&target, &seed).expect("setup: write map");
            fs::write(&production, &seed).expect("setup: write protected fixture");
            guards.push(new_guard(&target, &production, Mode::Rollback));
            productions.push(production);
        }
        let opened = Arc::new(AtomicUsize::new(0));
        assert_eq!(
            quarantine_occupancy(),
            0,
            "setup: isolated ledger starts empty"
        );
        println!("QUOTA_SETUP_OK {FQN}");
        for _ in 0..40 {
            match drift_open(&guards[0], &productions[0], OpenAccess::ReadWrite, &opened) {
                GuardError::ProtectedChanged
                | GuardError::MapQuarantineFull
                | GuardError::QuarantineFull => {}
                other => panic!("setup: unexpected noisy-target refusal: {other:?}"),
            }
        }
        clear_open_hook();
        let blocked_by_global = match guards[1].open(Role::Main, OpenAccess::ReadWrite) {
            Ok(handle) => {
                drop(handle);
                false
            }
            Err(GuardError::QuarantineFull) => true,
            Err(other) => {
                panic!("setup: unrelated clean map refused for another reason: {other:?}")
            }
        };
        assert!(
            !blocked_by_global,
            "[5614:other-target-open] noisy A exhausted clean B"
        );
        assert_eq!(
            guards[0].quarantine_domain_counts(),
            (8, 0),
            "[5614:domain-eight] A retained more than eight"
        );
        assert_eq!(
            opened.load(Ordering::SeqCst),
            8,
            "[5614:domain-eight] A crossed native open after quota"
        );
        for index in 1..4 {
            for _ in 0..8 {
                require_drift(drift_open(
                    &guards[index],
                    &productions[index],
                    OpenAccess::ReadWrite,
                    &opened,
                ));
            }
        }
        assert_eq!(
            quarantine_occupancy(),
            32,
            "[5614:global-retention] every suspect handle is retained"
        );
        assert_eq!(
            quarantine_reserved_count(),
            0,
            "[5614:reservation-release] completed opens leave no reservation"
        );
        let overflow = dir.path().join("map-overflow.db");
        let global_refusal = match CodeMapHandleGuard::new(overflow, Mode::Rollback, Vec::new()) {
            Err(GuardError::QuarantineFull) => true,
            Ok(_) => false,
            Err(other) => panic!("setup: unexpected global-cap refusal: {other:?}"),
        };
        assert!(
            global_refusal,
            "[5614:global-limit-32] a thirty-third global slot was admitted"
        );
        assert_eq!(
            quarantine_occupancy(),
            32,
            "[5614:global-retention] quota refusal discarded handles"
        );
    });
}

#[test]
fn reconstructed_target_keeps_its_eight_slot_budget() {
    const FQN: &str =
        "code_map_vfs::quarantine_budget_tests::reconstructed_target_keeps_its_eight_slot_budget";
    isolated(FQN, || {
        let dir = fixture();
        let production = dir.path().join("protected.db");
        let seed = seeded_bytes(&production);
        let target = dir.path().join("map.db");
        fs::write(&target, seed).expect("setup: write map");
        let opened = Arc::new(AtomicUsize::new(0));
        let guard = new_guard(&target, &production, Mode::Rollback);
        println!("QUOTA_SETUP_OK {FQN}");
        for _ in 0..4 {
            require_drift(drift_open(
                &guard,
                &production,
                OpenAccess::ReadWrite,
                &opened,
            ));
        }
        drop(guard);
        let rebuilt = new_guard(&target, &production, Mode::Rollback);
        assert_eq!(
            rebuilt.quarantine_domain_counts(),
            (4, 0),
            "[5614:reconstructed-budget] rebuilding A reset its budget"
        );
        for _ in 0..4 {
            require_drift(drift_open(
                &rebuilt,
                &production,
                OpenAccess::ReadWrite,
                &opened,
            ));
        }
        let error = drift_open(&rebuilt, &production, OpenAccess::ReadWrite, &opened);
        assert!(
            matches!(error, GuardError::MapQuarantineFull),
            "[5614:reconstructed-budget] ninth A open was not quota-refused: {error:?}"
        );
        assert_eq!(
            opened.load(Ordering::SeqCst),
            8,
            "[5614:reconstructed-budget] ninth A attempt reached native open"
        );
        assert_eq!(
            rebuilt.quarantine_domain_counts(),
            (8, 0),
            "[5614:reconstructed-budget] shared budget is not eight"
        );
        assert_eq!(
            quarantine_occupancy(),
            8,
            "[5614:global-retention] reconstructed guard discarded suspects"
        );
    });
}

#[test]
fn concurrent_opens_cannot_exceed_eight_retained_handles() {
    const FQN: &str = "code_map_vfs::quarantine_budget_tests::concurrent_opens_cannot_exceed_eight_retained_handles";
    isolated(FQN, || {
        let dir = fixture();
        let seed = seeded_bytes(&dir.path().join("seed.db"));
        let target = dir.path().join("map.db");
        fs::write(&target, &seed).expect("setup: write stable shared map");
        let opened = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let (ready_tx, ready_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let mut workers = Vec::new();
        let mut observer = None;
        for index in 0..24 {
            let production = dir.path().join(format!("protected-{index}.db"));
            fs::write(&production, &seed).expect("setup: independent protected fixture");
            let guard = Arc::new(new_guard(&target, &production, Mode::Rollback));
            observer = Some(Arc::clone(&guard));
            let opened = Arc::clone(&opened);
            let gate = Arc::clone(&gate);
            let ready_tx = ready_tx.clone();
            let done_tx = done_tx.clone();
            workers.push(std::thread::spawn(move || {
                ready_tx.send(()).expect("setup: worker ready channel");
                let (lock, ready) = &*gate;
                let start = lock.lock().expect("setup: start gate");
                let (start, timeout) = ready
                    .wait_timeout_while(start, Duration::from_secs(15), |start| !*start)
                    .expect("setup: wait start gate");
                assert!(
                    *start && !timeout.timed_out(),
                    "WATCHDOG: worker start gate expired"
                );
                drop(start);
                let error = drift_open(&guard, &production, OpenAccess::ReadWrite, &opened);
                drop(guard);
                done_tx.send(error).expect("setup: worker result channel");
            }));
        }
        drop(ready_tx);
        drop(done_tx);
        for _ in 0..24 {
            ready_rx
                .recv_timeout(Duration::from_secs(15))
                .expect("WATCHDOG: worker did not become ready");
        }
        println!("QUOTA_SETUP_OK {FQN}");
        let (lock, start) = &*gate;
        *lock.lock().expect("setup: release start gate") = true;
        start.notify_all();
        let mut retained = 0;
        let mut refused = 0;
        for _ in 0..24 {
            match done_rx
                .recv_timeout(Duration::from_secs(15))
                .expect("WATCHDOG: worker open did not finish")
            {
                GuardError::ProtectedChanged => retained += 1,
                GuardError::MapQuarantineFull => refused += 1,
                other => panic!("setup: unexpected concurrent refusal: {other:?}"),
            }
        }
        for worker in workers {
            worker.join().expect("setup: worker panicked");
        }
        assert_eq!(
            (retained, refused),
            (8, 16),
            "[5614:concurrent-eight] concurrent attempts exceeded eight or lost a slot"
        );
        assert_eq!(
            opened.load(Ordering::SeqCst),
            8,
            "[5614:concurrent-eight] excess native opens were admitted"
        );
        assert_eq!(
            observer.unwrap().quarantine_domain_counts(),
            (8, 0),
            "[5614:concurrent-eight] retained/reserved domain state differs"
        );
        assert_eq!(
            quarantine_occupancy(),
            8,
            "[5614:global-retention] suspect descriptors were deduplicated"
        );
        assert_eq!(
            quarantine_reserved_count(),
            0,
            "[5614:reservation-release] concurrent reservations leaked"
        );
    });
}

fn production_lock_probe() {
    let production = std::env::var_os(PROBE_ENV).expect("setup: production lock probe path");
    let connection =
        Connection::open(PathBuf::from(production)).expect("setup: independent probe connection");
    connection
        .busy_timeout(Duration::ZERO)
        .expect("setup: immediate lock probe");
    let remained_locked = match connection.execute_batch("BEGIN IMMEDIATE") {
        Err(rusqlite::Error::SqliteFailure(error, _))
            if matches!(
                error.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) =>
        {
            true
        }
        Ok(()) => {
            connection
                .execute_batch("ROLLBACK")
                .expect("setup: release unexpected probe transaction");
            false
        }
        Err(other) => panic!("setup: independent probe failed for another reason: {other:?}"),
    };
    assert!(
        remained_locked,
        "[5614:production-lock-retained] another process acquired the production writer lock"
    );
}

#[cfg(unix)]
#[test]
fn quarantined_aliases_keep_the_production_writer_locked() {
    const FQN: &str = "code_map_vfs::quarantine_budget_tests::quarantined_aliases_keep_the_production_writer_locked";
    isolated(FQN, || {
        let dir = fixture();
        let production = dir.path().join("production.db");
        let seed = seeded_bytes(&production);
        let target = dir.path().join("map.db");
        fs::write(&target, &seed).expect("setup: write independent map");
        let guard = new_guard(&target, &production, Mode::Rollback);
        let connection = Connection::open(&production).expect("setup: production writer");
        connection
            .execute_batch("BEGIN IMMEDIATE")
            .expect("setup: hold production writer lock");
        println!("QUOTA_SETUP_OK {FQN}");
        for index in 0..8 {
            if index != 0 {
                fs::remove_file(&target).expect("setup: remove previous target link");
                fs::write(&target, &seed).expect("setup: restore target from cached bytes");
            }
            let source = production.clone();
            let destination = target.clone();
            set_before_os_open(move || {
                fs::remove_file(&destination).expect("setup: remove clean target");
                fs::hard_link(source, destination).expect("setup: plant production alias");
            });
            let error = guard
                .open(Role::Main, OpenAccess::ReadWrite)
                .expect_err("setup: alias must be refused");
            clear_open_hook();
            assert!(
                matches!(error, GuardError::ProtectedAlias { .. }),
                "setup: wrong alias refusal: {error:?}"
            );
        }
        // Do not read/copy/open production in this process while its POSIX lock is held.
        let probe = run_child(FQN, "probe", Some(&production));
        require_child_pass(&probe, &format!("QUOTA_PROBE_OK {FQN}"));
        assert_eq!(
            quarantine_occupancy(),
            8,
            "[5614:global-retention] every opened alias must remain retained"
        );
        assert_eq!(
            guard.quarantine_domain_counts(),
            (8, 0),
            "[5614:reservation-release] alias accounting differs"
        );
        connection
            .execute_batch("INSERT INTO witness VALUES(9); COMMIT")
            .expect("production writer must finish after probe");
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM witness", [], |row| row.get(0))
            .expect("read committed witness");
        assert_eq!(count, 2, "production writer lost its committed witness");
    });
}

#[test]
fn read_ingest_and_transition_share_one_target_budget() {
    const FQN: &str =
        "code_map_vfs::quarantine_budget_tests::read_ingest_and_transition_share_one_target_budget";
    isolated(FQN, || {
        let dir = fixture();
        let seed = seeded_bytes(&dir.path().join("seed.db"));
        let target = dir.path().join("map.db");
        fs::write(&target, &seed).expect("setup: write shared map");
        let identity = crate::file_identity::database_file_identity(&target)
            .expect("setup: registered-read identity");
        let mut productions = Vec::new();
        for index in 0..3 {
            let production = dir.path().join(format!("protected-{index}.db"));
            fs::write(&production, &seed).expect("setup: write protected fixture");
            productions.push(production);
        }
        let ingest = new_guard(&target, &productions[0], Mode::Rollback);
        let read = CodeMapHandleGuard::new_with_expected_identity(
            target.clone(),
            Mode::Rollback,
            Some(identity),
            vec![protected(&productions[1])],
        )
        .expect("setup: registered-read guard");
        let transition = new_guard(&target, &productions[2], Mode::QuiescentWalTransition);
        let opened = Arc::new(AtomicUsize::new(0));
        println!("QUOTA_SETUP_OK {FQN}");
        for (guard, production, access, count) in [
            (&ingest, &productions[0], OpenAccess::ReadWrite, 3),
            (&read, &productions[1], OpenAccess::ReadOnly, 3),
            (&transition, &productions[2], OpenAccess::ReadOnly, 2),
        ] {
            for _ in 0..count {
                require_drift(drift_open(guard, production, access, &opened));
            }
        }
        for guard in [&ingest, &read, &transition] {
            assert_eq!(
                guard.quarantine_domain_counts(),
                (8, 0),
                "[5614:shared-mode-budget] a read/ingest/transition contract split the slot budget"
            );
        }
        for (guard, production, access) in [
            (&ingest, &productions[0], OpenAccess::ReadWrite),
            (&read, &productions[1], OpenAccess::ReadOnly),
            (&transition, &productions[2], OpenAccess::ReadOnly),
        ] {
            let error = drift_open(guard, production, access, &opened);
            assert!(matches!(error, GuardError::MapQuarantineFull), "[5614:shared-mode-budget] exhausted shared domain admitted another open: {error:?}");
        }
        assert_eq!(
            opened.load(Ordering::SeqCst),
            8,
            "[5614:shared-mode-budget] another contract reopened the exhausted slot"
        );
        assert_eq!(
            quarantine_occupancy(),
            8,
            "[5614:global-retention] shared modes discarded suspects"
        );
        assert_eq!(
            quarantine_reserved_count(),
            0,
            "[5614:reservation-release] mixed-mode reservations leaked"
        );
    });
}
