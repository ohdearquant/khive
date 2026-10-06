use std::process::Command;

use khive_db::{SqliteError, StorageBackend};
use khive_runtime::{KhiveRuntime, RuntimeConfig, RuntimeError, WalCeilingSource};

const CHILD_CASE: &str = "KHIVE_WAL_CONSTRUCTOR_TEST_CASE";
const CHILD_ROOT: &str = "KHIVE_WAL_CONSTRUCTOR_TEST_ROOT";

fn run_case(constructor: &str, raw: Option<&str>) {
    let root = tempfile::tempdir().expect("isolated constructor fixture");
    let mut child = Command::new(std::env::current_exe().expect("test executable"));
    child
        .args([
            "--exact",
            "wal_constructor_child",
            "--ignored",
            "--nocapture",
        ])
        .env_clear()
        .env("HOME", root.path().join("home"))
        .env("KHIVE_VOLUME_LOCK_DIR", root.path().join("volume-locks"))
        .env(CHILD_CASE, constructor)
        .env(CHILD_ROOT, root.path());
    if let Some(raw) = raw {
        child.env("KHIVE_SQLITE_WAL_CEILING_BYTES", raw);
    }
    let output = child.output().expect("spawn isolated constructor fixture");
    assert!(
        output.status.success(),
        "CONSTRUCTOR_POLICY: {constructor}, {raw:?}\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("WAL_CONSTRUCTOR_CASE_EXECUTED"),
        "the child must execute the requested fixture"
    );
}

#[test]
fn file_runtime_constructors_use_captured_wal_environment() {
    for constructor in ["new", "new_for_test", "readonly", "readonly_for_test"] {
        for raw in [None, Some("0"), Some("abc"), Some("8192")] {
            run_case(constructor, raw);
        }
    }
}

#[test]
fn runtime_metadata_exemption_does_not_disable_true_memory_policy() {
    for constructor in ["memory", "metadata"] {
        for raw in [None, Some("0"), Some("abc"), Some("8192")] {
            run_case(constructor, raw);
        }
    }
}

#[test]
fn runtime_explicit_zero_policy_overrides_invalid_environment() {
    run_case("field_zero", Some("abc"));
}

#[test]
#[ignore = "executed only in a fresh process by the parent fixtures"]
fn wal_constructor_child() {
    let constructor = std::env::var(CHILD_CASE).expect("isolated fixture case");
    let root = std::path::PathBuf::from(std::env::var_os(CHILD_ROOT).expect("fixture root"));
    let raw = std::env::var("KHIVE_SQLITE_WAL_CEILING_BYTES").ok();
    let readonly = constructor.starts_with("readonly");
    let path = root.join("before-open").join("fixture.db");
    if readonly {
        std::fs::create_dir_all(path.parent().expect("fixture parent")).expect("seed parent");
        let seed = StorageBackend::sqlite_for_test(&path).expect("explicit disabled seed backend");
        seed.prepare_core_schema().expect("seed current schema");
        drop(seed);
    }
    let mut config = RuntimeConfig {
        db_path: Some(path.clone()),
        ..RuntimeConfig::no_embeddings()
    };
    if constructor == "memory" {
        config.db_path = None;
    }
    if constructor == "metadata" {
        config = config.for_metadata_registry();
    }
    if constructor == "field_zero" {
        config.wal_ceiling_source = WalCeilingSource::BackendField;
    }

    // This process owns its environment; opening must use the captured value.
    std::env::remove_var("KHIVE_SQLITE_WAL_CEILING_BYTES");
    let result = match constructor.as_str() {
        "new_for_test" => KhiveRuntime::new_for_test(config),
        "readonly" => KhiveRuntime::new_readonly(config),
        "readonly_for_test" => KhiveRuntime::new_readonly_for_test(config),
        _ => KhiveRuntime::new(config),
    };
    if constructor == "metadata" || constructor == "field_zero" {
        let runtime = result.expect("explicit zero policy permits this constructor");
        assert_eq!(runtime.config().wal_ceiling_bytes, 0);
        assert_eq!(
            runtime.config().wal_ceiling_source,
            WalCeilingSource::BackendField
        );
        if constructor == "metadata" {
            assert!(!runtime.backend().is_file_backed());
            assert!(!path.exists());
        }
    } else if constructor == "memory" {
        let runtime = result.expect("environment ceiling never applies to memory");
        assert!(!runtime.backend().is_file_backed());
        assert_eq!(runtime.config().wal_ceiling_bytes, 0);
        assert_eq!(
            runtime.config().wal_ceiling_source,
            WalCeilingSource::Default
        );
        assert!(!path.exists());
    } else if raw.as_deref() == Some("abc") {
        assert!(
            matches!(result, Err(RuntimeError::Sqlite(SqliteError::InvalidConfig(ref message)))
                if message.contains("KHIVE_SQLITE_WAL_CEILING_BYTES")),
            "INVALID_CAPTURED_WAL_ENV: malformed captured environment must be a typed refusal"
        );
        if !readonly {
            assert!(!path.parent().expect("fixture parent").exists());
        }
    } else if raw.as_deref() == Some("8192") && !readonly {
        assert!(
            matches!(
                result,
                Err(RuntimeError::Sqlite(SqliteError::WalCapacityUnavailable {
                    bytes: 8192,
                    ..
                }))
            ),
            "FILE_WAL_CAPACITY_REFUSAL: an enabled file policy must reach the real pool refusal"
        );
    } else {
        let runtime = result.expect("disabled or read-only policy opens");
        let configured = if raw.as_deref() == Some("8192") {
            8192
        } else {
            0
        };
        let expected_source = if raw.is_some() {
            WalCeilingSource::Environment
        } else {
            WalCeilingSource::Default
        };
        assert_eq!(runtime.config().wal_ceiling_configured_bytes, configured);
        assert_eq!(runtime.config().wal_ceiling_bytes, 0);
        assert_eq!(runtime.config().wal_ceiling_source, expected_source);
        assert_eq!(
            runtime.backend().pool_arc().config().wal_ceiling.bytes,
            configured
        );
        assert_eq!(
            runtime.backend().pool_arc().config().wal_ceiling.source,
            expected_source
        );
    }
    println!("WAL_CONSTRUCTOR_CASE_EXECUTED");
}
