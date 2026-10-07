#![cfg(unix)]

use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use khive_mcp::serve::preflight_events_socket_for_boot;
use khive_runtime::events_split::{
    events_db_path_beside, events_socket_path_beside, EventsSplitConfig,
};
use khive_runtime::{BackendConfig, BackendKind, RuntimeConfig};

fn capacity() -> usize {
    // SAFETY: sockaddr_un contains integer fields and a character array.
    let address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_path.len()
}

struct ProcessGuard(Option<Child>);

impl ProcessGuard {
    fn wait_for_exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = self.0.as_mut().unwrap().try_wait().unwrap() {
                self.0.take();
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "preflight must exit before serving"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            // SAFETY: this child started a new process group with its own PID.
            // Kill that private group too if a regression spawned descendants.
            unsafe {
                libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
            }
            let _ = child.wait();
        }
    }
}

fn command(root: &Path, stderr: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kkernel"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("KHIVE_") {
            command.env_remove(key);
        }
    }
    command
        .env_remove("LATTICE_MODEL_CACHE")
        .env("HOME", root.join("home"))
        .env("KHIVE_TEST_HARNESS", "1")
        .env("KHIVE_VOLUME_LOCK_DIR", root.join("volume-locks"))
        .env("KHIVE_WRITER_TIMEOUT_SINK_DIR", root.join("writer-logs"))
        .env("KHIVE_SOCKET", root.join("host.sock"))
        .env("KHIVE_PID", root.join("host.pid"))
        .env("KHIVE_LOCK", root.join("boot.lock"))
        .env("KHIVE_SQLITE_WAL_CEILING_BYTES", "0")
        .env("KHIVE_SQLITE_DISK_RESERVE_BYTES", "0")
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(stderr).unwrap())
        .process_group(0);
    // Ordinary inherited build/coverage variables, including LLVM_PROFILE_FILE,
    // are retained. Only application configuration is scrubbed for this child.
    command
}

fn files_below(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            paths.extend(files_below(&entry.path()));
        } else {
            paths.push(entry.path());
        }
    }
    paths.sort();
    paths
}

fn assert_refusal(error: &str, socket: &Path) {
    let bytes = socket.as_os_str().as_bytes().len();
    assert!(error.contains(&format!("{socket:?}")), "{error}");
    assert!(error.contains(&format!("{bytes} path bytes")), "{error}");
    assert!(
        error.contains(&format!("{} including NUL", bytes + 1)),
        "{error}"
    );
    assert!(
        error.contains(&format!("sun_path limit of {} bytes", capacity())),
        "{error}"
    );
}

#[test]
fn every_daemon_host_refuses_before_database_or_lock_files() {
    for backend_count in [0, 1, 2] {
        let dir = tempfile::Builder::new()
            .prefix("evh-")
            .tempdir_in("/tmp")
            .unwrap();
        let root = dir.path().canonicalize().unwrap();
        let socket_name = "main.db.events.sock";
        let length = capacity()
            .checked_sub(root.as_os_str().as_bytes().len() + 2 + socket_name.len())
            .unwrap();
        let config_dir = root.join("c".repeat(length));
        std::fs::create_dir(&config_dir).unwrap();
        let main = config_dir.join("main.db");
        let secondary = config_dir.join("secondary.db");
        let socket = events_socket_path_beside(&events_db_path_beside(&main));
        assert_eq!(socket.as_os_str().as_bytes().len() + 1, capacity() + 1);
        let config = config_dir.join("khive.toml");
        let mut body = "[runtime]\npacks = [\"kg\"]\n".to_owned();
        if backend_count > 0 {
            body.push_str(&format!(
                "\n[[backends]]\nname = \"main\"\nkind = \"sqlite\"\npath = {:?}\n",
                main
            ));
        }
        if backend_count > 1 {
            body.push_str(&format!(
                "\n[[backends]]\nname = \"secondary\"\nkind = \"sqlite\"\npath = {:?}\n",
                secondary
            ));
        }
        std::fs::write(&config, body).unwrap();
        let stderr = root.join("stderr");
        let mut cmd = command(&root, &stderr);
        cmd.args(["mcp", "--daemon", "--no-embed", "--pack", "kg", "--config"])
            .arg(&config);
        if backend_count == 0 {
            cmd.arg("--db").arg(&main);
        }
        let mut child = ProcessGuard(Some(cmd.spawn().unwrap()));
        assert!(!child.wait_for_exit().success());
        let error = std::fs::read_to_string(&stderr).unwrap();
        assert_refusal(&error, &socket);
        assert!(!main.exists());
        assert!(!secondary.exists());
        assert!(!root.join("boot.lock").exists());
        assert!(!socket.with_extension("lock").exists());
        let mut allowed = vec![config, stderr];
        allowed.sort();
        assert_eq!(
            files_below(&root),
            allowed,
            "no backend, store claim, volume lock, PID or log file"
        );
    }
}

#[test]
fn standalone_events_daemon_refuses_before_database_or_lock_files() {
    let dir = tempfile::Builder::new()
        .prefix("evd-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = dir.path().canonicalize().unwrap();
    let socket = root.join("s".repeat(capacity() - root.as_os_str().as_bytes().len() - 1));
    assert_eq!(socket.as_os_str().as_bytes().len() + 1, capacity() + 1);
    let db = root.join("config").join("events.db");
    let stderr = root.join("stderr");
    let mut cmd = command(&root, &stderr);
    cmd.arg("events-daemon")
        .arg("--db")
        .arg(&db)
        .arg("--socket")
        .arg(&socket);
    let mut child = ProcessGuard(Some(cmd.spawn().unwrap()));
    assert!(!child.wait_for_exit().success());
    assert_refusal(&std::fs::read_to_string(&stderr).unwrap(), &socket);
    assert!(!db.parent().unwrap().exists());
    assert!(!socket.with_extension("lock").exists());
    assert_eq!(files_below(&root), vec![stderr]);
}

fn backend(path: Option<PathBuf>) -> BackendConfig {
    BackendConfig {
        name: "main".into(),
        kind: if path.is_some() {
            BackendKind::Sqlite
        } else {
            BackendKind::Memory
        },
        path,
        cache_mb: None,
        journal_mode: None,
        wal_ceiling_bytes: None,
        disk_reserve_bytes: None,
        disk_guard_deadline_ms: None,
        served_kinds: None,
        read_only: false,
    }
}

#[test]
fn boot_preflight_uses_effective_main_and_preserves_transport_exemptions() {
    let dir = tempfile::Builder::new()
        .prefix("evr-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = dir.path().canonicalize().unwrap();
    let long_main = root.join("m".repeat(capacity())).with_extension("db");
    let long_socket = events_socket_path_beside(&events_db_path_beside(&long_main));
    let mut config = RuntimeConfig::no_embeddings();
    config.db_path = Some(long_main.clone());
    config.events_split = Some(EventsSplitConfig {
        db_path: events_db_path_beside(&long_main),
        socket_path: Some(long_socket.clone()),
    });
    // No runtime/backend is constructed in this pure configuration test.
    let short_main = root.join("short.db");
    preflight_events_socket_for_boot(&config, &[backend(Some(short_main.clone()))], false)
        .expect("declared short main replaces stale long fallback socket");
    config.events_split.as_mut().unwrap().socket_path = Some(root.join("short.sock"));
    let error =
        preflight_events_socket_for_boot(&config, &[backend(Some(long_main.clone()))], false)
            .unwrap_err();
    assert_refusal(&error.to_string(), &long_socket);
    preflight_events_socket_for_boot(&config, &[backend(Some(long_main.clone()))], true)
        .expect("force memory");
    preflight_events_socket_for_boot(&config, &[backend(None)], false)
        .expect("declared memory main");
    config.events_split.as_mut().unwrap().socket_path = None;
    preflight_events_socket_for_boot(&config, &[backend(Some(long_main))], false)
        .expect("embedded/admin direct mode");
    config.events_split = None;
    preflight_events_socket_for_boot(&config, &[], false).expect("legacy event plane");
    assert!(files_below(&root).is_empty());
}

#[tokio::test]
async fn public_builders_refuse_before_opening_any_backend() {
    let mut fixture = None;
    if khive_storage::test_support::run_exact_test_in_child(
        "EVENTS_SOCKET_BUILDER_TEST",
        false,
        |command| {
            let private = tempfile::Builder::new()
                .prefix("evb-env-")
                .tempdir_in("/tmp")
                .unwrap();
            for (key, _) in std::env::vars_os() {
                if key.to_string_lossy().starts_with("KHIVE_") {
                    command.env_remove(key);
                }
            }
            command
                .env_remove("LATTICE_MODEL_CACHE")
                .env("HOME", private.path().join("home"))
                .env("KHIVE_TEST_HARNESS", "1")
                .env(
                    "KHIVE_WRITER_TIMEOUT_SINK_DIR",
                    private.path().join("writer-logs"),
                );
            fixture = Some(private);
        },
    ) {
        assert!(std::fs::read_dir(fixture.unwrap().path())
            .unwrap()
            .next()
            .is_none());
        return;
    }
    let dir = tempfile::Builder::new()
        .prefix("evb-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = dir.path().canonicalize().unwrap();
    let main = root.join("m".repeat(capacity())).with_extension("db");
    let events_db = events_db_path_beside(&main);
    let socket = events_socket_path_beside(&events_db);
    let config = RuntimeConfig {
        db_path: Some(main.clone()),
        embedding_model: None,
        additional_embedding_models: vec![],
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: khive_runtime::WalCeilingSource::Default,
        wal_ceiling_env_raw: None,
        disk_guard_environment: khive_db::DiskGuardEnvironment::default(),
        disk_guard_config: None,
        volume_lock_dir: Some(root.join("volume-locks")),
        actor_id: None,
        brain_profile: None,
        brain: Default::default(),
        credentials: vec![],
        visibility_receipts: None,
        mounts: vec![],
        packs: vec!["kg".into()],
        events_split: Some(EventsSplitConfig {
            db_path: events_db,
            socket_path: Some(socket.clone()),
        }),
        ..RuntimeConfig::no_embeddings()
    };
    let error = khive_mcp::serve::build_single_backend_runtime(
        config.clone(),
        &khive_runtime::KhiveConfig::default(),
    )
    .await
    .err()
    .expect("single builder preflight");
    assert_refusal(&error.to_string(), &socket);
    assert!(files_below(&root).is_empty());

    let mut secondary = backend(Some(root.join("secondary.db")));
    secondary.name = "secondary".into();
    let declared = khive_runtime::KhiveConfig {
        backends: vec![secondary, backend(Some(main.clone()))],
        ..Default::default()
    };
    let error = khive_mcp::serve::build_registry_for_multi_backend_with_db_anchor(
        config,
        &declared,
        None,
        Some(&main),
    )
    .await
    .err()
    .expect("all backends remain unopened, even secondary listed first");
    assert_refusal(&error.to_string(), &socket);
    assert!(files_below(&root).is_empty());
}
