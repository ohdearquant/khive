use std::process::Command;

use khive_db::SqliteError;
use khive_runtime::{KhiveConfig, RuntimeConfig};

fn cli(home: &std::path::Path, raw: Option<&str>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kkernel"));
    command.env_clear().env("HOME", home).current_dir(home);
    if let Some(raw) = raw {
        command.env("KHIVE_SQLITE_WAL_CEILING_BYTES", raw);
    }
    command
}

#[test]
fn backend_list_validates_wal_environment_at_actual_file_open() {
    for raw in [None, Some("0"), Some("abc"), Some("8192")] {
        let home = tempfile::tempdir().expect("private CLI HOME");
        let output = cli(home.path(), raw)
            .args(["backend", "list"])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        match raw {
            Some("abc") => {
                assert!(!output.status.success(), "BACKEND_LIST_INVALID_ENV");
                assert!(
                    stderr.contains("KHIVE_SQLITE_WAL_CEILING_BYTES"),
                    "{stderr}"
                );
                assert!(!home.path().join(".khive").exists());
            }
            Some("8192") => {
                assert!(!output.status.success(), "BACKEND_LIST_CAPACITY_REFUSAL");
                assert!(
                    stderr.contains("sqlite_wal_capacity_unavailable"),
                    "{stderr}"
                );
            }
            _ => {
                assert!(output.status.success(), "disabled policy: {stderr}");
                let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(value["backends"], serde_json::json!(["main"]));
                assert!(home.path().join(".khive/khive.db").is_file());
            }
        }
    }
}

#[test]
fn metadata_cli_registries_explicitly_opt_out_of_writer_ceiling() {
    for raw in ["8192", "abc"] {
        let home = tempfile::tempdir().expect("private metadata HOME");
        let repo = home.path().join("repo");
        let kg = repo.join(".khive/kg");
        std::fs::create_dir_all(&kg).unwrap();
        for name in ["entities.ndjson", "edges.ndjson", "notes.ndjson"] {
            std::fs::write(kg.join(name), "").unwrap();
        }
        std::fs::write(
            kg.join("rules.toml"),
            "[edge_endpoint_types]\nenabled = true\n",
        )
        .unwrap();
        let output = cli(home.path(), Some(raw))
            .args([
                "kg",
                "validate",
                "--repo",
                repo.to_str().unwrap(),
                "--format",
                "json",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "KG_METADATA_CEILING_EXEMPTION: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(report["rules"]
            .as_array()
            .unwrap()
            .iter()
            .any(|rule| rule["id"] == "edge-endpoint-types" && rule["passed"] == true));
        let output = cli(home.path(), Some(raw))
            .args(["pack", "list"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "PACK_METADATA_CEILING_EXEMPTION: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let packs: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(packs.to_string().contains("kg"));
        assert!(!home.path().join(".khive/khive.db").exists());
    }
}

#[test]
fn mcp_single_backend_host_uses_captured_wal_policy() {
    for raw in ["unset", "0", "abc", "8192"] {
        let home = tempfile::tempdir().unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "mcp_single_backend_wal_child",
                "--ignored",
                "--nocapture",
            ])
            .env_clear()
            .env("HOME", home.path())
            .env("KHIVE_WAL_HOST_CASE", raw);
        if raw != "unset" {
            command.env("KHIVE_SQLITE_WAL_CEILING_BYTES", raw);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "SINGLE_HOST_CAPTURED_POLICY: {raw}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("WAL_HOST_CASE_EXECUTED"));
    }
}

#[test]
#[ignore = "only the isolated parent fixture executes this case"]
fn mcp_single_backend_wal_child() {
    let raw = std::env::var("KHIVE_WAL_HOST_CASE").unwrap();
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
    let config = RuntimeConfig {
        db_path: Some(home.join("before-open/database.db")),
        ..RuntimeConfig::no_embeddings()
    };
    std::env::remove_var("KHIVE_SQLITE_WAL_CEILING_BYTES");
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = executor.block_on(khive_mcp::serve::build_single_backend_runtime(
        config,
        &KhiveConfig::default(),
    ));
    match raw.as_str() {
        "abc" => {
            let error = result.err().expect("SINGLE_HOST_INVALID_CAPTURED_ENV");
            assert!(
                matches!(error.downcast_ref::<khive_runtime::RuntimeError>(), Some(khive_runtime::RuntimeError::Sqlite(SqliteError::InvalidConfig(message))) if message.contains("KHIVE_SQLITE_WAL_CEILING_BYTES"))
            );
            assert!(!home.join("before-open").exists());
        }
        "8192" => {
            let error = result.err().expect("SINGLE_HOST_CAPACITY_REFUSAL");
            assert!(matches!(
                error.downcast_ref::<SqliteError>(),
                Some(SqliteError::WalCapacityUnavailable { bytes: 8192, .. })
            ));
        }
        _ => {
            let runtime = result.expect("disabled host policy opens");
            assert_eq!(runtime.config().wal_ceiling_bytes, 0);
            assert!(runtime.backend().is_file_backed());
        }
    }
    println!("WAL_HOST_CASE_EXECUTED");
}

fn run_topology_case(case: &str) {
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "wal_topology_child", "--ignored", "--nocapture"])
        .env_clear()
        .env("HOME", home.path())
        .env("KHIVE_WAL_TOPOLOGY_CASE", case)
        .env(
            "KHIVE_SQLITE_WAL_CEILING_BYTES",
            if case == "force-field" || case == "declared-memory" {
                "0"
            } else {
                "8192"
            },
        )
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "WAL_TOPOLOGY_POLICY: {case}: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("WAL_TOPOLOGY_CASE_EXECUTED"));
}

#[test]
fn forced_memory_override_clears_inherited_backend_field() {
    run_topology_case("force-field");
}
#[test]
fn forced_memory_override_ignores_environment_ceiling() {
    run_topology_case("force-env");
}
#[test]
fn declared_memory_own_nonzero_field_is_typed_refusal() {
    run_topology_case("declared-memory");
}
#[test]
fn file_topology_keeps_environment_ceiling_enforcement() {
    run_topology_case("file-env");
}

#[test]
#[ignore = "only the isolated parent fixtures execute this case"]
fn wal_topology_child() {
    use khive_mcp::serve::{
        build_registry_for_multi_backend, resolve_runtime_config, RuntimeConfigInputs,
    };
    use khive_runtime::{ConfigError, Namespace, WalCeilingSource};
    let case = std::env::var("KHIVE_WAL_TOPOLOGY_CASE").unwrap();
    let root = tempfile::tempdir().unwrap();
    let main = root.path().join("not-created/main.db");
    let secondary = if case == "force-field" {
        main.clone()
    } else {
        root.path().join("not-created/secondary.db")
    };
    let config_path = root.path().join("config.toml");
    let mut document = format!(
        "[runtime]\npacks = ['kg']\n[[backends]]\nname = 'main'\nkind = '{}'\npath = {:?}\n",
        if case == "declared-memory" {
            "memory"
        } else {
            "sqlite"
        },
        main.to_str().unwrap()
    );
    if case == "force-field" || case == "declared-memory" {
        document.push_str("wal_ceiling_bytes = 8192\n");
    }
    document.push_str(&format!(
        "[[backends]]\nname = 'secondary'\nkind = 'sqlite'\npath = {:?}\n",
        secondary.to_str().unwrap()
    ));
    if case == "force-field" {
        document.push_str("wal_ceiling_bytes = 16384\n");
    }
    document.push_str("[packs.kg]\nbackend = 'main'\nno_embed = true\n");
    std::fs::write(&config_path, document).unwrap();
    let topology = KhiveConfig::load(Some(&config_path));
    if case == "declared-memory" {
        let error = topology.expect_err("DECLARED_MEMORY_OWN_FIELD_REFUSAL");
        let underlying = match &error {
            ConfigError::InFile { source, .. } => source.as_ref(),
            error => error,
        };
        assert!(matches!(
            underlying,
            ConfigError::WalCeilingMemoryBackend { value: 8192, .. }
        ));
    } else {
        let topology = topology.unwrap().unwrap();
        let db = if case.starts_with("force-") {
            Some(":memory:")
        } else {
            None
        };
        let config = resolve_runtime_config(RuntimeConfigInputs {
            db,
            config: Some(&config_path),
            namespace: Namespace::local(),
            namespace_explicit: false,
            actor_explicit: false,
            no_embed: true,
            packs: Some(vec!["kg".into()]),
            brain_profile: None,
        })
        .expect("FORCED_MEMORY_RESOLUTION: resolve selected topology");
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = executor.block_on(build_registry_for_multi_backend(config, &topology, db));
        if case.starts_with("force-") {
            let multi =
                result.unwrap_or_else(|error| panic!("FORCED_MEMORY_CEILING_DISABLED: {error:#}"));
            assert!(!multi.main_backend.is_file_backed());
            assert!(!multi.per_pack_runtimes["kg"].backend().is_file_backed());
            assert_eq!(multi.main_backend.pool_arc().config().wal_ceiling.bytes, 0);
            assert_eq!(
                multi.main_backend.pool_arc().config().wal_ceiling.source,
                WalCeilingSource::Default
            );
            assert!(!main.exists());
            assert!(!secondary.exists());
        } else {
            let error = result.err().expect("FILE_TOPOLOGY_CAPACITY_REFUSAL");
            assert!(matches!(
                error.downcast_ref::<SqliteError>(),
                Some(SqliteError::WalCapacityUnavailable { bytes: 8192, .. })
            ));
        }
    }
    println!("WAL_TOPOLOGY_CASE_EXECUTED");
}
