//! ADR-220 acceptance arms for the bridge, end to end in one process.
//!
//! The caller is a bridge over a file-backed store whose daemon forward is a
//! spy: it records every frame and then lets the request run here, as a
//! bridge with no daemon does. Its computer is a khive server of its own (the
//! kg pack's `whoami` and an `exec` stand-in that records each run and writes
//! a sentinel file), reached over the in-process pipe. The store has the same
//! `exec` stand-in, so a command that runs on the store's machine leaves its
//! sentinel there. Each side also has a `git` stand-in with a verb of its own,
//! so a catalog shows whose verbs it lists.
//!
//! The arms at the end of the file reach the same computer over loopback TCP
//! with the production HTTP client. A socket is needed for those, so they
//! run in the hosted checks.

use std::convert::Infallible;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use http::{Request, Response, StatusCode};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Bytes, Frame, Incoming};
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use khive_runtime::pack::{HandlerDef, PackRuntime, VerbRegistry, VerbRegistryBuilder};
use khive_runtime::{
    ConfigError, DaemonRequestFrame, KhiveConfig, KhiveRuntime, Namespace, NamespaceToken,
    RuntimeConfig, RuntimeError,
};
use khive_storage::{EventFilter, PageRequest};
use khive_types::{Pack, VerbCategory, Visibility};
use rmcp::handler::server::ServerHandler;
use rmcp::model::{CallToolRequestParams, CallToolResult, ErrorData as McpError, ServerInfo};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::session::SessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::RoleServer;
use serde_json::{json, Value};
use tokio::net::TcpListener;

use super::super::bridge::ForwardFuture;
use super::super::computer_catalog::CATALOG_READ_TIMEOUT;
use super::super::computer_client::{Connection, Connector, HttpConnector, COMPUTER_UNREACHABLE};
use super::super::computer_http::{MAX_ERROR_BODY, MAX_EVENT, MAX_JSON_BODY};
use super::super::computer_pipe_tests::{PipeComputer, PipeConnector, URL};
use super::super::KhiveMcpServer;
use super::ComputerBridge;
use crate::tools::request::RequestParams;

const ACTOR: &str = "lambda:test-caller";
// Assembled from parts so that no literal in the source reads as a credential.
const KEY: &str = concat!("computer-", "fixture-", "token");
const LABEL: &str = "build-1 (localhost:80)";
const SENTINEL: &str = "exec-ran";
const RUN: &str = r#"exec.run(tool="sh", args=["-c", "true"])"#;
const REFUSED_RUN: &str = r#"exec.run(tool="sh", fail=true)"#;
const CREATE: &str = r#"create(kind="entity", entity_kind="concept", name="must-not-exist")"#;
const SCHEDULED_COMMAND: &str =
    r#"schedule.schedule(action="exec.run(tool=\"sh\")", at="2030-01-01T00:00:00Z")"#;
const SCHEDULED_WRITE: &str = r#"schedule.schedule(action="create(kind=\"entity\", entity_kind=\"concept\", name=\"later\")", at="2030-01-01T00:00:00Z")"#;

// ---- the exec stand-in ----

const fn exec_handler(name: &'static str) -> HandlerDef {
    HandlerDef {
        name,
        description: "exec stand-in for the ADR-220 arms",
        visibility: Visibility::Verb,
        category: VerbCategory::Directive,
        params: &[],
    }
}

const EXEC_HANDLERS: &[HandlerDef] = &[exec_handler("exec.run"), exec_handler("exec.runs")];

/// The runs one machine's `exec` stand-in recorded.
#[derive(Clone, Default)]
struct Runs(Arc<StdMutex<Vec<Value>>>);

impl Runs {
    fn all(&self) -> Vec<Value> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// Records each `exec.run` with the actor its token carries, writes a
/// sentinel file under `root`, and lists the runs for `exec.runs`. A run with
/// `fail=true` is refused and records nothing.
struct ExecStandIn {
    root: PathBuf,
    runs: Runs,
}

impl Pack for ExecStandIn {
    const NAME: &'static str = "exec";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = EXEC_HANDLERS;
}

#[async_trait]
impl PackRuntime for ExecStandIn {
    fn name(&self) -> &str {
        "exec"
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        &[]
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        &[]
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        EXEC_HANDLERS
    }

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        _registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        match verb {
            "exec.run" => {
                if params.get("fail").and_then(Value::as_bool) == Some(true) {
                    return Err(RuntimeError::InvalidInput(
                        "the exec stand-in refuses this run".into(),
                    ));
                }
                let receipt = json!({ "actor": token.actor().id, "args": params });
                std::fs::write(self.root.join(SENTINEL), b"ran")
                    .map_err(|error| RuntimeError::Internal(error.to_string()))?;
                self.runs
                    .0
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(receipt.clone());
                Ok(json!({ "receipt": receipt }))
            }
            "exec.runs" => Ok(json!({ "runs": self.runs.all() })),
            other => Err(RuntimeError::InvalidInput(format!("unknown verb {other}"))),
        }
    }
}

// ---- the git stand-in ----

const STORE_GIT_DESCRIPTION: &str = "git stand-in on the store's machine";
const COMPUTER_GIT_DESCRIPTION: &str = "git stand-in on the computer";

const fn git_handler(name: &'static str, description: &'static str) -> HandlerDef {
    HandlerDef {
        name,
        description,
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[],
    }
}

const STORE_GIT: &[HandlerDef] = &[git_handler("git.status", STORE_GIT_DESCRIPTION)];
const COMPUTER_GIT: &[HandlerDef] = &[git_handler("git.log", COMPUTER_GIT_DESCRIPTION)];

/// A `git` pack whose one verb differs by machine.
struct GitStandIn<const ON_COMPUTER: bool>;

impl<const ON_COMPUTER: bool> Pack for GitStandIn<ON_COMPUTER> {
    const NAME: &'static str = "git";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = if ON_COMPUTER { COMPUTER_GIT } else { STORE_GIT };
}

#[async_trait]
impl<const ON_COMPUTER: bool> PackRuntime for GitStandIn<ON_COMPUTER> {
    fn name(&self) -> &str {
        "git"
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        &[]
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        &[]
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        <Self as Pack>::HANDLERS
    }

    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Ok(json!({ "verb": verb }))
    }
}

// ---- the spy daemon forward ----

static FRAMES: StdMutex<Vec<String>> = StdMutex::new(Vec::new());

/// Records the frame a bridge would send its daemon, then answers as a bridge
/// with no daemon: no outcome, so the request runs on this process's store.
fn spy_forward(
    frame: DaemonRequestFrame,
    _config: Option<PathBuf>,
    _db: Option<String>,
    _packs: Option<Vec<String>>,
    _replay_read_only: bool,
) -> ForwardFuture {
    FRAMES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(frame.ops);
    Box::pin(async { None })
}

fn frames() -> Vec<String> {
    FRAMES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

fn reset_frames() {
    FRAMES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
}

// ---- the computer ----

/// A khive server that records the `ops` of every call it is sent.
#[derive(Clone)]
struct Recording {
    inner: KhiveMcpServer,
    calls: Arc<StdMutex<Vec<String>>>,
}

impl ServerHandler for Recording {
    fn get_info(&self) -> ServerInfo {
        self.inner.get_info()
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let ops = request
            .arguments
            .as_ref()
            .and_then(|arguments| arguments.get("ops"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(ops);
        self.inner.call_tool(request, context).await
    }
}

struct TestComputer {
    dir: tempfile::TempDir,
    handler: Recording,
    pipe: Arc<PipeComputer>,
    sessions: Arc<LocalSessionManager>,
    runs: Runs,
    calls: Arc<StdMutex<Vec<String>>>,
}

impl TestComputer {
    /// A computer whose `whoami` answers for `actor`.
    fn start(actor: &str) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        write_key(&dir.path().join("computer.key"), 0o600);
        let root = dir.path().join("computer-machine");
        std::fs::create_dir(&root).expect("the computer's root");
        let runtime = KhiveRuntime::memory().expect("the computer's runtime");
        let runs = Runs::default();
        let mut builder = VerbRegistryBuilder::new();
        builder.with_actor_id(Some(actor.into()));
        builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
        builder.register_trusted(ExecStandIn {
            root,
            runs: runs.clone(),
        });
        builder.register_trusted(GitStandIn::<true>);
        let server = KhiveMcpServer::from_registry(builder.build().expect("computer registry"))
            .with_runtime(runtime);
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let sessions = Arc::new(LocalSessionManager::default());
        let handler = Recording {
            inner: server,
            calls: Arc::clone(&calls),
        };
        let pipe = PipeComputer::serve(handler.clone(), Arc::clone(&sessions));
        Self {
            dir,
            handler,
            pipe,
            sessions,
            runs,
            calls,
        }
    }

    fn key_file(&self) -> PathBuf {
        self.dir.path().join("computer.key")
    }

    /// A caller configuration naming this computer for `actor`, for `exec`.
    fn config(&self, actor: &str) -> KhiveConfig {
        self.config_for_packs(actor, &["exec"])
    }

    /// The same configuration with the computer at `url`.
    fn config_at(&self, actor: &str, url: &str) -> KhiveConfig {
        self.config_with(actor, url, &["exec"], 30)
    }

    /// The same configuration with a call timeout of `call_timeout_s`.
    fn config_with_timeout(&self, actor: &str, url: &str, call_timeout_s: u64) -> KhiveConfig {
        self.config_with(actor, url, &["exec"], call_timeout_s)
    }

    /// A caller configuration naming this computer for `actor` and `packs`.
    fn config_for_packs(&self, actor: &str, packs: &[&str]) -> KhiveConfig {
        self.config_with(actor, URL, packs, 30)
    }

    fn config_with(
        &self,
        actor: &str,
        url: &str,
        packs: &[&str],
        call_timeout_s: u64,
    ) -> KhiveConfig {
        let packs = packs
            .iter()
            .map(|pack| format!("\"{pack}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let text = format!(
            "[actor]\nid = \"{actor}\"\ncomputer = \"build-1\"\n\n[computer]\nname = \"build-1\"\n\
             url = \"{url}\"\nkey_file = \"{}\"\npacks = [{packs}]\n\
             call_timeout_s = {call_timeout_s}\n",
            self.key_file().display()
        );
        toml::from_str(&text).expect("a computer configuration")
    }

    fn calls(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn sentinel(&self) -> bool {
        self.dir
            .path()
            .join("computer-machine")
            .join(SENTINEL)
            .exists()
    }

    /// Forget every session, as a restarted computer does: the next request
    /// on one of them is answered HTTP 404.
    async fn expire_sessions(&self) {
        let ids: Vec<_> = self
            .sessions
            .sessions
            .read()
            .await
            .keys()
            .cloned()
            .collect();
        assert!(!ids.is_empty(), "a session was open before it was expired");
        for id in ids {
            self.sessions
                .close_session(&id)
                .await
                .expect("close the session");
        }
    }
}

impl Drop for TestComputer {
    fn drop(&mut self) {
        self.pipe.stop();
    }
}

fn write_key(path: &Path, mode: u32) {
    std::fs::write(path, format!("{KEY}\n")).expect("write the key file");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

// ---- the caller ----

struct Caller {
    dir: tempfile::TempDir,
    server: KhiveMcpServer,
    runtime: KhiveRuntime,
    runs: Runs,
}

impl Caller {
    /// A bridge over a file-backed store, with no computer yet.
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("store-machine");
        std::fs::create_dir(&root).expect("the store's root");
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(dir.path().join("main.db")),
            actor_id: Some(ACTOR.to_string()),
            embedding_model: None,
            additional_embedding_models: vec![],
            events_split: None,
            packs: vec!["kg".to_string()],
            ..RuntimeConfig::default()
        })
        .expect("the store's runtime");
        let runs = Runs::default();
        let mut builder = VerbRegistryBuilder::new();
        builder.with_actor_id(Some(ACTOR.into()));
        builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
        builder.register(khive_pack_schedule::SchedulePack::new(runtime.clone()));
        builder.register_trusted(ExecStandIn {
            root,
            runs: runs.clone(),
        });
        builder.register_trusted(GitStandIn::<false>);
        builder
            .with_runtime_event_store(&runtime)
            .expect("the store's event store");
        let server = KhiveMcpServer::from_registry(builder.build().expect("store registry"))
            .with_runtime(runtime.clone());
        Self {
            dir,
            server,
            runtime,
            runs,
        }
    }

    /// Attach `computer` as the caller's computer, configured for this caller.
    fn routing_to(self, computer: &TestComputer) -> Self {
        self.routing_packs_to(computer, &["exec"])
    }

    /// Attach `computer` as the caller's computer, serving it `packs`.
    fn routing_packs_to(mut self, computer: &TestComputer, packs: &[&str]) -> Self {
        let connector = Arc::new(PipeConnector {
            computer: Arc::clone(&computer.pipe),
        });
        let config = computer.config_for_packs(ACTOR, packs);
        self.server.computer = ComputerBridge::from_config(&config, connector)
            .expect("a valid computer table")
            .map(Arc::new);
        assert!(self.server.computer.is_some());
        self
    }

    /// Attach the computer `config` names, reached with the production HTTP
    /// client.
    fn routing_over_http(mut self, config: &KhiveConfig) -> Self {
        self.server.computer = ComputerBridge::from_config(config, Arc::new(HttpConnector))
            .expect("a valid computer table")
            .map(Arc::new);
        assert!(self.server.computer.is_some());
        self
    }

    fn sentinel(&self) -> bool {
        self.dir
            .path()
            .join("store-machine")
            .join(SENTINEL)
            .exists()
    }

    async fn text(&self, ops: &str) -> String {
        self.server
            .request_with_forward(
                RequestParams {
                    ops: ops.into(),
                    ..Default::default()
                },
                spy_forward,
            )
            .await
            .expect("the request is answered")
    }

    async fn request(&self, ops: &str) -> Value {
        let text = self.text(ops).await;
        serde_json::from_str(&text).expect("the khive envelope")
    }

    async fn entities(&self) -> Value {
        let stats = self.request("stats()").await;
        stats["results"][0]["result"]["entities"].clone()
    }

    /// The audit rows the store holds for `verb`, after the batch drains.
    async fn rows(&self, verb: &str) -> usize {
        self.server
            .shutdown_audit_batch()
            .await
            .expect("drain the audit batch");
        let token = self
            .runtime
            .authorize(Namespace::local())
            .expect("a store token");
        self.runtime
            .events(&token)
            .expect("the store's events")
            .query_events(
                EventFilter {
                    verbs: vec![verb.into()],
                    ..Default::default()
                },
                PageRequest {
                    limit: 100,
                    offset: 0,
                },
            )
            .await
            .expect("read the audit rows")
            .items
            .len()
    }
}

fn assert_failed(envelope: &Value, index: usize, kind: &str) {
    let entry = &envelope["results"][index];
    assert_eq!(entry["ok"], false, "{envelope}");
    assert_eq!(entry["error"]["kind"], kind, "{envelope}");
    assert_eq!(entry["domain_disposition"], "not_committed", "{envelope}");
    let message = entry["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("build-1"), "{envelope}");
}

// ---- the arms ----

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn arm_1_one_tool_runs_a_command_on_the_computer_and_reads_its_receipt() {
    reset_frames();
    let computer = TestComputer::start(ACTOR);
    let caller = Caller::new().routing_to(&computer);

    let run = caller.request(RUN).await;
    assert_eq!(run["results"][0]["ok"], true, "{run}");
    assert_eq!(
        run["results"][0]["result"]["receipt"]["actor"], ACTOR,
        "{run}"
    );
    let runs = caller.request("exec.runs()").await;
    let listed = runs["results"][0]["result"]["runs"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(listed.len(), 1, "{runs}");
    assert_eq!(
        listed[0]["actor"], ACTOR,
        "the receipt's actor is [actor].id"
    );

    assert_eq!(computer.calls(), ["whoami()", RUN, "exec.runs()"]);
    assert!(computer.sentinel());
    assert!(caller.runs.all().is_empty());
    assert!(!caller.sentinel());
    assert!(frames().is_empty(), "{:?}", frames());
}

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn arm_2_a_computer_that_is_down_is_unreachable_and_runs_nothing_after_it_returns() {
    reset_frames();
    let computer = TestComputer::start(ACTOR);
    let caller = Caller::new().routing_to(&computer);

    computer.pipe.stop();
    let refused = caller.request(RUN).await;
    assert_failed(&refused, 0, "computer_unreachable");

    computer.pipe.restart();
    let runs = caller.request("exec.runs()").await;
    assert_eq!(runs["results"][0]["ok"], true, "{runs}");
    // The agent presentation leaves an empty list out of the result.
    let listed = &runs["results"][0]["result"]["runs"];
    assert!(listed.is_null() || listed == &json!([]), "{runs}");
    assert!(computer.runs.all().is_empty());
    assert_eq!(computer.calls(), ["whoami()", "exec.runs()"]);
    assert!(frames().is_empty(), "{:?}", frames());
}

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn arm_3_nothing_runs_on_the_store_machine() {
    reset_frames();
    let computer = TestComputer::start(ACTOR);
    let caller = Caller::new().routing_to(&computer);

    let run = caller.request(RUN).await;
    assert_eq!(run["results"][0]["ok"], true, "{run}");

    assert!(
        frames().is_empty(),
        "no frame for the daemon: {:?}",
        frames()
    );
    assert!(
        !caller.sentinel(),
        "no sentinel file on the store's machine"
    );
    assert!(caller.runs.all().is_empty());
    assert_eq!(caller.rows("exec.run").await, 0, "no dispatch row");
    assert!(computer.sentinel());
}

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn arm_4_with_routing_off_the_store_runs_the_command_and_every_instrument_sees_it() {
    reset_frames();
    let computer = TestComputer::start(ACTOR);
    let mut caller = Caller::new().routing_to(&computer);
    // The daemon's own server: the computer is dropped before it serves.
    caller.server = caller.server.without_computer();
    assert!(caller.server.computer.is_none());

    let run = caller.request(RUN).await;
    assert_eq!(run["results"][0]["ok"], true, "{run}");

    assert_eq!(frames().len(), 1, "{:?}", frames());
    assert!(frames()[0].contains("exec.run"), "{:?}", frames());
    assert!(caller.sentinel());
    assert_eq!(caller.runs.all().len(), 1);
    assert!(
        caller.rows("exec.run").await >= 1,
        "the dispatch row is read"
    );
    assert!(computer.calls().is_empty());
    assert!(!computer.sentinel());
}

/// Every byte the log writer is handed.
#[derive(Clone, Default)]
struct Capture(Arc<StdMutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn arm_5_the_key_never_surfaces_in_answers_logs_diagnostics_or_frames() {
    reset_frames();
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let computer = TestComputer::start(ACTOR);
    let mut caller = Caller::new().routing_to(&computer);
    caller.server.stdio_bridge = true;
    let mut answers = vec![caller.text(RUN).await];
    computer.expire_sessions().await;
    answers.push(caller.text("exec.runs()").await);
    answers.push(caller.text(&format!("[{REFUSED_RUN}, stats()]")).await);
    computer.pipe.stop();
    answers.push(caller.text(RUN).await);
    let diagnostics = caller.text("bridge.diagnostics()").await;
    let read: Value = serde_json::from_str(&diagnostics).expect("the diagnostics envelope");
    assert_eq!(
        read["results"][0]["result"]["computer"],
        json!({ "label": LABEL, "routed_requests": 4 }),
        "{read}"
    );
    answers.push(diagnostics);

    let authorization = computer.pipe.authorization();
    assert!(!authorization.is_empty(), "the key reached the computer");
    assert!(authorization
        .iter()
        .all(|value| value == &format!("Bearer {KEY}")));
    let log = String::from_utf8(
        capture
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone(),
    )
    .expect("a UTF-8 log");
    assert!(!log.is_empty(), "the log writer recorded the requests");
    assert!(
        !frames().is_empty(),
        "the store part reached the daemon seam"
    );
    for answer in &answers {
        assert!(!answer.contains(KEY), "{answer}");
    }
    assert!(!log.contains(KEY), "the key is in the log");
    for frame in frames() {
        assert!(!frame.contains(KEY), "{frame}");
    }
}

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn arm_6_a_computer_acting_for_another_actor_is_refused_and_runs_nothing() {
    reset_frames();
    let computer = TestComputer::start("lambda:someone-else");
    let caller = Caller::new().routing_to(&computer);

    let refused = caller.request(RUN).await;
    assert_failed(&refused, 0, "computer_identity_mismatch");

    assert!(computer.runs.all().is_empty(), "the run list is unchanged");
    assert!(!computer.sentinel());
    assert_eq!(computer.calls(), ["whoami()"]);
    assert!(caller.runs.all().is_empty());
    assert!(frames().is_empty(), "{:?}", frames());
}

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn arm_7_a_command_pack_the_computer_does_not_serve_is_refused_here() {
    reset_frames();
    let computer = TestComputer::start(ACTOR);
    let caller = Caller::new().routing_to(&computer);

    let refused = caller.request("git.status()").await;
    assert_failed(&refused, 0, "runs_on_computer");

    assert!(frames().is_empty(), "{:?}", frames());
    assert!(computer.calls().is_empty());
}

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn arm_8_without_a_computer_table_exec_runs_on_the_store() {
    reset_frames();
    let config: KhiveConfig =
        toml::from_str(&format!("[actor]\nid = \"{ACTOR}\"\n")).expect("a configuration");
    let mut caller = Caller::new();
    caller.server = caller
        .server
        .with_computer(&config)
        .expect("no table is a valid configuration");
    assert!(caller.server.computer.is_none());

    let run = caller.request(RUN).await;
    assert_eq!(run["results"][0]["ok"], true, "{run}");
    assert_eq!(caller.runs.all().len(), 1);
    assert!(caller.sentinel());
    assert_eq!(frames().len(), 1, "{:?}", frames());
}

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn arm_9_a_failed_computer_operation_aborts_the_store_write_after_it() {
    reset_frames();
    let computer = TestComputer::start(ACTOR);
    let caller = Caller::new().routing_to(&computer);
    computer.pipe.stop();

    let chain = caller.request(&format!("{RUN} | {CREATE}")).await;
    assert_failed(&chain, 0, "computer_unreachable");
    assert_eq!(chain["results"][1]["ok"], false, "{chain}");
    assert_eq!(chain["results"][1]["aborted"], true, "{chain}");
    assert_eq!(caller.entities().await, 0, "no row was written");

    // The count can see a row: the same write on its own lands.
    let created = caller.request(CREATE).await;
    assert_eq!(created["results"][0]["ok"], true, "{created}");
    assert_eq!(caller.entities().await, 1);
}

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn arm_9_a_store_result_reaches_the_computer_resolved() {
    reset_frames();
    let computer = TestComputer::start(ACTOR);
    let caller = Caller::new().routing_to(&computer);

    let chain = caller
        .request(r#"whoami() | exec.run(tool="sh", note=$prev.actor_id)"#)
        .await;
    assert_eq!(chain["results"][0]["ok"], true, "{chain}");
    assert_eq!(chain["results"][1]["ok"], true, "{chain}");

    let runs = computer.runs.all();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["args"]["note"], ACTOR, "{runs:?}");
    let sent = computer.calls();
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert!(!sent[1].contains("$prev"), "{sent:?}");
    assert_eq!(frames().len(), 1, "the whoami part went to the store");
}

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn arm_9_a_refusal_from_the_computer_is_the_same_as_a_direct_call() {
    reset_frames();
    let computer = TestComputer::start(ACTOR);
    let caller = Caller::new().routing_to(&computer);

    let direct = caller.request(REFUSED_RUN).await;
    assert_eq!(direct["results"][0]["ok"], false, "{direct}");
    let mixed = caller.request(&format!("[{REFUSED_RUN}, stats()]")).await;
    assert_eq!(mixed["results"][1]["ok"], true, "{mixed}");

    assert_eq!(
        serde_json::to_string(&mixed["results"][0]).expect("entry"),
        serde_json::to_string(&direct["results"][0]).expect("entry"),
    );
    // Both refusals came from the computer; the split writes its part back
    // as a one-member batch.
    let sent = computer.calls();
    assert_eq!(sent.len(), 3, "{sent:?}");
    assert_eq!(sent[1], REFUSED_RUN);
    assert!(
        sent[2].contains("exec.run(") && sent[2].contains("fail=true"),
        "{sent:?}"
    );
    assert!(caller.runs.all().is_empty());
}

#[tokio::test]
async fn arm_11_a_computer_table_without_the_actor_id_is_refused_at_startup() {
    let computer = TestComputer::start(ACTOR);
    let text = format!(
        "[actor]\ncomputer = \"build-1\"\n\n[computer]\nname = \"build-1\"\nurl = \"{URL}\"\n\
         key_file = \"{}\"\npacks = [\"exec\"]\n",
        computer.key_file().display()
    );
    let config: KhiveConfig = toml::from_str(&text).expect("a configuration");
    let server =
        KhiveMcpServer::from_registry(VerbRegistryBuilder::new().build().expect("registry"));

    let Err(error) = server.with_computer(&config) else {
        panic!("a computer table without [actor].id must be refused");
    };
    assert!(
        matches!(error, ConfigError::InvalidActorComputer { .. }),
        "{error:?}"
    );
    assert!(error.to_string().contains("[actor].id"), "{error}");
}

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn arm_11_a_key_open_to_others_is_refused_before_the_computer_is_reached() {
    reset_frames();
    let computer = TestComputer::start(ACTOR);
    write_key(&computer.key_file(), 0o644);
    let caller = Caller::new().routing_to(&computer);

    let refused = caller.request(RUN).await;
    assert_eq!(
        refused["results"][0]["error"]["kind"], "computer_key_unavailable",
        "{refused}"
    );
    assert_eq!(refused["results"][0]["domain_disposition"], "not_committed");
    assert!(computer.calls().is_empty());
    assert!(computer.pipe.authorization().is_empty());
    assert!(frames().is_empty(), "{:?}", frames());
}

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn arm_12_one_run_across_a_session_expiry_with_whoami_first_on_the_new_session() {
    reset_frames();
    let computer = TestComputer::start(ACTOR);
    let caller = Caller::new().routing_to(&computer);
    let runs = caller.request("exec.runs()").await;
    assert_eq!(runs["results"][0]["ok"], true, "{runs}");

    computer.expire_sessions().await;
    let run = caller.request(RUN).await;
    assert_eq!(run["results"][0]["ok"], true, "{run}");

    assert_eq!(
        computer.calls(),
        ["whoami()", "exec.runs()", "whoami()", RUN],
        "the run was sent once, after whoami on the new session"
    );
    assert_eq!(computer.runs.all().len(), 1);
}

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn arm_13_a_scheduled_command_is_refused_and_nothing_is_stored() {
    reset_frames();
    let computer = TestComputer::start(ACTOR);
    let caller = Caller::new().routing_to(&computer);

    let refused = caller.request(SCHEDULED_COMMAND).await;
    assert_failed(&refused, 0, "runs_on_computer");
    assert!(frames().is_empty(), "{:?}", frames());
    assert!(computer.calls().is_empty());
}

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn arm_13_a_scheduled_store_action_is_stored_as_before() {
    reset_frames();
    let computer = TestComputer::start(ACTOR);
    let caller = Caller::new().routing_to(&computer);

    let stored = caller.request(SCHEDULED_WRITE).await;
    assert_eq!(stored["results"][0]["ok"], true, "{stored}");
    assert_eq!(frames(), [SCHEDULED_WRITE], "sent to the store unchanged");
    assert!(computer.calls().is_empty());
}

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn a_plan_names_the_home_of_each_stage_and_sends_nothing() {
    reset_frames();
    let computer = TestComputer::start(ACTOR);
    let caller = Caller::new().routing_to(&computer);

    let text = caller
        .server
        .request_with_forward(
            RequestParams {
                ops: format!("[{RUN}, stats(), git.status()]"),
                plan: Some(true),
                ..Default::default()
            },
            spy_forward,
        )
        .await
        .expect("the plan");
    let plan: Value = serde_json::from_str(&text).expect("the plan document");
    let stages = &plan["stages"];
    assert_eq!(stages[0]["served_by"], "computer", "{plan}");
    assert_eq!(stages[1]["served_by"], "store", "{plan}");
    assert_eq!(stages[2]["served_by"], Value::Null, "{plan}");
    assert_eq!(stages[2]["refused"], "runs_on_computer", "{plan}");
    assert!(computer.calls().is_empty());
    assert!(frames().is_empty(), "{:?}", frames());
}

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn save_to_is_refused_for_a_request_with_an_operation_for_the_computer() {
    reset_frames();
    let computer = TestComputer::start(ACTOR);
    let caller = Caller::new().routing_to(&computer);
    let sink = caller.dir.path().join("routed.jsonl");

    let error = caller
        .server
        .request_with_forward(
            RequestParams {
                ops: RUN.into(),
                save_to: Some(sink.display().to_string()),
                ..Default::default()
            },
            spy_forward,
        )
        .await
        .expect_err("save_to is refused");
    assert_eq!(
        error.code,
        rmcp::model::ErrorCode::INVALID_PARAMS,
        "{error:?}"
    );
    assert!(error.message.contains("nothing was run"), "{error:?}");
    assert!(!sink.exists());
    assert!(computer.calls().is_empty());
    assert!(frames().is_empty(), "{:?}", frames());
}

#[tokio::test]
async fn the_deadline_of_a_request_for_the_computer_is_its_call_timeout() {
    let computer = TestComputer::start(ACTOR);
    let bridge = ComputerBridge::from_config(
        &computer.config(ACTOR),
        Arc::new(PipeConnector {
            computer: Arc::clone(&computer.pipe),
        }),
    )
    .expect("a valid table")
    .expect("a computer");

    assert_eq!(bridge.deadline_for(RUN), Some(Duration::from_secs(30)));
    assert_eq!(
        bridge.deadline_for(&format!("stats() | {RUN}")),
        Some(Duration::from_secs(30))
    );
    assert_eq!(
        bridge.deadline_for("verbs()"),
        Some(Duration::from_secs(30))
    );
    assert_eq!(bridge.deadline_for("stats()"), None);
    assert_eq!(bridge.deadline_for("git.status()"), None);
    assert_eq!(bridge.label(), LABEL);
}

/// The verb names of the first answer, a `verbs` answer, in order.
fn verb_names(envelope: &Value) -> Vec<String> {
    envelope["results"][0]["result"]["verbs"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|row| row["verb"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn arm_10_verbs_for_a_routed_pack_lists_exactly_the_computers_verbs() {
    reset_frames();
    let computer = TestComputer::start(ACTOR);
    let caller = Caller::new().routing_packs_to(&computer, &["exec", "git"]);

    let listed = caller.request(r#"verbs(pack="git")"#).await;
    assert_eq!(verb_names(&listed), ["git.log"], "{listed}");
    let row = &listed["results"][0]["result"]["verbs"][0];
    assert_eq!(row["served_by"], "computer", "{listed}");
    assert_eq!(row["computer"], "build-1", "{listed}");
    assert_eq!(row["description"], COMPUTER_GIT_DESCRIPTION, "{listed}");
    assert_eq!(
        listed["results"][0]["result"]["pack_counts"]["git"], 1,
        "{listed}"
    );
    assert_eq!(computer.calls(), ["whoami()", "verbs()"]);
}

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn arm_10_without_a_computer_verbs_lists_the_stores_verbs() {
    reset_frames();
    let caller = Caller::new();

    let listed = caller.request(r#"verbs(pack="git")"#).await;
    assert_eq!(verb_names(&listed), ["git.status"], "{listed}");
    let row = &listed["results"][0]["result"]["verbs"][0];
    assert_eq!(row["description"], STORE_GIT_DESCRIPTION, "{listed}");
    assert!(row.get("served_by").is_none(), "{listed}");
}

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn arm_10_every_row_names_its_home_and_a_routed_pack_is_the_computers_alone() {
    reset_frames();
    let computer = TestComputer::start(ACTOR);
    let caller = Caller::new().routing_packs_to(&computer, &["exec", "git"]);

    let listed = caller.request("verbs()").await;
    let rows = listed["results"][0]["result"]["verbs"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(!rows.is_empty(), "{listed}");
    for row in &rows {
        let routed = matches!(row["pack"].as_str(), Some("exec" | "git"));
        let home = if routed { "computer" } else { "store" };
        assert_eq!(row["served_by"], home, "{row}");
    }
    let names = verb_names(&listed);
    assert!(names.iter().any(|name| name == "whoami"), "{listed}");
    assert!(names.iter().any(|name| name == "git.log"), "{listed}");
    assert!(!names.iter().any(|name| name == "git.status"), "{listed}");
    let exec = names
        .iter()
        .filter(|name| name.starts_with("exec."))
        .count();
    assert_eq!(
        exec, 2,
        "the computer's two exec verbs, not a union: {listed}"
    );
}

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn arm_10_help_for_a_routed_verb_is_answered_by_the_computer() {
    reset_frames();
    let computer = TestComputer::start(ACTOR);
    let caller = Caller::new().routing_packs_to(&computer, &["exec", "git"]);

    let help = caller.request("git.log(help=true)").await;
    assert_eq!(help["results"][0]["ok"], true, "{help}");
    assert!(
        help.to_string().contains(COMPUTER_GIT_DESCRIPTION),
        "{help}"
    );
    assert_eq!(computer.calls(), ["whoami()", "git.log(help=true)"]);
    assert!(frames().is_empty(), "{:?}", frames());
}

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn arm_10_the_tool_description_lists_the_computers_verbs_read_once() {
    reset_frames();
    let computer = TestComputer::start(ACTOR);
    let caller = Caller::new().routing_packs_to(&computer, &["exec", "git"]);

    let catalog = caller.server.request_tool_catalog().await;
    assert!(catalog.contains("git.log"), "{catalog}");
    assert!(catalog.contains(COMPUTER_GIT_DESCRIPTION), "{catalog}");
    assert!(!catalog.contains("git.status"), "{catalog}");
    assert!(catalog.contains("whoami"), "{catalog}");
    assert_eq!(caller.server.request_tool_catalog().await, catalog);
    assert_eq!(computer.calls(), ["whoami()", "verbs()"]);

    let without = Caller::new().server.request_tool_catalog().await;
    assert!(without.contains("git.status"), "{without}");
    assert!(!without.contains("git.log"), "{without}");
}

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn arm_10_a_computer_down_at_start_is_named_in_the_description_and_verbs_reads_it_again() {
    reset_frames();
    let computer = TestComputer::start(ACTOR);
    let caller = Caller::new().routing_packs_to(&computer, &["exec", "git"]);
    computer.pipe.stop();

    let catalog = caller.server.request_tool_catalog().await;
    assert!(catalog.contains("git.*"), "{catalog}");
    assert!(catalog.contains("could not be read"), "{catalog}");
    assert!(catalog.contains(LABEL), "{catalog}");
    assert!(!catalog.contains("git.status"), "{catalog}");

    let listed = caller.request(r#"verbs(pack="git")"#).await;
    assert_failed(&listed, 0, COMPUTER_UNREACHABLE);
    assert_eq!(listed["status"], "partial", "{listed}");

    computer.pipe.restart();
    let listed = caller.request(r#"verbs(pack="git")"#).await;
    assert_eq!(verb_names(&listed), ["git.log"], "{listed}");
}

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn arm_10_save_to_is_refused_for_verbs_while_a_computer_serves_part_of_the_catalog() {
    reset_frames();
    let computer = TestComputer::start(ACTOR);
    let caller = Caller::new().routing_packs_to(&computer, &["exec", "git"]);
    let sink = caller.dir.path().join("catalog.jsonl");

    let error = caller
        .server
        .request_with_forward(
            RequestParams {
                ops: "verbs()".into(),
                save_to: Some(sink.display().to_string()),
                ..Default::default()
            },
            spy_forward,
        )
        .await
        .expect_err("save_to is refused");
    assert!(error.message.contains("nothing was run"), "{error:?}");
    assert!(!sink.exists());
    assert!(computer.calls().is_empty());
}

/// A computer that accepts the connection and never answers: the session it
/// is asked to open never completes.
struct SilentConnector;

#[async_trait]
impl Connector for SilentConnector {
    async fn connect(
        &self,
        _transport: StreamableHttpClientTransportConfig,
    ) -> Result<Connection, String> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn a_catalog_read_is_bounded_by_the_smaller_of_the_call_timeout_and_its_own() {
    let computer = TestComputer::start(ACTOR);
    let bridge = |call_timeout_s| {
        ComputerBridge::from_config(
            &computer.config_with(ACTOR, URL, &["exec"], call_timeout_s),
            Arc::new(SilentConnector),
        )
        .expect("a valid table")
        .expect("a computer")
    };

    assert_eq!(bridge(3700).catalog_timeout, CATALOG_READ_TIMEOUT);
    assert_eq!(bridge(3).catalog_timeout, Duration::from_secs(3));
}

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn arm_10_a_computer_that_accepts_and_never_answers_costs_only_the_routed_catalog() {
    reset_frames();
    let computer = TestComputer::start(ACTOR);
    let mut caller = Caller::new();
    let config = computer.config_with(ACTOR, URL, &["exec", "git"], 3700);
    caller.server.computer = ComputerBridge::from_config(&config, Arc::new(SilentConnector))
        .expect("a valid computer table")
        .map(|bridge| Arc::new(bridge.with_catalog_timeout(Duration::from_secs(1))));
    let bound = Duration::from_secs(10);

    let started = std::time::Instant::now();
    let catalog = caller.server.request_tool_catalog().await;
    assert!(started.elapsed() < bound, "{:?}", started.elapsed());
    assert!(catalog.contains("git.*"), "{catalog}");
    assert!(catalog.contains("could not be read"), "{catalog}");
    assert!(catalog.contains(LABEL), "{catalog}");
    assert!(catalog.contains("whoami"), "{catalog}");

    let started = std::time::Instant::now();
    let listed = caller.request(r#"verbs(pack="git")"#).await;
    assert!(started.elapsed() < bound, "{:?}", started.elapsed());
    assert_failed(&listed, 0, COMPUTER_UNREACHABLE);
    let message = listed["results"][0]["error"]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(message.contains("within 1 s"), "{listed}");

    let started = std::time::Instant::now();
    let store = caller.request("whoami()").await;
    assert!(started.elapsed() < bound, "{:?}", started.elapsed());
    assert_eq!(store["results"][0]["ok"], true, "{store}");
    assert_eq!(store["results"][0]["result"]["actor_id"], ACTOR, "{store}");
    assert!(computer.calls().is_empty());
}

// ---- over loopback TCP, with the production HTTP client ----

/// One HTTP/1.1 listener on a loopback port that counts the requests it
/// receives.
struct Loopback {
    url: String,
    requests: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Loopback {
    async fn bind() -> (TcpListener, String) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let address = listener.local_addr().expect("the loopback address");
        (listener, format!("http://{address}/mcp"))
    }

    /// `computer`'s handler and sessions, served by rmcp's streamable HTTP
    /// service.
    async fn computer(computer: &TestComputer) -> Self {
        let (listener, url) = Self::bind().await;
        let requests = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&requests);
        let handler = computer.handler.clone();
        let service = StreamableHttpService::new(
            move || Ok(handler.clone()),
            Arc::clone(&computer.sessions),
            StreamableHttpServerConfig::default(),
        );
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let service = TowerToHyperService::new(service.clone());
                let counted = Arc::clone(&counted);
                let counting = hyper::service::service_fn(move |request: Request<Incoming>| {
                    counted.fetch_add(1, Ordering::SeqCst);
                    hyper::service::Service::call(&service, request)
                });
                tokio::spawn(async move {
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), counting)
                        .await;
                });
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }

    /// Answers every request with a temporary redirect to `location`.
    async fn redirecting_to(location: &str) -> Self {
        let (listener, url) = Self::bind().await;
        let requests = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&requests);
        let location = location.to_owned();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let counted = Arc::clone(&counted);
                let location = location.clone();
                let redirect = hyper::service::service_fn(move |_request: Request<Incoming>| {
                    counted.fetch_add(1, Ordering::SeqCst);
                    let response = Response::builder()
                        .status(StatusCode::TEMPORARY_REDIRECT)
                        .header(http::header::LOCATION, location.as_str())
                        .body(Full::new(Bytes::new()));
                    async move { response }
                });
                tokio::spawn(async move {
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), redirect)
                        .await;
                });
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }

    /// `computer` served as by [`Loopback::computer`], except that the
    /// `tools/call` of [`RUN`] never reaches it: `answer` answers it, given
    /// its JSON-RPC id.
    async fn intercepting(computer: &TestComputer, answer: Answer) -> Self {
        let (listener, url) = Self::bind().await;
        let requests = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&requests);
        let handler = computer.handler.clone();
        let service = StreamableHttpService::new(
            move || Ok(handler.clone()),
            Arc::clone(&computer.sessions),
            StreamableHttpServerConfig::default(),
        );
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let service = service.clone();
                let counted = Arc::clone(&counted);
                let answer = Arc::clone(&answer);
                let intercept = hyper::service::service_fn(move |request: Request<Incoming>| {
                    counted.fetch_add(1, Ordering::SeqCst);
                    let service = service.clone();
                    let answer = Arc::clone(&answer);
                    async move {
                        let (parts, body) = request.into_parts();
                        let body = match body.collect().await {
                            Ok(collected) => collected.to_bytes(),
                            Err(_) => Bytes::new(),
                        };
                        let message: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                        if message["method"] == "tools/call"
                            && message["params"]["arguments"]["ops"] == RUN
                        {
                            return Ok::<_, Infallible>(answer(&message["id"]));
                        }
                        let response = service
                            .handle(Request::from_parts(parts, Full::new(body)))
                            .await;
                        Ok(response.map(BodyExt::boxed_unsync))
                    }
                });
                tokio::spawn(async move {
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), intercept)
                        .await;
                });
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }

    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

type AnswerBody = UnsyncBoxBody<Bytes, Infallible>;

/// What an intercepting listener answers the `tools/call` of [`RUN`] with.
type Answer = Arc<dyn Fn(&Value) -> Response<AnswerBody> + Send + Sync>;

const PROBE: &str = "bounded-body-probe";

fn answer_with(status: StatusCode, content_type: &str, body: AnswerBody) -> Response<AnswerBody> {
    Response::builder()
        .status(status)
        .header(http::header::CONTENT_TYPE, content_type)
        .body(body)
        .expect("a response")
}

/// `bytes` in one body of known length.
fn whole(bytes: Vec<u8>) -> AnswerBody {
    Full::new(Bytes::from(bytes)).boxed_unsync()
}

/// `bytes` in chunks of 1 MiB, with no declared length.
fn chunked(bytes: Vec<u8>) -> AnswerBody {
    let frames: Vec<Result<Frame<Bytes>, Infallible>> = bytes
        .chunks(1 << 20)
        .map(|chunk| Ok(Frame::data(Bytes::copy_from_slice(chunk))))
        .collect();
    StreamBody::new(futures::stream::iter(frames)).boxed_unsync()
}

/// The JSON-RPC answer to request `id`: a tool result whose text is a khive
/// envelope naming [`PROBE`], padded with trailing spaces to `size` bytes.
fn padded_answer(id: &Value, size: usize) -> Vec<u8> {
    let envelope = json!({
        "results": [{"ok": true, "tool": "exec.run", "result": {"probe": PROBE}}],
        "summary": {"total": 1, "succeeded": 1, "failed": 0, "aborted": 0},
        "status": "success",
    });
    let message = json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {"content": [{"type": "text", "text": envelope.to_string()}]},
    });
    let mut body = message.to_string().into_bytes();
    assert!(body.len() <= size, "the answer fits in {size} bytes");
    body.resize(size, b' ');
    body
}

/// The routed request failed the way ADR-220 section 5 fails a computer that
/// answered after the request was sent: the computer may have run it.
fn assert_may_have_run(envelope: &Value, kind: &str) -> String {
    let entry = &envelope["results"][0];
    assert_eq!(entry["ok"], false, "{envelope}");
    assert_eq!(entry["error"]["kind"], kind, "{envelope}");
    assert_eq!(entry["domain_disposition"], "unknown", "{envelope}");
    let message = entry["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(message.contains("build-1"), "{envelope}");
    message
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn arm_1_over_loopback_tcp_one_tool_runs_a_command_and_reads_its_receipt() {
    reset_frames();
    let computer = TestComputer::start(ACTOR);
    let listener = Loopback::computer(&computer).await;
    let caller = Caller::new().routing_over_http(&computer.config_at(ACTOR, &listener.url));

    let run = caller.request(RUN).await;
    assert_eq!(run["results"][0]["ok"], true, "{run}");
    assert_eq!(
        run["results"][0]["result"]["receipt"]["actor"], ACTOR,
        "{run}"
    );
    let runs = caller.request("exec.runs()").await;
    let listed = runs["results"][0]["result"]["runs"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(listed.len(), 1, "{runs}");
    assert_eq!(
        listed[0]["actor"], ACTOR,
        "the receipt's actor is [actor].id"
    );

    assert_eq!(computer.calls(), ["whoami()", RUN, "exec.runs()"]);
    assert!(
        listener.requests() >= 3,
        "the calls crossed the socket: {}",
        listener.requests()
    );
    assert!(computer.sentinel());
    assert!(caller.runs.all().is_empty());
    assert!(!caller.sentinel());
    assert!(frames().is_empty(), "{:?}", frames());
}

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn arm_5_a_redirect_is_refused_and_its_target_receives_no_request() {
    reset_frames();
    let computer = TestComputer::start(ACTOR);
    // The redirect points at a working computer, so a client that followed
    // it would run the command there.
    let target = Loopback::computer(&computer).await;
    let redirect = Loopback::redirecting_to(&target.url).await;
    let caller = Caller::new().routing_over_http(&computer.config_at(ACTOR, &redirect.url));

    let answer = caller.text(RUN).await;
    let refused: Value = serde_json::from_str(&answer).expect("the khive envelope");
    assert_failed(&refused, 0, "computer_unreachable");

    assert!(redirect.requests() >= 1, "the redirect was answered");
    assert_eq!(
        target.requests(),
        0,
        "the redirect target received a request"
    );
    assert!(computer.calls().is_empty());
    assert!(!computer.sentinel());
    assert!(caller.runs.all().is_empty());
    assert!(!answer.contains(KEY), "{answer}");
    assert!(frames().is_empty(), "{:?}", frames());
}

// ---- response bodies are bounded (RUSTSEC-2026-0462) ----

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn bound_1_a_declared_length_over_the_json_bound_is_refused_before_the_body_is_read() {
    reset_frames();
    let computer = TestComputer::start(ACTOR);
    // The headers declare one byte over the bound and no byte of the body
    // is ever sent, so a client that read the body would wait for the call
    // timeout and fail as `computer_unreachable` instead.
    let listener = Loopback::intercepting(
        &computer,
        Arc::new(|_: &Value| {
            let never = futures::stream::pending::<Result<Frame<Bytes>, Infallible>>();
            let mut response = answer_with(
                StatusCode::OK,
                "application/json",
                StreamBody::new(never).boxed_unsync(),
            );
            response.headers_mut().insert(
                http::header::CONTENT_LENGTH,
                http::HeaderValue::from(MAX_JSON_BODY + 1),
            );
            response
        }),
    )
    .await;
    let caller = Caller::new().routing_over_http(&computer.config_at(ACTOR, &listener.url));

    let answer = caller.request(RUN).await;
    let message = assert_may_have_run(&answer, "computer_protocol");
    assert!(
        message.contains(&format!("larger than {MAX_JSON_BODY} bytes")),
        "{message}"
    );

    assert_eq!(computer.calls(), ["whoami()"]);
    assert!(!computer.sentinel());
    assert!(caller.runs.all().is_empty());
    assert!(!caller.sentinel());
    assert!(frames().is_empty(), "{:?}", frames());
}

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn bound_2_a_json_body_with_no_length_that_runs_past_the_bound_is_refused() {
    reset_frames();
    let computer = TestComputer::start(ACTOR);
    // The same answer that bound_4 accepts, one byte longer.
    let listener = Loopback::intercepting(
        &computer,
        Arc::new(|id: &Value| {
            answer_with(
                StatusCode::OK,
                "application/json",
                chunked(padded_answer(id, MAX_JSON_BODY + 1)),
            )
        }),
    )
    .await;
    let caller = Caller::new().routing_over_http(&computer.config_at(ACTOR, &listener.url));

    let answer = caller.text(RUN).await;
    assert!(!answer.contains(PROBE), "no part of the body is used");
    let envelope: Value = serde_json::from_str(&answer).expect("the khive envelope");
    let message = assert_may_have_run(&envelope, "computer_protocol");
    assert!(
        message.contains(&format!("larger than {MAX_JSON_BODY} bytes")),
        "{message}"
    );
    assert!(caller.runs.all().is_empty());
    assert!(frames().is_empty(), "{:?}", frames());
}

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn bound_3_an_error_body_past_its_bound_is_refused_and_not_quoted() {
    reset_frames();
    let computer = TestComputer::start(ACTOR);
    let listener = Loopback::intercepting(
        &computer,
        Arc::new(|_: &Value| {
            answer_with(
                StatusCode::INTERNAL_SERVER_ERROR,
                "application/json",
                chunked(vec![b'x'; MAX_ERROR_BODY + 1]),
            )
        }),
    )
    .await;
    let caller = Caller::new().routing_over_http(&computer.config_at(ACTOR, &listener.url));

    let answer = caller.text(RUN).await;
    assert!(
        !answer.contains("xxxxxxxxxxxxxxxx"),
        "the body is not quoted"
    );
    let envelope: Value = serde_json::from_str(&answer).expect("the khive envelope");
    let message = assert_may_have_run(&envelope, "computer_protocol");
    assert!(message.contains("HTTP 500"), "{message}");
    assert!(
        message.contains(&format!("larger than {MAX_ERROR_BODY} bytes")),
        "{message}"
    );
    assert!(caller.runs.all().is_empty());
    assert!(frames().is_empty(), "{:?}", frames());
}

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn bound_4_a_json_body_exactly_at_the_bound_is_read() {
    reset_frames();
    let computer = TestComputer::start(ACTOR);
    let listener = Loopback::intercepting(
        &computer,
        Arc::new(|id: &Value| {
            answer_with(
                StatusCode::OK,
                "application/json",
                whole(padded_answer(id, MAX_JSON_BODY)),
            )
        }),
    )
    .await;
    let caller = Caller::new().routing_over_http(&computer.config_at(ACTOR, &listener.url));

    let answer = caller.request(RUN).await;
    assert_eq!(answer["results"][0]["ok"], true, "{answer}");
    assert_eq!(answer["results"][0]["result"]["probe"], PROBE, "{answer}");
    assert_eq!(computer.calls(), ["whoami()"]);
    assert!(frames().is_empty(), "{:?}", frames());
}

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn bound_5_an_event_past_its_bound_stops_the_read() {
    reset_frames();
    let computer = TestComputer::start(ACTOR);
    // One event that never ends: 1 MiB chunks for as long as the client
    // reads them.
    let produced = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&produced);
    let listener = Loopback::intercepting(
        &computer,
        Arc::new(move |_: &Value| {
            let counted = Arc::clone(&counted);
            let chunk = Bytes::from(vec![b'a'; 1 << 20]);
            let head = futures::stream::iter([Ok::<_, Infallible>(Frame::data(
                Bytes::from_static(b"data: "),
            ))]);
            let tail = futures::stream::repeat_with(move || {
                counted.fetch_add(chunk.len(), Ordering::SeqCst);
                Ok(Frame::data(chunk.clone()))
            });
            answer_with(
                StatusCode::OK,
                "text/event-stream",
                StreamBody::new(futures::StreamExt::chain(head, tail)).boxed_unsync(),
            )
        }),
    )
    .await;
    let caller =
        Caller::new().routing_over_http(&computer.config_with_timeout(ACTOR, &listener.url, 3));

    let answer = caller.request(RUN).await;
    // No answer arrives on the refused stream, so the call ends at its
    // timeout, which may have run the request.
    let message = assert_may_have_run(&answer, "computer_unreachable");
    assert!(message.contains("did not answer within 3 s"), "{message}");
    // The client stopped reading at the bound. What the listener produced
    // past it is what the socket buffers held when the client let go.
    let produced = produced.load(Ordering::SeqCst);
    assert!(
        produced <= MAX_EVENT + (16 << 20),
        "the client read {produced} bytes of one event"
    );
    assert!(caller.runs.all().is_empty());
    assert!(frames().is_empty(), "{:?}", frames());
}

#[tokio::test]
#[serial_test::serial(computer_bridge)]
async fn bound_6_json_that_is_not_json_rpc_answering_a_request_is_refused_at_once() {
    reset_frames();
    let computer = TestComputer::start(ACTOR);
    let listener = Loopback::intercepting(
        &computer,
        Arc::new(|_: &Value| {
            answer_with(
                StatusCode::OK,
                "application/json",
                whole(json!({ "probe": PROBE }).to_string().into_bytes()),
            )
        }),
    )
    .await;
    // No answer to the request follows such a body. A client that waited
    // for one would end at this timeout as `computer_unreachable` instead.
    let caller =
        Caller::new().routing_over_http(&computer.config_with_timeout(ACTOR, &listener.url, 30));

    let started = std::time::Instant::now();
    let answer = caller.text(RUN).await;
    let elapsed = started.elapsed();
    assert!(!answer.contains(PROBE), "no part of the body is used");
    let envelope: Value = serde_json::from_str(&answer).expect("the khive envelope");
    let message = assert_may_have_run(&envelope, "computer_protocol");
    assert!(message.contains("not a JSON-RPC message"), "{message}");
    assert!(elapsed < Duration::from_secs(15), "{elapsed:?}");

    assert_eq!(computer.calls(), ["whoami()"]);
    assert!(caller.runs.all().is_empty());
    assert!(frames().is_empty(), "{:?}", frames());
}

// Arms 3, 4 and 8 with the real `exec` pack on the store's side.
#[cfg(target_os = "macos")]
#[path = "computer_bridge_exec_tests.rs"]
mod real_exec;
