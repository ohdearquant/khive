use super::{build_multi_backend_server_with_coordinator_and_pool_size, Args, Command};
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
        let (khive_cfg, source) = KhiveConfig::load_with_home_fallback_and_source(
            args.config.as_deref(),
            hint.as_deref(),
        )
        .unwrap()
        .expect("explicit multi-backend configuration");
        assert_eq!(khive_cfg.backends.len(), 2);
        assert_eq!(khive_cfg.backends[0].path.as_ref(), Some(&main));
        assert_eq!(khive_cfg.backends[1].path.as_ref(), Some(&tasks));
        serve::reject_conflicting_db_override_with_source(
            args.db.as_deref(),
            &khive_cfg.backends,
            Some(source.as_path()),
        )
        .unwrap();
        let (explicit, namespace) = khive_mcp::args::resolve_cli_namespace(&args).unwrap();
        let (base_cfg, anchor) =
            serve::resolve_runtime_config_with_db_anchor(serve::RuntimeConfigInputs {
                db: args.db.as_deref(),
                config: args.config.as_deref(),
                namespace,
                namespace_explicit: explicit,
                actor_explicit: explicit,
                no_embed: args.no_embed,
                packs: if args.pack.is_empty() {
                    None
                } else {
                    Some(args.pack.clone())
                },
                brain_profile: args.brain_profile.clone(),
            })
            .unwrap();
        let max_readers = serve::mcp_max_readers(&args, &base_cfg, &khive_cfg.backends, None);
        let (server, schedule) = build_multi_backend_server_with_coordinator_and_pool_size(
            base_cfg,
            &khive_cfg,
            args.db.as_deref(),
            anchor.as_deref(),
            max_readers,
            None,
            args.catalog_verbosity,
        )
        .await
        .expect("actual kkernel coordinator host");
        assert!(schedule.is_none());
        assert!(main.is_file());
        assert!(tasks.is_file());
        let identity = server.config_id().to_owned();
        let (listed, verbs) = observe_catalog(server)
            .await
            .expect("settled RMCP exchange");
        observations.push((identity, listed, verbs_result(&verbs)));
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
