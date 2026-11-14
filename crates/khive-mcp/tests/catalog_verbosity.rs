//! Public catalog presentation, help, and parsed host-construction contracts.
//!
//! Each case runs in its own private-home child so config discovery and daemon
//! policy cannot depend on another test's process environment.

use std::collections::BTreeSet;
use std::future::Future;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use async_trait::async_trait;
use clap::Parser;
use khive_mcp::{
    args::{Args, CatalogVerbosity},
    serve::build_server,
    server::KhiveMcpServer,
};
use khive_runtime::{
    mount_config::MountEffect, mounted_verb::MountedVerb, KhiveRuntime, NamespaceToken,
    PackRuntime, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_types::{HandlerDef, IdResolutionMode, Pack, ParamDef, VerbCategory, Visibility};
use rmcp::{
    model::{CallToolRequestParams, ClientInfo},
    ClientHandler, ServiceError, ServiceExt,
};
use serde_json::{json, Value};

const CHILD: &str = "KHIVE_CATALOG_VERBOSITY_TEST_CHILD";
const HEADING: &str = "\n\nVerbs registered on this server:\n";
const COMPACT_HEADER: &str = "Compact catalog: each entry shows the first nonempty line of its description, shortened to at most 160 characters where marked with …; verb(help=true) returns the full text.";
const ECHO_DESCRIPTION: &str = concat!(
    "Echo a supplied message without changing its content. ",
    "This deliberately long first line retains useful context for callers. ",
    "Additional words make shortening observable without depending on a production pack's prose. ",
    "\nECHO_FULL_TAIL: the second line belongs to full discovery and help. ",
    "Complete help also preserves parameter explanations, examples, and details beyond discovery's first line."
);
const UNICODE_DESCRIPTION: &str = concat!(
    "λλλλλλλλλλλλλλλλλλλλλλλλλλλλλλλλ",
    "λλλλλλλλλλλλλλλλλλλλλλλλλλλλλλλλ",
    "λλλλλλλλλλλλλλλλλλλλλλλλλλλλλλλλ",
    "λλλλλλλλλλλλλλλλλλλλλλλλλλλλλλλλ",
    "λλλλλλλλλλλλλλλλλλλλλλλλλλλλλλλλ",
    "λλλλλλλλλλλλλλλλλλλλλλλλλλλλλλλλ",
    "\nUNICODE_FULL_TAIL"
);
const MOUNT_OLD: &str = "Mounted original description.\nMOUNT_OLD_FULL_TAIL";
const MOUNT_NEW: &str = "Mounted refreshed description.\nMOUNT_NEW_FULL_TAIL";
const VERBS_DESCRIPTION: &str = "List all MCP-callable verbs registered on this server. Internal subhandlers are excluded. Pass category=<name> to filter by illocutionary category (Assertive | Commissive | Declaration | Directive). Pass pack=<name> to filter by pack.";

fn isolated(name: &str, body: impl Future<Output = ()>) {
    if std::env::var(CHILD).as_deref() == Ok(name) {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
            .block_on(body);
        return;
    }
    let home = tempfile::tempdir().expect("private test home");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let temporary = home.path().join("tmp");
    std::fs::create_dir(&temporary).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env_clear()
        .env(CHILD, name)
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .env("TMPDIR", &temporary)
        .env("KHIVE_NO_DAEMON", "1")
        .current_dir(home.path())
        .output()
        .expect("isolated test child");
    assert!(
        output.status.success(),
        "{name}: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("running 1 test"),
        "the isolated child must execute exactly one selected test"
    );
}

struct CatalogPack;

impl Pack for CatalogPack {
    const NAME: &'static str = "catalog-fixture";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[
        HandlerDef {
            name: "catalog.unicode",
            description: UNICODE_DESCRIPTION,
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        },
        HandlerDef {
            name: "catalog.echo",
            description: ECHO_DESCRIPTION,
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[ParamDef {
                name: "message",
                param_type: "string",
                required: true,
                description: "Message returned unchanged.",
                resolution_mode: IdResolutionMode::NotApplicable,
            }],
        },
        HandlerDef {
            name: "catalog.disabled",
            description: "Disabled public entry must not be advertised.",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        },
        HandlerDef {
            name: "catalog.hidden",
            description: "Internal entry must not be advertised.",
            visibility: Visibility::Subhandler,
            category: VerbCategory::Assertive,
            params: &[],
        },
    ];
}

#[async_trait]
impl PackRuntime for CatalogPack {
    fn name(&self) -> &str {
        Self::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        Self::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        Self::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        Self::HANDLERS
    }
    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        match verb {
            "catalog.echo" => Ok(json!({"message": params["message"]})),
            "catalog.unicode" => Ok(json!({"unicode": true})),
            _ => Err(RuntimeError::UnknownVerb(verb.into())),
        }
    }
}

#[derive(Default)]
struct MountedState {
    selected: AtomicUsize,
    fail: AtomicBool,
    refreshes: AtomicUsize,
    snapshot: Mutex<Vec<MountedVerb>>,
}

impl MountedState {
    fn definition(generation: usize) -> MountedVerb {
        MountedVerb {
            name: if generation == 0 { "old" } else { "fresh" }.into(),
            description: Some(
                if generation == 0 {
                    MOUNT_OLD
                } else {
                    MOUNT_NEW
                }
                .into(),
            ),
            input_schema: json!({"type":"object", "properties":{}}),
            output_schema: None,
            effect: MountEffect::Read,
            digest: format!("catalog-{generation}"),
            generation: generation as i64,
        }
    }
}

struct MountedCatalog(Arc<MountedState>);

#[async_trait]
impl PackRuntime for MountedCatalog {
    fn name(&self) -> &str {
        "mounted"
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        &[]
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        &[]
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        &[]
    }
    fn mounted_namespace(&self) -> Option<&str> {
        Some("mounted")
    }
    fn mounted_catalog_snapshot(&self) -> Vec<MountedVerb> {
        self.0.snapshot.lock().unwrap().clone()
    }
    async fn mounted_catalog(&self) -> Result<Vec<MountedVerb>, RuntimeError> {
        self.0.refreshes.fetch_add(1, Ordering::SeqCst);
        if self.0.fail.load(Ordering::SeqCst) {
            return Err(RuntimeError::InvalidInput(
                "fixture catalog refresh refused".into(),
            ));
        }
        let catalog = vec![MountedState::definition(
            self.0.selected.load(Ordering::SeqCst),
        )];
        *self.0.snapshot.lock().unwrap() = catalog.clone();
        Ok(catalog)
    }
    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Ok(json!({"mounted": verb}))
    }
}

fn fixture() -> (KhiveMcpServer, VerbRegistry, Arc<MountedState>) {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        packs: vec!["kg".into()],
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.with_runtime_event_store(&runtime).unwrap();
    builder.register(khive_pack_kg::KgPack::new(runtime));
    // Use the real verbs handler while keeping a small, exact discovery fixture.
    let disabled = khive_pack_kg::KgPack::HANDLERS
        .iter()
        .filter(|handler| handler.visibility == Visibility::Verb && handler.name != "verbs")
        .map(|handler| handler.name.to_owned())
        .collect::<Vec<_>>();
    builder.with_disabled_verbs("kg", &disabled);
    builder.register(CatalogPack);
    builder.with_disabled_verbs(CatalogPack::NAME, &["catalog.disabled".into()]);
    let state = Arc::new(MountedState::default());
    *state.snapshot.lock().unwrap() = vec![MountedState::definition(0)];
    builder
        .register_mounted(Box::new(MountedCatalog(state.clone())))
        .unwrap();
    let registry = builder.build().unwrap();
    let server =
        KhiveMcpServer::from_registry_with_meta(registry.clone(), "local", "catalog-fixture");
    (server, registry, state)
}

#[derive(Default)]
struct Client;
impl ClientHandler for Client {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::default()
    }
}

enum Action {
    List,
    Call(&'static str),
    Refresh,
    FailRefresh,
}

fn wire_error(error: ServiceError) -> Value {
    match error {
        ServiceError::McpError(error) => serde_json::to_value(error).unwrap(),
        other => json!({"transport_error": other.to_string()}),
    }
}

async fn exchange(
    server: KhiveMcpServer,
    actions: &[Action],
    mounted: Option<&Arc<MountedState>>,
) -> Vec<Result<Value, Value>> {
    let (server_io, client_io) = tokio::io::duplex(65536);
    let mut serving = tokio::spawn(async move {
        let service = server.serve(server_io).await.expect("server handshake");
        service.waiting().await.expect("server task settles");
    });
    let client = tokio::time::timeout(Duration::from_secs(10), Client.serve(client_io))
        .await
        .expect("client handshake deadline")
        .expect("client handshake");
    let mut observed = Vec::new();
    for action in actions {
        let reply = match action {
            Action::List => tokio::time::timeout(Duration::from_secs(10), client.list_tools(None))
                .await
                .map_err(|_| json!({"request_timeout":true}))
                .and_then(|result| {
                    result
                        .map(|value| serde_json::to_value(value).unwrap())
                        .map_err(wire_error)
                }),
            Action::Call(ops) => {
                let params = CallToolRequestParams::new("request").with_arguments(
                    json!({"ops":ops, "presentation":"verbose", "format":"json"})
                        .as_object()
                        .unwrap()
                        .clone(),
                );
                tokio::time::timeout(Duration::from_secs(10), client.call_tool(params))
                    .await
                    .map_err(|_| json!({"request_timeout":true}))
                    .and_then(|result| {
                        result
                            .map(|value| serde_json::to_value(value).unwrap())
                            .map_err(wire_error)
                    })
            }
            Action::Refresh => {
                let state = mounted.expect("mounted fixture");
                state.selected.store(1, Ordering::SeqCst);
                // Selecting the next remote generation must not change the local
                // snapshot. Only the awaited catalog refresh publishes it.
                Ok(
                    json!({"snapshot":state.snapshot.lock().unwrap().iter().map(|v| v.name.clone()).collect::<Vec<_>>()}),
                )
            }
            Action::FailRefresh => {
                mounted
                    .expect("mounted fixture")
                    .fail
                    .store(true, Ordering::SeqCst);
                continue;
            }
        };
        observed.push(reply);
    }
    tokio::time::timeout(Duration::from_secs(10), client.cancel())
        .await
        .expect("client shutdown deadline")
        .expect("client shutdown");
    match tokio::time::timeout(Duration::from_secs(10), &mut serving).await {
        Ok(result) => result.expect("server task joined"),
        Err(_) => {
            serving.abort();
            let _ = serving.await;
            panic!("server shutdown deadline");
        }
    }
    observed
}

fn tool(reply: &Result<Value, Value>) -> &Value {
    let listed = reply.as_ref().expect("tools/list succeeded");
    let tools = listed["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "request");
    &tools[0]
}

fn description(tool: &Value) -> &str {
    tool["description"].as_str().unwrap()
}

fn result(reply: &Result<Value, Value>) -> Value {
    let call = reply.as_ref().expect("tools/call succeeded");
    let body: Value = serde_json::from_str(call["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(body["results"][0]["ok"], true, "{body}");
    body["results"][0]["result"].clone()
}

fn descriptor_without_description(tool: &Value) -> Value {
    let mut value = tool.clone();
    value.as_object_mut().unwrap().remove("description");
    value
}

fn compact_rows(description: &str) -> Vec<(&str, &str)> {
    let catalog = description.split_once(HEADING).unwrap().1;
    let (header, entries) = catalog.split_once('\n').unwrap();
    assert_eq!(header, COMPACT_HEADER);
    entries
        .lines()
        .map(|line| {
            let (name, contribution) = line.split_once(": ").expect("compact row");
            assert!(contribution.ends_with(&format!(" — {name}(help=true)")));
            (name, contribution)
        })
        .collect()
}

#[test]
#[serial_test::serial(config_ledger)]
fn default_and_full_catalog_preserve_complete_wire_description() {
    isolated(
        "default_and_full_catalog_preserve_complete_wire_description",
        async {
            let empty = KhiveMcpServer::from_registry(VerbRegistryBuilder::new().build().unwrap());
            let baseline = exchange(empty, &[Action::List], None).await;
            let base = description(tool(&baseline[0]));
            assert!(base.ends_with(HEADING));
            let expected = format!(
            "{base}  catalog.echo — {ECHO_DESCRIPTION}\n  catalog.unicode — {UNICODE_DESCRIPTION}\n  mounted.old — {MOUNT_OLD}\n  verbs — {VERBS_DESCRIPTION}\n"
        );
            let mut descriptions = Vec::new();
            for explicit in [false, true] {
                let (server, registry, _) = fixture();
                let server = if explicit {
                    server.with_catalog_verbosity(CatalogVerbosity::Full)
                } else {
                    server
                };
                let replies = exchange(server, &[Action::List], None).await;
                registry.shutdown_audit_batch().await.unwrap();
                let actual = tool(&replies[0]);
                assert_eq!(description(actual), expected);
                assert_eq!(
                    descriptor_without_description(actual),
                    descriptor_without_description(tool(&baseline[0]))
                );
                assert!(actual["inputSchema"]["properties"].get("ops").is_some());
                assert!(actual["inputSchema"]["properties"].get("plan").is_some());
                descriptions.push(actual.clone());
            }
            assert_eq!(descriptions[0], descriptions[1]);
        },
    );
}

#[test]
#[serial_test::serial(config_ledger)]
fn compact_catalog_keeps_exact_callables_full_help_and_dispatch() {
    isolated(
        "compact_catalog_keeps_exact_callables_full_help_and_dispatch",
        async {
            let mut snapshots = Vec::new();
            for mode in [CatalogVerbosity::Full, CatalogVerbosity::Compact] {
                let (server, registry, _) = fixture();
                let identity = server.config_id().to_owned();
                let server = server.with_catalog_verbosity(mode);
                assert_eq!(server.config_id(), identity);
                let replies = exchange(
                    server,
                    &[
                        Action::List,
                        Action::Call("verbs()"),
                        Action::Call("catalog.echo(help=true)"),
                        Action::Call("catalog.echo(message=\"wire payload\")"),
                    ],
                    None,
                )
                .await;
                registry.shutdown_audit_batch().await.unwrap();
                snapshots.push(replies);
            }
            let full = &snapshots[0];
            let compact = &snapshots[1];
            let full_tool = tool(&full[0]);
            let compact_tool = tool(&compact[0]);
            assert_eq!(
                descriptor_without_description(full_tool),
                descriptor_without_description(compact_tool)
            );
            assert_eq!(
                description(full_tool).split_once(HEADING).unwrap().0,
                description(compact_tool).split_once(HEADING).unwrap().0
            );
            assert!(description(compact_tool).len() < description(full_tool).len());
            let rows = compact_rows(description(compact_tool));
            let names = rows.iter().map(|(name, _)| *name).collect::<Vec<_>>();
            assert_eq!(
                names,
                ["catalog.echo", "catalog.unicode", "mounted.old", "verbs"]
            );
            assert_eq!(names.iter().collect::<BTreeSet<_>>().len(), names.len());
            for (name, row) in &rows {
                let owner = match *name {
                    "verbs" => "kg",
                    "mounted.old" => "mounted",
                    _ => "catalog-fixture",
                };
                assert!(row.starts_with(&format!("[{owner}] ")), "{row}");
                assert_eq!(row.matches(&format!("[{owner}] ")).count(), 1);
                assert!(!row.contains("FULL_TAIL"));
            }
            let unicode = rows[1]
                .1
                .strip_prefix("[catalog-fixture] ")
                .unwrap()
                .strip_suffix(" — catalog.unicode(help=true)")
                .unwrap();
            assert_eq!(unicode.chars().count(), 160);
            assert!(unicode.ends_with('…'));
            assert!(unicode.chars().take(159).all(|ch| ch == 'λ'));
            let advertised = result(&compact[1]);
            assert_eq!(advertised, result(&full[1]));
            let mut public_names = advertised["verbs"]
                .as_array()
                .unwrap()
                .iter()
                .map(|verb| verb["verb"].as_str().unwrap())
                .collect::<Vec<_>>();
            public_names.sort_unstable();
            assert_eq!(names, public_names);
            for (name, expected) in [
                ("catalog.echo", ECHO_DESCRIPTION),
                ("catalog.unicode", UNICODE_DESCRIPTION),
                ("mounted.old", MOUNT_OLD),
                ("verbs", VERBS_DESCRIPTION),
            ] {
                let verb = advertised["verbs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|v| v["verb"] == name)
                    .unwrap();
                assert_eq!(verb["description"], expected);
            }
            let help = result(&compact[2]);
            assert_eq!(help, result(&full[2]));
            assert_eq!(help["description"], ECHO_DESCRIPTION);
            assert_eq!(help["params"][0]["name"], "message");
            assert_eq!(help["params"][0]["required"], true);
            assert_eq!(result(&compact[3]), json!({"message":"wire payload"}));
            assert_eq!(result(&compact[3]), result(&full[3]));
        },
    );
}

#[test]
#[serial_test::serial(config_ledger)]
fn compact_catalog_refreshes_mounted_snapshot_and_preserves_failure() {
    isolated(
        "compact_catalog_refreshes_mounted_snapshot_and_preserves_failure",
        async {
            let (server, registry, state) = fixture();
            let replies = exchange(
                server.with_catalog_verbosity(CatalogVerbosity::Compact),
                &[
                    Action::List,
                    Action::Refresh,
                    Action::List,
                    Action::Call("mounted.fresh(help=true)"),
                    Action::FailRefresh,
                    Action::List,
                ],
                Some(&state),
            )
            .await;
            registry.shutdown_audit_batch().await.unwrap();
            assert!(compact_rows(description(tool(&replies[0])))
                .iter()
                .any(|(name, _)| *name == "mounted.old"));
            assert_eq!(replies[1], Ok(json!({"snapshot":["old"]})));
            let rows = compact_rows(description(tool(&replies[2])));
            assert_eq!(
                rows.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
                ["catalog.echo", "catalog.unicode", "mounted.fresh", "verbs"]
            );
            assert!(rows[2]
                .1
                .starts_with("[mounted] Mounted refreshed description."));
            assert_eq!(result(&replies[3])["description"], MOUNT_NEW);
            assert_eq!(state.snapshot.lock().unwrap()[0].name, "fresh");
            assert!(state.refreshes.load(Ordering::SeqCst) >= 3);
            let error = replies[4]
                .as_ref()
                .expect_err("catalog refresh must fail tools/list");
            assert_eq!(error["code"], -32603);
            assert_eq!(error["message"], "mounted catalog unavailable");
            assert_eq!(error["data"]["kind"], "unavailable");
            assert_eq!(error["data"]["details"]["reason"], "catalog_drift");
            assert_eq!(error["data"]["domain_disposition"], "not_committed");
        },
    );
}

async fn parsed_host_case(multi: bool) {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    let db = dir.path().join("main.db");
    let mut contents =
        "[runtime]\npacks = ['kg', 'gtd']\n[packs.kg]\nengines = []\n[packs.gtd]\nengines = []\n"
            .to_owned();
    if multi {
        contents = format!("[runtime]\npacks = ['kg', 'gtd']\n[packs.kg]\nbackend = 'main'\nengines = []\n[packs.gtd]\nbackend = 'tasks'\nengines = []\n[[backends]]\nname = 'main'\npath = {db:?}\n[[backends]]\nname = 'tasks'\npath = {:?}\n", dir.path().join("tasks.db"));
    }
    std::fs::write(&config, contents).unwrap();
    let mut observations = Vec::new();
    for mode in [None, Some("full"), Some("compact")] {
        let mut argv = vec![
            "mcp".to_owned(),
            "--config".into(),
            config.to_str().unwrap().into(),
            "--no-embed".into(),
            "--actor".into(),
            "catalog-test".into(),
        ];
        if !multi {
            argv.extend(["--db".into(), db.to_str().unwrap().into()]);
        }
        if let Some(mode) = mode {
            argv.extend(["--catalog-verbosity".into(), mode.into()]);
        }
        let args = Args::try_parse_from(argv).expect("actual MCP argv");
        let (server, schedule) = build_server(&args).await.expect("actual configured host");
        assert!(schedule.is_none());
        let identity = server.config_id().to_owned();
        let replies = exchange(server, &[Action::List, Action::Call("verbs()")], None).await;
        observations.push((identity, replies));
    }
    assert_eq!(observations[0].0, observations[1].0);
    assert_eq!(observations[0].0, observations[2].0);
    let default = tool(&observations[0].1[0]);
    let full = tool(&observations[1].1[0]);
    let compact = tool(&observations[2].1[0]);
    assert_eq!(default, full);
    assert_eq!(
        descriptor_without_description(full),
        descriptor_without_description(compact)
    );
    assert_eq!(
        description(full).split_once(HEADING).unwrap().0,
        description(compact).split_once(HEADING).unwrap().0
    );
    assert!(!description(full).contains(COMPACT_HEADER));
    let rows = compact_rows(description(compact));
    let verbs = result(&observations[2].1[1]);
    assert_eq!(verbs, result(&observations[0].1[1]));
    assert_eq!(verbs, result(&observations[1].1[1]));
    let mut names = verbs["verbs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["verb"].as_str().unwrap())
        .collect::<Vec<_>>();
    names.sort_unstable();
    assert_eq!(
        rows.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
        names
    );
    assert_eq!(
        verbs["pack_counts"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["gtd", "kg"])
    );
}

#[test]
#[serial_test::serial(config_ledger)]
fn parsed_single_backend_host_carries_catalog_verbosity() {
    isolated(
        "parsed_single_backend_host_carries_catalog_verbosity",
        parsed_host_case(false),
    );
}

#[test]
#[serial_test::serial(config_ledger)]
fn parsed_multi_backend_host_carries_catalog_verbosity() {
    isolated(
        "parsed_multi_backend_host_carries_catalog_verbosity",
        parsed_host_case(true),
    );
}

#[test]
#[serial_test::serial(config_ledger)]
fn invalid_catalog_verbosity_is_refused_before_host_open() {
    isolated(
        "invalid_catalog_verbosity_is_refused_before_host_open",
        async {
            let dir = tempfile::tempdir().unwrap();
            let unopened = dir.path().join("must-not-open.db");
            let error = Args::try_parse_from([
                "mcp",
                "--db",
                unopened.to_str().unwrap(),
                "--catalog-verbosity",
                "terse",
            ])
            .expect_err("unknown verbosity must fail argument parsing");
            assert_eq!(error.kind(), clap::error::ErrorKind::InvalidValue);
            let diagnostic = error.to_string();
            assert!(diagnostic.contains("--catalog-verbosity"));
            assert!(diagnostic.contains("full"));
            assert!(diagnostic.contains("compact"));
            assert!(!unopened.exists());
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        },
    );
}
