#![cfg(unix)]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use khive_db::{ConnectionPool, SqliteError, StorageBackend, WalCeilingPolicy, WalCeilingSource};
use khive_runtime::events_split::{EventsSplitConfig, TestRegistryGuard};
use khive_runtime::{BackendId, KhiveRuntime, Namespace, RuntimeConfig, RuntimeError};
use khive_storage::{EventFilter, SqlStatement, SqlValue};

const CHILD_CASE: &str = "KHIVE_EVENTS_MAIN_POLICY_CASE";
const CHILD_ROOT: &str = "KHIVE_EVENTS_MAIN_POLICY_ROOT";
const WAL_ENV: &str = "KHIVE_SQLITE_WAL_CEILING_BYTES";
const TIMEOUT_SINK_ENV: &str = "KHIVE_WRITER_TIMEOUT_SINK_DIR";

fn run_case(case: &str, raw: &str) {
    let root = tempfile::Builder::new()
        .prefix("events-main-policy-")
        .tempdir_in("/tmp")
        .expect("isolated events policy fixture");
    let output = Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "events_main_policy_child",
            "--ignored",
            "--nocapture",
        ])
        .env_clear()
        .env("HOME", root.path().join("home"))
        .env("KHIVE_VOLUME_LOCK_DIR", root.path().join("volume-locks"))
        .env(TIMEOUT_SINK_ENV, root.path().join("writer-timeouts"))
        .env(CHILD_CASE, case)
        .env(CHILD_ROOT, root.path())
        .env(WAL_ENV, raw)
        .output()
        .expect("spawn isolated policy fixture");
    assert!(
        output.status.success(),
        "EVENT_MAIN_POLICY: {case}\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("EVENT_MAIN_POLICY_EXECUTED"),
        "a successful process must execute the requested child case"
    );
    assert!(
        !root.path().join("home").exists(),
        "the child must leave its private HOME absent through process exit"
    );
}

#[test]
fn secondary_event_store_inherits_main_policy_instead_of_pack_policy() {
    run_case("secondary_events", "24576");
    run_case("secondary_writable_refusal", "0");
}

#[test]
fn secondary_sidecar_sql_inherits_actual_main_policy_over_stale_config() {
    run_case("secondary_sql", "24576");
    run_case("secondary_sql_zero", "24576");
}

#[test]
fn events_keep_the_main_construction_environment_snapshot() {
    run_case("construction_snapshot", "8192");
}

fn seed_snapshot(path: &Path) {
    let seed = StorageBackend::sqlite_for_test(path).expect("disabled snapshot seed opens");
    seed.prepare_core_schema().expect("current snapshot schema");
}

fn open_backend(path: &Path, policy: WalCeilingPolicy, readonly: bool) -> Arc<StorageBackend> {
    if readonly {
        seed_snapshot(path);
    }
    let backend = if readonly {
        StorageBackend::sqlite_read_only_with_max_readers_and_wal_ceiling(path, Some(2), policy)
            .expect("read-only explicit policy opens")
    } else {
        let backend =
            StorageBackend::sqlite_with_max_readers_and_wal_ceiling(path, Some(2), policy)
                .expect("disabled writable policy opens");
        backend
            .prepare_core_schema()
            .expect("current writable schema");
        backend
    };
    Arc::new(backend)
}

fn assert_policy(pool: &ConnectionPool, policy: WalCeilingPolicy, readonly: bool) {
    assert_eq!(pool.config().wal_ceiling, policy);
    assert_eq!(pool.config().read_only, readonly);
    let report = khive_db::diagnostics::collect(
        pool,
        khive_db::diagnostics::BuildIdentity::from_env("test", None),
        Duration::from_secs(30),
    );
    assert_eq!(report.wal_ceiling.configured_bytes, policy.bytes);
    assert_eq!(report.wal_ceiling.source, policy.source);
    assert_eq!(report.wal_ceiling.effective_bytes, 0);
    assert!(!report.wal_ceiling.enabled);
    assert_eq!(
        report.wal_ceiling.status,
        if readonly && policy.bytes != 0 {
            "read_only_not_enforced"
        } else {
            "disabled"
        }
    );
}

fn assert_events_pool(
    runtime: &KhiveRuntime,
    path: &Path,
    policy: WalCeilingPolicy,
    readonly: bool,
) {
    let canonical = path.canonicalize().expect("actual events file");
    let opened = runtime.diagnostic_backends();
    let lanes: Vec<_> = opened
        .iter()
        .filter(|entry| entry.canonical_path.as_deref() == Some(canonical.as_path()))
        .collect();
    assert_eq!(lanes.len(), 1, "one actual late-opened events pool");
    assert_eq!(lanes[0].backend_names, vec!["events".to_owned()]);
    assert_policy(&lanes[0].pool, policy, readonly);
}

async fn read_events(
    runtime: &KhiveRuntime,
    path: &Path,
    policy: WalCeilingPolicy,
    readonly: bool,
) {
    let token = runtime.authorize(Namespace::local()).expect("local token");
    let events = runtime.events(&token).expect("actual events capability");
    assert_eq!(
        events.count_events(EventFilter::default()).await.unwrap(),
        0
    );
    assert_events_pool(runtime, path, policy, readonly);
}

async fn read_sidecar(runtime: &KhiveRuntime, path: &Path, policy: WalCeilingPolicy) {
    let sql = runtime
        .events_sidecar_sql_read_only()
        .expect("actual sidecar SQL accessor")
        .expect("existing sidecar returns a capability");
    let canonical = path.canonicalize().expect("existing canonical events file");
    assert_eq!(sql.database_path().as_deref(), Some(canonical.as_path()));
    let mut reader = sql.reader().await.expect("actual sidecar SQL reader");
    let count = reader
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM events".to_owned(),
            params: vec![],
            label: Some("events-main-policy-fixture".to_owned()),
        })
        .await
        .expect("actual event table read");
    assert!(matches!(count, Some(SqlValue::Integer(0))));
    assert_events_pool(runtime, path, policy, true);
}

async fn secondary_case(root: &Path, case: &str) {
    let writable = case == "secondary_writable_refusal";
    let main_policy = WalCeilingPolicy {
        bytes: if case == "secondary_sql_zero" {
            0
        } else {
            8192
        },
        source: WalCeilingSource::BackendField,
    };
    let pack_policy = WalCeilingPolicy {
        bytes: if writable { 0 } else { 16384 },
        source: WalCeilingSource::Environment,
    };
    let main = open_backend(&root.join("main.db"), main_policy, true);
    let pack_path = root.join("pack.db");
    let pack = open_backend(&pack_path, pack_policy, !writable);
    let events_path = root.join("events.db");
    seed_snapshot(&events_path);
    let config = RuntimeConfig {
        backend_id: BackendId::parse("messages").expect("secondary backend id"),
        db_path: Some(pack_path),
        wal_ceiling_bytes: 32768,
        wal_ceiling_configured_bytes: 32768,
        wal_ceiling_source: WalCeilingSource::BackendField,
        events_split: Some(EventsSplitConfig {
            db_path: events_path.clone(),
            socket_path: None,
        }),
        ..RuntimeConfig::no_embeddings()
    };
    assert_eq!(config.wal_ceiling_policy().bytes, 32768);
    assert_ne!(config.wal_ceiling_policy(), main_policy);
    assert_ne!(pack_policy, main_policy);
    assert_eq!(
        std::env::var(WAL_ENV).unwrap(),
        if writable { "0" } else { "24576" }
    );
    let runtime =
        KhiveRuntime::from_backend(Arc::clone(&pack), config).with_core_backend(Arc::clone(&main));
    assert_eq!(runtime.diagnostic_backends().len(), 1);
    assert_policy(runtime.backend().pool(), pack_policy, !writable);
    assert_policy(main.pool(), main_policy, true);

    if writable {
        let expected = StorageBackend::sqlite_with_max_readers_and_wal_ceiling(
            root.join("capacity-probe.db"),
            Some(2),
            main_policy,
        )
        .err()
        .expect("nonzero writable lower policy must refuse without the I/O limiter");
        let (bytes, capability) = match expected {
            SqliteError::WalCapacityUnavailable { bytes, capability } => (bytes, capability),
            other => panic!("fixture requires actual nonzero writable capacity refusal: {other:?}"),
        };
        assert_eq!(bytes, 8192);
        assert_eq!(capability, "WAL I/O limiter");
        let token = runtime.authorize(Namespace::local()).unwrap();
        let actual = runtime
            .events(&token)
            .err()
            .expect("events must apply MAIN's nonzero policy to the writable lane");
        assert!(matches!(
            actual,
            RuntimeError::Sqlite(SqliteError::WalCapacityUnavailable {
                bytes: actual_bytes,
                capability: actual_capability,
            }) if actual_bytes == bytes && actual_capability == capability
        ));
        assert_eq!(runtime.diagnostic_backends().len(), 1);
    } else if case.starts_with("secondary_sql") {
        read_sidecar(&runtime, &events_path, main_policy).await;
    } else {
        read_events(&runtime, &events_path, main_policy, true).await;
    }
    assert_policy(runtime.backend().pool(), pack_policy, !writable);
    assert_policy(main.pool(), main_policy, true);
}

async fn snapshot_case(root: &Path) {
    let main_path = root.join("main.db");
    let events_path = root.join("events.db");
    seed_snapshot(&main_path);
    seed_snapshot(&events_path);
    let config = RuntimeConfig {
        db_path: Some(main_path),
        events_split: Some(EventsSplitConfig {
            db_path: events_path.clone(),
            socket_path: None,
        }),
        ..RuntimeConfig::no_embeddings()
    };
    assert_eq!(config.wal_ceiling_env_raw.as_deref(), Some("8192"));
    // Environment mutations are confined to this single-test child process.
    std::env::set_var(WAL_ENV, "16384");
    let runtime = KhiveRuntime::new_readonly_for_test(config)
        .expect("main opens using its captured construction environment");
    let policy = WalCeilingPolicy {
        bytes: 8192,
        source: WalCeilingSource::Environment,
    };
    assert_policy(runtime.backend().pool(), policy, true);
    std::env::set_var(WAL_ENV, "24576");
    assert_eq!(std::env::var(WAL_ENV).unwrap(), "24576");
    read_events(&runtime, &events_path, policy, true).await;
    assert_policy(runtime.backend().pool(), policy, true);
}

#[tokio::test]
#[ignore = "executed only by a parent fixture in a fresh process"]
async fn events_main_policy_child() {
    let root = std::path::PathBuf::from(std::env::var_os(CHILD_ROOT).expect("private child root"));
    let case = std::env::var(CHILD_CASE).expect("child case");
    let _registry_guard = TestRegistryGuard::new(&root);
    assert!(!root.join("home").exists());
    assert_eq!(
        std::path::PathBuf::from(std::env::var_os(TIMEOUT_SINK_ENV).unwrap()),
        root.join("writer-timeouts")
    );
    match case.as_str() {
        "secondary_events"
        | "secondary_writable_refusal"
        | "secondary_sql"
        | "secondary_sql_zero" => {
            secondary_case(&root, &case).await;
        }
        "construction_snapshot" => snapshot_case(&root).await,
        _ => panic!("unknown isolated case: {case}"),
    }
    assert!(!root.join("home").exists());
    println!("EVENT_MAIN_POLICY_EXECUTED");
}
