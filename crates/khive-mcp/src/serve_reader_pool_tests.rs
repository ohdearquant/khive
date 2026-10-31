use super::*;
use clap::Parser;
use khive_runtime::Namespace;
use serial_test::serial;

struct FixtureEnv(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl FixtureEnv {
    fn clear() -> Self {
        let previous = [
            "KHIVE_NO_DAEMON",
            "KHIVE_DB",
            "KHIVE_CONFIG",
            "KHIVE_PACKS",
            "KHIVE_BLOB_ROOT",
            "KHIVE_SAVE_TO_ROOT",
            "KHIVE_NAMESPACE",
            "KHIVE_EVENTS_SPLIT",
        ]
        .into_iter()
        .map(|name| {
            let value = std::env::var_os(name);
            std::env::remove_var(name);
            (name, value)
        })
        .collect();
        Self(previous)
    }
}

impl Drop for FixtureEnv {
    fn drop(&mut self) {
        for (name, value) in self.0.drain(..) {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

fn fixture(dir: &std::path::Path) -> (Args, RuntimeConfig, KhiveConfig) {
    let args = Args::parse_from(["mcp", "--no-embed", "--actor", "reader-pool-test"]);
    let mut text = String::new();
    for name in ["main", "sessions", "knowledge", "comm"] {
        let path = dir.join(format!("{name}.db"));
        text.push_str(&format!("[[backends]]\nname = {name:?}\npath = {path:?}\n"));
    }
    text.push_str(
        "[packs.session]\nbackend = 'sessions'\nno_embed = true\n\
         [packs.knowledge]\nbackend = 'knowledge'\nno_embed = true\n\
         [packs.comm]\nbackend = 'comm'\nno_embed = true\n",
    );
    let khive_cfg = toml::from_str(&text).expect("explicit store topology");
    let config = RuntimeConfig {
        db_path: Some(dir.join("main.db")),
        actor_id: Some("reader-pool-test".into()),
        default_namespace: Namespace::local(),
        packs: vec![
            "kg".into(),
            "session".into(),
            "knowledge".into(),
            "comm".into(),
        ],
        events_split: Some(khive_runtime::events_split::EventsSplitConfig {
            db_path: dir.join("main.db.events.db"),
            socket_path: None,
        }),
        ..RuntimeConfig::no_embeddings()
    };
    (args, config, khive_cfg)
}

fn assert_store_readers(multi: &MultiBackendRegistry, expected: usize) {
    for (name, runtime) in std::iter::once(("main", &multi.default_runtime)).chain(
        multi
            .per_pack_runtimes
            .iter()
            .map(|(name, runtime)| (name.as_str(), runtime.as_ref())),
    ) {
        assert_eq!(
            runtime.backend().pool().max_readers(),
            expected,
            "{name} reader count"
        );
        assert_eq!(
            runtime.backend().pool().config().max_readers,
            expected,
            "{name} construction config"
        );
    }
    let split = multi
        .default_runtime
        .config()
        .events_split
        .as_ref()
        .expect("events split");
    // Registry construction already opened the audit lane; this only observes
    // its cached pool, without supplying the reader override again.
    let events = khive_runtime::events_split::direct_backend_for(&split.db_path)
        .expect("cached events backend");
    assert_eq!(
        events.pool().max_readers(),
        expected,
        "events sidecar must inherit the selected reader count"
    );
    assert_eq!(events.pool().config().max_readers, expected);
}

#[tokio::test]
#[serial]
#[serial(config_ledger)]
async fn forwarding_runtime_uses_one_reader_on_every_store() {
    let _env = FixtureEnv::clear();
    let dir = tempfile::tempdir().unwrap();
    let (args, config, khive_cfg) = fixture(dir.path());
    let readers = mcp_max_readers(&args, &config, &khive_cfg.backends, None);
    assert_eq!(
        readers,
        Some(1),
        "forwarding default must be selected before store construction"
    );
    let multi = build_registry_for_multi_backend_inner_with_max_readers(
        config, &khive_cfg, None, readers, None,
    )
    .await
    .expect("forwarding registry");
    assert_store_readers(&multi, 1);
}

#[tokio::test]
#[serial]
#[serial(config_ledger)]
async fn direct_hosts_keep_default_readers() {
    let _env = FixtureEnv::clear();
    for daemon in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let (mut args, config, khive_cfg) = fixture(dir.path());
        args.daemon = daemon;
        if !daemon {
            std::env::set_var("KHIVE_NO_DAEMON", "1");
        }
        let readers = mcp_max_readers(&args, &config, &khive_cfg.backends, None);
        assert_eq!(
            readers, None,
            "direct hosts must retain the default reader policy"
        );
        let multi = build_registry_for_multi_backend_inner_with_max_readers(
            config, &khive_cfg, None, readers, None,
        )
        .await
        .expect("direct registry");
        assert_store_readers(&multi, khive_db::pool::PoolConfig::default().max_readers);
    }
}

#[test]
#[serial]
#[serial(config_ledger)]
fn each_boot_records_its_own_forwarding_role() {
    let _env = FixtureEnv::clear();
    let args = Args::parse_from(["mcp", "--no-embed"]);
    mark_mcp_host_role(&args);
    assert_eq!(is_forwarding_client(), cfg!(unix));
    std::env::set_var("KHIVE_NO_DAEMON", "1");
    mark_mcp_host_role(&args);
    assert!(
        !is_forwarding_client(),
        "a daemonless boot after a forwarding one must run the store-owning boot work"
    );
}

#[tokio::test]
#[serial]
#[serial(config_ledger)]
async fn explicit_reader_count_wins_for_forwarding_runtime() {
    let _env = FixtureEnv::clear();
    let dir = tempfile::tempdir().unwrap();
    let (args, config, khive_cfg) = fixture(dir.path());
    let explicit = khive_db::pool::PoolConfig::default().max_readers + 1;
    let readers = mcp_max_readers(&args, &config, &khive_cfg.backends, Some(explicit));
    assert_eq!(
        readers,
        Some(explicit),
        "explicit reader count must win over the forwarding default"
    );
    let anchor = config.db_path.clone();
    let multi = build_registry_for_multi_backend_with_db_anchor_and_max_readers(
        config,
        &khive_cfg,
        None,
        anchor.as_deref(),
        readers,
    )
    .await
    .expect("explicit reader registry");
    assert_store_readers(&multi, explicit);
}

#[tokio::test]
#[serial]
#[serial(config_ledger)]
async fn memory_runtime_keeps_single_connection_mode() {
    let _env = FixtureEnv::clear();
    let dir = tempfile::tempdir().unwrap();
    let (mut args, mut config, mut khive_cfg) = fixture(dir.path());
    args.db = Some(":memory:".into());
    config.db_path = None;
    config.events_split = None;
    config.packs = vec!["kg".into()];
    khive_cfg.backends.clear();
    khive_cfg.packs.clear();
    let readers = mcp_max_readers(&args, &config, &khive_cfg.backends, None);
    assert_eq!(readers, None);
    let runtime = build_single_backend_runtime_with_max_readers(config, &khive_cfg, readers, None)
        .await
        .expect("in-memory runtime");
    assert!(!runtime.backend().is_file_backed());
    assert_eq!(runtime.backend().pool().max_readers(), 0);
}

#[tokio::test]
#[serial]
#[serial(config_ledger)]
async fn mcp_boot_applies_reader_policy_before_opening_single_store() {
    let _env = FixtureEnv::clear();
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("single.db");
    let cfg = dir.path().join("config.toml");
    std::fs::write(&cfg, "[runtime]\npacks = ['kg']\n").unwrap();
    let args = Args::parse_from([
        "mcp",
        "--no-embed",
        "--actor",
        "reader-pool-test",
        "--pack",
        "kg",
        "--db",
        db.to_str().unwrap(),
        "--config",
        cfg.to_str().unwrap(),
    ]);
    let (server, _) = build_server(&args).await.expect("single-store MCP server");
    #[cfg(unix)]
    assert_eq!(
        server.daemon_spawn_overrides(),
        (Some(cfg.as_path()), db.to_str()),
        "single-store MCP boot must retain explicit daemon spawn overrides"
    );
    let pool = server.pool().expect("file-backed server pool");
    assert_eq!(
        pool.max_readers(),
        1,
        "single-store MCP boot must retain the selected reader count"
    );
}

#[cfg(unix)]
#[tokio::test]
#[serial]
#[serial(config_ledger)]
async fn mcp_boot_retains_config_for_declared_backends() {
    let _env = FixtureEnv::clear();
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("main.db");
    let cfg = dir.path().join("config.toml");
    let config = format!("[runtime]\npacks = ['kg']\n[[backends]]\nname = 'main'\npath = {db:?}\n");
    std::fs::write(&cfg, config).unwrap();
    let args = Args::parse_from([
        "mcp",
        "--no-embed",
        "--actor",
        "reader-pool-test",
        "--pack",
        "kg",
        "--db",
        db.to_str().unwrap(),
        "--config",
        cfg.to_str().unwrap(),
    ]);
    let (server, _) = build_server(&args)
        .await
        .expect("declared-backend MCP server");
    assert_eq!(
        server.daemon_spawn_overrides(),
        (Some(cfg.as_path()), None),
        "declared-backend MCP boot must retain the config without a DB override"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn mcp_boot_retains_discovered_config_for_declared_backends() {
    const CHILD_ROOT: &str = "KHIVE_TEST_DISCOVERED_CONFIG_CHILD_ROOT";
    const CHILD_TOKEN: &str = "KHIVE_TEST_DISCOVERED_CONFIG_CHILD_TOKEN";
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let allowed = ["HOME", "TMPDIR", CHILD_ROOT, CHILD_TOKEN];
        for (key, _) in std::env::vars_os() {
            assert!(
                allowed
                    .iter()
                    .any(|name| key.as_os_str() == std::ffi::OsStr::new(*name)),
                "unexpected environment variable in discovery child"
            );
        }
        let _env = FixtureEnv::clear();
        let root = std::fs::canonicalize(root).expect("fixture root");
        // This token guards fixture isolation, not adversarial authentication.
        let token = std::env::var(CHILD_TOKEN).expect("fixture token");
        let suffix = token
            .strip_prefix("khive-child-")
            .expect("fixture token prefix");
        assert!(!suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_alphanumeric()));
        let token_path = root.join(&token);
        assert_eq!(std::fs::canonicalize(&token_path).unwrap(), token_path);
        assert!(token_path.is_file());
        let marker = root.join("owner.marker");
        assert_eq!(std::fs::canonicalize(&marker).unwrap(), marker);
        assert_eq!(std::fs::read_to_string(marker).unwrap(), token);
        let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
        let home = std::fs::canonicalize(std::env::var_os("HOME").expect("fixture HOME"))
            .expect("fixture HOME path");
        let tmp = std::fs::canonicalize(std::env::var_os("TMPDIR").expect("fixture TMPDIR"))
            .expect("fixture TMPDIR path");
        assert_eq!(cwd, root.join("cwd"));
        assert_eq!(home, root.join("home"));
        assert_eq!(tmp, root.join("tmp"));
        assert!(!cwd.join("khive.toml").exists());
        let config_dir = root.join(".khive");
        assert_eq!(config_dir.canonicalize().unwrap(), config_dir);
        let db = config_dir.join("khive.db");
        let cfg = config_dir.join("config.toml");
        assert_eq!(cfg.canonicalize().unwrap(), cfg);
        let expected_config =
            format!("[runtime]\npacks = ['kg']\n[[backends]]\nname = 'main'\npath = {db:?}\n");
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), expected_config);
        match std::fs::symlink_metadata(&db) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => panic!("fixture DB path must not already exist"),
            Err(error) => panic!("inspect fixture DB path: {error}"),
        }
        let args = Args::parse_from([
            "mcp",
            "--no-embed",
            "--actor",
            "reader-pool-test",
            "--pack",
            "kg",
            "--db",
            db.to_str().unwrap(),
        ]);
        let (server, _) = build_server(&args)
            .await
            .expect("discovered declared-backend MCP server");
        assert_eq!(
            server.daemon_spawn_overrides(),
            (Some(cfg.as_path()), None),
            "declared-backend MCP boot must retain the discovered config"
        );
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let cwd = root.join("cwd");
    let home = root.join("home");
    let tmp = root.join("tmp");
    let config_dir = root.join(".khive");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::create_dir_all(&config_dir).unwrap();
    let token_file = tempfile::Builder::new()
        .prefix("khive-child-")
        .tempfile_in(&root)
        .unwrap();
    let token = token_file.path().file_name().unwrap().to_str().unwrap();
    std::fs::write(root.join("owner.marker"), token).unwrap();
    let db = config_dir.join("khive.db");
    let cfg = config_dir.join("config.toml");
    let config = format!("[runtime]\npacks = ['kg']\n[[backends]]\nname = 'main'\npath = {db:?}\n");
    std::fs::write(&cfg, config).unwrap();
    let function_name =
        std::any::type_name_of_val(&mcp_boot_retains_discovered_config_for_declared_backends);
    // Libtest names begin at the module, whereas the function type includes the crate.
    let (_, test_name) = function_name.split_once("::").expect("qualified test name");
    let stdout_path = dir.path().join("child.stdout");
    let stderr_path = dir.path().join("child.stderr");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .current_dir(&cwd)
        .env_clear()
        .env("HOME", &home)
        .env("TMPDIR", &tmp)
        .env(CHILD_ROOT, &root)
        .env(CHILD_TOKEN, token)
        .stdout(std::fs::File::create(&stdout_path).unwrap())
        .stderr(std::fs::File::create(&stderr_path).unwrap())
        .spawn()
        .expect("run isolated discovery test");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let (status, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (status, false),
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("poll isolated discovery test: {error}");
            }
        }
        if std::time::Instant::now() >= deadline {
            // It may exit between the poll and kill; wait still reaps it.
            let _ = child.kill();
            break (child.wait().expect("reap timed-out discovery test"), true);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let stdout = std::fs::read_to_string(stdout_path).unwrap();
    let stderr = std::fs::read_to_string(stderr_path).unwrap();
    assert!(
        !timed_out
            && status.success()
            && stdout.contains("running 1 test")
            && stdout.contains("1 passed; 0 failed"),
        "isolated discovery child must run exactly one passing test (timed out: {timed_out})\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[tokio::test]
#[serial]
#[serial(config_ledger)]
async fn mcp_boot_direct_roles_keep_default_pools() {
    let _env = FixtureEnv::clear();
    // This exercises daemon-role construction, without starting a sidecar
    // transport worker or a daemon lifecycle loop in the test process.
    std::env::set_var("KHIVE_EVENTS_SPLIT", "0");
    for daemon in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("direct.db");
        let cfg = dir.path().join("config.toml");
        std::fs::write(&cfg, "[runtime]\npacks = ['kg']\n").unwrap();
        let mut args = Args::parse_from([
            "mcp",
            "--no-embed",
            "--actor",
            "reader-pool-test",
            "--pack",
            "kg",
            "--db",
            db.to_str().unwrap(),
            "--config",
            cfg.to_str().unwrap(),
        ]);
        args.daemon = daemon;
        if !daemon {
            std::env::set_var("KHIVE_NO_DAEMON", "1");
        }
        let (server, _) = build_server(&args).await.expect("direct-role MCP server");
        assert_eq!(
            server.pool().unwrap().max_readers(),
            khive_db::pool::PoolConfig::default().max_readers,
            "daemon and explicit daemonless boot must keep their existing pool size",
        );
    }
}

#[tokio::test]
#[serial]
#[serial(config_ledger)]
async fn forwarding_runtime_retains_pool_for_local_dispatch_and_save_to() {
    let _env = FixtureEnv::clear();
    let dir = tempfile::tempdir().unwrap();
    let (args, config, khive_cfg) = fixture(dir.path());
    let readers = mcp_max_readers(&args, &config, &khive_cfg.backends, None);
    let multi = build_registry_for_multi_backend_inner_with_max_readers(
        config, &khive_cfg, None, readers, None,
    )
    .await
    .expect("forwarding registry");
    let pool = multi.main_backend.pool_arc();
    let server = build_server_from_multi_backend_registry(multi, &khive_cfg, None);
    let sink = dir.path().join("stats.jsonl");
    std::env::set_var("KHIVE_SAVE_TO_ROOT", dir.path());
    server
        .dispatch_request_wire(crate::tools::request::RequestParams {
            ops: "stats()".into(),
            save_to: Some(sink.to_string_lossy().into_owned()),
            ..Default::default()
        })
        .await
        .expect("local save_to dispatch succeeds");
    assert!(!std::fs::read_to_string(&sink).unwrap().is_empty());
    let response = server
        .dispatch_request_wire(crate::tools::request::RequestParams {
            ops: "stats()".into(),
            ..Default::default()
        })
        .await
        .expect("the local fallback dispatch entrypoint uses the existing registry");
    let response: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(response["summary"]["succeeded"], 1);
    assert!(
        Arc::ptr_eq(&pool, &server.pool().unwrap()),
        "local work must not rebuild the pool"
    );
    assert_eq!(pool.max_readers(), 1);
}

#[tokio::test]
#[serial]
#[serial(config_ledger)]
async fn native_one_shot_builder_keeps_default_pool() {
    let _env = FixtureEnv::clear();
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("native.db");
    let cfg = dir.path().join("config.toml");
    std::fs::write(&cfg, "[runtime]\npacks = ['kg']\n").unwrap();
    let args = Args::parse_from([
        "mcp",
        "--no-embed",
        "--actor",
        "reader-pool-test",
        "--pack",
        "kg",
        "--db",
        db.to_str().unwrap(),
        "--config",
        cfg.to_str().unwrap(),
    ]);
    let (server, _) = build_server_with_explicit_namespace(
        &args,
        Namespace::parse("reader-pool-test").unwrap(),
        true,
        false,
    )
    .await
    .expect("native one-shot server");
    assert_eq!(
        server.pool().unwrap().max_readers(),
        khive_db::pool::PoolConfig::default().max_readers,
        "native one-shot construction must not inherit the MCP forwarding default"
    );
}
