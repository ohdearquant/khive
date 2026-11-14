//! Configuration refusal evidence through Unix framing and the MCP request boundary.

use std::ffi::OsString;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Duration;

use khive_pack_kg::KgPack;
use khive_runtime::daemon::{
    config_ids_compatible, read_frame, write_frame, DaemonRequestFrame, PROTOCOL_VERSION,
};
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig, VerbRegistryBuilder};
use khive_storage::types::PageRequest;
use khive_storage::Note;
use serde_json::{json, Value};
use serial_test::serial;
use tokio::net::UnixListener;
use tokio::task::JoinHandle;

use super::test_harness::{
    clear_daemon_env, connect_when_ready, exchange, reset_counters, HarnessDispatch,
    InProcessDaemonHandle, InProcessDaemonLauncher, KILL_COUNT, SIGTERM_COUNT, SPAWN_COUNT,
};
use super::{
    fallback_count, fallback_strict_violations, fallback_total, forward_or_spawn,
    reset_fallback_counters, DaemonLauncher, FallbackReason, STRICT_FALLBACK_MARKER,
};
use crate::server::{compute_config_id, KhiveMcpServer};
use crate::tools::request::RequestParams;

const HOSTILE: &str = "untrusted-peer-private-config-do-not-echo";
const RAW_SUCCESS: &str = r#"{"entities":0,"edges":0,"notes":0}"#;
const CREATE: &str = r#"create(kind="observation", content="refused observation")"#;

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(15), future)
        .await
        .expect("configuration refusal fixture must complete within its watchdog")
}

struct Rendezvous {
    dir: tempfile::TempDir,
    previous_strict: Option<OsString>,
}

impl Rendezvous {
    fn new(strict: bool) -> Self {
        clear_daemon_env();
        reset_counters();
        reset_fallback_counters();
        let dir = tempfile::tempdir().expect("private rendezvous directory");
        let previous_strict = std::env::var_os("KHIVE_DAEMON_STRICT");
        std::env::set_var("KHIVE_DAEMON_STRICT", if strict { "1" } else { "0" });
        std::env::set_var("KHIVE_PID", dir.path().join("p"));
        std::env::set_var("KHIVE_LOCK", dir.path().join("l"));
        std::env::set_var("KHIVE_RECOVERER_LOCK", dir.path().join("r"));
        let fixture = Self {
            dir,
            previous_strict,
        };
        fixture.activate_main();
        fixture
    }

    fn socket(&self) -> PathBuf {
        self.dir.path().join("s")
    }

    fn activate_main(&self) {
        std::env::set_var("KHIVE_SOCKET", self.socket());
    }
}

impl Drop for Rendezvous {
    fn drop(&mut self) {
        clear_daemon_env();
        match self.previous_strict.take() {
            Some(value) => std::env::set_var("KHIVE_DAEMON_STRICT", value),
            None => std::env::remove_var("KHIVE_DAEMON_STRICT"),
        }
        reset_counters();
        reset_fallback_counters();
    }
}

fn configurations() -> (RuntimeConfig, String, String) {
    let client = RuntimeConfig {
        db_path: None,
        packs: vec!["kg".into()],
        ..RuntimeConfig::no_embeddings()
    };
    let mut daemon = client.clone();
    daemon.packs.push("gtd".into());
    let client_id = compute_config_id(&client, None);
    let daemon_id = compute_config_id(&daemon, None);
    assert!(!config_ids_compatible(&client_id, &daemon_id));
    (client, client_id, daemon_id)
}

fn frame(config_id: &str) -> DaemonRequestFrame {
    DaemonRequestFrame {
        owned_namespaces: Vec::new(),
        plan: false,
        ops: "stats()".into(),
        presentation: None,
        presentation_per_op: None,
        namespace: "local".into(),
        actor_id: None,
        process_ref: None,
        visible_namespaces: Vec::new(),
        config_id: config_id.into(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: false,
        request_id: Some(6542),
    }
}

async fn start_daemon(
    rendezvous: &Rendezvous,
    config_id: &str,
) -> (HarnessDispatch, InProcessDaemonHandle) {
    rendezvous.activate_main();
    let dispatch = HarnessDispatch::new("local", config_id);
    let launcher = InProcessDaemonLauncher::new(dispatch.clone());
    let handle = launcher.launch().expect("launch real in-process receiver");
    drop(bounded(connect_when_ready(&rendezvous.socket())).await);
    assert!(!handle.is_finished());
    assert_eq!(launcher.launched_count(), 1);
    (dispatch, handle)
}

fn assert_no_process_actions() {
    assert_eq!(SPAWN_COUNT.load(Ordering::SeqCst), 0, "no auto-spawn");
    assert_eq!(
        KILL_COUNT.load(Ordering::SeqCst),
        0,
        "no stale-daemon recovery"
    );
    assert_eq!(SIGTERM_COUNT.load(Ordering::SeqCst), 0, "no signal attempt");
}

fn assert_one_fallback(strict: bool) {
    assert_eq!(fallback_count(FallbackReason::ConfigMismatch), 1);
    assert_eq!(
        fallback_total(),
        1,
        "one refusal must be recorded exactly once"
    );
    assert_eq!(fallback_strict_violations(), usize::from(strict));
    assert_no_process_actions();
}

fn assert_remedy(message: &str, pid: Option<u32>) {
    assert!(message.contains("reason=config_mismatch"), "{message}");
    let remedy = match pid {
        Some(pid) => format!(
            "daemon reports PID {pid}; stop or restart that daemon (through its supervisor if managed), then retry"
        ),
        None => "daemon PID is unavailable; stop or restart the daemon serving this configuration (through its supervisor if managed), then retry".into(),
    };
    assert!(message.ends_with(&remedy), "{message}");
    assert!(!message.contains(HOSTILE), "peer text must not be exposed");
    assert!(
        !message.contains("make local"),
        "rebuilding does not stop an old daemon"
    );
}

fn assert_strict_error(result: Option<Result<String, rmcp::ErrorData>>, pid: Option<u32>) {
    let error = result
        .expect("strict refusal cannot permit local fallback")
        .expect_err("incompatible configuration cannot dispatch");
    assert_eq!(error.code, rmcp::model::ErrorCode::INTERNAL_ERROR);
    assert_remedy(&error.message, pid);
    let data = error.data.expect("typed strict fallback detail");
    assert_eq!(data[STRICT_FALLBACK_MARKER], true);
    assert_eq!(data["reason"], "config_mismatch");
    assert_eq!(data["kind"], "internal");
    assert_eq!(data["domain_disposition"], "unknown");
    assert_eq!(data["message"], error.message.as_ref());
    match pid {
        Some(pid) => assert_eq!(data["daemon_pid"], pid),
        None => assert!(data.get("daemon_pid").is_none(), "{data}"),
    }
    assert!(!data.to_string().contains(HOSTILE));
}

#[derive(Clone, Copy, Debug)]
enum ResponseMode {
    ExplicitMismatch,
    MissingEcho,
    IncompatibleEcho,
    CompatibleStructuredError,
}

const RESPONSE_MODES: [ResponseMode; 3] = [
    ResponseMode::ExplicitMismatch,
    ResponseMode::MissingEcho,
    ResponseMode::IncompatibleEcho,
];

struct PidCase {
    name: &'static str,
    detail: Option<Value>,
    expected: Option<u32>,
}

fn pid_cases() -> Vec<PidCase> {
    let mut cases = vec![
        PidCase {
            name: "legacy missing detail",
            detail: None,
            expected: None,
        },
        PidCase {
            name: "legacy missing PID",
            detail: Some(json!({"message": HOSTILE})),
            expected: None,
        },
    ];
    for (name, value) in [
        ("null", Value::Null),
        ("zero", json!(0)),
        ("negative", json!(-1)),
        ("overflow", json!(u64::from(u32::MAX) + 1)),
        ("string", json!(HOSTILE)),
        ("numeric string", json!("123")),
        ("whole float", json!(123.0)),
        ("fractional", json!(1.5)),
        ("boolean", json!(true)),
        ("array", json!([123])),
        ("object", json!({"pid": 123})),
    ] {
        cases.push(PidCase {
            name,
            detail: Some(json!({"daemon_pid": value, "message": HOSTILE, "config": HOSTILE})),
            expected: None,
        });
    }
    let distinct = std::process::id().checked_add(1).unwrap_or(1);
    assert_ne!(distinct, std::process::id());
    cases.push(PidCase {
        name: "distinct reported PID",
        detail: Some(json!({"daemon_pid": distinct, "message": HOSTILE})),
        expected: Some(distinct),
    });
    cases
}

/// The listener is bound before the request, so readiness cannot consume its
/// sole exchange. Drop aborts the peer on assertion failure as well as timeout.
struct Peer(Option<JoinHandle<DaemonRequestFrame>>);

impl Peer {
    async fn finish(mut self) -> DaemonRequestFrame {
        let request = bounded(self.0.as_mut().expect("peer task"))
            .await
            .expect("framed peer must not panic");
        self.0.take();
        request
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

fn start_peer(
    rendezvous: &Rendezvous,
    sequence: usize,
    mode: ResponseMode,
    case: &PidCase,
    served_config_id: &str,
) -> Peer {
    let socket = rendezvous.dir.path().join(format!("s{sequence}"));
    let listener = UnixListener::bind(&socket).expect("bind private framed peer");
    std::env::set_var("KHIVE_SOCKET", socket);
    let detail = case.detail.clone();
    let served_config_id = served_config_id.to_owned();
    Peer(Some(tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept real forwarding");
        let bytes = read_frame(&mut stream)
            .await
            .expect("read real request frame");
        let request: DaemonRequestFrame = serde_json::from_slice(&bytes).expect("decode request");
        assert!(!request.probe_only && !request.metrics_only && !request.plan);
        assert_eq!(request.protocol_version, PROTOCOL_VERSION);
        let explicit = matches!(mode, ResponseMode::ExplicitMismatch);
        let success = !matches!(
            mode,
            ResponseMode::ExplicitMismatch | ResponseMode::CompatibleStructuredError
        );
        let mut response = json!({
            "ok": success,
            "result": if success { json!(HOSTILE) } else { Value::Null },
            "error": HOSTILE,
            "namespace_mismatch": false,
            "config_mismatch": explicit,
            "version_mismatch": false,
            "daemon_protocol_version": PROTOCOL_VERSION,
            "request_id": request.request_id,
        });
        if !matches!(mode, ResponseMode::MissingEcho) {
            response["served_config_id"] = json!(served_config_id);
        }
        if let Some(detail) = detail {
            response["error_detail"] = detail;
        }
        write_frame(&mut stream, &serde_json::to_vec(&response).unwrap())
            .await
            .expect("write real response frame");
        request
    })))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
#[serial_test::serial(config_ledger)]
async fn strict_config_mismatch_reports_daemon_pid_and_restart_remedy() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let rendezvous = Rendezvous::new(true);
    let (_, client_id, daemon_id) = configurations();
    let (dispatch, handle) = start_daemon(&rendezvous, &daemon_id).await;
    let request = frame(&client_id);
    let response = bounded(exchange(&rendezvous.socket(), &request)).await;
    assert!(!response.ok);
    assert!(response.config_mismatch);
    assert!(response.result.is_none());
    assert_eq!(
        response.served_config_id.as_deref(),
        Some(daemon_id.as_str())
    );
    let detail = response
        .error_detail
        .expect("real receiver mismatch metadata");
    assert_eq!(detail["daemon_pid"], std::process::id());
    assert_eq!(detail["code"], "config_mismatch");
    assert_eq!(detail["domain_disposition"], "not_committed");
    assert_eq!(dispatch.dispatch_count(), 0);
    assert_eq!(fallback_total(), 0, "raw exchange is not a client fallback");

    assert_strict_error(
        bounded(forward_or_spawn(&request)).await,
        Some(std::process::id()),
    );
    assert_one_fallback(true);
    assert_eq!(dispatch.dispatch_count(), 0);
    assert!(!handle.is_finished(), "refusal must not stop the receiver");

    // The harness returns raw dispatcher JSON, not a fabricated MCP envelope.
    let matching = bounded(forward_or_spawn(&frame(&daemon_id)))
        .await
        .expect("matching configuration remains on daemon")
        .expect("matching configuration succeeds");
    assert_eq!(matching, RAW_SUCCESS);
    assert_eq!(dispatch.dispatch_count(), 1);
    assert_one_fallback(true);
    assert!(!handle.is_finished());
    bounded(handle.stop()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
#[serial_test::serial(config_ledger)]
async fn config_mismatch_non_strict_preserves_local_fallback() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let rendezvous = Rendezvous::new(false);
    let (_, client_id, daemon_id) = configurations();
    let (dispatch, handle) = start_daemon(&rendezvous, &daemon_id).await;
    assert!(bounded(forward_or_spawn(&frame(&client_id)))
        .await
        .is_none());
    assert_one_fallback(false);
    assert_eq!(dispatch.dispatch_count(), 0);
    assert!(!handle.is_finished());
    bounded(handle.stop()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
#[serial_test::serial(config_ledger)]
async fn config_mismatch_legacy_and_invalid_pid_remain_actionable() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let rendezvous = Rendezvous::new(true);
    // Even a populated private PID file is not a substitute for response metadata.
    std::fs::write(
        rendezvous.dir.path().join("p"),
        std::process::id().to_string(),
    )
    .unwrap();
    let (_, client_id, daemon_id) = configurations();
    let request = frame(&client_id);
    let mut sequence = 0;
    for mode in RESPONSE_MODES {
        for case in pid_cases() {
            reset_fallback_counters();
            let peer = start_peer(&rendezvous, sequence, mode, &case, &daemon_id);
            sequence += 1;
            assert_strict_error(bounded(forward_or_spawn(&request)).await, case.expected);
            let received = peer.finish().await;
            assert_eq!(received.config_id, client_id, "{mode:?}/{}", case.name);
            assert_eq!(received.ops, request.ops);
            assert_one_fallback(true);
        }
    }
}

async fn note_snapshot(runtime: &KhiveRuntime) -> Vec<Note> {
    let token = runtime
        .authorize(Namespace::local())
        .expect("local snapshot token");
    let page = bounded(runtime.notes(&token).unwrap().query_notes(
        "local",
        None,
        PageRequest {
            offset: 0,
            limit: 100,
        },
    ))
    .await
    .expect("complete local note snapshot");
    assert_eq!(page.total, Some(page.items.len() as u64));
    let mut notes = page.items;
    notes.sort_by_key(|note| note.id);
    notes
}

fn params(ops: &str) -> RequestParams {
    RequestParams {
        ops: ops.into(),
        presentation: Some("verbose".into()),
        format: Some("json".into()),
        ..Default::default()
    }
}

fn assert_envelope(wire: &str, total: usize, aborted: usize, pid: Option<u32>) {
    let body: Value = serde_json::from_str(wire).expect("normal MCP JSON envelope");
    assert_eq!(body["status"], "partial");
    assert_eq!(
        body["summary"],
        json!({"total": total, "succeeded": 0, "failed": total - aborted, "aborted": aborted})
    );
    let entries = body["results"].as_array().expect("failed-op results");
    assert_eq!(entries.len(), total);
    for (index, entry) in entries.iter().enumerate() {
        assert_eq!(entry["tool"], "create");
        assert_eq!(entry["ok"], false);
        assert_eq!(entry["domain_disposition"], "not_committed");
        assert!(entry.get("result").is_none());
        if index >= total - aborted {
            assert_eq!(entry["aborted"], true);
            assert!(entry.get("error").is_none());
        } else {
            assert!(entry.get("aborted").is_none());
            assert_eq!(entry["error"]["domain_disposition"], "not_committed");
            assert_remedy(entry["error"]["message"].as_str().unwrap(), pid);
        }
    }
    assert!(!wire.contains(HOSTILE));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial]
#[serial_test::serial(config_ledger)]
async fn config_mismatch_remedy_reaches_final_mcp_envelope_without_dispatch() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let rendezvous = Rendezvous::new(true);
    let (config, client_id, daemon_id) = configurations();
    let runtime = KhiveRuntime::new(config).expect("private client store");
    let token = runtime.authorize(Namespace::local()).unwrap();
    runtime
        .notes(&token)
        .unwrap()
        .upsert_note(Note::new(
            "local",
            "observation",
            "populated state must remain byte-for-byte equal",
        ))
        .await
        .unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    let registry = builder.build().expect("real KG registry");
    runtime.install_edge_rules(registry.all_edge_rules());
    // This constructor leaves the host runtime unset: forwarding remains live,
    // while an erroneous local fallback can execute real note writes.
    let server = KhiveMcpServer::from_registry_with_meta(registry, "local", &client_id);
    let before = note_snapshot(&runtime).await;
    assert_eq!(before.len(), 1);
    std::fs::write(
        rendezvous.dir.path().join("p"),
        std::process::id().to_string(),
    )
    .unwrap();

    let mut sequence = 0;
    for mode in RESPONSE_MODES {
        for case in pid_cases() {
            reset_fallback_counters();
            let peer = start_peer(&rendezvous, sequence, mode, &case, &daemon_id);
            sequence += 1;
            let wire = bounded(server.request_for_config_lock_test(params(CREATE)))
                .await
                .expect("strict fallback is a failed-op envelope, not an RPC error");
            assert_envelope(&wire, 1, 0, case.expected);
            let received = peer.finish().await;
            assert_eq!(received.config_id, client_id, "{mode:?}/{}", case.name);
            assert_eq!(received.ops, CREATE);
            assert_one_fallback(true);
            assert_eq!(note_snapshot(&runtime).await, before);
        }
    }

    // A compatible ordinary error reaches the bridge without the config
    // wrapper rewriting its message. This independently proves that final
    // projection reconstructs the remedy instead of copying peer text.
    let reported_pid = std::process::id().checked_add(1).unwrap_or(1);
    assert_ne!(reported_pid, std::process::id());
    let structured = PidCase {
        name: "compatible hostile structured error",
        detail: Some(json!({
            STRICT_FALLBACK_MARKER: true,
            "reason": "config_mismatch",
            "daemon_pid": reported_pid,
            "message": HOSTILE,
        })),
        expected: Some(reported_pid),
    };
    reset_fallback_counters();
    let peer = start_peer(
        &rendezvous,
        sequence,
        ResponseMode::CompatibleStructuredError,
        &structured,
        &client_id,
    );
    let wire = bounded(server.request_for_config_lock_test(params(CREATE)))
        .await
        .expect("typed ordinary error reaches the final strict envelope");
    assert_envelope(&wire, 1, 0, structured.expected);
    let received = peer.finish().await;
    assert_eq!(received.config_id, client_id);
    assert_eq!(received.ops, CREATE);
    assert_eq!(
        fallback_total(),
        0,
        "no config wrapper ran for compatible echo"
    );
    assert_eq!(fallback_strict_violations(), 0);
    assert_no_process_actions();
    assert_eq!(note_snapshot(&runtime).await, before);

    std::fs::remove_file(rendezvous.dir.path().join("p")).unwrap();
    let (dispatch, handle) = start_daemon(&rendezvous, &daemon_id).await;
    for (ops, total, aborted) in [
        (CREATE.to_owned(), 1, 0),
        (format!("[{CREATE}, {CREATE}]"), 2, 0),
        (format!("{CREATE} | {CREATE}"), 2, 1),
    ] {
        reset_fallback_counters();
        let wire = bounded(server.request_for_config_lock_test(params(&ops)))
            .await
            .expect("normal strict failed-op envelope");
        assert_envelope(&wire, total, aborted, Some(std::process::id()));
        assert_one_fallback(true);
        assert_eq!(dispatch.dispatch_count(), 0);
        assert_eq!(note_snapshot(&runtime).await, before);
        assert!(!handle.is_finished());
    }
    bounded(handle.stop()).await;

    // Prove the same observation operation is a valid write when the caller
    // explicitly selects local execution. Invalid syntax is not our no-write oracle.
    std::env::set_var("KHIVE_NO_DAEMON", "1");
    let local: Value = serde_json::from_str(
        &bounded(server.request_for_config_lock_test(params(CREATE)))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(local["results"][0]["ok"], true, "{local}");
    let after = note_snapshot(&runtime).await;
    assert_eq!(after.len(), before.len() + 1);
    assert!(after.contains(&before[0]));
    assert_no_process_actions();
}
