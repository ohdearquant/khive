#![cfg(unix)]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use khive_db::{SqliteError, StorageBackend, WalCeilingSource};
use khive_runtime::daemon::{read_frame, write_frame};
use khive_runtime::events_split::{
    direct_backend_for, direct_backend_read_only_for, run_events_daemon, EventsRequest,
    EventsResponse, TestRegistryGuard, EVENTS_PROTOCOL_VERSION,
};
use khive_runtime::{KhiveRuntime, RuntimeConfig, RuntimeError};
use khive_storage::EventFilter;
use tokio::net::UnixStream;

const CHILD_CASE: &str = "KHIVE_EVENTS_WAL_TEST_CASE";
const CHILD_ROOT: &str = "KHIVE_EVENTS_WAL_TEST_ROOT";
const TIMEOUT_SINK_ENV: &str = "KHIVE_WRITER_TIMEOUT_SINK_DIR";
const WAL_ENV: &str = "KHIVE_SQLITE_WAL_CEILING_BYTES";
const RAW_CASES: [Option<&str>; 6] = [
    None,
    Some("0"),
    Some("abc"),
    Some("8192"),
    Some("1"),
    Some("9223372036854775808"),
];

fn run_case(case: &str, raw: Option<&str>) {
    let root = tempfile::Builder::new()
        .prefix("events-wal-")
        .tempdir_in("/tmp")
        .expect("isolated event fixture");
    let mut child = Command::new(std::env::current_exe().expect("test executable"));
    child
        .args(["--exact", "events_wal_child", "--ignored", "--nocapture"])
        .env_clear()
        .env("HOME", root.path().join("home"))
        .env(TIMEOUT_SINK_ENV, root.path().join("writer-timeouts"))
        .env(CHILD_CASE, case)
        .env(CHILD_ROOT, root.path());
    if let Some(raw) = raw {
        child.env(WAL_ENV, raw);
    }
    let output = child.output().expect("spawn isolated event fixture");
    assert!(
        output.status.success(),
        "EVENT_WAL_POLICY: {case}, {raw:?}\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("EVENT_WAL_CASE_EXECUTED"),
        "the child must execute the requested fixture"
    );
    assert!(
        !root.path().join("home").exists(),
        "the isolated event child must leave HOME absent through process exit"
    );
    if case == "daemon" && matches!(raw, None | Some("0")) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("wal_ceiling_configured_bytes=0"));
        assert!(stderr.contains("wal_ceiling_effective_bytes=0"));
        let source = if raw.is_some() {
            "Environment"
        } else {
            "Default"
        };
        assert!(stderr.contains(&format!("wal_ceiling_source={source}")));
    }
}

#[test]
fn split_event_openers_resolve_wal_ceiling() {
    for case in ["writable", "readonly"] {
        for raw in RAW_CASES {
            run_case(case, raw);
        }
    }
}

#[test]
fn events_daemon_resolves_wal_ceiling() {
    for raw in RAW_CASES {
        run_case("daemon", raw);
    }
}

#[test]
fn cached_event_backends_validate_requested_wal_ceiling() {
    run_case("cache_writable", None);
    run_case("cache_readonly", Some("8192"));
}

fn seed_snapshot(path: &Path) {
    let seed = StorageBackend::sqlite_for_test(path).expect("explicit disabled fixture backend");
    seed.prepare_core_schema().expect("current fixture schema");
}

fn main_config(path: &Path) -> RuntimeConfig {
    RuntimeConfig {
        db_path: Some(path.to_path_buf()),
        ..RuntimeConfig::no_embeddings()
    }
}

fn assert_same_refusal(event_error: &RuntimeError, main_error: &RuntimeError) {
    match (event_error, main_error) {
        (
            RuntimeError::Sqlite(SqliteError::InvalidConfig(event)),
            RuntimeError::Sqlite(SqliteError::InvalidConfig(main)),
        ) => assert_eq!(event, main),
        (
            RuntimeError::Sqlite(SqliteError::WalCapacityUnavailable {
                bytes: event_bytes,
                capability: event_capability,
            }),
            RuntimeError::Sqlite(SqliteError::WalCapacityUnavailable {
                bytes: main_bytes,
                capability: main_capability,
            }),
        ) => assert_eq!(
            (event_bytes, event_capability),
            (main_bytes, main_capability)
        ),
        (
            RuntimeError::Sqlite(SqliteError::WalCeilingBelowMinimum {
                bytes: event_bytes,
                page_size: event_page,
                minimum_bytes: event_minimum,
            }),
            RuntimeError::Sqlite(SqliteError::WalCeilingBelowMinimum {
                bytes: main_bytes,
                page_size: main_page,
                minimum_bytes: main_minimum,
            }),
        ) => assert_eq!(
            (event_bytes, event_page, event_minimum),
            (main_bytes, main_page, main_minimum)
        ),
        _ => panic!("event refusal differs from main: {event_error:?}; {main_error:?}"),
    }
}

fn assert_policy_report(backend: &StorageBackend, raw: Option<&str>, readonly: bool) {
    let configured = raw.map_or(0, |value| {
        value.parse::<u64>().expect("valid fixture ceiling")
    });
    let source = if raw.is_some() {
        WalCeilingSource::Environment
    } else {
        WalCeilingSource::Default
    };
    assert_eq!(backend.pool().config().wal_ceiling.bytes, configured);
    assert_eq!(backend.pool().config().wal_ceiling.source, source);
    let report = khive_db::diagnostics::collect(
        backend.pool(),
        khive_db::diagnostics::BuildIdentity::from_env("test", None),
        Duration::from_secs(30),
    );
    assert_eq!(report.wal_ceiling.configured_bytes, configured);
    assert_eq!(report.wal_ceiling.effective_bytes, 0);
    assert_eq!(report.wal_ceiling.source, source);
    assert!(!report.wal_ceiling.enabled);
    assert_eq!(
        report.wal_ceiling.status,
        if readonly && configured > 0 {
            "read_only_not_enforced"
        } else {
            "disabled"
        }
    );
}

async fn daemon_case(root: &Path, raw: Option<&str>) {
    let db = root.join("before-open").join("events.db");
    let socket = root.join("before-open").join("events.sock");
    if !matches!(raw, None | Some("0")) {
        let main_error = KhiveRuntime::new_for_test(main_config(&root.join("main.db")))
            .err()
            .expect("main writer refuses configured ceiling");
        let error = tokio::time::timeout(Duration::from_secs(2), run_events_daemon(&db, &socket))
            .await
            .expect("event configuration refusal must not enter the serving loop")
            .expect_err("event daemon must refuse the same policy as main");
        let event_error = error
            .downcast_ref::<RuntimeError>()
            .expect("typed runtime refusal");
        assert_same_refusal(event_error, &main_error);
        if matches!(
            event_error,
            RuntimeError::Sqlite(SqliteError::InvalidConfig(_))
        ) {
            assert!(!db.parent().expect("fixture parent").exists());
        }
        assert!(!socket.exists());
        return;
    }

    let main = KhiveRuntime::new_for_test(main_config(&root.join("main.db")))
        .expect("disabled main writer opens");
    assert_policy_report(main.backend(), raw, false);
    let (db_clone, socket_clone) = (db.clone(), socket.clone());
    let daemon = tokio::spawn(async move { run_events_daemon(&db_clone, &socket_clone).await });
    let mut stream = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            assert!(!daemon.is_finished(), "events daemon exited before binding");
            if let Ok(stream) = UnixStream::connect(&socket).await {
                break stream;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("events daemon must bind within the fixture deadline");
    let request = EventsRequest::CountEvents {
        protocol_version: EVENTS_PROTOCOL_VERSION,
        namespace: "local".to_owned(),
        filter: EventFilter::default(),
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        write_frame(&mut stream, &serde_json::to_vec(&request).unwrap())
            .await
            .unwrap();
        let response: EventsResponse =
            serde_json::from_slice(&read_frame(&mut stream).await.unwrap())
                .expect("event response");
        assert!(matches!(response, EventsResponse::Count { count: 0 }));
    })
    .await
    .expect("events daemon must serve a request");
    drop(stream);
    daemon.abort();
    assert!(daemon.await.unwrap_err().is_cancelled());
}

fn cache_case(root: &Path, readonly: bool) {
    let db = root.join("events.db");
    if readonly {
        seed_snapshot(&db);
    }
    let open = || {
        if readonly {
            direct_backend_read_only_for(&db)
        } else {
            direct_backend_for(&db)
        }
    };
    let original = open().expect("initial configured backend");
    std::env::set_var(WAL_ENV, "abc");
    assert!(
        matches!(open(), Err(RuntimeError::Sqlite(SqliteError::InvalidConfig(message)))
        if message.contains(WAL_ENV))
    );
    std::env::set_var(WAL_ENV, if readonly { "0" } else { "8192" });
    assert!(
        matches!(open(), Err(RuntimeError::Sqlite(SqliteError::InvalidConfig(message)))
        if message.contains("drain and restart"))
    );
    std::env::set_var(WAL_ENV, if readonly { "8192" } else { "0" });
    let reused = open().expect("equal numeric policy reuses the original pool");
    assert!(Arc::ptr_eq(&original, &reused));
}

async fn assert_private_timeout_sink(root: &Path, expected: bool) {
    let directory = root.join("writer-timeouts");
    assert_eq!(
        std::path::PathBuf::from(
            std::env::var_os(TIMEOUT_SINK_ENV).expect("private timeout sink override")
        ),
        directory
    );
    if !expected {
        assert!(
            !directory.exists(),
            "fresh writable policy refusal must not initialize timeout telemetry"
        );
        return;
    }

    let filename = format!("writer_timeouts.{}.ndjson", std::process::id());
    let log = directory.join(&filename);
    // Pool construction starts this thread asynchronously, so observe its complete startup row.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(mut file) = std::fs::File::open(&log) {
                let mut prefix = [0_u8; 4096];
                let length = std::io::Read::read(&mut file, &mut prefix)
                    .expect("read private timeout startup row");
                if let Some(end) = prefix[..length].iter().position(|byte| *byte == b'\n') {
                    let row: serde_json::Value = serde_json::from_slice(&prefix[..end])
                        .expect("complete timeout startup JSON");
                    assert_eq!(row["kind"].as_str(), Some("startup"));
                    assert_eq!(row["pid"].as_u64(), Some(u64::from(std::process::id())));
                    break;
                }
                assert!(
                    length < prefix.len(),
                    "timeout startup row exceeds fixture bound"
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("private writer timeout sink must publish its startup row within five seconds");

    for entry in std::fs::read_dir(&directory).expect("private timeout sink directory") {
        let entry = entry.expect("private timeout sink entry");
        assert_eq!(
            entry.file_name().as_os_str(),
            std::ffi::OsStr::new(&filename)
        );
        assert!(entry
            .file_type()
            .expect("timeout sink entry type")
            .is_file());
    }
}

#[tokio::test]
#[ignore = "executed only in a fresh process by the parent fixtures"]
async fn events_wal_child() {
    tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(std::io::stderr)
        .init();
    let case = std::env::var(CHILD_CASE).expect("isolated fixture case");
    let root = std::path::PathBuf::from(std::env::var_os(CHILD_ROOT).expect("fixture root"));
    let _registry_guard = TestRegistryGuard::new(&root);
    let raw = std::env::var(WAL_ENV).ok();
    if case == "daemon" {
        daemon_case(&root, raw.as_deref()).await;
    } else if case.starts_with("cache_") {
        cache_case(&root, case == "cache_readonly");
    } else {
        let readonly = case == "readonly";
        let db = root.join("before-open").join("events.db");
        let main_db = root.join("main.db");
        if readonly {
            std::fs::create_dir_all(db.parent().expect("fixture parent")).unwrap();
            seed_snapshot(&db);
            seed_snapshot(&main_db);
        }
        let main_result = if readonly {
            KhiveRuntime::new_readonly_for_test(main_config(&main_db))
        } else {
            KhiveRuntime::new_for_test(main_config(&main_db))
        };
        let event_result = if readonly {
            direct_backend_read_only_for(&db)
        } else {
            direct_backend_for(&db)
        };
        match main_result {
            Err(main_error) => {
                let event_error = event_result
                    .err()
                    .expect("event opener must refuse main policy");
                assert_same_refusal(&event_error, &main_error);
                if !readonly
                    && matches!(
                        event_error,
                        RuntimeError::Sqlite(SqliteError::InvalidConfig(_))
                    )
                {
                    assert!(!db.parent().expect("fixture parent").exists());
                }
            }
            Ok(main) => {
                let event = event_result.expect("event opener accepts the main policy");
                assert_policy_report(main.backend(), raw.as_deref(), readonly);
                assert_policy_report(&event, raw.as_deref(), readonly);
            }
        }
    }
    let timeout_sink_expected = case == "readonly"
        || case.starts_with("cache_")
        || matches!(raw.as_deref(), None | Some("0"));
    assert_private_timeout_sink(&root, timeout_sink_expected).await;
    assert!(!root.join("home").exists(), "fixtures must not touch HOME");
    println!("EVENT_WAL_CASE_EXECUTED");
}
