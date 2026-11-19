#![cfg(unix)]

use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
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

struct CallerFixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    config: PathBuf,
    main: PathBuf,
    socket: PathBuf,
    declared: bool,
}

impl CallerFixture {
    fn new(socket_bytes: usize, backend_count: usize) -> Self {
        let directory = tempfile::Builder::new()
            .prefix("evc-")
            .tempdir_in("/tmp")
            .unwrap();
        let root = directory.path().canonicalize().unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let leaf = "main.db.events.sock";
        let padding = socket_bytes
            .checked_sub(root.as_os_str().as_bytes().len() + 2 + leaf.len())
            .unwrap();
        let parent = root.join("c".repeat(padding));
        std::fs::create_dir(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
        let main = parent.join("main.db");
        let socket = events_socket_path_beside(&events_db_path_beside(&main));
        assert_eq!(socket.as_os_str().as_bytes().len(), socket_bytes);
        let config = root.join("khive.toml");
        // Both exec and daemon boot resolve this engine list into their identity.
        let mut body =
            "engines = []\n[runtime]\npacks = [\"kg\"]\n[packs.kg]\nno_embed = true\n".to_owned();
        if backend_count > 0 {
            body.push_str(&format!(
                "\n[[backends]]\nname = \"main\"\nkind = \"sqlite\"\npath = {main:?}\n"
            ));
        }
        if backend_count > 1 {
            body.push_str(&format!(
                "\n[[backends]]\nname = \"secondary\"\nkind = \"sqlite\"\npath = {:?}\n",
                root.join("secondary.db")
            ));
        }
        std::fs::write(&config, body).unwrap();
        Self {
            _directory: directory,
            root,
            config,
            main,
            socket,
            declared: backend_count > 0,
        }
    }

    fn command(&self, action: &[&str], log: &str) -> Command {
        let mut cmd = command(&self.root, &self.root.join(log));
        cmd.args(action)
            .arg("--config")
            .arg(&self.config)
            .args(["--actor", "actor:events-preflight"])
            .env("KHIVE_PACKS", "kg")
            .env("KHIVE_RECOVERER_LOCK", self.root.join("recoverer.lock"))
            .env("KHIVE_SUPERVISOR_MARKER", self.root.join("supervisor"));
        if !self.declared {
            cmd.arg("--db").arg(&self.main);
        }
        cmd
    }

    fn assert_no_daemon(&self) {
        for name in ["host.sock", "host.pid", "boot.lock", "supervisor"] {
            assert!(!self.root.join(name).exists(), "unexpected {name}");
        }
        assert!(!self.socket.exists());
        assert!(!self.root.join("home/.khive/logs/khived.log").exists());
    }
}

#[tokio::test(start_paused = true)]
async fn public_forwarders_refuse_socket_length_without_recovery_time() {
    let mut private = None;
    if khive_storage::test_support::run_exact_test_in_child(
        "EVENTS_SOCKET_CALLER_TEST",
        false,
        |command| {
            let directory = tempfile::Builder::new()
                .prefix("evc-env-")
                .tempdir_in("/tmp")
                .unwrap();
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
            for (key, _) in std::env::vars_os() {
                if key.to_string_lossy().starts_with("KHIVE_") {
                    command.env_remove(key);
                }
            }
            command
                .env_remove("LATTICE_MODEL_CACHE")
                .env("HOME", directory.path().join("home"))
                .env("KHIVE_TEST_HARNESS", "1")
                .env("KHIVE_NO_EMBED", "true")
                .current_dir(directory.path());
            private = Some(directory);
        },
    ) {
        assert!(files_below(private.unwrap().path()).is_empty());
        return;
    }
    let fixture = CallerFixture::new(capacity(), 2);
    std::env::set_var("KHIVE_CONFIG", &fixture.config);
    std::env::set_var("KHIVE_SOCKET", fixture.root.join("host.sock"));
    std::env::set_var("KHIVE_PID", fixture.root.join("host.pid"));
    std::env::set_var("KHIVE_LOCK", fixture.root.join("boot.lock"));
    let before = files_below(&fixture.root);
    let frame = khive_runtime::daemon::DaemonRequestFrame {
        ops: "stats()".into(),
        namespace: "local".into(),
        config_id: "preflight-must-precede-transport".into(),
        protocol_version: khive_runtime::daemon::PROTOCOL_VERSION,
        ..Default::default()
    };
    for strict in ["0", "1"] {
        std::env::set_var("KHIVE_DAEMON_STRICT", strict);
        for bare in [false, true] {
            let started = tokio::time::Instant::now();
            let result = if bare {
                khive_mcp::daemon::forward_or_spawn(&frame).await
            } else {
                khive_mcp::daemon::forward_or_spawn_with_config_and_packs(
                    &frame,
                    Some(&fixture.config),
                    None,
                    Some(&["kg".into()]),
                )
                .await
            };
            let error = result
                .expect("configuration refusal is not fallback")
                .unwrap_err();
            assert_refusal(&error.message, &fixture.socket);
            assert_eq!(tokio::time::Instant::now(), started, "no recovery timer");
            assert_eq!(files_below(&fixture.root), before);
        }
    }
    tokio::time::resume();
    std::fs::write(&fixture.config, "invalid [configuration").unwrap();
    for bare in [false, true] {
        use khive_runtime::daemon::{
            read_frame, write_frame, DaemonRequestFrame, DaemonResponseFrame,
        };

        let socket_path = fixture.root.join("host.sock");
        let listener = tokio::net::UnixListener::bind(&socket_path).unwrap();
        let peer = async {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request: DaemonRequestFrame =
                serde_json::from_slice(&read_frame(&mut stream).await.unwrap()).unwrap();
            let response = DaemonResponseFrame {
                ok: true,
                result: Some("live-daemon-response".into()),
                error: None,
                error_detail: None,
                namespace_mismatch: false,
                config_mismatch: false,
                served_config_id: Some(request.config_id),
                version_mismatch: false,
                daemon_protocol_version: khive_runtime::daemon::PROTOCOL_VERSION,
                metrics: None,
                request_id: request.request_id,
            };
            write_frame(&mut stream, &serde_json::to_vec(&response).unwrap())
                .await
                .unwrap();
        };
        let forward = async {
            if bare {
                khive_mcp::daemon::forward_or_spawn(&frame).await
            } else {
                khive_mcp::daemon::forward_or_spawn_with_config(&frame, Some(&fixture.config), None)
                    .await
            }
        };
        let (response, peer) = tokio::join!(
            tokio::time::timeout(Duration::from_secs(5), forward),
            tokio::time::timeout(Duration::from_secs(5), peer),
        );
        assert_eq!(
            response.expect("bounded live forward").unwrap().unwrap(),
            "live-daemon-response",
            "a responding daemon must not depend on reloading its launch config"
        );
        peer.expect("live peer served the request");
        drop(listener);
        std::fs::remove_file(socket_path).unwrap();
    }
    std::env::set_var("KHIVE_NO_DAEMON", "1");
    assert!(khive_mcp::daemon::forward_or_spawn(&frame).await.is_none());
    assert!(
        khive_mcp::daemon::forward_or_spawn_with_config(&frame, Some(&fixture.config), None)
            .await
            .is_none()
    );
    assert_eq!(files_below(&fixture.root), before);
}

#[test]
fn exec_and_supervisor_preserve_socket_length_refusal() {
    for backends in [0, 1, 2] {
        for supervised in [false, true] {
            let fixture = CallerFixture::new(capacity(), backends);
            let action: &[&str] = if supervised {
                &[
                    "supervisor",
                    "launch",
                    "--label",
                    "test:events-preflight",
                    "--restart-interval-secs",
                    "1",
                    "--",
                ]
            } else {
                &["exec", "stats()"]
            };
            let mut child = ProcessGuard(Some(fixture.command(action, "stderr").spawn().unwrap()));
            let status = child.wait_for_exit();
            let error = std::fs::read_to_string(fixture.root.join("stderr")).unwrap();
            // Assert the diagnostic first: an omitted preflight must fail for
            // the lost refusal, not merely for a changed exit status.
            assert_refusal(&error, &fixture.socket);
            if supervised {
                assert!(
                    status.success(),
                    "CONFIG exit stops supervisor restart: {error}"
                );
                assert!(error.contains("CONFIG: supervisor"), "{error}");
            } else {
                assert!(!status.success());
            }
            fixture.assert_no_daemon();
            assert!(!fixture.main.exists());
            assert!(!fixture.root.join("secondary.db").exists());
        }
    }
}

#[tokio::test]
async fn mcp_request_preserves_socket_length_refusal_in_both_fallback_modes() {
    use rmcp::model::CallToolRequestParams;
    use rmcp::transport::TokioChildProcess;
    use rmcp::ServiceExt;

    for strict in ["0", "1"] {
        let fixture = CallerFixture::new(capacity(), 1);
        let mut command = tokio::process::Command::from(fixture.command(&["mcp"], "stderr"));
        command
            .env("KHIVE_DAEMON_STRICT", strict)
            .kill_on_drop(true);
        let (transport, _) = TokioChildProcess::builder(command)
            .stderr(std::fs::File::create(fixture.root.join("stderr")).unwrap())
            .spawn()
            .unwrap();
        let client = tokio::time::timeout(Duration::from_secs(30), ().serve(transport))
            .await
            .expect("bounded MCP startup")
            .expect("direct-mode bridge starts before the daemon request");
        let response = tokio::time::timeout(
            Duration::from_secs(20),
            client.call_tool(
                CallToolRequestParams::new("request").with_arguments(
                    serde_json::json!({"ops": "stats()"})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            ),
        )
        .await;
        let shutdown = tokio::time::timeout(Duration::from_secs(5), client.cancel()).await;
        let error = response.expect("bounded caller refusal").unwrap_err();
        assert_refusal(&error.to_string(), &fixture.socket);
        shutdown
            .expect("bounded MCP shutdown")
            .expect("MCP shutdown");
        assert!(!fixture.root.join("host.pid").exists());
        assert!(!fixture.root.join("host.sock").exists());
        assert!(!fixture.socket.exists());
        assert!(!fixture.root.join("home/.khive/logs/khived.log").exists());
    }
}

#[test]
fn maximum_valid_events_socket_path_boots_and_serves_exec() {
    for backends in [0, 2] {
        let fixture = CallerFixture::new(capacity() - 1, backends);
        let mut daemon = ProcessGuard(Some(
            fixture
                .command(&["mcp", "--daemon"], "daemon.stderr")
                .spawn()
                .unwrap(),
        ));
        let deadline = Instant::now() + Duration::from_secs(30);
        while !fixture.root.join("host.sock").exists() || !fixture.socket.exists() {
            assert!(
                daemon.0.as_mut().unwrap().try_wait().unwrap().is_none(),
                "daemon exited: {}",
                std::fs::read_to_string(fixture.root.join("daemon.stderr")).unwrap()
            );
            assert!(
                Instant::now() < deadline,
                "daemon did not bind both sockets"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let output_path = fixture.root.join("response.json");
        let mut command = fixture.command(&["exec", "stats()"], "client.stderr");
        command.stdout(std::fs::File::create(&output_path).unwrap());
        let mut client = ProcessGuard(Some(command.spawn().unwrap()));
        let status = client.wait_for_exit();
        let stderr = std::fs::read_to_string(fixture.root.join("client.stderr")).unwrap();
        assert!(status.success(), "{stderr}");
        assert!(stderr.contains("execution: answered by daemon"), "{stderr}");
        let response: serde_json::Value =
            serde_json::from_slice(&std::fs::read(output_path).unwrap()).unwrap();
        assert_eq!(response["summary"]["failed"], 0);
        assert_eq!(response["summary"]["succeeded"], 1);
        assert!(daemon.0.as_mut().unwrap().try_wait().unwrap().is_none());
    }
}
