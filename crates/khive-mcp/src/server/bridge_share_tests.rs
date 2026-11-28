//! ADR-220 Amendment 1 acceptance arms for `bridge.share`, end to end in one
//! process.
//!
//! The sharer is a stdio bridge whose daemon forward is a spy that records
//! every frame. The target is a khive server of its own, reached over the
//! in-process pipe: the kg pack's `whoami`, the real blob and exec packs over
//! a file blob store, and a `computer.share` stand-in that checks each digest
//! against the tree's own entries, refuses a mismatch and records the share.
//! Its request log holds every operation it was sent.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use khive_db::stores::blob::FsBlobStore;
use khive_runtime::pack::{HandlerDef, PackRuntime, VerbRegistry, VerbRegistryBuilder};
use khive_runtime::{
    ConfigError, DaemonRequestFrame, KhiveConfig, KhiveRuntime, NamespaceToken, RuntimeConfig,
    RuntimeError,
};
use khive_types::{Pack, VerbCategory, Visibility};
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, ErrorData as McpError, ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::RoleServer;
use serde_json::{json, Value};

use super::super::bridge::ForwardFuture;
use super::super::computer_pipe_tests::{PipeComputer, PipeConnector, URL};
use super::super::{mark_all_failed_request_result, KhiveMcpServer};
use super::{ShareBridge, ShareSetup};
use crate::tools::request::RequestParams;

const ACTOR: &str = "lambda:test-sharer";
const RECEIVER: &str = "lambda:test-receiver";
// Assembled from parts so that no literal in the source reads as a credential.
const KEY: &str = concat!("share-", "fixture-", "token");
const LABEL: &str = "build-1 (localhost:80)";
/// The published BLAKE3 test vector: the digest of the three bytes `abc`.
const ABC_DIGEST: &str = "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85";
/// Bytes no answer, log or frame may hold.
const SECRET_BYTES: &[u8] = b"fixture-file-bytes-5b7c1e";

// ---- the computer.share stand-in ----

const COMPUTER_HANDLERS: &[HandlerDef] = &[HandlerDef {
    name: "computer.share",
    description: "computer.share stand-in for the share arms",
    visibility: Visibility::Verb,
    category: VerbCategory::Commissive,
    params: &[],
}];

/// Derives each digest from the tree's own entries, refuses a mismatch and
/// records the share. `refuse` refuses every share as a digest mismatch;
/// `hang` records the share and never answers.
#[derive(Clone, Default)]
struct ShareStandIn {
    records: Arc<StdMutex<Vec<Value>>>,
    trees: Arc<StdMutex<Vec<String>>>,
    refuse: Arc<AtomicBool>,
    hang: Arc<AtomicBool>,
}

impl Pack for ShareStandIn {
    const NAME: &'static str = "computer";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = COMPUTER_HANDLERS;
}

#[async_trait]
impl PackRuntime for ShareStandIn {
    fn name(&self) -> &str {
        "computer"
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        &[]
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        &[]
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        COMPUTER_HANDLERS
    }

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        if verb != "computer.share" {
            return Err(RuntimeError::InvalidInput(format!("unknown verb {verb}")));
        }
        let tree = params["tree"]
            .as_str()
            .ok_or_else(|| RuntimeError::InvalidInput("files is required".into()))?
            .to_owned();
        self.trees
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(tree.clone());
        if self.refuse.load(Ordering::SeqCst) {
            return Err(RuntimeError::InvalidInput(format!(
                "share_digest_mismatch: the stand-in refuses every share of {tree}"
            )));
        }
        let listed = registry
            .dispatch("exec.tree_get", json!({ "tree": tree }))
            .await?;
        let entries = listed["entries"].as_array().cloned().unwrap_or_default();
        let files = params["files"].as_array().cloned().unwrap_or_default();
        for file in &files {
            let path = file["path"].as_str().unwrap_or_default();
            let Some(entry) = entries.iter().find(|entry| entry["path"] == path) else {
                return Err(RuntimeError::InvalidInput(format!(
                    "share_path_absent: {path:?} is not in tree {tree}"
                )));
            };
            if entry["ref"] != file["digest"] {
                return Err(RuntimeError::InvalidInput(format!(
                    "share_digest_mismatch: {path:?} in tree {tree}"
                )));
            }
        }
        let record = json!({
            "share_id": uuid::Uuid::new_v4().to_string(),
            "tree": tree,
            "sharer": token.actor().id,
            "receiver": params["receiver"],
            "files": files,
            "note": params.get("note").cloned().unwrap_or(Value::Null),
        });
        self.records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(record.clone());
        if self.hang.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        Ok(record)
    }
}

// ---- the target ----

/// A khive server that records the `ops` of every call it is sent.
#[derive(Clone)]
struct Recorder {
    inner: KhiveMcpServer,
    calls: Arc<StdMutex<Vec<String>>>,
}

impl ServerHandler for Recorder {
    fn get_info(&self) -> ServerInfo {
        self.inner.get_info()
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let arguments = Value::Object(request.arguments.unwrap_or_default());
        let ops = arguments["ops"].as_str().unwrap_or_default().to_owned();
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(ops);
        // A file-backed server hands its requests to a daemon; the target
        // answers each one on its own store, as a computer's daemon does.
        let params: RequestParams = serde_json::from_value(arguments)
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let text = self
            .inner
            .request_with_forward(params, on_own_store)
            .await?;
        let mut result = CallToolResult::success(vec![ContentBlock::text(text)]);
        mark_all_failed_request_result(&mut result);
        Ok(result)
    }
}

/// No daemon answers, so the target's request runs on its own store.
fn on_own_store(
    _frame: DaemonRequestFrame,
    _config: Option<PathBuf>,
    _db: Option<String>,
    _packs: Option<Vec<String>>,
    _replay_read_only: bool,
) -> ForwardFuture {
    Box::pin(async { None })
}

struct Target {
    _dir: tempfile::TempDir,
    server: KhiveMcpServer,
    pipe: Arc<PipeComputer>,
    calls: Arc<StdMutex<Vec<String>>>,
    stand_in: ShareStandIn,
}

impl Target {
    /// A target whose `whoami` answers for `actor`.
    fn start(actor: &str) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        // A file database: staged uploads need a durable main database.
        let backend = Arc::new(
            khive_db::StorageBackend::sqlite_for_test(dir.path().join("target.db"))
                .expect("the target's database"),
        );
        backend
            .prepare_core_schema()
            .expect("the target's core schema");
        let runtime = KhiveRuntime::from_backend(backend, RuntimeConfig::no_embeddings());
        let blobs = FsBlobStore::new(dir.path().join("blobs"), 0).expect("a blob store");
        runtime
            .install_blob_store(Arc::new(blobs))
            .expect("install the blob store");
        let stand_in = ShareStandIn::default();
        let mut builder = VerbRegistryBuilder::new();
        builder.with_actor_id(Some(actor.into()));
        builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
        builder.register(khive_pack_blob::BlobPack::new(runtime.clone()));
        builder.register(khive_pack_tool::ToolPack::new(runtime.clone()));
        builder.register(khive_pack_exec::ExecPack::new(runtime.clone()));
        builder.register_trusted(stand_in.clone());
        let registry = builder.build().expect("the target's registry");
        registry.apply_schema_plans(runtime.backend());
        let server = KhiveMcpServer::from_registry(registry).with_runtime(runtime);
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let recorder = Recorder {
            inner: server.clone(),
            calls: Arc::clone(&calls),
        };
        let pipe = PipeComputer::serve(recorder, Arc::new(LocalSessionManager::default()));
        Self {
            _dir: dir,
            server,
            pipe,
            calls,
            stand_in,
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// How many operations the target received whose verb is `verb`.
    fn received(&self, verb: &str) -> usize {
        self.calls()
            .iter()
            .filter(|ops| ops.starts_with(&format!("{verb}(")))
            .count()
    }

    fn records(&self) -> Vec<Value> {
        self.stand_in
            .records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn trees(&self) -> Vec<String> {
        self.stand_in
            .trees
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    async fn verb(&self, verb: &str, params: Value) -> Value {
        self.server
            .registry
            .dispatch(verb, params)
            .await
            .unwrap_or_else(|error| panic!("{verb} on the target: {error}"))
    }

    /// The bytes the target's tree holds at `path`, and the entry's reference.
    async fn tree_file(&self, tree: &str, path: &str) -> (String, Vec<u8>) {
        let listed = self.verb("exec.tree_get", json!({ "tree": tree })).await;
        let entry = listed["entries"]
            .as_array()
            .and_then(|entries| entries.iter().find(|entry| entry["path"] == path))
            .cloned()
            .unwrap_or_else(|| panic!("{path} is in the tree: {listed}"));
        assert_eq!(entry["mode"], 644, "{entry}");
        let reference = entry["ref"].as_str().expect("a reference").to_owned();
        let blob = self
            .verb("blob.get", json!({ "content_ref": reference }))
            .await;
        let bytes = BASE64
            .decode(blob["bytes"].as_str().expect("base64 bytes"))
            .expect("decode");
        (reference, bytes)
    }
}

impl Drop for Target {
    fn drop(&mut self) {
        self.pipe.stop();
    }
}

// ---- the spy daemon forward ----

static FRAMES: StdMutex<Vec<String>> = StdMutex::new(Vec::new());

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

// ---- the sharer ----

struct Sharer {
    dir: tempfile::TempDir,
    server: KhiveMcpServer,
}

fn sharer_server() -> KhiveMcpServer {
    let runtime = KhiveRuntime::memory().expect("the sharer's runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some(ACTOR.into()));
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    let mut server = KhiveMcpServer::from_registry(builder.build().expect("the sharer's registry"))
        .with_runtime(runtime);
    server.stdio_bridge = true;
    server
}

fn write_key(path: &Path) {
    std::fs::write(path, format!("{KEY}\n")).expect("write the key file");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
}

impl Sharer {
    /// A sharer whose one target is `target`, with the default limits.
    fn new(target: &Target) -> Self {
        Self::with(target, "", 30)
    }

    /// A sharer with `limits` (lines of the `[share]` table) and a target
    /// call timeout of `call_timeout_s`.
    fn with(target: &Target, limits: &str, call_timeout_s: u64) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("share")).expect("the share root");
        let key_file = dir.path().join("share.key");
        write_key(&key_file);
        let text = format!(
            "[actor]\nid = \"{ACTOR}\"\n\n[share]\nroot = \"{}\"\n{limits}\n\n\
             [[share.targets]]\nname = \"build-1\"\nurl = \"{URL}\"\nkey_file = \"{}\"\n\
             call_timeout_s = {call_timeout_s}\n",
            dir.path().join("share").display(),
            key_file.display()
        );
        let config: KhiveConfig = toml::from_str(&text).expect("a share configuration");
        let connector = Arc::new(PipeConnector {
            computer: Arc::clone(&target.pipe),
        });
        let mut server = sharer_server();
        server.share =
            ShareSetup::from_config(&config, Some(&dir.path().join("config.toml")), connector)
                .expect("a valid share table");
        assert!(server.share.bridge.is_some());
        Self { dir, server }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().join("share")
    }

    fn bridge(&self) -> &ShareBridge {
        self.server.share.bridge.as_deref().expect("a share bridge")
    }

    fn write(&self, path: &str, bytes: &[u8]) {
        let path = self.root().join(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parent directories");
        }
        std::fs::write(path, bytes).expect("write a file to share");
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
}

/// `bridge.share` of `paths` to the one target.
fn share_ops(paths: &[&str]) -> String {
    format!(
        "bridge.share(target=\"build-1\", receiver=\"{RECEIVER}\", paths={})",
        json!(paths)
    )
}

/// The refusal of the request's first operation, which must be `kind` and
/// `disposition`; returns its message.
fn refusal(envelope: &Value, kind: &str, disposition: &str) -> String {
    let entry = &envelope["results"][0];
    assert_eq!(entry["ok"], false, "{envelope}");
    assert_eq!(entry["error"]["kind"], kind, "{envelope}");
    assert_eq!(entry["domain_disposition"], disposition, "{envelope}");
    entry["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

// ---- the arms ----

#[tokio::test]
#[serial_test::serial(bridge_share)]
async fn s1_s2_one_file_becomes_a_tree_on_the_target_and_its_share_is_recorded() {
    reset_frames();
    let target = Target::start(ACTOR);
    let sharer = Sharer::new(&target);
    sharer.write("notes/abc.md", b"abc");

    let reply = sharer
        .request(&format!(
            "bridge.share(target=\"build-1\", receiver=\"{RECEIVER}\", \
             paths=[\"notes/abc.md\"], note=\"for review\")"
        ))
        .await;
    let entry = &reply["results"][0];
    assert_eq!(entry["ok"], true, "{reply}");
    assert_eq!(entry["tool"], "bridge.share", "{reply}");
    let result = &entry["result"];
    assert_eq!(result["target"], "build-1", "{reply}");
    assert_eq!(result["receiver"], RECEIVER, "{reply}");
    assert_eq!(
        result["files"],
        json!([{ "path": "notes/abc.md", "digest": ABC_DIGEST, "size": 3 }]),
        "S2: the digest is the BLAKE3 of the file's bytes"
    );

    let records = target.records();
    assert_eq!(records.len(), 1, "{records:?}");
    let record = &records[0];
    assert_eq!(result["share_id"], record["share_id"], "{reply}");
    assert_eq!(result["tree"], record["tree"], "{reply}");
    assert_eq!(record["receiver"], RECEIVER);
    assert_eq!(record["sharer"], ACTOR, "the actor of the target key");
    assert_eq!(record["note"], "for review");
    assert_eq!(
        record["files"],
        json!([{ "path": "notes/abc.md", "digest": ABC_DIGEST }])
    );

    let tree = result["tree"].as_str().expect("a tree");
    let (reference, bytes) = target.tree_file(tree, "notes/abc.md").await;
    assert_eq!(bytes, b"abc", "the tree entry holds the file's bytes");
    assert_eq!(
        reference, ABC_DIGEST,
        "S2: the digest is the tree entry's reference"
    );

    let calls = target.calls();
    assert_eq!(
        calls.first().map(String::as_str),
        Some("whoami()"),
        "{calls:?}"
    );
    assert_eq!(target.received("blob.begin"), 1, "{calls:?}");
    assert_eq!(target.received("blob.put_part"), 1, "{calls:?}");
    assert_eq!(target.received("blob.commit"), 1, "{calls:?}");
    assert_eq!(target.received("exec.tree"), 1, "{calls:?}");
    assert_eq!(target.received("computer.share"), 1, "{calls:?}");
    assert!(
        calls
            .last()
            .is_some_and(|ops| ops.starts_with("computer.share(")),
        "computer.share is the last call: {calls:?}"
    );
    assert!(frames().is_empty(), "no daemon frame: {:?}", frames());

    // A second share of the same bytes skips the upload the target holds.
    sharer.write("copy.md", b"abc");
    let again = sharer.request(&share_ops(&["copy.md"])).await;
    assert_eq!(again["results"][0]["ok"], true, "{again}");
    assert_eq!(target.received("blob.begin"), 2);
    assert_eq!(
        target.received("blob.put_part"),
        1,
        "no part for a known blob"
    );

    let diagnostics = sharer.request("bridge.diagnostics()").await;
    assert_eq!(
        diagnostics["results"][0]["result"]["share"],
        json!([{ "target": "build-1", "label": LABEL, "shares": 2 }]),
        "{diagnostics}"
    );
}

/// The S3 fixtures: files, links, a directory and a FIFO under the root.
fn s3_fixtures(sharer: &Sharer) {
    let root = sharer.root();
    for name in ["ok.md", "a.md", "b.md", "c.md"] {
        sharer.write(name, b"0123456789");
    }
    sharer.write("big.md", &[b'x'; 17]);
    sharer.write("back\\slash.md", b"0123456789");
    sharer.write("real/inner.md", b"0123456789");
    std::os::unix::fs::symlink("ok.md", root.join("inside-link.md")).expect("a file link");
    std::os::unix::fs::symlink("real", root.join("linked")).expect("a directory link");
    std::fs::create_dir(root.join("adir")).expect("a directory");
    let made = std::process::Command::new("mkfifo")
        .arg(root.join("pipe"))
        .status()
        .expect("run mkfifo");
    assert!(made.success(), "a FIFO");
}

const S3_LIMITS: &str = "max_file_bytes = 16\nmax_total_bytes = 24\nmax_files = 3";

#[tokio::test]
#[serial_test::serial(bridge_share)]
async fn s3_every_bound_refuses_before_anything_is_sent() {
    reset_frames();
    let target = Target::start(ACTOR);
    let sharer = Sharer::with(&target, S3_LIMITS, 30);
    s3_fixtures(&sharer);

    let cases: &[(&[&str], &str)] = &[
        (&["/etc/hosts"], "share_path_refused"),
        (&["../outside.md"], "share_path_refused"),
        (&["a//b.md"], "share_path_refused"),
        (&["./a.md"], "share_path_refused"),
        (&["back\\slash.md"], "share_path_refused"),
        (&["nul\u{0}.md"], "share_path_refused"),
        (&["inside-link.md"], "share_path_refused"),
        (&["linked/inner.md"], "share_path_refused"),
        (&["adir"], "share_path_refused"),
        (&["pipe"], "share_path_refused"),
        (&["big.md"], "share_too_large"),
        (&["ok.md", "a.md", "b.md"], "share_too_large"),
        (&["ok.md", "a.md", "b.md", "c.md"], "share_too_large"),
        (&["ok.md", "ok.md"], "invalid_input"),
        (&[], "invalid_input"),
    ];
    for (paths, kind) in cases {
        let reply = sharer.request(&share_ops(paths)).await;
        let message = refusal(&reply, kind, "not_committed");
        assert!(message.contains("nothing was sent"), "{paths:?}: {message}");
        if *kind == "share_path_refused" {
            assert!(message.contains(&format!("{:?}", paths[0])), "{message}");
        }
    }
    assert!(
        target.calls().is_empty(),
        "the target's request log gained nothing: {:?}",
        target.calls()
    );
    assert!(target.pipe.authorization().is_empty(), "no connection");
    assert!(frames().is_empty(), "{:?}", frames());

    // Control: a path inside every bound is sent.
    let reply = sharer.request(&share_ops(&["ok.md", "a.md"])).await;
    assert_eq!(reply["results"][0]["ok"], true, "{reply}");
}

#[tokio::test]
#[serial_test::serial(bridge_share)]
async fn s4_without_the_link_check_the_inside_links_reach_the_target() {
    reset_frames();
    let target = Target::start(ACTOR);
    let sharer = Sharer::with(&target, S3_LIMITS, 30);
    s3_fixtures(&sharer);
    sharer
        .bridge()
        .seams
        .skip_link_check
        .store(true, Ordering::SeqCst);

    for path in ["inside-link.md", "linked/inner.md"] {
        let reply = sharer.request(&share_ops(&[path])).await;
        assert_eq!(reply["results"][0]["ok"], true, "{path}: {reply}");
    }
    assert_eq!(target.received("computer.share"), 2, "{:?}", target.calls());
}

#[tokio::test]
#[serial_test::serial(bridge_share)]
async fn s5_an_unknown_target_is_refused_and_nothing_is_connected() {
    reset_frames();
    let target = Target::start(ACTOR);
    let sharer = Sharer::new(&target);
    sharer.write("a.md", b"abc");

    let reply = sharer
        .request(&format!(
            "bridge.share(target=\"nowhere\", receiver=\"{RECEIVER}\", paths=[\"a.md\"])"
        ))
        .await;
    let message = refusal(&reply, "share_target_unknown", "not_committed");
    assert!(message.contains("nowhere"), "{message}");
    assert!(target.calls().is_empty(), "{:?}", target.calls());
    assert!(target.pipe.authorization().is_empty(), "no connection");
}

#[tokio::test]
#[serial_test::serial(bridge_share)]
async fn s6_a_target_key_of_another_actor_is_refused_before_any_upload() {
    reset_frames();
    let target = Target::start("lambda:someone-else");
    let sharer = Sharer::new(&target);
    sharer.write("a.md", b"abc");

    let reply = sharer.request(&share_ops(&["a.md"])).await;
    refusal(&reply, "computer_identity_mismatch", "not_committed");
    assert_eq!(target.calls(), ["whoami()"]);
    assert!(target.records().is_empty());
}

#[tokio::test]
#[serial_test::serial(bridge_share)]
async fn s7_a_refusal_by_the_target_reaches_the_caller_unchanged() {
    reset_frames();
    let target = Target::start(ACTOR);
    target.stand_in.refuse.store(true, Ordering::SeqCst);
    let sharer = Sharer::new(&target);
    sharer.write("a.md", b"abc");

    let reply = sharer.request(&share_ops(&["a.md"])).await;
    let entry = &reply["results"][0];
    assert_eq!(entry["ok"], false, "{reply}");
    let message = entry["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("share_digest_mismatch"), "{reply}");
    assert!(
        entry["error"].get("tree").is_none() && !message.contains("no share was sent"),
        "the bridge added nothing: {reply}"
    );

    // The target's own answer to the same call, read directly.
    let trees = target.trees();
    assert_eq!(trees.len(), 1, "{trees:?}");
    let direct = target
        .server
        .request_with_forward(
            RequestParams {
                ops: format!(
                    "computer.share(tree=\"{}\", receiver=\"{RECEIVER}\", \
                     files=[{{\"path\":\"a.md\",\"digest\":\"{ABC_DIGEST}\"}}])",
                    trees[0]
                ),
                presentation: Some("verbose".into()),
                format: Some("json".into()),
                ..Default::default()
            },
            spy_forward,
        )
        .await
        .expect("a direct answer");
    let direct: Value = serde_json::from_str(&direct).expect("an envelope");
    assert_eq!(entry["error"], direct["results"][0]["error"], "{reply}");
    assert_eq!(
        entry["domain_disposition"], direct["results"][0]["domain_disposition"],
        "{reply}"
    );

    let listed = target
        .verb("exec.tree_get", json!({ "tree": trees[0] }))
        .await;
    assert_eq!(listed["tree"], trees[0], "the tree exists");
    assert!(target.records().is_empty(), "the stand-in recorded nothing");
}

#[tokio::test]
#[serial_test::serial(bridge_share)]
async fn s8_a_share_whose_answer_is_lost_is_unknown_and_sent_once() {
    reset_frames();
    let target = Target::start(ACTOR);
    target.stand_in.hang.store(true, Ordering::SeqCst);
    let sharer = Sharer::with(&target, "", 1);
    sharer.write("a.md", b"abc");

    let reply = sharer.request(&share_ops(&["a.md"])).await;
    let message = refusal(&reply, "computer_unreachable", "unknown");
    let records = target.records();
    assert_eq!(records.len(), 1, "{records:?}");
    let tree = records[0]["tree"].as_str().expect("a tree");
    assert_eq!(reply["results"][0]["error"]["tree"], tree, "{reply}");
    assert!(message.contains(tree), "{message}");
    assert_eq!(target.received("computer.share"), 1, "{:?}", target.calls());
}

#[tokio::test]
#[serial_test::serial(bridge_share)]
async fn s9_a_file_that_grows_after_its_check_is_refused_and_not_uploaded() {
    reset_frames();
    let target = Target::start(ACTOR);
    let sharer = Sharer::new(&target);
    sharer.write("grows.md", b"abc");
    *sharer
        .bridge()
        .seams
        .before_read
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(Box::new(|path: &Path| {
        std::fs::write(path, b"abcdef").expect("grow the file");
    }));

    let reply = sharer.request(&share_ops(&["grows.md"])).await;
    let message = refusal(&reply, "share_file_changed", "not_committed");
    assert!(message.contains("grows.md"), "{message}");
    assert_eq!(target.received("blob.begin"), 0, "{:?}", target.calls());
    assert!(target.records().is_empty());
}

/// A directory replaced by a link after the checks: the link refusal on the
/// last component does not cover a parent, so the opened handle's own path
/// decides.
#[tokio::test]
#[serial_test::serial(bridge_share)]
async fn s9_a_directory_swapped_for_a_link_after_its_check_is_refused_and_not_uploaded() {
    reset_frames();
    let target = Target::start(ACTOR);
    let sharer = Sharer::new(&target);
    sharer.write("sub/same.md", b"abc");
    // Outside the root: a directory holding a file of the same name and size.
    let outside = sharer.dir.path().join("elsewhere");
    std::fs::create_dir(&outside).expect("a directory outside the root");
    std::fs::write(outside.join("same.md"), b"xyz").expect("a file outside the root");
    *sharer
        .bridge()
        .seams
        .before_read
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(Box::new(move |path: &Path| {
        let parent = path.parent().expect("the file's directory");
        std::fs::rename(parent, parent.with_file_name("sub-checked"))
            .expect("move the checked directory away");
        std::os::unix::fs::symlink(&outside, parent).expect("a link where the directory was");
    }));

    let reply = sharer.request(&share_ops(&["sub/same.md"])).await;
    let message = refusal(&reply, "share_path_refused", "not_committed");
    assert!(message.contains("outside the share root"), "{message}");
    assert_eq!(target.received("blob.begin"), 0, "{:?}", target.calls());
    assert!(target.records().is_empty());
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

fn holds(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[tokio::test]
#[serial_test::serial(bridge_share)]
async fn s10_no_file_byte_and_no_key_surfaces_in_answers_logs_diagnostics_or_frames() {
    reset_frames();
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let mut answers = Vec::new();
    let mut authorization = Vec::new();
    // S1, S3, S7 and S8 against one target each.
    for arm in ["s1", "s3", "s7", "s8"] {
        let target = Target::start(ACTOR);
        target.stand_in.refuse.store(arm == "s7", Ordering::SeqCst);
        target.stand_in.hang.store(arm == "s8", Ordering::SeqCst);
        let limits = if arm == "s3" {
            "max_file_bytes = 8"
        } else {
            ""
        };
        let sharer = Sharer::with(&target, limits, if arm == "s8" { 1 } else { 30 });
        sharer.write("secret.md", SECRET_BYTES);
        answers.push(sharer.text(&share_ops(&["secret.md"])).await);
        answers.push(sharer.text("bridge.diagnostics()").await);
        authorization.extend(target.pipe.authorization());
    }
    // S6.
    let other = Target::start("lambda:someone-else");
    let sharer = Sharer::new(&other);
    sharer.write("secret.md", SECRET_BYTES);
    answers.push(sharer.text(&share_ops(&["secret.md"])).await);
    authorization.extend(other.pipe.authorization());

    assert!(!authorization.is_empty(), "the key reached the targets");
    assert!(authorization
        .iter()
        .all(|value| value == &format!("Bearer {KEY}")));
    let log = capture
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    assert!(!log.is_empty(), "the log writer recorded the shares");
    for answer in &answers {
        assert!(!answer.contains(KEY), "{answer}");
        assert!(!holds(answer.as_bytes(), SECRET_BYTES), "{answer}");
    }
    assert!(!holds(&log, KEY.as_bytes()), "the key is in the log");
    assert!(
        !holds(&log, SECRET_BYTES),
        "the file's bytes are in the log"
    );
    assert!(frames().is_empty(), "no daemon frame: {:?}", frames());
}

#[tokio::test]
#[serial_test::serial(bridge_share)]
async fn s11_without_a_share_table_the_verb_is_refused_naming_the_file() {
    reset_frames();
    let mut server = sharer_server();
    let config: KhiveConfig =
        toml::from_str(&format!("[actor]\nid = \"{ACTOR}\"\n")).expect("a configuration");
    let target = Target::start(ACTOR);
    let connector = Arc::new(PipeConnector {
        computer: Arc::clone(&target.pipe),
    });
    server.share = ShareSetup::from_config(
        &config,
        Some(Path::new("/etc/khive/config.toml")),
        connector,
    )
    .expect("no share table is valid");
    assert!(server.share.bridge.is_none());
    let sharer = Sharer {
        dir: tempfile::tempdir().expect("tempdir"),
        server,
    };

    let reply = sharer.request(&share_ops(&["a.md"])).await;
    let message = refusal(&reply, "share_not_configured", "not_committed");
    assert!(message.contains("/etc/khive/config.toml"), "{message}");
    assert!(target.calls().is_empty());

    // Help is answered without the table.
    let help = sharer.request("bridge.share(help=true)").await;
    assert_eq!(
        help["results"][0]["result"]["verb"], "bridge.share",
        "{help}"
    );
}

#[test]
fn s11_a_share_table_needs_the_actor_id() {
    let config: KhiveConfig = toml::from_str(
        "[share]\nroot = \"/tmp/share\"\n\n[[share.targets]]\nname = \"build-1\"\n\
         url = \"http://localhost/mcp\"\nkey_file = \"/tmp/share.key\"\n",
    )
    .expect("a share table");
    let Err(error) = ShareSetup::from_config(&config, None, Arc::new(super::HttpConnector)) else {
        panic!("a share table without [actor].id is refused");
    };
    assert!(
        matches!(error, ConfigError::InvalidShareConfig { .. }),
        "{error}"
    );
}

#[tokio::test]
#[serial_test::serial(bridge_share)]
async fn s12_bridge_share_beside_another_operation_is_refused_and_nothing_is_sent() {
    reset_frames();
    let target = Target::start(ACTOR);
    let sharer = Sharer::new(&target);
    sharer.write("a.md", b"abc");
    let share = share_ops(&["a.md"]);

    for ops in [
        format!("[{share}, stats()]"),
        format!("{share} | stats()"),
        format!("stats() | {share}"),
        format!("[{share}]"),
    ] {
        let reply = sharer.request(&ops).await;
        let results = reply["results"].as_array().cloned().unwrap_or_default();
        assert!(!results.is_empty(), "{reply}");
        for entry in &results {
            assert_eq!(entry["ok"], false, "{ops}: {reply}");
            assert_eq!(entry["error"]["kind"], "invalid_input", "{ops}: {reply}");
        }
    }
    assert!(target.calls().is_empty(), "{:?}", target.calls());
    assert!(frames().is_empty(), "{:?}", frames());
}

#[tokio::test]
#[serial_test::serial(bridge_share)]
async fn only_the_stdio_bridge_answers_and_the_daemon_holds_no_share_table() {
    reset_frames();
    let target = Target::start(ACTOR);
    let mut sharer = Sharer::new(&target);
    sharer.write("a.md", b"abc");
    sharer.server.stdio_bridge = false;
    let reply = sharer.request(&share_ops(&["a.md"])).await;
    refusal(&reply, "invalid_input", "not_committed");

    let daemon = sharer.server.clone().without_computer();
    assert!(daemon.share.bridge.is_none());
    assert!(target.calls().is_empty(), "{:?}", target.calls());
}

#[tokio::test]
#[serial_test::serial(bridge_share)]
async fn a_plan_names_bridge_share_as_known_and_sends_nothing() {
    reset_frames();
    let target = Target::start(ACTOR);
    let sharer = Sharer::new(&target);
    let plan = sharer
        .server
        .request_with_cancellation(RequestParams {
            ops: share_ops(&["a.md"]),
            plan: Some(true),
            ..Default::default()
        })
        .await
        .expect("a plan");
    let plan: Value = serde_json::from_str(&plan).expect("plan JSON");
    assert_eq!(plan["stages"][0]["known"], true, "{plan}");
    assert_eq!(plan["stages"][0]["pack"], "bridge-control", "{plan}");
    assert!(target.calls().is_empty());
}
