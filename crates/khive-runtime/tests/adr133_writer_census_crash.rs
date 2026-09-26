//! ADR-133 crash boundary: a returned obligation survives an abrupt process
//! death while a distinct, uncommitted observability generation may vanish.

#![cfg(all(unix, feature = "fault-injection", feature = "test-internals"))]

use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use khive_db::StorageBackend;
use khive_runtime::audit_batch::{
    fault_injection, AuditBatch, AuditBatchConfig, AuditBatchControl, AuditCommitOutcome,
    AuditProducer, PreparedAuditRow,
};
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};
use khive_storage::Event;
use khive_types::{EventKind, SubstrateKind};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

const PHASE_ENV: &str = "KHIVE_WRITER_CENSUS_CRASH_PHASE";
const ROOT_ENV: &str = "KHIVE_WRITER_CENSUS_CRASH_ROOT";
const TEST_NAME: &str = "writer_census_crash_preserves_returned_rows";

#[derive(Serialize, Deserialize)]
struct RecordIds {
    note: Uuid,
    returned_audit: Uuid,
    in_flight_observability: Uuid,
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for_ready(guard: &mut ChildGuard, root: &Path) {
    let marker = root.join("in_flight_ready");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !marker.exists() {
        assert!(
            guard.0.try_wait().expect("probe writer child").is_none(),
            "writer child exited before the distinct in-flight generation: {}",
            std::fs::read_to_string(root.join("writer.stderr")).unwrap_or_default()
        );
        assert!(
            Instant::now() < deadline,
            "writer child never reached the in-flight generation: {}",
            std::fs::read_to_string(root.join("writer.stderr")).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn spawn_child(root: &Path, phase: &str) -> ChildGuard {
    let home = root.join("home");
    std::fs::create_dir_all(&home).expect("private child HOME");
    let stderr = std::fs::File::create(root.join(format!("{phase}.stderr"))).expect("child stderr");
    let mut command = Command::new(std::env::current_exe().expect("test executable"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("KHIVE_") {
            command.env_remove(key);
        }
    }
    ChildGuard(
        command
            .args(["--exact", TEST_NAME, "--nocapture", "--test-threads=1"])
            .current_dir(root)
            .env("HOME", &home)
            .env_remove("LATTICE_MODEL_CACHE")
            .env("KHIVE_TEST_HARNESS", "1")
            .env("KHIVE_WRITE_ROUTING", "compat")
            .env("KHIVE_WRITE_QUEUE", "0")
            .env(PHASE_ENV, phase)
            .env(ROOT_ENV, root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr))
            .spawn()
            .expect("spawn crash fixture child"),
    )
}

async fn writer_child(root: &Path) {
    let database = root.join("census.sqlite3");
    let backend = Arc::new(StorageBackend::sqlite_for_test(&database).expect("writer database"));
    backend.prepare_core_schema().expect("core schema");
    let events = backend.events().expect("event store");
    let runtime = KhiveRuntime::from_backend(
        backend,
        RuntimeConfig {
            db_path: Some(database),
            events_split: None,
            actor_id: Some("test:writer-census-crash".into()),
            packs: vec!["kg".into()],
            ..RuntimeConfig::no_embeddings()
        },
    );
    let token = runtime.authorize(Namespace::local()).expect("local token");
    let note_id = runtime
        .create_note(
            &token,
            "observation",
            None,
            "returned user-facing record",
            None,
            None,
            vec![],
        )
        .await
        .expect("user-facing note committed before return")
        .id;

    let batch = AuditBatch::new(events.clone(), AuditBatchConfig::default());
    let returned_audit = Event::new(
        "local",
        "create",
        EventKind::Audit,
        SubstrateKind::Note,
        "test:writer-census-crash",
    )
    .with_target(note_id);
    assert_eq!(
        batch
            .submit(PreparedAuditRow {
                event: returned_audit.clone(),
                producer: AuditProducer::DispatchSucceeded,
            })
            .await
            .expect("obligation committed before return"),
        AuditCommitOutcome::Committed
    );
    batch.quiesce().await.expect("returned generation idle");
    let returned = batch.test_snapshot();
    assert_eq!(returned.submitted_rows, 1);
    assert_eq!(returned.committed_rows, 1);
    assert_eq!(returned.store_batch_calls, 1);
    assert!(returned.is_idle());

    let in_flight_observability = Event::new(
        "local",
        "config.lock",
        EventKind::ConfigLocked,
        SubstrateKind::Event,
        "test:writer-census-crash",
    )
    .with_payload(json!({"key": "writer_census_crash", "value": "held"}));
    let ids = RecordIds {
        note: note_id,
        returned_audit: returned_audit.id,
        in_flight_observability: in_flight_observability.id,
    };
    std::fs::write(
        root.join("record_ids.json"),
        serde_json::to_vec(&ids).unwrap(),
    )
    .expect("publish record identities");

    fault_injection::arm_supervisor_sleep_before_spawn();
    let pending_batch = batch.clone();
    let pending = tokio::spawn(async move {
        pending_batch
            .submit(PreparedAuditRow {
                event: in_flight_observability,
                producer: AuditProducer::ConfigLocked,
            })
            .await
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let snapshot = batch.test_snapshot();
        if snapshot.in_flight_generation.is_some() {
            assert_eq!(snapshot.submitted_rows, 2);
            assert_eq!(snapshot.committed_rows, 1);
            assert_eq!(snapshot.store_batch_calls, 1);
            assert!(!pending.is_finished(), "held observability row returned");
            assert!(events
                .get_event(ids.in_flight_observability)
                .await
                .expect("probe uncommitted observability row")
                .is_none());
            std::fs::write(root.join("in_flight_ready"), b"ready")
                .expect("publish in-flight boundary");
            std::future::pending::<()>().await;
        }
        assert!(
            Instant::now() < deadline,
            "second generation never entered flight"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

fn recovery_child(root: &Path) {
    let ids: RecordIds = serde_json::from_slice(
        &std::fs::read(root.join("record_ids.json")).expect("published record identities"),
    )
    .expect("record identities");
    let reopened = StorageBackend::sqlite_for_test(root.join("census.sqlite3"))
        .expect("reopen database after abrupt termination");
    reopened.prepare_core_schema().expect("reopen schema");
    let notes = reopened.notes().expect("reopened notes");
    let events = reopened.events().expect("reopened events");
    let check = tokio::runtime::Runtime::new().expect("recovery runtime");
    check.block_on(async {
        let note = notes
            .get_note_including_deleted(ids.note)
            .await
            .expect("read returned note")
            .expect("returned user-facing note survives process crash");
        assert_eq!(note.content, "returned user-facing record");
        let returned = events
            .get_event(ids.returned_audit)
            .await
            .expect("read returned audit row")
            .expect("returned obligation survives process crash");
        assert_eq!(returned.target_id, Some(ids.note));
        assert!(
            events
                .get_event(ids.in_flight_observability)
                .await
                .expect("read held observability row")
                .is_none(),
            "only the uncommitted pure-observability row may be absent"
        );
    });
}

#[test]
fn writer_census_crash_preserves_returned_rows() {
    if let Ok(phase) = std::env::var(PHASE_ENV) {
        let root = std::path::PathBuf::from(std::env::var_os(ROOT_ENV).expect("child root"));
        match phase.as_str() {
            "writer" => {
                tokio::runtime::Runtime::new()
                    .expect("child runtime")
                    .block_on(writer_child(&root));
                unreachable!("parent must terminate writer child abruptly");
            }
            "recovery" => recovery_child(&root),
            _ => panic!("unexpected crash fixture phase: {phase}"),
        }
        return;
    }

    let fixture = tempfile::tempdir().expect("crash fixture");
    let root = fixture.path();
    let mut writer = spawn_child(root, "writer");
    wait_for_ready(&mut writer, root);
    writer.0.kill().expect("SIGKILL writer child");
    let status = writer.0.wait().expect("reap writer child");
    assert_eq!(status.signal(), Some(libc::SIGKILL));

    let mut recovery = spawn_child(root, "recovery");
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match recovery.0.try_wait().expect("probe recovery child") {
            Some(status) => {
                assert!(
                    status.success(),
                    "recovery child failed: {}",
                    std::fs::read_to_string(root.join("recovery.stderr")).unwrap_or_default()
                );
                break;
            }
            None => {
                assert!(
                    Instant::now() < deadline,
                    "recovery child exceeded 20 seconds"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}
