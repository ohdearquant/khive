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
    let multi =
        build_registry_for_multi_backend_inner_with_max_readers(config, &khive_cfg, None, readers)
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
            config, &khive_cfg, None, readers,
        )
        .await
        .expect("direct registry");
        assert_store_readers(&multi, khive_db::pool::PoolConfig::default().max_readers);
    }
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
    let runtime = build_single_backend_runtime_with_max_readers(config, &khive_cfg, readers)
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
    let pool = server.pool().expect("file-backed server pool");
    assert_eq!(
        pool.max_readers(),
        1,
        "single-store MCP boot must retain the selected reader count"
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
    let multi =
        build_registry_for_multi_backend_inner_with_max_readers(config, &khive_cfg, None, readers)
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
