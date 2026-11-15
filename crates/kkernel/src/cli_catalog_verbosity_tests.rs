use super::{prepare_multi_backend_mcp_command, Args, Command};
use clap::Parser;
use khive_mcp::{serve, server::KhiveMcpServer};
use khive_runtime::KhiveConfig;
use rmcp::{
    model::{CallToolRequestParams, ClientInfo},
    ClientHandler, ServiceExt,
};
use serde_json::{json, Value};
use std::{collections::BTreeSet, path::PathBuf, time::Duration};

const WAIT: Duration = Duration::from_secs(10);
const HEADING: &str = "\n\nVerbs registered on this server:\n";
const COMPACT_HEADER: &str = "Compact catalog: each entry shows the first nonempty line of its description, shortened to at most 160 characters where marked with …; verb(help=true) returns the full text.";

struct RestoreDirectory(PathBuf);

impl Drop for RestoreDirectory {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.0);
    }
}

struct Client;

impl ClientHandler for Client {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::default()
    }
}

async fn observe_catalog(server: KhiveMcpServer) -> Result<(Value, Value), String> {
    let audit = server.clone();
    let (server_io, client_io) = tokio::io::duplex(65536);
    let mut serving = tokio::spawn(async move {
        let service = server
            .serve(server_io)
            .await
            .map_err(|error| error.to_string())?;
        service.waiting().await.map_err(|error| error.to_string())?;
        Ok::<(), String>(())
    });
    let observed = async {
        let client = tokio::time::timeout(WAIT, Client.serve(client_io))
            .await
            .map_err(|_| "client handshake deadline".to_owned())?
            .map_err(|error| error.to_string())?;
        let replies = async {
            let listed = tokio::time::timeout(WAIT, client.list_tools(None))
                .await
                .map_err(|_| "tools/list deadline".to_owned())?
                .map_err(|error| error.to_string())?;
            let params = CallToolRequestParams::new("request").with_arguments(
                json!({"ops":"verbs()", "presentation":"verbose", "format":"json"})
                    .as_object()
                    .unwrap()
                    .clone(),
            );
            let verbs = tokio::time::timeout(WAIT, client.call_tool(params))
                .await
                .map_err(|_| "verbs request deadline".to_owned())?
                .map_err(|error| error.to_string())?;
            Ok::<_, String>((
                serde_json::to_value(listed).map_err(|error| error.to_string())?,
                serde_json::to_value(verbs).map_err(|error| error.to_string())?,
            ))
        }
        .await;
        let cancelled = tokio::time::timeout(WAIT, client.cancel())
            .await
            .map_err(|_| "client shutdown deadline".to_owned())?
            .map_err(|error| error.to_string());
        cancelled?;
        replies
    }
    .await;
    let settled = match tokio::time::timeout(WAIT, &mut serving).await {
        Ok(result) => result
            .map_err(|error| error.to_string())
            .and_then(|result| result),
        Err(_) => {
            serving.abort();
            let _ = serving.await;
            Err("server shutdown deadline".to_owned())
        }
    };
    let drained = tokio::time::timeout(WAIT, audit.shutdown_audit_batch())
        .await
        .map_err(|_| "audit shutdown deadline".to_owned())?
        .map_err(|error| format!("audit shutdown: {error:?}"));
    settled?;
    drained?;
    observed
}

fn request_descriptor(listed: &Value) -> &Value {
    let tools = listed["tools"].as_array().expect("tools/list array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "request");
    &tools[0]
}

fn verbs_result(reply: &Value) -> Value {
    let text = reply["content"][0]["text"].as_str().expect("request text");
    let body: Value = serde_json::from_str(text).expect("request envelope");
    assert_eq!(body["results"][0]["ok"], true, "{body}");
    body["results"][0]["result"].clone()
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn parsed_kkernel_multi_backend_catalog_verbosity_reaches_tools_list() {
    if crate::test_process::run_in_child() {
        return;
    }
    std::env::set_var("KHIVE_NO_DAEMON", "1");
    let fixture = tempfile::tempdir().unwrap();
    let original_directory = std::env::current_dir().unwrap();
    let restore = RestoreDirectory(original_directory.clone());
    std::env::set_current_dir(fixture.path()).unwrap();
    let config = fixture.path().join("config.toml");
    let main = fixture.path().join("main.db");
    let tasks = fixture.path().join("tasks.db");
    assert_ne!(main, tasks);
    std::fs::write(
        &config,
        format!(
            "[runtime]\npacks = ['kg', 'gtd']\n\
             [packs.kg]\nbackend = 'main'\nengines = []\n\
             [packs.gtd]\nbackend = 'tasks'\nengines = []\n\
             [[backends]]\nname = 'main'\npath = {main:?}\n\
             [[backends]]\nname = 'tasks'\npath = {tasks:?}\n"
        ),
    )
    .unwrap();
    let mut observations = Vec::new();
    for mode in [None, Some("full"), Some("compact")] {
        let mut argv = vec![
            "kkernel".to_owned(),
            "mcp".into(),
            "--config".into(),
            config.to_str().unwrap().into(),
            "--no-embed".into(),
            "--actor".into(),
            "catalog-test".into(),
        ];
        if let Some(mode) = mode {
            argv.extend(["--catalog-verbosity".into(), mode.into()]);
        }
        let parsed = Args::try_parse_from(argv).expect("actual kkernel arguments");
        let Command::Mcp(args) = parsed.command.expect("MCP command") else {
            panic!("expected MCP subcommand");
        };
        let hint = serve::config_discovery_db_anchor(args.db.as_deref());
        let (khive_cfg, _) = KhiveConfig::load_with_home_fallback_and_source(
            args.config.as_deref(),
            hint.as_deref(),
        )
        .unwrap()
        .expect("explicit multi-backend configuration");
        assert_eq!(khive_cfg.backends.len(), 2);
        assert_eq!(khive_cfg.backends[0].path.as_ref(), Some(&main));
        assert_eq!(khive_cfg.backends[1].path.as_ref(), Some(&tasks));
        let host = prepare_multi_backend_mcp_command(&args)
            .await
            .expect("actual parsed MCP preparation")
            .expect("actual kkernel coordinator host");
        assert!(host.schedule_rt.is_none());
        assert!(main.is_file());
        assert!(tasks.is_file());
        let identity = host.server.config_id().to_owned();
        let (listed, verbs) = observe_catalog(host.server.clone())
            .await
            .expect("settled RMCP exchange");
        observations.push((identity, listed, verbs_result(&verbs)));
        drop(host);
    }
    drop(restore);
    assert_eq!(std::env::current_dir().unwrap(), original_directory);

    assert_eq!(observations[0].0, observations[1].0);
    assert_eq!(observations[0].0, observations[2].0);
    let default = request_descriptor(&observations[0].1);
    let full = request_descriptor(&observations[1].1);
    let compact = request_descriptor(&observations[2].1);
    assert_eq!(default, full);
    let without_description = |tool: &Value| {
        let mut tool = tool.clone();
        tool.as_object_mut().unwrap().remove("description");
        tool
    };
    assert_eq!(without_description(full), without_description(compact));
    let (full_prefix, full_catalog) = full["description"]
        .as_str()
        .unwrap()
        .split_once(HEADING)
        .unwrap();
    let (compact_prefix, compact_catalog) = compact["description"]
        .as_str()
        .unwrap()
        .split_once(HEADING)
        .unwrap();
    assert_eq!(full_prefix.as_bytes(), compact_prefix.as_bytes());
    assert!(!full_catalog.contains(COMPACT_HEADER));
    let (header, entries) = compact_catalog.split_once('\n').unwrap();
    assert_eq!(header, COMPACT_HEADER);
    assert_ne!(full_catalog, compact_catalog);
    let verbs = &observations[2].2;
    assert_eq!(verbs, &observations[0].2);
    assert_eq!(verbs, &observations[1].2);
    let mut expected = verbs["verbs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|verb| verb["verb"].as_str().unwrap())
        .collect::<Vec<_>>();
    expected.sort_unstable();
    assert!(!expected.is_empty());
    let actual = entries
        .lines()
        .map(|line| {
            let (name, contribution) = line.split_once(": ").expect("compact catalog row");
            assert!(contribution.ends_with(&format!(" — {name}(help=true)")));
            name
        })
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
    assert_eq!(
        verbs["pack_counts"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["gtd", "kg"])
    );
    for (name, pack) in [("verbs", "kg"), ("gtd.tasks", "gtd")] {
        let line = entries
            .lines()
            .find(|line| line.starts_with(&format!("{name}: ")))
            .unwrap();
        assert!(line.contains(&format!("[{pack}] ")), "{line}");
    }
}

fn parsed_mcp_args(config: &std::path::Path, extra: &[&str]) -> khive_mcp::args::Args {
    let mut argv = vec![
        "kkernel",
        "mcp",
        "--config",
        config.to_str().unwrap(),
        "--no-embed",
        "--actor",
        "catalog-test",
    ];
    argv.extend_from_slice(extra);
    let parsed = Args::try_parse_from(argv).expect("actual kkernel arguments");
    let Command::Mcp(args) = parsed.command.expect("MCP command") else {
        panic!("expected MCP subcommand");
    };
    args
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn parsed_mcp_preparation_refuses_invalid_inputs_before_opening_backends() {
    if crate::test_process::run_in_child() {
        return;
    }
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let main = root.join("main.db");
    let tasks = root.join("tasks.db");
    let override_path = root.join("override.db");
    let missing = root.join("missing.toml");
    let malformed = root.join("malformed.toml");
    std::fs::write(&malformed, "[runtime\npacks = [").unwrap();
    for config in [&missing, &malformed] {
        let args = parsed_mcp_args(config, &[]);
        let result = tokio::time::timeout(WAIT, prepare_multi_backend_mcp_command(&args))
            .await
            .expect("bounded explicit-config refusal");
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("invalid explicit config must refuse"),
        };
        assert_eq!(
            error.to_string(),
            "failed to load the explicitly selected config file"
        );
        assert!(!main.exists());
        assert!(!tasks.exists());
        assert!(!override_path.exists());
    }
    let config = root.join("multi.toml");
    std::fs::write(
        &config,
        format!(
            "[runtime]\npacks = ['kg', 'gtd']\n\
             [packs.kg]\nbackend = 'main'\nengines = []\n\
             [packs.gtd]\nbackend = 'tasks'\nengines = []\n\
             [[backends]]\nname = 'main'\npath = {main:?}\n\
             [[backends]]\nname = 'tasks'\npath = {tasks:?}\n"
        ),
    )
    .unwrap();
    let args = parsed_mcp_args(&config, &["--db", override_path.to_str().unwrap()]);
    let result = tokio::time::timeout(WAIT, prepare_multi_backend_mcp_command(&args))
        .await
        .expect("bounded database-override refusal");
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("conflicting database override must refuse"),
    };
    let envelope = serve::db_override_refusal_envelope(&error)
        .expect("existing typed database-override refusal");
    assert_eq!(envelope["ok"], false);
    assert_eq!(envelope["invocation"]["started"], false);
    assert_eq!(envelope["error"]["code"], serve::DB_OVERRIDE_CONFLICT_CODE);
    assert_eq!(
        envelope["error"]["db_override"],
        override_path.to_str().unwrap()
    );
    assert_eq!(envelope["error"]["declared_backends"], 2);
    assert!(!main.exists());
    assert!(!tasks.exists());
    assert!(!override_path.exists());

    let single = root.join("single.toml");
    std::fs::write(
        &single,
        format!(
            "[runtime]\npacks = ['kg']\n\
             [packs.kg]\nbackend = 'main'\nengines = []\n\
             [[backends]]\nname = 'main'\npath = {main:?}\n"
        ),
    )
    .unwrap();
    let args = parsed_mcp_args(&single, &[]);
    let host = tokio::time::timeout(WAIT, prepare_multi_backend_mcp_command(&args))
        .await
        .expect("bounded single-backend classification")
        .expect("valid single-backend configuration");
    assert!(host.is_none());
    assert!(!main.exists());
    assert!(!tasks.exists());
    assert!(!override_path.exists());
}

#[cfg(unix)]
const STORE_CLAIM_TEST: &str =
    "cli::catalog_verbosity_tests::prepared_mcp_host_retains_daemon_store_claims_until_drop";
#[cfg(unix)]
const STORE_CLAIM_MODE: &str = "KKERNEL_CATALOG_STORE_CLAIM_MODE";
#[cfg(unix)]
const STORE_CLAIM_PATH: &str = "KKERNEL_CATALOG_STORE_CLAIM_PATH";

#[cfg(unix)]
fn run_store_claim_probe_child() -> bool {
    let mode = std::env::var_os(STORE_CLAIM_MODE);
    let path = std::env::var_os(STORE_CLAIM_PATH);
    if mode.is_none() && path.is_none() {
        return false;
    }
    assert_eq!(
        mode.as_deref(),
        Some(std::ffi::OsStr::new("claim-stores-v1"))
    );
    let path = PathBuf::from(path.expect("store-claim probe path"));
    assert!(path.is_absolute());
    assert_eq!(
        std::env::args().skip(1).collect::<Vec<_>>(),
        [
            "--exact",
            STORE_CLAIM_TEST,
            "--nocapture",
            "--test-threads=1"
        ]
    );
    assert_eq!(std::thread::current().name(), Some(STORE_CLAIM_TEST));
    match khive_runtime::daemon::claim_stores(std::slice::from_ref(&path), &[]) {
        Ok(guards) => {
            assert_eq!(guards.len(), 1);
            drop(guards);
            println!("KKERNEL_STORE_CLAIM=ACQUIRED");
        }
        Err(error) => {
            let message = error.to_string();
            assert!(
                message.starts_with("refusing to start: khived is already running as ")
                    && message.contains(&format!("for database {};", path.display()))
                    && message.contains("daemon store lock ")
                    && message.ends_with(" is held"),
                "unexpected store-claim failure: {error:#}"
            );
            println!("KKERNEL_STORE_CLAIM=HELD");
        }
    }
    true
}

#[cfg(unix)]
fn probe_store_claim(path: &std::path::Path) -> Result<String, String> {
    let stdout = tempfile::NamedTempFile::new().map_err(|error| error.to_string())?;
    let stderr = tempfile::NamedTempFile::new().map_err(|error| error.to_string())?;
    let mut child =
        std::process::Command::new(std::env::current_exe().map_err(|error| error.to_string())?)
            .args([
                "--exact",
                STORE_CLAIM_TEST,
                "--nocapture",
                "--test-threads=1",
            ])
            .env(STORE_CLAIM_MODE, "claim-stores-v1")
            .env(STORE_CLAIM_PATH, path)
            .stdin(std::process::Stdio::null())
            .stdout(stdout.reopen().map_err(|error| error.to_string())?)
            .stderr(stderr.reopen().map_err(|error| error.to_string())?)
            .spawn()
            .map_err(|error| error.to_string())?;
    let deadline = std::time::Instant::now() + WAIT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => break Err("store-claim probe deadline".to_owned()),
            Err(error) => break Err(format!("store-claim probe wait: {error}")),
        }
    };
    if status.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let output = std::fs::read_to_string(stdout.path()).map_err(|error| error.to_string())?;
    let errors = std::fs::read_to_string(stderr.path()).map_err(|error| error.to_string())?;
    let status = status.map_err(|error| format!("{error}\nstdout: {output}\nstderr: {errors}"))?;
    if !status.success() || !output.contains("test result: ok. 1 passed; 0 failed;") {
        return Err(format!(
            "store-claim probe failed: {status}\nstdout: {output}\nstderr: {errors}"
        ));
    }
    let observations = output
        .lines()
        .filter_map(|line| {
            line.split_once("KKERNEL_STORE_CLAIM=")
                .map(|(_, value)| value)
        })
        .collect::<Vec<_>>();
    match observations.as_slice() {
        ["HELD"] => Ok("HELD".to_owned()),
        ["ACQUIRED"] => Ok("ACQUIRED".to_owned()),
        _ => Err(format!(
            "expected exactly one store-claim observation: {output}"
        )),
    }
}

#[test]
#[cfg(unix)]
#[serial_test::serial(config_ledger)]
fn prepared_mcp_host_retains_daemon_store_claims_until_drop() {
    if run_store_claim_probe_child() || crate::test_process::run_in_child() {
        return;
    }
    // SAFETY: this exact isolated child has not opened SQLite or built a runtime yet.
    unsafe { khive_db::pool::initialize_claimed_file_observer().unwrap() };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        let config = root.join("daemon.toml");
        let tasks = root.join("tasks.db");
        std::fs::write(
            &config,
            format!(
                "[runtime]\npacks = ['kg', 'gtd']\n\
                 [packs.kg]\nbackend = 'main'\nengines = []\n\
                 [packs.gtd]\nbackend = 'tasks'\nengines = []\n\
                 [[backends]]\nname = 'main'\nkind = 'memory'\n\
                 [[backends]]\nname = 'tasks'\npath = {tasks:?}\n"
            ),
        )
        .unwrap();
        let args = parsed_mcp_args(&config, &["--daemon", "--catalog-verbosity", "compact"]);
        let host = tokio::time::timeout(WAIT, prepare_multi_backend_mcp_command(&args))
            .await
            .expect("bounded actual daemon preparation")
            .expect("actual daemon preparation")
            .expect("actual multi-backend host");
        assert!(tasks.is_file());
        let tasks = tasks.canonicalize().unwrap();
        let socket = PathBuf::from(std::env::var_os("KHIVE_SOCKET").unwrap());
        let socket_before = socket.exists();
        let held_host = probe_store_claim(&tasks);
        let retained = host.store_guards.as_ref().map_or(0, Vec::len);
        let schedule_absent = host.schedule_rt.is_none();
        let boot_retained = host.boot_guard.is_some();
        let drained = tokio::time::timeout(WAIT, host.server.shutdown_audit_batch()).await;
        let transports = khive_mcp::transport::TransportRegistry::with_builtins();
        let serving = host.serve(&args, &transports);
        let held_unpolled = probe_store_claim(&tasks);
        drop(serving);
        let acquired = probe_store_claim(&tasks);

        assert_eq!(held_host.expect("first held-host probe"), "HELD");
        assert_eq!(retained, 1);
        assert!(schedule_absent);
        assert!(boot_retained);
        drained
            .expect("bounded audit drain")
            .expect("audit drained");
        assert_eq!(held_unpolled.expect("held unpolled-future probe"), "HELD");
        assert_eq!(acquired.expect("released-host probe"), "ACQUIRED");
        assert!(!socket_before);
        assert!(!socket.exists());
    });
}
