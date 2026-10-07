use super::{
    attach_audit_persistence_advisories, backend_errors_value, bounded_backend_error_key,
    bounded_backend_error_message, build_instructions, build_verb_catalog,
    canonical_fingerprint_path, chain_aggregation_depth_reject, chain_ok_envelope_or_depth_error,
    compute_config_id, compute_config_id_with_ann_fresh_tail,
    compute_config_id_with_runtime_policies, compute_config_id_with_storage_mode,
    coordinator_search_visibility, dsl_err_to_mcp, empty_rendered_daemon_frame_len,
    encode_backend_topology, ensure_bridge_request_id, entry_escaped_len, envelope_escaped_len,
    envelope_metadata, envelope_metadata_escaped_len, execute_bounded_batch,
    fit_rendered_batch_envelope, format_served_kinds_suffix, frame_budget_omission,
    note_content_scope, ok_envelope, parallel_batch_envelope, present_ok_envelope_or_depth_error,
    render_result, rendered_response_daemon_frame_len, rendered_response_fits_daemon_frame,
    request_read_timeout, result_exceeds_depth_limit, runtime_error_value,
    scope_mcp_request_read_cancellation, search_diagnostic_value, search_diagnostic_wire_len,
    search_retry_after_ms, serialize_response_value, serialized_response_len,
    BackendErrorDiagnostic, BatchTask, DispatchOrigin, KhiveMcpServer, OpSuccess, RenderContext,
    RunParsedContext, SearchArmEvidence, SearchArmParticipation, SearchArmStatus,
    SearchDegradation, SearchStatus, BATCH_RESPONSE_BUDGET_BYTES, MAX_BACKEND_ERROR_ENTRIES,
    MAX_BACKEND_ERROR_KEY_CHARS, MAX_BACKEND_ERROR_MESSAGE_CHARS, MAX_BATCH_CONCURRENCY,
    MAX_SEARCH_DIAGNOSTIC_BYTES_PER_OP, MISSING_BACKEND_ERROR_MESSAGE,
};
#[cfg(unix)]
use super::{stdio_serve_mode_for, ForwardFuture, StdioServeMode};
use crate::coordinator::{
    BackendSearchFailure, BackendSearchFailureKind, CoordSearchResult, CoordinatorService,
};
use crate::tools::request::RequestParams;
use khive_request::{parse_request, ExecutionMode, TypedJsonOp};
use khive_runtime::presentation::NoteContentScope;
use khive_runtime::{
    render_format, DomainDisposition, KhiveRuntime, Namespace, OutputFormat, PresentationMode,
    RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use rmcp::{handler::server::wrapper::Parameters, ErrorData as McpError};
use serde_json::{json, Value};
use std::{collections::BTreeMap, future::Future, sync::Arc};
include!("server/plan_tests.rs");
include!("server/search_text_reason_tests.rs");
include!("server/search_text_mode_tests.rs");
include!("server/search_ranking_tests.rs");
use khive_storage::{EventFilter, PageRequest};
use serial_test::serial;

#[test]
fn daemon_timeout_marker_requires_successful_knowledge_result_flag() {
    let mut response = json!({
        "results": [
            {"ok": false, "tool": "knowledge.search", "result": {
                "degraded": {"lexical_timeout": true}
            }},
            {"ok": true, "tool": "stats", "result": {
                "degraded": {"lexical_timeout": true}
            }},
            {"ok": true, "tool": "knowledge.search", "result": {
                "content": "lexical_timeout: true", "degraded": {"lexical_timeout": false}
            }}
        ]
    });
    super::mark_daemon_lexical_timeout(&mut response);
    assert!(response.get(super::DAEMON_LEXICAL_TIMEOUT_MARKER).is_none());
    response["results"][2]["result"]["degraded"]["lexical_timeout"] = json!(true);
    super::mark_daemon_lexical_timeout(&mut response);
    assert_eq!(response[super::DAEMON_LEXICAL_TIMEOUT_MARKER], true);
}

#[test]
fn per_op_overrides_are_bounded_and_validated_before_forwarding() {
    let mut p = RequestParams {
        ops: "stats()".into(),
        presentation_per_op: Some(vec![None, None]),
        ..Default::default()
    };
    let error = super::validate_request_overrides(&p, 1).unwrap_err();
    assert!(error.message.contains("presentation_per_op"));

    p.presentation_per_op = Some(vec![Some("bogus".into())]);
    let error = super::validate_request_overrides(&p, 1).unwrap_err();
    assert!(error.message.contains("unknown presentation mode"));

    p.presentation_per_op = None;
    p.format_per_op = Some(vec![None, None]);
    let error = super::validate_request_overrides(&p, 1).unwrap_err();
    assert!(error.message.contains("format_per_op"));

    p.format_per_op = Some(vec![Some("bogus".into())]);
    let error = super::validate_request_overrides(&p, 1).unwrap_err();
    assert!(error.message.contains("unknown output format"));

    p.format_per_op = Some(vec![Some("table".into())]);
    super::validate_request_overrides(&p, 1).unwrap();
}

#[test]
fn remember_key_named_disposition_preserves_details_and_other_errors() {
    let id = uuid::Uuid::new_v4().to_string();
    let key = "k".repeat(512);
    let error =
        khive_types::KhiveError::conflict("held").with_details(khive_types::Details::new_owned([
            ("reason", "key_conflict".to_owned()),
            ("key", key.clone()),
            ("existing_id", id.clone()),
        ]));
    let value = runtime_error_value(error.into(), DomainDisposition::Committed);
    assert_eq!(value["kind"], "conflict");
    assert_eq!(value["domain_disposition"], "not_committed");
    assert_eq!(value["details"]["key"], key);
    assert_eq!(value["details"]["existing_id"], id);

    let unresolved = khive_types::KhiveError::unavailable("holder missing").with_details(
        khive_types::Details::new_owned([
            ("reason", "key_holder_unresolved".to_owned()),
            ("key", String::new()),
        ]),
    );
    assert_eq!(
        runtime_error_value(unresolved.into(), DomainDisposition::Committed)["domain_disposition"],
        "unknown"
    );
    for error in [
        khive_types::KhiveError::conflict("unrelated"),
        khive_types::KhiveError::unavailable("unrelated"),
    ] {
        let mut expected = serde_json::to_value(&error).unwrap();
        expected["domain_disposition"] = json!("unknown");
        assert_eq!(
            runtime_error_value(error.into(), DomainDisposition::Unknown),
            expected
        );
    }
}

#[derive(Clone, Default)]
struct SearchCapturedLog(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for SearchCapturedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("captured search log mutex poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SearchCapturedLog {
    type Writer = SearchCapturedLog;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl SearchCapturedLog {
    fn contents(&self) -> String {
        String::from_utf8(
            self.0
                .lock()
                .expect("captured search log mutex poisoned")
                .clone(),
        )
        .expect("captured search logs are UTF-8")
    }
}

#[cfg(unix)]
use khive_storage::test_support::freeze_snapshot_sidecars;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

#[test]
fn dsl_parse_errors_carry_stable_reason_data() {
    let parse_error = parse_request("stats(").expect_err("input must be malformed");
    let error = dsl_err_to_mcp(parse_error);
    assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    assert_eq!(
        error.data.as_ref().and_then(|data| data["reason"].as_str()),
        Some("parse-error")
    );
}

include!("server_forward_boundary_tests.rs");

/// khive-oss#1941 regression seam: `request_with_forward` must pass
/// THIS server's own resolved registry pack list to the daemon-forwarding
/// seam as `Some(...)`, not `None` and not some other list. The spy's
/// signature is `Option<Vec<String>>` — the exact shape
/// `forward_or_spawn_with_config_and_packs` itself receives — because the
/// `Some`/`None` decision is made at the shared call site in
/// `request_with_forward` before `forward_fn` is invoked, not inside the
/// production adapter (`forward_or_spawn_boxed`) that this spy replaces.
/// That adapter is now a pure pass-through with no logic of its own, so
/// this spy observes precisely what the real
/// `forward_or_spawn_with_config_and_packs` call would receive. Two independent
/// mutations must both redden this test:
/// - swapping the production `forward_fn(frame, Some(resolved_packs))`
///   call for `forward_fn(frame, Some(Vec::new()))` — the restricted
///   two-pack registry built here (`kg`, `gtd`) no longer matches what
///   the spy records;
/// - swapping that same call for `forward_fn(frame, None)` — the spy
///   records `None` instead of `Some(vec!["kg", "gtd"])`.
#[cfg(unix)]
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn restricted_registry_pack_list_reaches_forward_seam() {
    thread_local! {
        static SPY_CAPTURED_PACKS: std::cell::RefCell<Option<Option<Vec<String>>>> =
            const { std::cell::RefCell::new(None) };
    }

    fn spy_forward(
        _frame: khive_runtime::DaemonRequestFrame,
        packs: Option<Vec<String>>,
        _replay_read_only: bool,
    ) -> ForwardFuture {
        SPY_CAPTURED_PACKS.with(|c| *c.borrow_mut() = Some(packs));
        Box::pin(async {
            Some(Ok(json!({
                "results": [{"ok": true, "tool": "stats", "result": {}}],
                "summary": {"total": 1, "succeeded": 1, "failed": 0},
            })
            .to_string()))
        })
    }

    let dir = tempfile::tempdir().expect("forwarding fixture directory");
    let runtime = forward_test_runtime(Some(dir.path().join("main.db")), &["kg", "gtd"]);
    let server =
        KhiveMcpServer::new(runtime).expect("server builds with restricted kg+gtd registry");
    SPY_CAPTURED_PACKS.with(|c| *c.borrow_mut() = None);

    let params = RequestParams {
        plan: None,
        ops: "stats()".to_string(),
        presentation: None,
        presentation_per_op: None,
        save_to: None,
        format: None,
        format_per_op: None,
        request_id: None,
    };

    let result = server.request_with_forward(params, spy_forward).await;

    assert!(
        result.is_ok(),
        "the spy-forwarded dispatch must succeed: {result:?}"
    );
    assert_eq!(
        SPY_CAPTURED_PACKS.with(|captured| captured.borrow_mut().take()),
        Some(Some(vec!["kg".to_string(), "gtd".to_string()])),
        "the server's own resolved registry pack set must reach the daemon-forwarding \
             seam as Some(...) so a daemon this call spawns serves the same packs this \
             process registered"
    );
}

#[cfg(unix)]
mod read_replay_tests {
    use super::super::{ForwardFuture, KhiveMcpServer};
    use super::{clear_daemon_env, forward_test_runtime, stats_without_request_local_usage};
    use crate::tools::request::RequestParams;
    use khive_runtime::{RuntimeError, VerbRegistry, VerbRegistryBuilder};
    use rmcp::handler::server::wrapper::Parameters;
    use serde_json::Value;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    thread_local! {
        static CAPTURED_FORWARD: std::cell::RefCell<Option<(String, bool)>> =
            const { std::cell::RefCell::new(None) };
    }

    fn capture_forward_policy(
        frame: khive_runtime::DaemonRequestFrame,
        _packs: Option<Vec<String>>,
        replay_read_only: bool,
    ) -> ForwardFuture {
        CAPTURED_FORWARD.with(|capture| {
            *capture.borrow_mut() = Some((frame.ops, replay_read_only));
        });
        Box::pin(async { Some(Ok("forwarded-policy-fixture".to_string())) })
    }

    fn live_server() -> (tempfile::TempDir, KhiveMcpServer) {
        let dir = tempfile::tempdir().expect("replay fixture directory");
        let runtime =
            forward_test_runtime(Some(dir.path().join("main.db")), &["kg", "comm", "memory"]);
        let server = KhiveMcpServer::new(runtime).expect("live kg, comm and memory handlers");
        (dir, server)
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn request_forward_policy_requires_every_operation_to_be_an_opted_in_read() {
        let (_dir, server) = live_server();
        let cases = [
            ("stats()", true),
            (
                "comm.thread(id=\"00000000-0000-0000-0000-000000000001\")",
                true,
            ),
            ("comm.inbox(wait_ms=30000, box=\"sent\", limit=1)", true),
            ("comm.unread()", true),
            (
                "comm.delivered(id=\"00000000-0000-0000-0000-000000000001\")",
                true,
            ),
            ("[stats(), comm.unread(), comm.inbox(limit=1)]", true),
            ("stats() | comm.unread()", true),
            (
                "comm.thread(id=\"00000000-0000-0000-0000-000000000001\") | \
                     comm.thread(id=$prev.thread_id) | comm.thread(id=$prev.thread_id)",
                true,
            ),
            (
                r#"[{"tool":"stats"},{"tool":"comm.unread","args":{}}]"#,
                true,
            ),
            ("stats(help=true)", true),
            ("comm.send(to=\"bob\", content=\"policy-fixture\")", false),
            (
                "[comm.unread(), comm.send(to=\"bob\", content=\"policy-fixture\")]",
                false,
            ),
            (
                "comm.send(to=\"bob\", content=\"policy-fixture\") | comm.thread(id=$prev.id)",
                false,
            ),
            ("[stats(), unknown.read()]", false),
            ("unknown.read()", false),
            ("unknown.read(help=true)", false),
            ("comm.send(help=true)", false),
            ("stats() | comm.send(help=$prev.help)", false),
            ("memory.prune(dry_run=true)", false),
            ("merge(dry_run=true)", false),
            (
                "comm.mark_read(ids=[\"00000000-0000-0000-0000-000000000001\"], atomic=true)",
                false,
            ),
            // These assertive reads append fresh durable search/serve
            // records, so transport cannot replay them after a lost reply.
            ("search(kind=\"entity\", query=\"policy-fixture\")", false),
            ("memory.recall(query=\"policy-fixture\")", false),
            ("get(id=\"00000000-0000-0000-0000-000000000001\")", true),
        ];

        for (ops, expected) in cases {
            CAPTURED_FORWARD.with(|capture| *capture.borrow_mut() = None);
            let response = server
                .request_with_forward(
                    RequestParams {
                        ops: ops.to_string(),
                        ..Default::default()
                    },
                    capture_forward_policy,
                )
                .await
                .unwrap_or_else(|error| panic!("forward preflight rejected {ops}: {error}"));
            assert_eq!(response, "forwarded-policy-fixture");
            assert_eq!(
                CAPTURED_FORWARD.with(|capture| capture.borrow_mut().take()),
                Some((ops.to_string(), expected)),
                "forwarded replay policy for {ops}",
            );
        }
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn malformed_and_atomic_wrapper_requests_never_reach_forwarding() {
        let (_dir, server) = live_server();
        for ops in [
            "",
            "stats(",
            "[stats(), comm.unread()",
            r#"{"atomic":true,"ops":[{"tool":"stats"}]}"#,
        ] {
            CAPTURED_FORWARD.with(|capture| *capture.borrow_mut() = None);
            let error = server
                .request_with_forward(
                    RequestParams {
                        ops: ops.to_string(),
                        ..Default::default()
                    },
                    capture_forward_policy,
                )
                .await
                .expect_err("invalid DSL must fail before forwarding");
            assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
            assert_eq!(
                error.data.as_ref().and_then(|data| data["reason"].as_str()),
                Some("parse-error"),
            );
            assert!(CAPTURED_FORWARD.with(|capture| capture.borrow().is_none()));
        }
    }

    struct ImpostorCommPack;

    impl khive_types::Pack for ImpostorCommPack {
        const NAME: &'static str = "comm";
        const NOTE_KINDS: &'static [&'static str] = &[];
        const ENTITY_KINDS: &'static [&'static str] = &[];
        const HANDLERS: &'static [khive_runtime::HandlerDef] = &[khive_runtime::HandlerDef {
            name: "comm.thread",
            description: "untrusted same-name replay fixture",
            visibility: khive_runtime::Visibility::Verb,
            category: khive_runtime::VerbCategory::Assertive,
            params: &[],
        }];
    }

    #[async_trait::async_trait]
    impl khive_runtime::PackRuntime for ImpostorCommPack {
        fn name(&self) -> &str {
            <Self as khive_types::Pack>::NAME
        }

        fn note_kinds(&self) -> &'static [&'static str] {
            &[]
        }

        fn entity_kinds(&self) -> &'static [&'static str] {
            &[]
        }

        fn handlers(&self) -> &'static [khive_runtime::HandlerDef] {
            <Self as khive_types::Pack>::HANDLERS
        }

        async fn dispatch(
            &self,
            _verb: &str,
            _params: Value,
            _registry: &VerbRegistry,
            _token: &khive_runtime::NamespaceToken,
        ) -> Result<Value, RuntimeError> {
            panic!("the forwarding policy fixture must not dispatch locally")
        }
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn custom_same_name_pack_cannot_enable_replay_at_request_boundary() {
        let mut builder = VerbRegistryBuilder::new();
        builder.register(ImpostorCommPack);
        let registry = builder.build().expect("custom comm fixture registry");
        assert_eq!(
            registry.verb_category("comm.thread"),
            Some(khive_runtime::VerbCategory::Assertive),
        );
        let server = KhiveMcpServer::from_registry(registry);
        let ops = "comm.thread(id=\"00000000-0000-0000-0000-000000000001\")";
        CAPTURED_FORWARD.with(|capture| *capture.borrow_mut() = None);
        server
            .request_with_forward(
                RequestParams {
                    ops: ops.to_string(),
                    ..Default::default()
                },
                capture_forward_policy,
            )
            .await
            .expect("custom call reaches forwarding without replay permission");
        assert_eq!(
            CAPTURED_FORWARD.with(|capture| capture.borrow_mut().take()),
            Some((ops.to_string(), false)),
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    #[serial_test::serial(config_ledger)]
    async fn mixed_comm_batch_commits_once_when_its_daemon_response_is_lost() {
        if crate::test_isolation::rerun_with_private_home() {
            return;
        }

        clear_daemon_env();
        let dir = tempfile::tempdir().expect("mixed batch socket directory");
        let socket = dir.path().join("khived.sock");
        std::env::set_var("KHIVE_SOCKET", &socket);
        std::env::set_var("KHIVE_PID", dir.path().join("khived.pid"));
        let (_client_dir, client) = live_server();
        let (_daemon_dir, daemon) = live_server();
        let baseline = client
            .dispatch_request_local(RequestParams {
                ops: "stats()".to_string(),
                ..Default::default()
            })
            .await
            .expect("client baseline");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind fake daemon");
        let mutations = Arc::new(AtomicUsize::new(0));
        let observed_mutations = Arc::clone(&mutations);
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
        let daemon_task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = tokio::select! {
                    _ = &mut stop_rx => break,
                    incoming = listener.accept() => incoming.expect("accept mixed batch"),
                };
                let payload = khive_runtime::daemon::read_frame(&mut stream)
                    .await
                    .expect("receive full mixed batch");
                let frame: khive_runtime::DaemonRequestFrame =
                    serde_json::from_slice(&payload).expect("decode mixed batch frame");
                assert!(
                    !frame.probe_only,
                    "mixed response loss must not trigger recovery"
                );
                let response = daemon
                    .dispatch_request_local(RequestParams {
                        ops: frame.ops,
                        ..Default::default()
                    })
                    .await
                    .expect("execute mixed batch before losing the response");
                let envelope: Value = serde_json::from_str(&response).expect("batch response");
                assert_eq!(envelope["summary"]["succeeded"], 2, "{envelope}");
                let committed = envelope["results"]
                    .as_array()
                    .expect("batch entries")
                    .iter()
                    .filter(|entry| entry["tool"] == "comm.send" && entry["ok"] == true)
                    .count();
                observed_mutations.fetch_add(committed, Ordering::SeqCst);
                drop(stream);
            }
        });

        let result = client
            .request(
                Parameters(RequestParams {
                    ops: "[comm.unread(), comm.send(to=\"bob\", content=\"mixed-replay-fixture\")]"
                        .to_string(),
                    ..Default::default()
                }),
                tokio_util::sync::CancellationToken::new(),
            )
            .await;
        let _ = stop_tx.send(());
        let stopped = daemon_task.await;
        clear_daemon_env();
        stopped.expect("fake daemon exits cleanly");

        let error = result.expect_err("the mixed batch must preserve its ambiguous outcome");
        assert!(error.message.contains("not retrying"), "{error}");
        assert_eq!(
            mutations.load(Ordering::SeqCst),
            1,
            "comm.send committed more than once"
        );
        let after = client
            .dispatch_request_local(RequestParams {
                ops: "stats()".to_string(),
                ..Default::default()
            })
            .await
            .expect("client state after remote response loss");
        assert_eq!(
            stats_without_request_local_usage(&after),
            stats_without_request_local_usage(&baseline),
            "a lost mixed response must not dispatch comm.send locally",
        );
    }
}

/// A cancellation notification after daemon admission must not replace the
/// daemon's actual per-op outcome with a bare RPC-level error. The daemon
/// response is the only source that can say which independent operations
/// committed and which failed validation.
#[cfg(unix)]
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn forwarded_cancellation_returns_the_actual_partial_envelope() {
    thread_local! {
        static FORWARD_STARTED: std::cell::RefCell<Option<Arc<tokio::sync::Notify>>> =
            const { std::cell::RefCell::new(None) };
        static FORWARD_RELEASE: std::cell::RefCell<Option<Arc<tokio::sync::Notify>>> =
            const { std::cell::RefCell::new(None) };
    }

    // Gated on an explicit release signal rather than a sleep, so the
    // test can prove the ordering it actually needs (cancellation must
    // land while the forward is still outstanding) instead of hoping a
    // fixed delay is long enough.
    fn delayed_partial_forward(
        _frame: khive_runtime::DaemonRequestFrame,
        _packs: Option<Vec<String>>,
        _replay_read_only: bool,
    ) -> ForwardFuture {
        let started = FORWARD_STARTED
            .with(|c| c.borrow().clone())
            .expect("started notify armed");
        let release = FORWARD_RELEASE
            .with(|c| c.borrow().clone())
            .expect("release notify armed");
        Box::pin(async move {
            started.notify_one();
            release.notified().await;
            Some(Ok(json!({
                "results": [
                    {"ok": true, "tool": "comm.send", "result": {"id": "sent"}},
                    {"ok": false, "tool": "link", "error": "invalid endpoint pair"}
                ],
                "summary": {"total": 2, "succeeded": 1, "failed": 1, "aborted": 0},
                "status": "partial"
            })
            .to_string()))
        })
    }

    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    FORWARD_STARTED.with(|c| *c.borrow_mut() = Some(started.clone()));
    FORWARD_RELEASE.with(|c| *c.borrow_mut() = Some(release.clone()));

    let dir = tempfile::tempdir().expect("forwarding fixture directory");
    let runtime = forward_test_runtime(Some(dir.path().join("main.db")), &["kg"]);
    let server = KhiveMcpServer::new(runtime).expect("server builds with kg");
    let cancellation = tokio_util::sync::CancellationToken::new();
    let cancel_after_admission = cancellation.clone();
    // The forward cannot possibly complete before `release` fires below,
    // so once this task observes admission and cancels, the handler's
    // select is guaranteed to take the cancellation arm — no race with
    // the task-result arm is possible.
    let canceller = tokio::spawn(async move {
        started.notified().await;
        cancel_after_admission.cancel();
        release.notify_one();
    });

    let response = scope_mcp_request_read_cancellation(
        cancellation,
        server.request_with_forward(
            RequestParams {
                ops: "[stats(), stats()]".to_string(),
                ..Default::default()
            },
            delayed_partial_forward,
        ),
    )
    .await
    .expect("cancellation after admission must preserve the daemon response");
    canceller.await.expect("canceller task completes");

    let response: Value = serde_json::from_str(&response).expect("response envelope is JSON");
    assert_eq!(response["status"], "partial");
    assert_eq!(response["summary"]["succeeded"], 1);
    assert_eq!(response["summary"]["failed"], 1);
    assert_eq!(response["results"][1]["error"], "invalid endpoint pair");
}

/// The pre-admission cancellation check must run before the `save_to`
/// bypass branch, not inside it: a cancelled request that happens to
/// carry `save_to` must never fall through to local dispatch and start
/// mutating work before cancellation is ever checked. `save_to` bypasses
/// daemon forwarding either way (MCP-AUD-002), so the forward seam here
/// exists only to prove it is never reached; the real assertion is that
/// nothing was created.
#[cfg(unix)]
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn cancelled_request_with_save_to_refuses_before_local_dispatch() {
    fn unreachable_forward(
        _frame: khive_runtime::DaemonRequestFrame,
        _packs: Option<Vec<String>>,
        _replay_read_only: bool,
    ) -> ForwardFuture {
        panic!("save_to must bypass daemon forwarding regardless of cancellation");
    }

    let dir = tempfile::tempdir().expect("forwarding fixture directory");
    let runtime = forward_test_runtime(Some(dir.path().join("main.db")), &["kg"]);
    let server = KhiveMcpServer::new(runtime).expect("server builds with kg");

    let (_tx, cancelled_rx) = tokio::sync::watch::channel(true);
    let result = khive_storage::scope_request_read_cancellation(cancelled_rx, async {
        server
            .request_with_forward(
                RequestParams {
                    ops: "create(kind=\"entity\", entity_kind=\"concept\", \
                              name=\"fix2337-cancelled-save-to-probe\")"
                        .to_string(),
                    save_to: Some("unused.jsonl".to_string()),
                    ..Default::default()
                },
                unreachable_forward,
            )
            .await
    })
    .await;

    let error =
        result.expect_err("a cancelled request with save_to set must be refused before dispatch");
    assert!(
        error.message.contains("cancelled"),
        "unexpected error message: {error}"
    );

    let stats = server
        .dispatch_request_local(RequestParams {
            ops: "stats()".to_string(),
            ..Default::default()
        })
        .await
        .expect("probe stats() must succeed");
    let stats: Value = serde_json::from_str(&stats).expect("probe response is JSON");
    assert_eq!(
        stats["results"][0]["result"]["entities"], 0,
        "cancellation before save_to dispatch must mean no entity was ever \
             created: {stats}"
    );
}

/// `tokio::select!` is unbiased: when a forward seam sets the
/// cancellation flag itself and then resolves to `None` in the same
/// poll, the task-result arm can win the select exactly as often as the
/// cancellation arm (both branches become ready together). The old code
/// only refused local fallback when the cancellation *arm* had won
/// (`cancelled_during_forward`); this loops enough attempts to land the
/// task-result-arm outcome at least once and asserts every attempt
/// refuses regardless of which arm actually won, so the fix does not
/// depend on getting lucky with the race.
#[cfg(unix)]
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn cancellation_visible_after_forward_returns_none_refuses_local_dispatch() {
    thread_local! {
        static CANCEL_TX: std::cell::RefCell<Option<tokio::sync::watch::Sender<bool>>> =
            const { std::cell::RefCell::new(None) };
    }

    fn cancel_then_none_forward(
        _frame: khive_runtime::DaemonRequestFrame,
        _packs: Option<Vec<String>>,
        _replay_read_only: bool,
    ) -> ForwardFuture {
        Box::pin(async move {
            CANCEL_TX.with(|c| {
                c.borrow()
                    .as_ref()
                    .expect("cancel sender armed")
                    .send(true)
                    .expect("cancellation receiver still live")
            });
            None
        })
    }

    for attempt in 0..50 {
        let dir = tempfile::tempdir().expect("forwarding fixture directory");
        let runtime = forward_test_runtime(Some(dir.path().join("main.db")), &["kg"]);
        let server = KhiveMcpServer::new(runtime).expect("server builds with kg");

        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        CANCEL_TX.with(|c| *c.borrow_mut() = Some(cancel_tx));

        let result = khive_storage::scope_request_read_cancellation(
            cancel_rx,
            server.request_with_forward(
                RequestParams {
                    ops: "create(kind=\"entity\", entity_kind=\"concept\", \
                              name=\"fix2337-none-race-probe\")"
                        .to_string(),
                    ..Default::default()
                },
                cancel_then_none_forward,
            ),
        )
        .await;

        let error = result.expect_err(&format!(
            "attempt {attempt}: cancellation visible after the forward returns None \
                 must refuse local dispatch, not run it"
        ));
        assert_eq!(
            error.data.as_ref().map(|d| d["outcome"].clone()),
            Some(json!("not_dispatched")),
            "attempt {attempt}: unexpected error data: {error:?}"
        );

        let stats = server
            .dispatch_request_local(RequestParams {
                ops: "stats()".to_string(),
                ..Default::default()
            })
            .await
            .expect("probe stats() must succeed");
        let stats: Value = serde_json::from_str(&stats).expect("probe response is JSON");
        assert_eq!(
            stats["results"][0]["result"]["entities"], 0,
            "attempt {attempt}: cancellation visible after None must mean no entity \
                 was ever created: {stats}"
        );
    }
}

/// Once daemon forwarding is admitted, dropping the outer MCP handler
/// must detach rather than cancel the socket exchange. Otherwise the
/// daemon can commit while the bridge silently discards the only result.
#[cfg(unix)]
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn dropping_handler_does_not_drop_an_admitted_forward() {
    thread_local! {
        static FORWARD_STARTED: std::cell::RefCell<Option<Arc<tokio::sync::Notify>>> =
            const { std::cell::RefCell::new(None) };
        static FORWARD_RELEASE: std::cell::RefCell<Option<Arc<tokio::sync::Notify>>> =
            const { std::cell::RefCell::new(None) };
        static FORWARD_COMPLETED: std::cell::RefCell<Option<Arc<tokio::sync::Notify>>> =
            const { std::cell::RefCell::new(None) };
    }

    // Gated on explicit release/completion signals rather than sleeps:
    // the forward only proceeds past admission once the test releases
    // it (after aborting the outer handler), and completion is observed
    // by waiting on a notification instead of hoping a fixed delay was
    // long enough for the detached task to finish.
    fn slow_forward(
        _frame: khive_runtime::DaemonRequestFrame,
        _packs: Option<Vec<String>>,
        _replay_read_only: bool,
    ) -> ForwardFuture {
        let started = FORWARD_STARTED
            .with(|c| c.borrow().clone())
            .expect("started notify armed");
        let release = FORWARD_RELEASE
            .with(|c| c.borrow().clone())
            .expect("release notify armed");
        let completed = FORWARD_COMPLETED
            .with(|c| c.borrow().clone())
            .expect("completed notify armed");
        Box::pin(async move {
            started.notify_one();
            release.notified().await;
            completed.notify_one();
            Some(Ok(json!({
                "results": [{"ok": true, "tool": "stats", "result": {}}],
                "summary": {"total": 1, "succeeded": 1, "failed": 0, "aborted": 0},
                "status": "success"
            })
            .to_string()))
        })
    }

    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let completed = Arc::new(tokio::sync::Notify::new());
    FORWARD_STARTED.with(|c| *c.borrow_mut() = Some(started.clone()));
    FORWARD_RELEASE.with(|c| *c.borrow_mut() = Some(release.clone()));
    FORWARD_COMPLETED.with(|c| *c.borrow_mut() = Some(completed.clone()));

    let dir = tempfile::tempdir().expect("forwarding fixture directory");
    let runtime = forward_test_runtime(Some(dir.path().join("main.db")), &["kg"]);
    let server = KhiveMcpServer::new(runtime).expect("server builds with kg");
    let handler = tokio::spawn(async move {
        server
            .request_with_forward(
                RequestParams {
                    ops: "stats()".to_string(),
                    ..Default::default()
                },
                slow_forward,
            )
            .await
    });

    tokio::time::timeout(Duration::from_secs(1), started.notified())
        .await
        .expect("forward never reached admission");
    handler.abort();
    let _ = handler.await;
    release.notify_one();

    tokio::time::timeout(Duration::from_secs(1), completed.notified())
        .await
        .expect("dropping the handler must not cancel an already-admitted daemon forward");
}

/// An admitted forward must not shield a cancelled request forever: if
/// the daemon (or the socket read underneath it) never answers, the
/// post-cancellation wait is bounded by the remaining time on the
/// request's own absolute deadline (captured at admission via
/// `scope_mcp_request_read_cancellation`'s `scope_request_read_deadline`
/// call), falling back to a fresh `request_read_timeout()` only when no
/// deadline was installed. Cancellation here arrives immediately after
/// admission, so the remaining time is nearly the full ceiling — this is
/// the control case; see
/// `admitted_forward_cancelled_near_deadline_bounds_by_remaining_time_not_a_fresh_ceiling`
/// for the case where only a sliver of the deadline is left. On expiry
/// the handler reports an outcome-unknown error rather than hanging. The
/// forward task itself is left running (never aborted), so this only
/// asserts the handler's own return, not the task's fate.
#[cfg(unix)]
#[tokio::test(start_paused = true)]
#[serial_test::serial(config_ledger)]
async fn admitted_forward_that_never_resolves_reports_unknown_outcome_after_the_bound() {
    thread_local! {
        static FORWARD_STARTED: std::cell::RefCell<Option<Arc<tokio::sync::Notify>>> =
            const { std::cell::RefCell::new(None) };
    }

    fn never_resolves_forward(
        _frame: khive_runtime::DaemonRequestFrame,
        _packs: Option<Vec<String>>,
        _replay_read_only: bool,
    ) -> ForwardFuture {
        let started = FORWARD_STARTED
            .with(|c| c.borrow().clone())
            .expect("started notify armed");
        Box::pin(async move {
            started.notify_one();
            std::future::pending::<()>().await;
            unreachable!("this forward seam must never resolve");
        })
    }

    let started = Arc::new(tokio::sync::Notify::new());
    FORWARD_STARTED.with(|c| *c.borrow_mut() = Some(started.clone()));

    let dir = tempfile::tempdir().expect("forwarding fixture directory");
    let runtime = forward_test_runtime(Some(dir.path().join("main.db")), &["kg"]);
    let server = KhiveMcpServer::new(runtime).expect("server builds with kg");
    let cancellation = tokio_util::sync::CancellationToken::new();
    let cancel_after_admission = cancellation.clone();
    let handler = tokio::spawn(async move {
        scope_mcp_request_read_cancellation(
            cancel_after_admission,
            server.request_with_forward(
                RequestParams {
                    ops: "stats()".to_string(),
                    request_id: Some(7777),
                    ..Default::default()
                },
                never_resolves_forward,
            ),
        )
        .await
    });

    started.notified().await;
    cancellation.cancel();

    let bound = request_read_timeout();
    let result = tokio::time::timeout(bound.saturating_add(Duration::from_secs(5)), handler)
        .await
        .expect("handler never returned after the post-cancellation bound elapsed")
        .expect("handler task must not panic");

    let error = result.expect_err("an admitted forward that never resolves must not hang forever");
    let data = error
        .data
        .expect("unknown-outcome error must carry structured data");
    assert_eq!(data["outcome"], "unknown");
    assert_eq!(data["retryable"], false);
    assert_eq!(
        data["request_id"], 7777,
        "unknown-outcome error must carry the request id: {data}"
    );
}

/// A request cancelled a moment before its own absolute deadline must be
/// bounded by the remaining time to THAT deadline, not by a fresh
/// `request_read_timeout()` ceiling starting from the cancel. Before the
/// fix the post-cancellation wait always restarted a full
/// `request_read_timeout()` from the moment cancellation was observed,
/// so this test would only return after almost another full ceiling past
/// the point the clock was advanced to (i.e. close to
/// `2 * request_read_timeout()` total); after the fix it returns within
/// the small margin left on the original deadline.
#[cfg(unix)]
#[tokio::test(start_paused = true)]
#[serial_test::serial(config_ledger)]
async fn admitted_forward_cancelled_near_deadline_bounds_by_remaining_time_not_a_fresh_ceiling() {
    thread_local! {
        static FORWARD_STARTED: std::cell::RefCell<Option<Arc<tokio::sync::Notify>>> =
            const { std::cell::RefCell::new(None) };
    }

    fn never_resolves_forward(
        _frame: khive_runtime::DaemonRequestFrame,
        _packs: Option<Vec<String>>,
        _replay_read_only: bool,
    ) -> ForwardFuture {
        let started = FORWARD_STARTED
            .with(|c| c.borrow().clone())
            .expect("started notify armed");
        Box::pin(async move {
            started.notify_one();
            std::future::pending::<()>().await;
            unreachable!("this forward seam must never resolve");
        })
    }

    let started = Arc::new(tokio::sync::Notify::new());
    FORWARD_STARTED.with(|c| *c.borrow_mut() = Some(started.clone()));

    let dir = tempfile::tempdir().expect("forwarding fixture directory");
    let runtime = forward_test_runtime(Some(dir.path().join("main.db")), &["kg"]);
    let server = KhiveMcpServer::new(runtime).expect("server builds with kg");
    let cancellation = tokio_util::sync::CancellationToken::new();
    let cancel_after_admission = cancellation.clone();
    let handler = tokio::spawn(async move {
        scope_mcp_request_read_cancellation(
            cancel_after_admission,
            server.request_with_forward(
                RequestParams {
                    ops: "stats()".to_string(),
                    request_id: Some(4242),
                    ..Default::default()
                },
                never_resolves_forward,
            ),
        )
        .await
    });

    started.notified().await;

    // Advance the paused clock to shortly before the request's own
    // absolute deadline before cancelling, so only a small margin of
    // that deadline remains.
    let bound = request_read_timeout();
    let margin = Duration::from_millis(200);
    tokio::time::advance(bound.saturating_sub(margin)).await;
    cancellation.cancel();

    let wait_started = tokio::time::Instant::now();
    let result = tokio::time::timeout(margin * 10, handler)
        .await
        .expect(
            "handler did not return near the request's own deadline; it appears to have \
                 restarted a fresh full ceiling instead of using the remaining time",
        )
        .expect("handler task must not panic");
    let elapsed = tokio::time::Instant::now() - wait_started;

    let error = result
        .expect_err("an admitted forward that never resolves past its deadline must not hang");
    let data = error
        .data
        .expect("unknown-outcome error must carry structured data");
    assert_eq!(data["outcome"], "unknown");
    assert_eq!(
        data["request_id"], 4242,
        "unknown-outcome error must carry the request id: {data}"
    );
    assert!(
        elapsed < bound / 2,
        "expected the post-cancellation wait to be bounded by the remaining time on the \
             request's own deadline (~{margin:?}), not a fresh full ceiling of {bound:?}; \
             observed elapsed {elapsed:?}"
    );
}

/// Control for the fallback branch: `request_with_forward` called
/// without going through `scope_mcp_request_read_cancellation` (so no
/// absolute request deadline is ever installed — only a bare
/// cancellation receiver, via `khive_storage::scope_request_read_cancellation`
/// directly, as some callers do) must still bound the post-cancellation
/// wait by `request_read_timeout()` rather than hanging forever.
#[cfg(unix)]
#[tokio::test(start_paused = true)]
#[serial_test::serial(config_ledger)]
async fn admitted_forward_without_installed_deadline_falls_back_to_request_read_timeout() {
    thread_local! {
        static FORWARD_STARTED: std::cell::RefCell<Option<Arc<tokio::sync::Notify>>> =
            const { std::cell::RefCell::new(None) };
    }

    fn never_resolves_forward(
        _frame: khive_runtime::DaemonRequestFrame,
        _packs: Option<Vec<String>>,
        _replay_read_only: bool,
    ) -> ForwardFuture {
        let started = FORWARD_STARTED
            .with(|c| c.borrow().clone())
            .expect("started notify armed");
        Box::pin(async move {
            started.notify_one();
            std::future::pending::<()>().await;
            unreachable!("this forward seam must never resolve");
        })
    }

    let started = Arc::new(tokio::sync::Notify::new());
    FORWARD_STARTED.with(|c| *c.borrow_mut() = Some(started.clone()));

    let dir = tempfile::tempdir().expect("forwarding fixture directory");
    let runtime = forward_test_runtime(Some(dir.path().join("main.db")), &["kg"]);
    let server = KhiveMcpServer::new(runtime).expect("server builds with kg");

    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let handler = tokio::spawn(khive_storage::scope_request_read_cancellation(
        cancel_rx,
        async move {
            server
                .request_with_forward(
                    RequestParams {
                        ops: "stats()".to_string(),
                        request_id: Some(9191),
                        ..Default::default()
                    },
                    never_resolves_forward,
                )
                .await
        },
    ));

    started.notified().await;
    cancel_tx
        .send(true)
        .expect("cancellation receiver still live");

    let bound = request_read_timeout();
    let result = tokio::time::timeout(bound.saturating_add(Duration::from_secs(5)), handler)
        .await
        .expect("handler never returned after the fallback bound elapsed")
        .expect("handler task must not panic");

    let error = result.expect_err("an admitted forward that never resolves must not hang forever");
    let data = error
        .data
        .expect("unknown-outcome error must carry structured data");
    assert_eq!(data["outcome"], "unknown");
    assert_eq!(
        data["request_id"], 9191,
        "unknown-outcome error must carry the request id: {data}"
    );
}

/// Adapter-boundary regression: `restricted_registry_pack_list_reaches_forward_seam`
/// above proves the derivation site (`Some(resolved_packs)` in
/// `request_with_forward`) folds the right pack list, but its `spy_forward`
/// stands in for `forward_or_spawn_boxed` itself, so it never executes the
/// adapter's own `packs.as_deref()` conversion at
/// `crate::daemon::forward_or_spawn_with_config_and_packs`'s call site. This test
/// instead drives `request_with_cancellation` — the real production entry
/// point, which always calls the real `forward_or_spawn_boxed` — and
/// observes the argument via a one-shot capture hook armed at the entry of
/// `forward_or_spawn_with_config_and_packs` itself (`crate::daemon::test_forward_seam`),
/// past both the derivation site AND the adapter conversion. Changing the
/// adapter's `packs.as_deref()` argument to `None` reddens this test.
#[cfg(unix)]
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn restricted_registry_pack_list_reaches_real_adapter_boundary() {
    let dir = tempfile::tempdir().expect("forwarding fixture directory");
    let runtime = forward_test_runtime(Some(dir.path().join("main.db")), &["kg", "gtd"]);
    let server =
        KhiveMcpServer::new(runtime).expect("server builds with restricted kg+gtd registry");

    crate::daemon::test_forward_seam::arm();

    let params = RequestParams {
        plan: None,
        ops: "stats()".to_string(),
        presentation: None,
        presentation_per_op: None,
        save_to: None,
        format: None,
        format_per_op: None,
        request_id: None,
    };

    let result = server.request_with_cancellation(params).await;

    assert!(
        result.is_ok(),
        "the intercepted dispatch through the real production entry point must \
             succeed: {result:?}"
    );
    assert_eq!(
        crate::daemon::test_forward_seam::take_captured(),
        Some(Some(vec!["kg".to_string(), "gtd".to_string()])),
        "the real forward_or_spawn_boxed adapter must convert the server's resolved \
             registry pack set into Some(&packs) at the forward_or_spawn_with_config_and_packs \
             call boundary"
    );
}

/// `ensure_bridge_request_id` unit-tests the helper in isolation, but the
/// invariant "every admitted MCP request carries a bridge correlation
/// id" is actually established at `request_with_cancellation`, the real
/// production entry point. A test that only calls the helper directly
/// cannot prove that boundary applies it. This drives
/// `request_with_cancellation` itself and observes the `request_id` the
/// real daemon adapter boundary received, via the same
/// `test_forward_seam` hook `restricted_registry_pack_list_reaches_real_adapter_boundary`
/// above uses for the pack list.
#[cfg(unix)]
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn request_with_cancellation_stamps_bridge_id_at_the_real_adapter_boundary() {
    let dir = tempfile::tempdir().expect("forwarding fixture directory");
    let runtime = forward_test_runtime(Some(dir.path().join("main.db")), &["kg"]);
    let server = KhiveMcpServer::new(runtime).expect("server builds with kg");

    crate::daemon::test_forward_seam::arm();
    let without_id = RequestParams {
        ops: "stats()".to_string(),
        request_id: None,
        ..Default::default()
    };
    let result = server.request_with_cancellation(without_id).await;
    assert!(
        result.is_ok(),
        "the intercepted dispatch through the real production entry point must \
             succeed: {result:?}"
    );
    let minted = crate::daemon::test_forward_seam::take_captured_request_id()
        .expect("hook was armed and must have observed a call")
        .expect("the bridge must mint a request id when the caller supplied none");
    assert_ne!(minted, 0, "bridge-generated ids must be nonzero");

    crate::daemon::test_forward_seam::arm();
    let with_id = RequestParams {
        ops: "stats()".to_string(),
        request_id: Some(4242),
        ..Default::default()
    };
    let result = server.request_with_cancellation(with_id).await;
    assert!(
        result.is_ok(),
        "the intercepted dispatch through the real production entry point must \
             succeed: {result:?}"
    );
    assert_eq!(
        crate::daemon::test_forward_seam::take_captured_request_id(),
        Some(Some(4242)),
        "an explicit caller-supplied request id must reach the real adapter \
             boundary unchanged"
    );
}

/// ADR-118's serving toggle is baked into a runtime. Opposite policies
/// must never share one warm daemon even when every `RuntimeConfig` field
/// is otherwise identical.
#[test]
#[serial_test::serial(config_ledger)]
fn config_id_differs_when_ann_fresh_tail_policy_differs() {
    let config = RuntimeConfig::no_embeddings();

    assert_ne!(
        compute_config_id_with_ann_fresh_tail(&config, None, true),
        compute_config_id_with_ann_fresh_tail(&config, None, false),
        "opposite fresh-tail policies must not share one warm daemon"
    );
}

#[test]
fn config_id_separates_git_actor_resolver_and_fault_configuration() {
    use khive_runtime::engine_config::GitWriteActorConfig;

    let mut base = RuntimeConfig::no_embeddings();
    base.git_write.actors.insert(
        "example".to_string(),
        GitWriteActorConfig {
            name: "Example".to_string(),
            email: "example@example.invalid".to_string(),
            credential_ref: "example-reference".to_string(),
            platform_identity: "example-login".to_string(),
        },
    );
    let fingerprint =
        |config: &RuntimeConfig| compute_config_id_with_runtime_policies(config, None, true, false);
    let original = fingerprint(&base);
    for field in [
        "name",
        "email",
        "credential_ref",
        "platform_identity",
        "actor",
        "resolver",
        "contract_faults",
        "fault",
    ] {
        let mut changed = base.clone();
        match field {
            "name" => changed
                .git_write
                .actors
                .get_mut("example")
                .unwrap()
                .name
                .push('x'),
            "email" => changed
                .git_write
                .actors
                .get_mut("example")
                .unwrap()
                .email
                .push('x'),
            "credential_ref" => changed
                .git_write
                .actors
                .get_mut("example")
                .unwrap()
                .credential_ref
                .push('x'),
            "platform_identity" => changed
                .git_write
                .actors
                .get_mut("example")
                .unwrap()
                .platform_identity
                .push('x'),
            "actor" => {
                changed.git_write.actors.clear();
            }
            "resolver" => changed.git_write.credential_resolver[0].push('x'),
            "contract_faults" => changed.git_write.contract_faults = true,
            "fault" => {
                changed.git_write.fault = Some("git.push:reply-lost-after-effect".to_string())
            }
            _ => unreachable!(),
        }
        assert_ne!(original, fingerprint(&changed), "changed {field}");
    }
    assert!(!original.contains("example-reference"));
    assert_eq!(original, fingerprint(&base.clone()));
}

#[test]
fn config_id_separates_brain_read_policy_and_normalizes_reader_sets() {
    let base = RuntimeConfig::no_embeddings();
    let fingerprint =
        |config: &RuntimeConfig| compute_config_id_with_runtime_policies(config, None, true, false);
    let mut configured = base.clone();
    configured.brain.fleet_readers =
        vec!["lambda:reader".to_string(), "lambda:auditor".to_string()];
    let configured_id = fingerprint(&configured);
    assert_ne!(configured_id, fingerprint(&base));
    assert!(!configured_id.contains("lambda:reader"));
    assert!(!configured_id.contains("lambda:auditor"));

    let mut reordered = configured.clone();
    reordered.brain.fleet_readers.reverse();
    reordered
        .brain
        .fleet_readers
        .push("lambda:reader".to_string());
    assert_eq!(configured_id, fingerprint(&reordered));

    let mut revoked = configured;
    revoked.brain.fleet_readers.pop();
    assert_ne!(configured_id, fingerprint(&revoked));
    revoked.brain.fleet_readers.clear();
    assert_eq!(fingerprint(&revoked), fingerprint(&base));
}

#[test]
fn config_id_separates_telemetry_policy_without_exposing_values() {
    use khive_runtime::{TelemetryCarrier, TelemetryChannelConfig, TelemetryFailurePosture};

    let mut base = RuntimeConfig::no_embeddings();
    base.telemetry.stream = "example-telemetry-stream".into();
    base.telemetry.channels.push(TelemetryChannelConfig {
        kinds: vec!["run.completed".into()],
        carrier: TelemetryCarrier::Durable,
        failure_posture: TelemetryFailurePosture::Stop,
    });
    let fingerprint =
        |config: &RuntimeConfig| compute_config_id_with_runtime_policies(config, None, true, false);
    let original = fingerprint(&base);
    assert!(!original.contains("example-telemetry-stream"));
    assert!(!original.contains("run.completed"));
    assert_eq!(original, fingerprint(&base.clone()));
    for field in [
        "stream",
        "default_carrier",
        "kinds",
        "carrier",
        "failure_posture",
        "channels",
    ] {
        let mut changed = base.clone();
        match field {
            "stream" => changed.telemetry.stream.push('2'),
            "default_carrier" => {
                changed.telemetry.default_carrier = Some(TelemetryCarrier::Durable)
            }
            "kinds" => changed.telemetry.channels[0]
                .kinds
                .push("run.failed".into()),
            "carrier" => changed.telemetry.channels[0].carrier = TelemetryCarrier::Ephemeral,
            "failure_posture" => {
                changed.telemetry.channels[0].failure_posture = TelemetryFailurePosture::Gap
            }
            "channels" => changed.telemetry.channels.clear(),
            _ => unreachable!(),
        }
        assert_ne!(original, fingerprint(&changed), "changed {field}");
    }
}

#[test]
fn config_id_differs_when_caller_enrollment_policy_differs() {
    let base = RuntimeConfig::no_embeddings();
    let enrolled = RuntimeConfig {
        gate: Arc::new(khive_runtime::CallerEnrollmentGate::new(
            vec!["lambda:enrolled".to_string()],
            false,
        )),
        ..base.clone()
    };
    let revoked = RuntimeConfig {
        gate: Arc::new(khive_runtime::CallerEnrollmentGate::new(Vec::new(), false)),
        ..base
    };

    assert_ne!(
        compute_config_id_with_runtime_policies(&enrolled, None, true, false),
        compute_config_id_with_runtime_policies(&revoked, None, true, false),
        "different caller-enrollment policies must not share one warm daemon"
    );
    let restricted = |patterns: Vec<String>| RuntimeConfig {
        gate: Arc::new(khive_runtime::CallerEnrollmentGate::with_write_denials(
            vec!["lambda:enrolled".into()],
            false,
            patterns,
        )),
        ..enrolled.clone()
    };
    let fingerprint =
        |config: &RuntimeConfig| compute_config_id_with_runtime_policies(config, None, true, false);
    assert_eq!(fingerprint(&enrolled), fingerprint(&restricted(vec![])));
    let limited = restricted(vec!["*:duty".into(), "lambda:enrolled".into()]);
    assert_ne!(
        fingerprint(&enrolled),
        fingerprint(&limited),
        "an unrestricted daemon must not serve a restricted config"
    );
    assert_eq!(
        fingerprint(&limited),
        fingerprint(&restricted(vec![
            "lambda:enrolled".into(),
            "*:duty".into(),
            "*:duty".into()
        ]))
    );
    assert_ne!(
        fingerprint(&limited),
        fingerprint(&restricted(vec!["*".into()]))
    );
}

#[test]
fn config_id_tracks_mailbox_read_policy_and_preserves_pair_order_equivalence() {
    let base = RuntimeConfig::no_embeddings();
    let configured =
        |owner: &str, readers: &[&str], inner: Arc<dyn khive_runtime::Gate>| RuntimeConfig {
            gate: Arc::new(
                khive_runtime::MailboxReadGate::new(
                    inner,
                    khive_runtime::ActorRef::new("actor", owner),
                    readers
                        .iter()
                        .map(|reader| khive_runtime::ActorRef::new("actor", *reader))
                        .collect(),
                )
                .expect("valid exact mailbox policy"),
            ),
            ..base.clone()
        };
    let fingerprint =
        |config: &RuntimeConfig| compute_config_id_with_runtime_policies(config, None, true, false);
    let initial = configured(
        "agent:owner",
        &["agent:one", "agent:two"],
        Arc::new(khive_runtime::AllowAllGate),
    );
    let equivalent = configured(
        "agent:owner",
        &["agent:two", "agent:one", "agent:one"],
        Arc::new(khive_runtime::AllowAllGate),
    );
    assert_eq!(fingerprint(&initial), fingerprint(&equivalent));
    for changed in [
        configured(
            "agent:owner",
            &["agent:one"],
            Arc::new(khive_runtime::AllowAllGate),
        ),
        configured("agent:owner", &[], Arc::new(khive_runtime::AllowAllGate)),
        configured(
            "agent:other-owner",
            &["agent:one", "agent:two"],
            Arc::new(khive_runtime::AllowAllGate),
        ),
        configured(
            "agent:owner",
            &["agent:one", "agent:two"],
            Arc::new(khive_runtime::CallerEnrollmentGate::new(
                vec!["agent:one".into()],
                false,
            )),
        ),
    ] {
        assert_ne!(
            fingerprint(&initial),
            fingerprint(&changed),
            "a changed owner, reader set or inner policy must select a different daemon"
        );
    }
    assert_ne!(fingerprint(&initial), fingerprint(&base));
}

/// `gtd.assign` anchors a date-only `due` through `display_timezone` and
/// PERSISTS the resulting instant, so a warm daemon reused across two
/// runtimes differing only in that field writes an instant wrong by the
/// offset between the zones. Identity must separate them.
#[test]
#[serial_test::serial(config_ledger)]
fn config_id_differs_when_display_timezone_differs() {
    // One base, cloned, for the reason spelled out on the test below — and
    // it matters MORE here. This assertion is `assert_ne!`, so the shared
    // environment racing between two constructor calls would make it pass
    // by producing two different `db_path`s, which is a pass that would
    // survive deleting the fix this test exists to hold.
    let base = RuntimeConfig::no_embeddings();
    let utc = RuntimeConfig {
        display_timezone: "UTC".parse().expect("UTC is a known IANA zone"),
        events_split: None,
        ..base.clone()
    };
    let new_york = RuntimeConfig {
        display_timezone: "America/New_York".parse().expect("known IANA zone"),
        events_split: None,
        ..base
    };

    assert_ne!(
        compute_config_id_with_runtime_policies(&utc, None, true, false),
        compute_config_id_with_runtime_policies(&new_york, None, true, false),
        "runtimes differing only in display_timezone must not share one warm daemon: \
             a reused daemon would anchor date-only due values in the wrong zone and \
             persist the wrong instant"
    );
}

/// The other direction, so the assertion above cannot pass for an
/// incidental reason: identical zones must still collapse to one identity.
#[test]
#[serial_test::serial(config_ledger)]
fn config_id_matches_when_display_timezone_matches() {
    // ONE base, cloned — not two constructor calls. `RuntimeConfig::default`
    // reads `HOME` to build `db_path`, and `db_path` is folded into the id,
    // so two calls read that variable at two different instants. Other
    // tests in this binary set and restore `HOME` around their own work
    // (`config_id_matches_for_tilde_and_equivalent_absolute_db_override` is
    // one, and it matches the same `config_id` filter), and tests run in
    // parallel threads against one process-global environment. A mutation
    // landing between the two calls gave the two configs different paths
    // and reddened this test for a reason that has nothing to do with
    // timezones. Cloning one base removes the window: whatever `HOME` is,
    // both sides read the same one.
    let base = RuntimeConfig::no_embeddings();
    let a = RuntimeConfig {
        display_timezone: "America/New_York".parse().expect("known IANA zone"),
        events_split: None,
        ..base.clone()
    };
    let b = RuntimeConfig {
        display_timezone: "America/New_York".parse().expect("known IANA zone"),
        events_split: None,
        ..base
    };

    assert_eq!(
        compute_config_id_with_runtime_policies(&a, None, true, false),
        compute_config_id_with_runtime_policies(&b, None, true, false),
        "identical runtimes must share one warm daemon"
    );
}

#[test]
#[serial_test::serial(config_ledger)]
fn config_id_treats_absent_and_explicit_default_blob_hydration_budget_as_equivalent() {
    use khive_runtime::engine_config::RuntimeSectionConfig;
    use khive_runtime::{runtime_config_from_khive_config, KhiveConfig};

    let base = RuntimeConfig::no_embeddings();
    let absent = runtime_config_from_khive_config(&KhiveConfig::default(), base.clone());
    let explicit = runtime_config_from_khive_config(
        &KhiveConfig {
            runtime: RuntimeSectionConfig {
                blob_hydration_bytes: Some(base.blob_hydration_bytes),
                ..RuntimeSectionConfig::default()
            },
            ..KhiveConfig::default()
        },
        base,
    );

    assert_eq!(
        compute_config_id_with_ann_fresh_tail(&absent, None, true),
        compute_config_id_with_ann_fresh_tail(&explicit, None, true)
    );
}

#[test]
#[serial_test::serial(config_ledger)]
fn config_id_differs_when_resolved_blob_hydration_budget_differs() {
    let config = RuntimeConfig::no_embeddings();
    let mut changed = config.clone();
    changed.blob_hydration_bytes /= 2;

    assert_ne!(
        compute_config_id_with_ann_fresh_tail(&config, None, true),
        compute_config_id_with_ann_fresh_tail(&changed, None, true),
        "different resident-blob admission budgets must not share one warm daemon"
    );
}

async fn observed_batch_entry(
    _index: usize,
    delay_ms: u64,
    entry: Value,
    in_flight: Arc<AtomicUsize>,
    max_in_flight: Arc<AtomicUsize>,
) -> Value {
    let current = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    max_in_flight.fetch_max(current, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
    in_flight.fetch_sub(1, Ordering::SeqCst);
    entry
}

fn batch_task<F>(index: usize, future: F) -> BatchTask<F>
where
    F: Future<Output = Value>,
{
    BatchTask {
        index,
        tool: "probe".to_string(),
        future,
    }
}

struct RemoteFetchErrorPack;

impl khive_types::Pack for RemoteFetchErrorPack {
    const NAME: &'static str = "remote-fetch-error-test";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [khive_runtime::HandlerDef] = &[khive_runtime::HandlerDef {
        name: "remote_fetch_failure",
        description: "returns a typed remote fetch failure",
        visibility: khive_runtime::Visibility::Verb,
        category: khive_runtime::VerbCategory::Assertive,
        params: &[],
    }];
}

#[async_trait::async_trait]
impl khive_runtime::PackRuntime for RemoteFetchErrorPack {
    fn name(&self) -> &str {
        <Self as khive_types::Pack>::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        <Self as khive_types::Pack>::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        <Self as khive_types::Pack>::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [khive_runtime::HandlerDef] {
        <Self as khive_types::Pack>::HANDLERS
    }

    async fn dispatch(
        &self,
        _verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &khive_runtime::NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Err(RuntimeError::RemoteFetchError {
            remote: "broken-origin".to_string(),
            message: "injected fetch failure".to_string(),
        })
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn request_remote_fetch_error_retains_structured_fields() {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(RemoteFetchErrorPack);
    let server = KhiveMcpServer::from_registry(builder.build().expect("test registry"));
    let response = server
        .dispatch_request_local(RequestParams {
            ops: "remote_fetch_failure()".to_string(),
            presentation: Some("verbose".to_string()),
            format: Some("json".to_string()),
            ..Default::default()
        })
        .await
        .expect("remote fetch failure must remain a per-op error");
    let envelope: Value = serde_json::from_str(&response).expect("request envelope");
    assert_eq!(envelope["summary"]["failed"], 1, "{envelope}");
    assert_eq!(envelope["summary"]["succeeded"], 0, "{envelope}");
    let results = envelope["results"].as_array().expect("per-op results");
    assert_eq!(results.len(), 1, "{envelope}");
    assert_eq!(results[0]["ok"], false);
    assert_eq!(results[0]["tool"], "remote_fetch_failure");
    assert_eq!(
        results[0]["error"],
        json!({
            "kind": "remote_fetch_error",
            "remote": "broken-origin",
            "message": "injected fetch failure",
            "domain_disposition": "unknown",
        }),
        "{envelope}"
    );
}

struct LargeResultPack;

struct LexicalTimeoutResultPack;

impl khive_types::Pack for LexicalTimeoutResultPack {
    const NAME: &'static str = "lexical-timeout-result-test";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [khive_runtime::HandlerDef] = &[
        khive_runtime::HandlerDef {
            name: "knowledge.search",
            description: "returns a deterministic lexical timeout result",
            visibility: khive_runtime::Visibility::Verb,
            category: khive_runtime::VerbCategory::Assertive,
            params: &[],
        },
        khive_runtime::HandlerDef {
            name: "knowledge.suggest",
            description: "returns a deterministic lexical timeout result",
            visibility: khive_runtime::Visibility::Verb,
            category: khive_runtime::VerbCategory::Assertive,
            params: &[],
        },
    ];
}

#[async_trait::async_trait]
impl khive_runtime::PackRuntime for LexicalTimeoutResultPack {
    fn name(&self) -> &str {
        <Self as khive_types::Pack>::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        <Self as khive_types::Pack>::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        <Self as khive_types::Pack>::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [khive_runtime::HandlerDef] {
        <Self as khive_types::Pack>::HANDLERS
    }

    async fn dispatch(
        &self,
        _verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &khive_runtime::NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Ok(json!({
            "degraded": {"lexical_timeout": true},
            "items": [{"name": "first"}, {"name": "second"}],
        }))
    }
}

#[cfg(unix)]
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn daemon_timeout_marker_survives_explicit_and_configured_auto_table_rendering() {
    use khive_runtime::daemon::{DaemonDispatch, DaemonResponseFrame, PROTOCOL_VERSION};

    for (requested, configured) in [
        (Some("auto"), OutputFormat::Json),
        (Some("table"), OutputFormat::Json),
        (None, OutputFormat::Auto),
        (None, OutputFormat::Table),
    ] {
        let mut builder = VerbRegistryBuilder::new();
        builder.register(LexicalTimeoutResultPack);
        let server = KhiveMcpServer::from_registry(builder.build().expect("test registry"))
            .with_default_output_format(configured);
        let ops = "[knowledge.search(), knowledge.suggest()]";
        let raw = server
            .dispatch(
                ops.to_string(),
                None,
                None,
                requested.map(str::to_string),
                None,
                false,
                None,
            )
            .await
            .expect("daemon dispatch");
        let mut expected: Value = serde_json::from_str(&raw).expect("daemon envelope");
        assert_eq!(
            expected[super::DAEMON_LEXICAL_TIMEOUT_MARKER],
            json!(true),
            "requested={requested:?}, configured={configured:?}: {expected}"
        );
        for entry in expected["results"].as_array().expect("results") {
            assert_eq!(entry["ok"], true);
            assert!(
                entry["result"]
                    .as_str()
                    .is_some_and(|text| text.starts_with("| name |")),
                "auto/table must hide the structured timeout: {entry}"
            );
        }
        expected
            .as_object_mut()
            .expect("envelope")
            .remove(super::DAEMON_LEXICAL_TIMEOUT_MARKER);
        let public_result = expected.to_string();
        let frame = DaemonResponseFrame {
            ok: true,
            result: Some(public_result.clone()),
            error: None,
            error_detail: Some(json!({"lexical_timeout": true})),
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id: Some(server.config_id().to_string()),
            version_mismatch: false,
            daemon_protocol_version: PROTOCOL_VERSION,
            metrics: None,
            request_id: None,
        };
        let public = crate::daemon::map_response_for_test(
            frame,
            server.config_id(),
            server.default_namespace(),
        )
        .expect("accepted daemon response")
        .expect("successful daemon response");
        assert_eq!(public, public_result);
        assert!(!public.contains(super::DAEMON_LEXICAL_TIMEOUT_MARKER));

        let local = server
            .dispatch_request_local(RequestParams {
                ops: ops.to_string(),
                format: requested.map(str::to_string),
                ..Default::default()
            })
            .await
            .expect("local dispatch");
        let local: Value = serde_json::from_str(&local).expect("local envelope");
        assert!(local.get(super::DAEMON_LEXICAL_TIMEOUT_MARKER).is_none());
    }
}

impl khive_types::Pack for LargeResultPack {
    const NAME: &'static str = "large-result-test";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [khive_runtime::HandlerDef] = &[
        khive_runtime::HandlerDef {
            name: "large_result",
            description: "returns a caller-sized test result",
            visibility: khive_runtime::Visibility::Verb,
            category: khive_runtime::VerbCategory::Assertive,
            params: &[],
        },
        khive_runtime::HandlerDef {
            name: "large_write",
            description: "returns a caller-sized test result for a state-changing verb",
            visibility: khive_runtime::Visibility::Verb,
            category: khive_runtime::VerbCategory::Commissive,
            params: &[],
        },
        khive_runtime::HandlerDef {
            name: "record_write",
            description: "records a small committed write, for chain-ordering tests",
            visibility: khive_runtime::Visibility::Verb,
            category: khive_runtime::VerbCategory::Commissive,
            params: &[],
        },
        khive_runtime::HandlerDef {
            name: "memory.recall",
            description: "test double matching the real memory.recall verb's name and \
                               Assertive category, to exercise the qualified pack.verb name \
                               through the real registry lookup",
            visibility: khive_runtime::Visibility::Verb,
            category: khive_runtime::VerbCategory::Assertive,
            params: &[],
        },
    ];
}

#[async_trait::async_trait]
impl khive_runtime::PackRuntime for LargeResultPack {
    fn name(&self) -> &str {
        <Self as khive_types::Pack>::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        <Self as khive_types::Pack>::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        <Self as khive_types::Pack>::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [khive_runtime::HandlerDef] {
        <Self as khive_types::Pack>::HANDLERS
    }

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        _registry: &VerbRegistry,
        _token: &khive_runtime::NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        if verb == "record_write" {
            return Ok(json!({
                "committed": true,
                "marker": params.get("marker").cloned().unwrap_or(Value::Null),
            }));
        }
        if let Some(bytes) = params
            .get("table_bytes")
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
        {
            let payload = "x".repeat(bytes);
            return Ok(json!([
                {"payload": payload},
                {"payload": payload},
            ]));
        }
        let bytes = params
            .get("bytes")
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .expect("test supplies a valid byte count");
        Ok(json!("x".repeat(bytes)))
    }
}

struct SlowSqlReadPack {
    bridge: khive_db::SqlBridge,
}

impl khive_types::Pack for SlowSqlReadPack {
    const NAME: &'static str = "slow-sql-read-test";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [khive_runtime::HandlerDef] = &[
        khive_runtime::HandlerDef {
            name: "slow_sql_read",
            description: "runs SQLite work until the request read deadline interrupts it",
            visibility: khive_runtime::Visibility::Verb,
            category: khive_runtime::VerbCategory::Assertive,
            params: &[],
        },
        khive_runtime::HandlerDef {
            name: "pending_read_phase",
            description: "waits until the request read deadline interrupts it",
            visibility: khive_runtime::Visibility::Verb,
            category: khive_runtime::VerbCategory::Assertive,
            params: &[],
        },
    ];
}

#[async_trait::async_trait]
impl khive_runtime::PackRuntime for SlowSqlReadPack {
    fn name(&self) -> &str {
        <Self as khive_types::Pack>::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        <Self as khive_types::Pack>::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        <Self as khive_types::Pack>::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [khive_runtime::HandlerDef] {
        <Self as khive_types::Pack>::HANDLERS
    }

    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &khive_runtime::NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        use khive_storage::{SqlAccess, SqlStatement};

        if verb == "pending_read_phase" {
            khive_storage::await_request_read_phase(
                "outer-deadline-probe",
                std::future::pending::<()>(),
            )
            .await?;
            return Ok(Value::Null);
        }

        let mut reader = self.bridge.reader().await?;
        let rows = reader
            .query_all(SqlStatement {
                sql: "WITH RECURSIVE numbers(value) AS (\
                          SELECT 1 UNION ALL SELECT value + 1 FROM numbers WHERE value < 1000\
                          ) SELECT SUM(a.value * b.value * c.value) \
                          FROM numbers AS a CROSS JOIN numbers AS b CROSS JOIN numbers AS c"
                    .into(),
                params: vec![],
                label: Some("canonical-dispatch-deadline-probe".into()),
            })
            .await?;
        Ok(json!({ "rows": rows.len() }))
    }
}

fn slow_sql_read_test_server() -> KhiveMcpServer {
    let pool = Arc::new(
        khive_db::ConnectionPool::new(khive_db::PoolConfig::default())
            .expect("in-memory SQLite pool"),
    );
    let mut builder = VerbRegistryBuilder::new();
    builder.register(SlowSqlReadPack {
        bridge: khive_db::SqlBridge::new(pool, false),
    });
    KhiveMcpServer::from_registry(builder.build().expect("slow-SQL test registry"))
}

/// Deterministic concurrency probe for ADR-016 Amendment 2: `dispatch`
/// blocks every caller until exactly `threshold` calls have arrived at
/// once, then releases all of them together via a `watch` channel; a
/// late subscriber (a unit only admitted after an earlier one frees its
/// slot) reads the already-released value on its first `borrow()` and
/// never blocks, unlike `Notify::notify_waiters`, which only wakes
/// callers already waiting at the moment it fires. No test ever sleeps
/// to synchronize: a request that cannot reach `threshold` concurrent
/// calls hangs (caught by the caller's own `tokio::time::timeout`)
/// rather than racing a clock.
struct UnitBarrierPack {
    in_flight: Arc<AtomicUsize>,
    max_in_flight: Arc<AtomicUsize>,
    threshold: usize,
    release_tx: tokio::sync::watch::Sender<bool>,
    release_rx: tokio::sync::watch::Receiver<bool>,
}

fn unit_barrier_pack(threshold: usize) -> (UnitBarrierPack, Arc<AtomicUsize>) {
    let (release_tx, release_rx) = tokio::sync::watch::channel(false);
    let max_in_flight = Arc::new(AtomicUsize::new(0));
    let pack = UnitBarrierPack {
        in_flight: Arc::new(AtomicUsize::new(0)),
        max_in_flight: max_in_flight.clone(),
        threshold,
        release_tx,
        release_rx,
    };
    (pack, max_in_flight)
}

impl khive_types::Pack for UnitBarrierPack {
    const NAME: &'static str = "unit-barrier-test";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [khive_runtime::HandlerDef] = &[
        khive_runtime::HandlerDef {
            name: "barrier_wait",
            description: "blocks until `threshold` concurrent calls have arrived, for a \
                               deterministic concurrency-bound test",
            visibility: khive_runtime::Visibility::Verb,
            category: khive_runtime::VerbCategory::Assertive,
            params: &[],
        },
        khive_runtime::HandlerDef {
            name: "noop",
            description: "returns immediately, for chain-shape tests that need a harmless \
                               second leaf",
            visibility: khive_runtime::Visibility::Verb,
            category: khive_runtime::VerbCategory::Assertive,
            params: &[],
        },
        khive_runtime::HandlerDef {
            name: "always_fails",
            description: "always returns a per-op error, for unit-tail-abort tests",
            visibility: khive_runtime::Visibility::Verb,
            category: khive_runtime::VerbCategory::Assertive,
            params: &[],
        },
    ];
}

#[async_trait::async_trait]
impl khive_runtime::PackRuntime for UnitBarrierPack {
    fn name(&self) -> &str {
        <Self as khive_types::Pack>::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        <Self as khive_types::Pack>::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        <Self as khive_types::Pack>::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [khive_runtime::HandlerDef] {
        <Self as khive_types::Pack>::HANDLERS
    }

    async fn dispatch(
        &self,
        verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &khive_runtime::NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        if verb == "noop" {
            return Ok(json!({"noop": true}));
        }
        if verb == "always_fails" {
            return Err(RuntimeError::RemoteFetchError {
                remote: "unit-barrier-test".to_string(),
                message: "injected failure for a unit-tail-abort test".to_string(),
            });
        }
        let n = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(n, Ordering::SeqCst);
        if n >= self.threshold {
            self.release_tx.send(true).ok();
            // Stay in flight across one yield, so a unit admitted beyond
            // the cap and polled in the same pass is counted in the peak
            // instead of arriving after this call has already left.
            tokio::task::yield_now().await;
        } else {
            let mut rx = self.release_rx.clone();
            while !*rx.borrow() {
                rx.changed().await.ok();
            }
        }
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        Ok(json!({"arrival": n}))
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn bracketed_batch_of_chains_bounds_units_at_max_concurrency() {
    // ADR-016 Amendment 2: ten multi-leaf units, each a two-leaf
    // chain (`barrier_wait() | noop()`), inside one bracketed batch.
    // `barrier_wait` only releases once MAX_BATCH_CONCURRENCY calls have
    // arrived concurrently, so the request can only complete at all if
    // the executor both reaches that concurrency (units 9 and 10 could
    // never be admitted otherwise; the request hangs, it does not fail)
    // and never exceeds it (`max_in_flight` is an exact atomic peak
    // forced by the barrier, not a sampled snapshot that could miss a
    // higher peak).
    let (pack, max_in_flight) = unit_barrier_pack(MAX_BATCH_CONCURRENCY);
    let mut builder = VerbRegistryBuilder::new();
    builder.register(pack);
    let server = KhiveMcpServer::from_registry(builder.build().expect("barrier test registry"));

    let unit_count = 10usize;
    let ops = format!(
        "[{}]",
        vec!["barrier_wait() | noop()"; unit_count].join(", ")
    );

    let response = tokio::time::timeout(
        Duration::from_secs(5),
        server.dispatch_request_local(RequestParams {
            ops,
            presentation: Some("verbose".to_string()),
            format: Some("json".to_string()),
            ..Default::default()
        }),
    )
    .await
    .expect("the request must complete once MAX_BATCH_CONCURRENCY units are admitted, not hang")
    .expect("bracketed batch of barrier chains dispatches cleanly");

    let envelope: Value = serde_json::from_str(&response).expect("request envelope");
    assert_eq!(envelope["summary"]["total"], unit_count * 2, "{envelope}");
    assert_eq!(
        envelope["summary"]["succeeded"],
        unit_count * 2,
        "{envelope}"
    );
    assert_eq!(envelope["summary"]["failed"], 0, "{envelope}");
    assert_eq!(envelope["summary"]["aborted"], 0, "{envelope}");
    assert_eq!(
        max_in_flight.load(Ordering::SeqCst),
        MAX_BATCH_CONCURRENCY,
        "at most (and, since ten units were admitted, at least) MAX_BATCH_CONCURRENCY units \
             may have a leaf in flight at once"
    );

    // op_index/unit_index/step_index stay lexical (0..N-1, grouped by
    // static unit boundaries) regardless of which units actually
    // completed first under real concurrency; this is the one scenario
    // in this file where completion order is genuinely nondeterministic,
    // which is exactly why it is the right place to prove the ordering
    // claim rather than a synthetic single-threaded rebuild of it.
    let results = envelope["results"].as_array().expect("results");
    assert_eq!(results.len(), unit_count * 2, "{envelope}");
    for (index, row) in results.iter().enumerate() {
        assert_eq!(row["op_index"], index, "{envelope}");
        assert_eq!(row["unit_index"], index / 2, "{envelope}");
        assert_eq!(row["step_index"], index % 2, "{envelope}");
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn bracketed_unit_prev_ref_threads_within_its_own_unit_only() {
    // A later leaf inside a multi-leaf unit resolves `$prev` against
    // its OWN unit's immediately preceding leaf, exactly like plain
    // chain mode; there is no cross-unit reference, and a disjoint
    // unit dispatches on its own literal argument.
    let server = large_result_test_server();
    let response = server
        .dispatch_request_local(RequestParams {
            ops: concat!(
                "[",
                "record_write(marker=\"seed\") | record_write(marker=$prev.marker), ",
                "record_write(marker=\"other-unit\")",
                "]"
            )
            .to_string(),
            presentation: Some("verbose".to_string()),
            format: Some("json".to_string()),
            ..Default::default()
        })
        .await
        .expect("bracketed batch dispatches");
    let envelope: Value = serde_json::from_str(&response).expect("request envelope");
    assert_eq!(envelope["summary"]["succeeded"], 3, "{envelope}");
    let results = envelope["results"].as_array().expect("results");
    assert_eq!(results.len(), 3, "{envelope}");
    assert_eq!(results[0]["result"]["marker"], "seed", "{envelope}");
    assert_eq!(
        results[1]["result"]["marker"], "seed",
        "$prev must resolve against the immediately preceding leaf of the SAME unit: {envelope}"
    );
    assert_eq!(results[2]["result"]["marker"], "other-unit", "{envelope}");
    assert_eq!(results[0]["unit_index"], 0, "{envelope}");
    assert_eq!(results[1]["unit_index"], 0, "{envelope}");
    assert_eq!(results[2]["unit_index"], 1, "{envelope}");
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn bracketed_dynamic_prev_resolved_write_target_is_outside_static_preflight() {
    // `write_keys_for_op_pub` only reads LITERAL argument values; a
    // `$prev`-resolved target is invisible to the static cross-unit
    // preflight even when it happens to resolve to the same id another
    // unit's literal write target names, because resolving it would
    // require running the chain, and the preflight runs before any leaf
    // in the request dispatches.
    let server = large_result_test_server();
    let response = server
        .dispatch_request_local(RequestParams {
            ops: concat!(
                "[",
                "record_write(marker=\"x\") | update(id=$prev.marker, name=\"dynamic\"), ",
                "update(id=\"x\", name=\"literal\")",
                "]"
            )
            .to_string(),
            presentation: Some("verbose".to_string()),
            format: Some("json".to_string()),
            ..Default::default()
        })
        .await
        .expect("bracketed batch dispatches");
    let envelope: Value = serde_json::from_str(&response).expect("request envelope");
    let results = envelope["results"].as_array().expect("results");
    assert_eq!(results.len(), 3, "{envelope}");
    for row in results {
        let message = row["error"]["message"].as_str().unwrap_or_default();
        assert!(
            !message.contains("write-key conflict"),
            "a $prev-resolved target must not be visible to the static preflight: {envelope}"
        );
    }
    // Unit 0's own first leaf still dispatches and succeeds; only the
    // `update` verb (unimplemented on this test registry) fails, and it
    // fails as an ordinary per-op error, never as a conflict refusal.
    assert_eq!(results[0]["ok"], true, "{envelope}");
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn bracketed_unit_failure_aborts_only_its_own_tail() {
    // A failing leaf aborts the rest of its OWN unit only; a sibling
    // unit dispatches normally.
    let (pack, _max_in_flight) = unit_barrier_pack(usize::MAX);
    let mut builder = VerbRegistryBuilder::new();
    builder.register(pack);
    let server = KhiveMcpServer::from_registry(builder.build().expect("barrier test registry"));
    let response = server
        .dispatch_request_local(RequestParams {
            ops: concat!("[", "always_fails() | noop(), ", "noop() | noop()", "]").to_string(),
            presentation: Some("verbose".to_string()),
            format: Some("json".to_string()),
            ..Default::default()
        })
        .await
        .expect("bracketed batch dispatches");
    let envelope: Value = serde_json::from_str(&response).expect("request envelope");
    let results = envelope["results"].as_array().expect("results");
    assert_eq!(results.len(), 4, "{envelope}");
    assert_eq!(results[0]["ok"], false, "{envelope}");
    assert_eq!(results[1]["aborted"], true, "{envelope}");
    assert_eq!(results[2]["ok"], true, "{envelope}");
    assert_eq!(results[3]["ok"], true, "{envelope}");
    assert_eq!(
        envelope["summary"],
        json!({"total": 4, "succeeded": 2, "failed": 1, "aborted": 1}),
        "{envelope}"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn bracketed_batch_write_key_conflict_refuses_only_affected_units() {
    // A write key shared by two DIFFERENT units refuses every leaf
    // of BOTH units before any of them dispatch; a disjoint unit with no
    // shared key runs normally alongside the refused ones.
    let server = large_result_test_server();
    let response = server
        .dispatch_request_local(RequestParams {
            ops: concat!(
                "[",
                "update(id=\"same-id\", name=\"from-unit-0\") | record_write(marker=\"u0-tail\"), ",
                "update(id=\"same-id\", name=\"from-unit-1\"), ",
                "record_write(marker=\"u2-solo\") | record_write(marker=\"u2-tail\")",
                "]"
            )
            .to_string(),
            presentation: Some("verbose".to_string()),
            format: Some("json".to_string()),
            ..Default::default()
        })
        .await
        .expect("bracketed batch dispatches");
    let envelope: Value = serde_json::from_str(&response).expect("request envelope");
    let results = envelope["results"].as_array().expect("results");
    assert_eq!(results.len(), 5, "{envelope}");

    assert_eq!(results[0]["ok"], false, "{envelope}");
    assert!(
        results[0]["error"]["message"]
            .as_str()
            .expect("conflict message")
            .contains("write-key conflict"),
        "{envelope}"
    );
    assert_eq!(results[1]["aborted"], true, "{envelope}");

    assert_eq!(results[2]["ok"], false, "{envelope}");
    assert!(
        results[2]["error"]["message"]
            .as_str()
            .expect("conflict message")
            .contains("write-key conflict"),
        "{envelope}"
    );

    assert_eq!(results[3]["ok"], true, "{envelope}");
    assert_eq!(results[3]["result"]["marker"], "u2-solo", "{envelope}");
    assert_eq!(results[4]["ok"], true, "{envelope}");
    assert_eq!(results[4]["result"]["marker"], "u2-tail", "{envelope}");
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn bracketed_batch_same_key_within_one_unit_is_not_refused_as_conflict() {
    // A chain's own repeated write key is legal; both leaves reach
    // dispatch (and fail only because `update` is not a verb this test
    // registry implements, never because of a conflict refusal).
    let server = large_result_test_server();
    let response = server
        .dispatch_request_local(RequestParams {
            ops: r#"[update(id="x", name="1") | update(id="x", name="2")]"#.to_string(),
            presentation: Some("verbose".to_string()),
            format: Some("json".to_string()),
            ..Default::default()
        })
        .await
        .expect("bracketed batch dispatches");
    let envelope: Value = serde_json::from_str(&response).expect("request envelope");
    let results = envelope["results"].as_array().expect("results");
    assert_eq!(results.len(), 2, "{envelope}");
    for row in results {
        let message = row["error"]["message"].as_str().unwrap_or_default();
        assert!(
            !message.contains("write-key conflict"),
            "a chain's own ordered leaves must not be refused as a conflict: {envelope}"
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn bracketed_unit_aggregate_budget_matches_serial_semantics() {
    // The shared aggregate response budget behaves exactly like
    // plain chain mode's own budget accounting (already proven
    // serial-equivalent by
    // `typed_serial_dispatch_retains_one_aggregate_response_budget`): an
    // already-dispatched leaf keeps its real, committed disposition even
    // if its own bytes are what exhausts the budget; only the next
    // never-started leaf is refused with `response_budget_exceeded`, and
    // the rest of that unit's tail aborts behind it. A single bracketed
    // unit with twelve `|`-chained leaves keeps this fully sequential
    // and deterministic (no concurrency ambiguity), exercising the new
    // executor (`ranges = [0..12]`, one unit) with the SAME per-leaf
    // byte size and count as the flat-mode precedent, so the same
    // arithmetic applies.
    let result_bytes = BATCH_RESPONSE_BUDGET_BYTES / 3 - 4096;
    let leaves = vec![format!("large_result(bytes={result_bytes})"); 12].join(" | ");
    let server = large_result_test_server();
    let response = server
        .dispatch_request_local(RequestParams {
            ops: format!("[{leaves}]"),
            presentation: Some("verbose".to_string()),
            format: Some("json".to_string()),
            ..Default::default()
        })
        .await
        .expect("bracketed single-unit chain dispatches");
    let envelope: Value = serde_json::from_str(&response).expect("request envelope");
    let results = envelope["results"].as_array().expect("results");
    assert_eq!(results.len(), 12, "{envelope}");

    let first_budget_error = results
        .iter()
        .position(|row| {
            row["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains("batch response budget"))
        })
        .expect("the unit must reach the aggregate budget before its tail leaf");
    assert!(
        first_budget_error >= 3 && first_budget_error < results.len(),
        "the aggregate budget must not reset per leaf: {envelope}"
    );
    assert!(results[..first_budget_error]
        .iter()
        .all(|row| row["ok"] == json!(true)));
    assert!(results[first_budget_error..]
        .iter()
        .enumerate()
        .all(|(offset, row)| if offset == 0 {
            row["ok"] == json!(false)
                && row["error"]["message"]
                    .as_str()
                    .is_some_and(|m| m.contains(&BATCH_RESPONSE_BUDGET_BYTES.to_string()))
                && row["error"]["domain_disposition"] == json!("not_committed")
        } else {
            row["aborted"] == json!(true)
        }));
    for (index, row) in results.iter().enumerate() {
        assert_eq!(row["op_index"], index, "{envelope}");
        assert_eq!(row["unit_index"], 0, "{envelope}");
        assert_eq!(row["step_index"], index, "{envelope}");
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn bracketed_units_share_one_aggregate_response_budget() {
    // Two units of six large leaves each. Four such entries exceed the
    // request budget, and each unit has at most one leaf in flight, so
    // with one budget shared by the whole request at most five leaves
    // can complete: four recorded before the breach plus the one leaf
    // the other unit may already have in flight. A private budget per
    // unit would let each unit complete four leaves on its own.
    let result_bytes = BATCH_RESPONSE_BUDGET_BYTES / 3 - 4096;
    let unit = vec![format!("large_result(bytes={result_bytes})"); 6].join(" | ");
    let server = large_result_test_server();
    let response = server
        .dispatch_request_local(RequestParams {
            ops: format!("[{unit}, {unit}]"),
            presentation: Some("verbose".to_string()),
            format: Some("json".to_string()),
            ..Default::default()
        })
        .await
        .expect("bracketed two-unit batch dispatches");
    let envelope: Value = serde_json::from_str(&response).expect("request envelope");
    let results = envelope["results"].as_array().expect("results");
    assert_eq!(results.len(), 12, "{envelope}");
    let succeeded = results
        .iter()
        .filter(|row| row["ok"] == json!(true))
        .count();
    assert!(
        (4..=5).contains(&succeeded),
        "one budget shared across units admits four or five leaves, got {succeeded}"
    );
    assert_eq!(envelope["summary"]["succeeded"], succeeded, "{envelope}");
    for row in results.iter().filter(|row| row["ok"] != json!(true)) {
        let refused = row["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("batch response budget"));
        assert!(
            refused || row["aborted"] == json!(true),
            "a leaf past the shared budget is refused or aborted: {row}"
        );
    }
}

#[test]
#[serial_test::serial(config_ledger)]
fn canonical_request_deadline_wrapper_does_not_embed_dispatch_pipeline() {
    // Construct the generators on an explicitly roomy stack so this
    // regression reports their footprint instead of reproducing the LLVM
    // coverage stack abort it is meant to prevent. Nothing is polled, so an
    // empty registry is sufficient and the test performs no storage work.
    let (pipeline_bytes, wrapper_bytes) = std::thread::Builder::new()
        .name("request-dispatch-future-footprint".to_string())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let server = KhiveMcpServer::from_registry(
                VerbRegistryBuilder::new()
                    .build()
                    .expect("empty test registry"),
            );
            let pipeline_bytes = std::mem::size_of_val(&server.dispatch_request_inner_scoped(
                RequestParams {
                    ops: "stats()".to_string(),
                    ..Default::default()
                },
                false,
                None,
                DispatchOrigin::Local,
                false,
            ));
            let wrapper_bytes =
                std::mem::size_of_val(&server.dispatch_request_inner_with_strict_refusals(
                    RequestParams {
                        ops: "stats()".to_string(),
                        ..Default::default()
                    },
                    false,
                    None,
                    DispatchOrigin::Local,
                    false,
                ));
            (pipeline_bytes, wrapper_bytes)
        })
        .expect("spawn request future footprint measurement")
        .join()
        .expect("request future footprint measurement panicked");

    assert!(
        wrapper_bytes.saturating_mul(2) < pipeline_bytes,
        "the canonical deadline wrapper must keep the dispatch pipeline behind a pointer: \
             wrapper={wrapper_bytes}B pipeline={pipeline_bytes}B"
    );
}

#[cfg(unix)]
include!("server/long_poll_deadline_tests.rs");

fn assert_request_read_timed_out(response: &str) {
    let envelope: Value = serde_json::from_str(response).expect("JSON response envelope");
    assert_eq!(envelope["summary"]["failed"], 1, "{envelope}");
    let error = envelope["results"][0]["error"].to_string().to_lowercase();
    assert!(
        error.contains("timeout") || error.contains("timed out"),
        "request read must fail as a timeout: {envelope}"
    );
}

// These dispatch-layer regressions compose with khive-db's
// `request_deadline_interrupts_statement_without_outer_timeout`, which
// separately proves that the same SQL bridge deadline sees SQLite VM
// progress and stops it before returning. Do not assert that paused Tokio
// time reaches the full default here: the SQLite wall-clock/progress
// backstop may win first under instrumentation. The typed timeout proves
// the canonical scope reached the database; the test below independently
// pins absolute Tokio-deadline ordering.
#[tokio::test(start_paused = true)]
#[serial_test::serial(config_ledger)]
async fn local_exec_dispatch_installs_the_default_request_read_deadline() {
    let server = slow_sql_read_test_server();
    let expected = request_read_timeout();
    let response = tokio::time::timeout(
        expected
            .saturating_add(khive_db::sqlite_interrupt_grace_from_env())
            .saturating_add(Duration::from_secs(1)),
        server.dispatch_request_local_for_exec(
            RequestParams {
                ops: "slow_sql_read()".to_string(),
                ..Default::default()
            },
            false,
        ),
    )
    .await
    .expect("canonical local request-read deadline was never installed")
    .expect("deadline is a per-op failure, not an RPC failure");

    assert_request_read_timed_out(&response);
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(config_ledger)]
async fn replay_dispatch_installs_the_default_request_read_deadline() {
    let server = slow_sql_read_test_server();
    let expected = request_read_timeout();
    let response = tokio::time::timeout(
        expected
            .saturating_add(khive_db::sqlite_interrupt_grace_from_env())
            .saturating_add(Duration::from_secs(1)),
        server.dispatch_request_replay_as(
            RequestParams {
                ops: "slow_sql_read()".to_string(),
                ..Default::default()
            },
            "local",
            None,
        ),
    )
    .await
    .expect("canonical replay request-read deadline was never installed")
    .expect("deadline is a per-op failure, not an RPC failure");

    assert_request_read_timed_out(&response);
}

#[tokio::test(start_paused = true)]
#[serial(config_ledger)]
async fn canonical_dispatch_preserves_an_earlier_outer_deadline() {
    let server = slow_sql_read_test_server();
    let outer = Duration::from_millis(50);
    let default = request_read_timeout();
    let started = tokio::time::Instant::now();
    let response = khive_storage::scope_request_read_deadline(
        outer,
        server.dispatch_request_local(RequestParams {
            ops: "pending_read_phase()".to_string(),
            ..Default::default()
        }),
    )
    .await
    .expect("deadline is a per-op failure, not an RPC failure");
    let elapsed = tokio::time::Instant::now().duration_since(started);

    assert!(
        elapsed >= outer,
        "outer deadline fired too early: {elapsed:?}"
    );
    assert!(
        elapsed < default,
        "canonical dispatch renewed an earlier outer deadline: {elapsed:?}"
    );
    assert_request_read_timed_out(&response);
}

/// Receipt-only stand-in for the Git pack. Two operations can return
/// distinguishable reports for the same project without touching a git
/// repository, which lets the request-layer test prove that `request_id`
/// groups receipts but does not uniquely identify one operation.
struct RequestGroupDigestPack {
    project_id: uuid::Uuid,
}

impl khive_types::Pack for RequestGroupDigestPack {
    const NAME: &'static str = "request-group-digest-test";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [khive_runtime::HandlerDef] = &[khive_runtime::HandlerDef {
        name: "git.digest",
        description: "return a request-group receipt fixture",
        visibility: khive_runtime::Visibility::Verb,
        category: khive_runtime::VerbCategory::Commissive,
        params: &[khive_runtime::ParamDef {
            name: "marker",
            param_type: "string",
            required: true,
            description: "distinguishes reports in one request group",
            resolution_mode: khive_types::IdResolutionMode::NotApplicable,
        }],
    }];
}

#[async_trait::async_trait]
impl khive_runtime::PackRuntime for RequestGroupDigestPack {
    fn name(&self) -> &str {
        <Self as khive_types::Pack>::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        <Self as khive_types::Pack>::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        <Self as khive_types::Pack>::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [khive_runtime::HandlerDef] {
        <Self as khive_types::Pack>::HANDLERS
    }

    async fn dispatch(
        &self,
        _verb: &str,
        params: Value,
        _registry: &VerbRegistry,
        _token: &khive_runtime::NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Ok(json!({
            "project_id": self.project_id,
            "marker": params.get("marker").cloned().unwrap_or(Value::Null),
            "done": true,
        }))
    }
}

fn request_group_digest_test_server() -> (
    KhiveMcpServer,
    Arc<dyn khive_storage::EventStore>,
    uuid::Uuid,
) {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let token = runtime
        .authorize(Namespace::local())
        .expect("authorize local");
    let store = runtime.events(&token).expect("event store");
    let project_id = uuid::Uuid::new_v4();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(RequestGroupDigestPack { project_id });
    builder.with_event_store(store.clone());
    let registry = builder.build().expect("request-group registry");
    (KhiveMcpServer::from_registry(registry), store, project_id)
}

async fn assert_request_group_receipts(ops: &str) {
    let (server, store, project_id) = request_group_digest_test_server();
    let response = server
        .dispatch_request_local(RequestParams {
            ops: ops.to_string(),
            request_id: Some(16_470),
            ..Default::default()
        })
        .await
        .expect("grouped digest request succeeds");
    let envelope: Value = serde_json::from_str(&response).expect("JSON response");
    assert_eq!(envelope["summary"]["succeeded"], 2, "{envelope}");
    let mut returned_by_marker = std::collections::HashMap::new();
    for entry in envelope["results"]
        .as_array()
        .expect("batch/chain response results")
    {
        let result = entry.get("result").expect("successful result");
        let marker = result["marker"].as_str().expect("returned marker");
        let receipt_id = result["receipt_id"]
            .as_str()
            .expect("returned receipt_id remains a string");
        uuid::Uuid::parse_str(receipt_id)
            .expect("default MCP presentation must retain the full receipt UUID");
        assert!(
            returned_by_marker
                .insert(marker.to_string(), result.clone())
                .is_none(),
            "markers are unique"
        );
    }

    let page = store
        .query_events(
            EventFilter {
                verbs: vec!["git.digest".to_string()],
                ..EventFilter::default()
            },
            PageRequest {
                limit: 50,
                offset: 0,
            },
        )
        .await
        .expect("query grouped receipts");
    assert_eq!(page.items.len(), 2, "one receipt per successful operation");
    assert!(page.items.iter().all(|event| {
        event.target_id == Some(project_id)
            && event.payload["resource"]["request_id"] == json!(16_470)
    }));

    let mut markers: Vec<&str> = page
        .items
        .iter()
        .map(|event| {
            assert_eq!(
                event.payload["result"]["receipt_id"],
                json!(event.id),
                "receipt_id is the operation-unique selector"
            );
            let marker = event.payload["result"]["marker"].as_str().expect("marker");
            let returned = returned_by_marker
                .remove(marker)
                .expect("every stored marker was returned");
            assert_eq!(
                returned, event.payload["result"],
                "default MCP output must exactly equal the durable receipt result"
            );
            marker
        })
        .collect();
    markers.sort_unstable();
    assert_eq!(markers, vec!["first", "second"]);
    assert!(returned_by_marker.is_empty());
    assert_ne!(page.items[0].id, page.items[1].id);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn duplicate_digest_batch_and_chain_share_request_group_but_keep_distinct_receipts() {
    assert_request_group_receipts(r#"[git.digest(marker="first"), git.digest(marker="second")]"#)
        .await;
    assert_request_group_receipts(r#"git.digest(marker="first") | git.digest(marker="second")"#)
        .await;
}

#[cfg(unix)]
async fn dispatch_large_result_through_daemon(
    server: &KhiveMcpServer,
    ops: String,
    format: Option<String>,
) -> String {
    khive_runtime::daemon::DaemonDispatch::dispatch(
        server, ops, None, None, format, None, false, None,
    )
    .await
    .expect("daemon dispatch")
}

fn large_result_test_server() -> KhiveMcpServer {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(LargeResultPack);
    KhiveMcpServer::from_registry(builder.build().expect("test registry"))
}

fn typed_test_op(tool: &str, args: Value) -> TypedJsonOp {
    let Value::Object(args) = args else {
        panic!("typed test args must be an object")
    };
    TypedJsonOp {
        tool: tool.to_string(),
        args,
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn typed_serial_dispatch_retains_full_batch_write_conflict_preflight() {
    let ops = vec![
        typed_test_op("update", json!({"id": "same-id", "name": "new"})),
        typed_test_op("delete", json!({"id": "same-id"})),
    ];
    let server = large_result_test_server();
    let parallel: Value = serde_json::from_str(
        &server
            .dispatch_typed_json_batch_local_for_exec(
                ops.clone(),
                Some("verbose".to_string()),
                Some("json".to_string()),
                false,
            )
            .await
            .expect("parallel typed dispatch"),
    )
    .expect("parallel response JSON");
    let serial: Value = serde_json::from_str(
        &server
            .dispatch_typed_json_batch_serial_local_for_exec(
                ops,
                Some("verbose".to_string()),
                Some("json".to_string()),
                false,
            )
            .await
            .expect("serial typed dispatch"),
    )
    .expect("serial response JSON");

    assert_eq!(serial["summary"], parallel["summary"]);
    assert_eq!(serial["status"], parallel["status"]);
    for (serial_row, parallel_row) in serial["results"]
        .as_array()
        .expect("serial rows")
        .iter()
        .zip(parallel["results"].as_array().expect("parallel rows"))
    {
        assert_eq!(serial_row["ok"], false);
        assert_eq!(serial_row["tool"], parallel_row["tool"]);
        assert_eq!(serial_row["error"], parallel_row["error"]);
        assert!(serial_row["error"]["message"]
            .as_str()
            .expect("conflict error")
            .contains("writes overlap"));
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn typed_serial_dispatch_retains_one_aggregate_response_budget() {
    let result_bytes = BATCH_RESPONSE_BUDGET_BYTES / 3 - 4096;
    let ops: Vec<TypedJsonOp> = (0..12)
        .map(|_| typed_test_op("large_result", json!({"bytes": result_bytes})))
        .collect();
    let server = large_result_test_server();
    let parallel: Value = serde_json::from_str(
        &server
            .dispatch_typed_json_batch_local_for_exec(
                ops.clone(),
                Some("verbose".to_string()),
                Some("json".to_string()),
                false,
            )
            .await
            .expect("parallel typed dispatch"),
    )
    .expect("parallel response JSON");
    let serial: Value = serde_json::from_str(
        &server
            .dispatch_typed_json_batch_serial_local_for_exec(
                ops,
                Some("verbose".to_string()),
                Some("json".to_string()),
                false,
            )
            .await
            .expect("serial typed dispatch"),
    )
    .expect("serial response JSON");

    let serial_rows = serial["results"].as_array().expect("serial rows");
    let first_serial_budget_error = serial_rows
        .iter()
        .position(|row| {
            row["error"]["message"]
                .as_str()
                .is_some_and(|error| error.contains("batch response budget"))
        })
        .expect("serial tail must contain canonical budget errors");
    assert!(
        first_serial_budget_error >= 3 && first_serial_budget_error < serial_rows.len(),
        "serial dispatch must spend one aggregate budget, not reset it per op"
    );
    assert!(serial_rows[..first_serial_budget_error]
        .iter()
        .all(|row| row["ok"] == json!(true)));
    assert!(serial_rows[first_serial_budget_error..].iter().all(|row| {
        row["ok"] == json!(false)
            && row["error"]["message"]
                .as_str()
                .is_some_and(|error| error.contains(&BATCH_RESPONSE_BUDGET_BYTES.to_string()))
    }));

    let parallel_budget_error = parallel["results"]
        .as_array()
        .expect("parallel rows")
        .iter()
        .find_map(|row| row["error"]["message"].as_str())
        .expect("parallel undispatched tail must use the same budget error");
    assert_eq!(
        serial_rows[first_serial_budget_error]["error"]["message"],
        json!(parallel_budget_error),
        "serial and default typed scheduling must single-source the budget/error contract"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn bounded_batch_preserves_input_order() {
    let count = MAX_BATCH_CONCURRENCY + 3;
    let in_flight = Arc::new(AtomicUsize::new(0));
    let max_in_flight = Arc::new(AtomicUsize::new(0));
    let futures = (0..count).map(|index| {
        batch_task(
            index,
            observed_batch_entry(
                index,
                (count - index) as u64,
                json!({"ok": true, "tool": "probe", "result": {"index": index}}),
                in_flight.clone(),
                max_in_flight.clone(),
            ),
        )
    });

    let results = execute_bounded_batch(futures, usize::MAX, MAX_BATCH_CONCURRENCY).await;

    let indices: Vec<u64> = results
        .iter()
        .map(|entry| entry["result"]["index"].as_u64().expect("result index"))
        .collect();
    assert_eq!(indices, (0..count as u64).collect::<Vec<_>>());
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn bounded_batch_enforces_aggregate_response_budget() {
    assert_eq!(
        BATCH_RESPONSE_BUDGET_BYTES,
        khive_runtime::daemon::MAX_FRAME_BYTES / 2
    );
    let count = MAX_BATCH_CONCURRENCY * 2;
    let budget = BATCH_RESPONSE_BUDGET_BYTES;
    let small = json!({
        "ok": true,
        "tool": "probe",
        "result": "x".repeat(budget / 4 - 128),
    });
    let in_flight = Arc::new(AtomicUsize::new(0));
    let max_in_flight = Arc::new(AtomicUsize::new(0));
    let futures = (0..count).map(|index| {
        let (delay_ms, entry) = if index < 2 {
            (index as u64, small.clone())
        } else if index == 2 {
            (
                30,
                json!({"ok": true, "tool": "probe", "result": "x".repeat(budget)}),
            )
        } else {
            (
                60,
                json!({"ok": true, "tool": "probe", "result": {"index": index}}),
            )
        };
        batch_task(
            index,
            observed_batch_entry(
                index,
                delay_ms,
                entry,
                in_flight.clone(),
                max_in_flight.clone(),
            ),
        )
    });

    let results = tokio::time::timeout(
        Duration::from_secs(1),
        execute_bounded_batch(futures, budget, MAX_BATCH_CONCURRENCY),
    )
    .await
    .expect("started operations must settle promptly after a budget breach");
    let response = parallel_batch_envelope(results);

    assert_eq!(
        response["summary"],
        json!({"total": count, "succeeded": 10, "failed": count - 10, "aborted": 0})
    );
    assert_eq!(response["results"][0]["ok"], true);
    assert_eq!(response["results"][1]["ok"], true);
    assert_eq!(
        response["results"][2]["result"]
            .as_str()
            .expect("breaching result remains truthful")
            .len(),
        budget
    );
    for index in 3..10 {
        assert_eq!(response["results"][index]["ok"], true);
        assert_eq!(response["results"][index]["result"]["index"], index);
    }
    for entry in response["results"]
        .as_array()
        .expect("results")
        .iter()
        .skip(10)
    {
        assert_eq!(entry["ok"], false);
        assert_eq!(entry["domain_disposition"], "not_committed");
        assert_eq!(
            entry["domain_disposition"],
            entry["error"]["domain_disposition"]
        );
        let error = entry["error"]["message"]
            .as_str()
            .expect("budget error message");
        assert!(error.contains("batch response budget"));
        assert!(error.contains(&budget.to_string()));
    }
    assert_eq!(in_flight.load(Ordering::SeqCst), 0);
    serde_json::to_vec(&response).expect("budgeted response must serialize");
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn save_to_writes_full_results_without_inline_response_budgeting() {
    let server = large_result_test_server();
    let dir = tempfile::tempdir().expect("tempdir");
    let sink_path = dir.path().join("full-results.jsonl");
    let result_bytes = BATCH_RESPONSE_BUDGET_BYTES * 3 / 4;
    let response = server
        .dispatch_request_inner(
            RequestParams {
                plan: None,
                ops: format!(
                    "[large_result(bytes={result_bytes}), large_result(bytes={result_bytes})]"
                ),
                presentation: None,
                presentation_per_op: None,
                save_to: Some(sink_path.to_string_lossy().into_owned()),
                format: None,
                format_per_op: None,
                request_id: None,
            },
            false,
            None,
            DispatchOrigin::Local,
        )
        .await
        .expect("save_to dispatch");

    let manifest: Value = serde_json::from_str(&response).expect("manifest JSON");
    assert_eq!(manifest["rows"], 2);
    assert_eq!(manifest["summary"]["succeeded"], 2);
    let rows: Vec<Value> = std::fs::read_to_string(&sink_path)
        .expect("read JSONL")
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid JSONL row"))
        .collect();
    assert_eq!(rows.len(), 2);
    for row in rows {
        assert_eq!(row["ok"], true);
        assert_eq!(
            row["result"].as_str().expect("full result string").len(),
            result_bytes
        );
    }
}

#[tokio::test]
#[serial]
#[serial_test::serial(config_ledger)]
async fn invalid_save_to_refuses_before_create_on_wire_and_cli() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    let dir = tempfile::tempdir().expect("save_to fixture directory");
    std::env::set_var("KHIVE_SAVE_TO_ROOT", dir.path());
    let server = make_daemon_save_to_test_server(Some(dir.path().join("main.db")));
    let create = "create(kind=\"concept\", name=\"save-to-preflight\")";

    let wire_error = server
        .request(
            Parameters(RequestParams {
                ops: create.to_string(),
                save_to: Some("../outside-export-root.jsonl".to_string()),
                ..Default::default()
            }),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect_err("a wire destination outside the export root must fail before create");
    assert_eq!(
        wire_error.data.as_ref().unwrap()["domain_disposition"],
        "not_committed"
    );

    let stats = server
        .dispatch_request_local(RequestParams {
            ops: "stats()".to_string(),
            ..Default::default()
        })
        .await
        .expect("read post-refusal stats");
    let stats: Value = serde_json::from_str(&stats).unwrap();
    assert_eq!(stats["results"][0]["result"]["entities"], 0);

    let valid_path = dir.path().join("inside.jsonl");
    let manifest = server
        .request(
            Parameters(RequestParams {
                ops: create.to_string(),
                save_to: Some(valid_path.to_string_lossy().into_owned()),
                ..Default::default()
            }),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("the same create must succeed with a valid destination");
    let manifest: Value = serde_json::from_str(&manifest).unwrap();
    assert_eq!(manifest["summary"]["succeeded"], 1);

    let cli_error = server
        .dispatch_request_inner(
            RequestParams {
                ops: "create(kind=\"concept\", name=\"cli-save-to-preflight\")".to_string(),
                save_to: Some(dir.path().to_string_lossy().into_owned()),
                ..Default::default()
            },
            false,
            None,
            DispatchOrigin::Local,
        )
        .await
        .expect_err("an operator destination that is a directory must fail before create");
    assert_eq!(
        cli_error.data.as_ref().unwrap()["domain_disposition"],
        "not_committed"
    );

    let stats = server
        .dispatch_request_local(RequestParams {
            ops: "stats()".to_string(),
            ..Default::default()
        })
        .await
        .expect("read post-CLI-refusal stats");
    let stats: Value = serde_json::from_str(&stats).unwrap();
    assert_eq!(stats["results"][0]["result"]["entities"], 1);

    std::env::remove_var("KHIVE_SAVE_TO_ROOT");
}

#[test]
fn save_to_write_failure_retains_known_operation_outcomes() {
    let result = json!({
        "results": [
            {"ok": true, "tool": "create", "result": {"id": "created-id"}},
            {"ok": false, "tool": "link", "domain_disposition": "not_committed",
             "error": {"kind": "invalid_input", "message": "missing endpoint"}},
        ],
        "summary": {"total": 2, "succeeded": 1, "failed": 1, "aborted": 0},
    });
    let error = super::save_to_write_error("save_to: disk full".to_string(), &result);
    let details = error.data.as_ref().expect("save error details");
    assert_eq!(details["domain_disposition"], "unknown");
    assert_eq!(details["summary"]["succeeded"], 1);
    assert_eq!(details["results"][0]["domain_disposition"], "committed");
    assert_eq!(details["results"][0]["result"]["id"], "created-id");
    assert_eq!(details["results"][1]["domain_disposition"], "not_committed");
    assert_eq!(details["results"][1]["error"]["kind"], "invalid_input");
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn local_dispatch_returns_result_larger_than_daemon_frame() {
    let server = large_result_test_server();
    let result_bytes = khive_runtime::daemon::MAX_FRAME_BYTES + 1_024;
    let response = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: format!("large_result(bytes={result_bytes})"),
            presentation: None,
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        })
        .await
        .expect("local dispatch");

    let envelope: Value = serde_json::from_str(&response).expect("response envelope");
    assert_eq!(envelope["results"][0]["ok"], true);
    assert_eq!(
        envelope["results"][0]["result"]
            .as_str()
            .expect("full local result")
            .len(),
        result_bytes
    );
    assert!(envelope["results"][0].get("result_omitted").is_none());
}

#[cfg(unix)]
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn daemon_dispatch_marks_oversized_read_result_reducible_and_not_retryable() {
    let server = large_result_test_server();
    let result_bytes = khive_runtime::daemon::MAX_FRAME_BYTES + 1_024;
    let response = dispatch_large_result_through_daemon(
        &server,
        format!("large_result(bytes={result_bytes})"),
        None,
    )
    .await;

    let envelope: Value = serde_json::from_str(&response).expect("response envelope");
    assert_eq!(envelope["results"][0]["ok"], false);
    assert!(envelope["results"][0].get("result").is_none());
    assert_eq!(
        envelope["results"][0]["error"]["kind"],
        "response_frame_budget_exceeded"
    );
    // A frame-budget overflow is never a pace-and-retry condition —
    // reissuing the identical request overflows identically — so
    // `retryable` stays false even for a side-effect-free `Assertive`
    // verb; `recoverable` carries the actual guidance.
    assert_eq!(envelope["results"][0]["error"]["retryable"], false);
    assert_eq!(
        envelope["results"][0]["error"]["recoverable"],
        "reduce_result_size"
    );
    assert!(envelope["results"][0].get("executed").is_none());
    assert_eq!(envelope["summary"]["succeeded"], 0);
    assert_eq!(envelope["summary"]["failed"], 1);
    assert_eq!(envelope["status"], "partial");
    assert!(rendered_response_fits_daemon_frame(
        &response,
        &server.config_id
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn daemon_dispatch_marks_oversized_side_effecting_assertive_verb_non_retryable_and_executed()
{
    let server = large_result_test_server();
    let result_bytes = khive_runtime::daemon::MAX_FRAME_BYTES + 1_024;
    let response = dispatch_large_result_through_daemon(
        &server,
        format!("memory.recall(bytes={result_bytes})"),
        None,
    )
    .await;

    let envelope: Value = serde_json::from_str(&response).expect("response envelope");
    assert_eq!(envelope["results"][0]["ok"], false);
    assert!(envelope["results"][0].get("result").is_none());
    assert_eq!(
        envelope["results"][0]["error"]["kind"],
        "response_frame_budget_exceeded"
    );
    // `memory.recall` is declared `Assertive`, but every dispatch
    // schedules a persisted `brain.record_serve` write
    // (`VerbRegistry::SIDE_EFFECTING_ASSERTIVE_VERBS`); a lost response
    // must not be advertised as safe to reissue, or a caller acting on
    // that advice duplicates the serve-ledger write. This also proves
    // the omission decision resolves a qualified `pack.verb` name
    // (containing a `.`) through the same registry lookup as a bare
    // verb name.
    assert_eq!(envelope["results"][0]["error"]["retryable"], false);
    assert_eq!(
        envelope["results"][0]["error"]["recoverable"],
        "read_outcome"
    );
    assert_eq!(envelope["results"][0]["executed"], true);
    assert!(rendered_response_fits_daemon_frame(
        &response,
        &server.config_id
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn daemon_dispatch_marks_oversized_write_result_non_retryable_and_executed() {
    let server = large_result_test_server();
    let result_bytes = khive_runtime::daemon::MAX_FRAME_BYTES + 1_024;
    let response = dispatch_large_result_through_daemon(
        &server,
        format!("large_write(bytes={result_bytes})"),
        None,
    )
    .await;

    let envelope: Value = serde_json::from_str(&response).expect("response envelope");
    assert_eq!(envelope["results"][0]["ok"], false);
    assert!(envelope["results"][0].get("result").is_none());
    assert_eq!(
        envelope["results"][0]["error"]["kind"],
        "response_frame_budget_exceeded"
    );
    // `large_write` is Commissive: it already committed its change before
    // the transport discovered the response was too large to return, so
    // a caller must not be told it is safe to reissue the operation.
    assert_eq!(envelope["results"][0]["error"]["retryable"], false);
    assert_eq!(
        envelope["results"][0]["error"]["recoverable"],
        "read_outcome"
    );
    assert_eq!(envelope["results"][0]["executed"], true);
    assert!(rendered_response_fits_daemon_frame(
        &response,
        &server.config_id
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn daemon_chain_reports_later_write_truthfully_after_earlier_frame_budget_omission() {
    let server = large_result_test_server();
    let result_bytes = khive_runtime::daemon::MAX_FRAME_BYTES + 1_024;
    let response = dispatch_large_result_through_daemon(
        &server,
        format!(r#"large_write(bytes={result_bytes}) | record_write(marker="second")"#),
        None,
    )
    .await;

    let envelope: Value = serde_json::from_str(&response).expect("response envelope");
    // The frame-budget decision is made at render time, after `run_parsed`
    // has already dispatched every chain operation — `record_write` really
    // ran and committed. Its entry must report that real outcome, not a
    // fabricated `aborted: true`, even though the sibling entry before it
    // is reported as failed.
    assert_eq!(envelope["results"][0]["ok"], false);
    assert_eq!(envelope["results"][0]["executed"], true);
    assert_eq!(
        envelope["results"][0]["error"]["kind"],
        "response_frame_budget_exceeded"
    );
    assert_eq!(envelope["results"][1]["ok"], true);
    assert_eq!(envelope["results"][1]["tool"], "record_write");
    assert!(envelope["results"][1].get("aborted").is_none());
    assert_eq!(envelope["results"][1]["result"]["committed"], true);
    assert_eq!(envelope["results"][1]["result"]["marker"], "second");
    assert_eq!(envelope["summary"]["total"], 2);
    assert_eq!(envelope["summary"]["succeeded"], 1);
    assert_eq!(envelope["summary"]["failed"], 1);
    assert_eq!(envelope["summary"]["aborted"], 0);
    // `batch_status` only distinguishes success/partial; a frame-budget
    // omission must not be reported as an abort trigger.
    assert_eq!(envelope["status"], "partial");
}

#[test]
#[serial_test::serial(config_ledger)]
fn read_only_audit_advisory_decorates_success_but_not_help_or_error() {
    let mut builder = VerbRegistryBuilder::new();
    builder.with_read_only_audit_store();
    let registry = builder.build().expect("registry builds");
    let mut response = json!({
        "results": [
            {"ok": true, "tool": "stats", "result": {"entities": 0}},
            {"ok": true, "tool": "list", "result": {"items": []}},
            {"ok": true, "tool": "stats", "result": {
                "verb": "stats", "pack": "kg", "description": "help", "category": "assertive",
                "identifier_resolution": {
                    "full_uuid": "canonical", "short_prefix": "prefix",
                    "parameter_rule": "strict"
                }
            }},
            {"ok": false, "tool": "create", "error": "read-only"}
        ],
        "summary": {"total": 4, "succeeded": 3, "failed": 1, "aborted": 0},
        "status": "partial"
    });

    attach_audit_persistence_advisories(&mut response, &registry);

    assert_eq!(
        response["results"][0]["advisories"][0]["code"],
        khive_runtime::AUDIT_PERSISTENCE_SKIPPED_READ_ONLY
    );
    assert_eq!(
        response["results"][1]["advisories"][0]["code"],
        khive_runtime::AUDIT_PERSISTENCE_SKIPPED_READ_ONLY
    );
    assert!(response["results"][1]["result"]["items"].is_array());
    assert!(response["results"][2].get("advisories").is_none());
    assert!(response["results"][3].get("advisories").is_none());

    let omitted = frame_budget_omission(&response["results"][0], &registry);
    assert!(
        omitted.get("advisories").is_some(),
        "frame-budget degradation must preserve the warning"
    );
}

/// Registry carrying just the verb categories the `frame_budget_omission`
/// unit tests below need to resolve: `search` (Assertive, matches the KG
/// pack) and `create` (Commissive, matches the KG pack).
struct FrameBudgetCategoryTestPack;

impl khive_types::Pack for FrameBudgetCategoryTestPack {
    const NAME: &'static str = "frame-budget-category-test";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [khive_runtime::HandlerDef] = &[
        khive_runtime::HandlerDef {
            name: "search",
            description: "test double matching the KG pack's Assertive search category",
            visibility: khive_runtime::Visibility::Verb,
            category: khive_runtime::VerbCategory::Assertive,
            params: &[],
        },
        khive_runtime::HandlerDef {
            name: "create",
            description: "test double matching the KG pack's Commissive create category",
            visibility: khive_runtime::Visibility::Verb,
            category: khive_runtime::VerbCategory::Commissive,
            params: &[],
        },
    ];
}

#[async_trait::async_trait]
impl khive_runtime::PackRuntime for FrameBudgetCategoryTestPack {
    fn name(&self) -> &str {
        <Self as khive_types::Pack>::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        <Self as khive_types::Pack>::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        <Self as khive_types::Pack>::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [khive_runtime::HandlerDef] {
        <Self as khive_types::Pack>::HANDLERS
    }

    async fn dispatch(
        &self,
        _verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &khive_runtime::NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Ok(json!({}))
    }
}

fn frame_budget_category_test_registry() -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(FrameBudgetCategoryTestPack);
    builder
        .build()
        .expect("frame-budget category test registry")
}

#[test]
#[serial_test::serial(config_ledger)]
fn frame_budget_omission_preserves_search_degradation_advisory() {
    let registry = frame_budget_category_test_registry();
    let omitted = frame_budget_omission(
        &json!({
            "ok": true,
            "tool": "search",
            "result": "oversized",
            "status": "partial",
            "partial": true,
            "missing_backends": ["archive"],
            "backend_errors": {
                "archive": {
                    "kind": "backend_error",
                    "message": "storage unavailable"
                }
            },
        }),
        &registry,
    );

    assert_eq!(omitted["ok"], json!(false));
    // ADR-130 defines `status`/`partial`/`missing_backends`/`backend_errors`
    // only on a successful search entry; once `ok` flips to false they no
    // longer appear at the top level, but the diagnostic survives under
    // `error.search`.
    assert!(omitted.get("status").is_none());
    assert!(omitted.get("partial").is_none());
    assert!(omitted.get("missing_backends").is_none());
    assert!(omitted.get("backend_errors").is_none());
    assert_eq!(omitted["error"]["search"]["status"], json!("partial"));
    assert_eq!(omitted["error"]["search"]["partial"], json!(true));
    assert_eq!(
        omitted["error"]["search"]["missing_backends"],
        json!(["archive"])
    );
    assert_eq!(
        omitted["error"]["search"]["backend_errors"]["archive"]["message"],
        json!("storage unavailable")
    );
    assert!(omitted.get("result").is_none());
    assert_eq!(
        omitted["error"]["kind"],
        json!("response_frame_budget_exceeded")
    );
    // `search` is Assertive, but the kg pack's real handler schedules a
    // best-effort `SearchExecuted` telemetry event on every dispatch
    // with no dedup key (`VerbRegistry::SIDE_EFFECTING_ASSERTIVE_VERBS`),
    // so a lost response must not be advertised as safe to reissue.
    assert_eq!(omitted["error"]["retryable"], json!(false));
    assert_eq!(omitted["error"]["recoverable"], json!("read_outcome"));
    assert_eq!(omitted["executed"], json!(true));
}

#[test]
fn backend_error_evidence_is_masked_and_bounded_before_preservation() {
    let secret = format!("storage auth token sk_live_{} failed", "a".repeat(32));
    let masked = bounded_backend_error_message(&secret);
    assert!(masked.contains("***MASKED***"));
    assert!(!masked.contains("sk_live_"));

    let backend_secret = format!("archive auth token sk_live_{}", "b".repeat(32));
    let (key, key_masked, key_truncated, key_chars) = bounded_backend_error_key(&backend_secret);
    assert!(key_masked);
    assert!(!key_truncated);
    assert_eq!(key_chars, backend_secret.chars().count());
    assert!(key.contains("***MASKED***"));
    assert!(!key.contains("sk_live_"));
    assert!(key.chars().count() <= MAX_BACKEND_ERROR_KEY_CHARS);

    let oversized = "x".repeat(MAX_BACKEND_ERROR_MESSAGE_CHARS + 100);
    let bounded = bounded_backend_error_message(&oversized);
    assert_eq!(bounded.chars().count(), MAX_BACKEND_ERROR_MESSAGE_CHARS + 1);
    assert!(bounded.ends_with('…'));
    assert_eq!(
        bounded_backend_error_message(" \t\n"),
        MISSING_BACKEND_ERROR_MESSAGE
    );
}

#[test]
fn backend_error_message_drops_a_url_credential_whose_terminator_crosses_the_window() {
    // The credential's `scheme://user:` opens well inside the shared
    // MASK_WINDOW_CHARS window (and well inside the visible
    // MAX_BACKEND_ERROR_MESSAGE_CHARS output window), but the password is
    // padded long enough that the terminating `@` lands past the window.
    // A masker restricted to the window can never observe that `@`, so a
    // truncate-then-mask policy could never recognize the span and the
    // password prefix — including this marker — would survive untouched.
    // `mask_bounded` closes that hole a different way: since the whole
    // `postgres://...` token has no internal whitespace, it is dropped
    // in its entirety (back to the space before it) rather than passed
    // to the masker partially — so no fragment of it, marked or not,
    // reaches the output. The padding character is repeated so the run
    // stays low-entropy and cannot be caught by the entropy heuristic
    // instead of the url-userinfo detector this test used to target.
    let marker = "CustomDbPassMarkerXYZ789";
    let padding = "z".repeat(khive_runtime::secret_gate::MASK_WINDOW_CHARS + 200);
    let password = format!("{marker}{padding}");
    let url = format!("postgres://svc:{password}@internal-host.example.com/db");
    let message = format!("backend probe failed: {url}");

    let at_offset = message.find('@').expect("test fixture must contain '@'");
    assert!(at_offset > khive_runtime::secret_gate::MASK_WINDOW_CHARS);
    let marker_offset = message
        .find(marker)
        .expect("test fixture must contain marker");
    assert!(marker_offset < MAX_BACKEND_ERROR_MESSAGE_CHARS);

    let masked = bounded_backend_error_message(&message);
    assert!(
        !masked.contains(marker),
        "no fragment of the credential may survive: {masked}"
    );
    assert!(
        !masked.contains("postgres://"),
        "the straddling token must be dropped whole, not partially echoed: {masked}"
    );
    assert!(
        masked.starts_with("backend probe failed:"),
        "the untruncated prose before the dropped token must survive: {masked}"
    );
    assert!(masked.ends_with('…'));
}

#[test]
fn backend_error_key_masks_a_url_credential_whose_terminator_crosses_the_window() {
    // Mirrors backend_error_message_drops_a_url_credential_whose_terminator_crosses_the_window:
    // the backend id itself can carry a credential whose terminating `@`
    // lands past MASK_WINDOW_CHARS. Unlike the message case there is no
    // leading prose to fall back to, so the whole id is dropped and the
    // fingerprint path takes over.
    let marker = "CustomDbPassMarkerXYZ789";
    let padding = "z".repeat(khive_runtime::secret_gate::MASK_WINDOW_CHARS + 200);
    let password = format!("{marker}{padding}");
    let backend_id = format!("postgres://svc:{password}@internal-host.example.com/db");

    let at_offset = backend_id.find('@').expect("test fixture must contain '@'");
    assert!(at_offset > khive_runtime::secret_gate::MASK_WINDOW_CHARS);

    let (key, backend_id_masked, _truncated, _chars) = bounded_backend_error_key(&backend_id);
    assert!(
        !key.contains(marker),
        "no fragment of the credential may survive masking: {key}"
    );
    assert!(
        backend_id_masked,
        "backend_id_masked must be true when the id carried a credential"
    );
}

#[test]
#[serial_test::serial(config_ledger)]
fn backend_error_evidence_has_aggregate_budget_and_exact_key_parity() {
    fn degraded_result(reverse: bool) -> CoordSearchResult {
        let mut per_backend: Vec<crate::coordinator::BackendSearchResult> = (0
            ..MAX_BACKEND_ERROR_ENTRIES + 9)
            .map(|index| crate::coordinator::BackendSearchResult {
                backend_id: khive_runtime::BackendId::parse(format!(
                    "backend-{index:03}-{}",
                    "x".repeat(MAX_BACKEND_ERROR_KEY_CHARS)
                ))
                .expect("valid backend id"),
                entity_hits: Vec::new(),
                note_hits: Vec::new(),
                vector_selected: true,
                error: Some(BackendSearchFailure::backend(format!(
                    "backend failure {index}: {}",
                    "\0\"\\".repeat(MAX_BACKEND_ERROR_MESSAGE_CHARS)
                ))),
                vector_error: None,
            })
            .collect();
        if reverse {
            per_backend.reverse();
        }
        CoordSearchResult {
            entity_hits: Vec::new(),
            note_hits: Vec::new(),
            per_backend,
            partial: true,
            entity_kinds: std::collections::HashMap::new(),
            note_kinds: std::collections::HashMap::new(),
            entity_created_at: std::collections::HashMap::new(),
            entity_updated_at: std::collections::HashMap::new(),
            entity_versions: std::collections::HashMap::new(),
            note_created_at: std::collections::HashMap::new(),
            note_updated_at: std::collections::HashMap::new(),
            note_versions: std::collections::HashMap::new(),
            note_names: std::collections::HashMap::new(),
        }
    }

    let forward = SearchDegradation::from_result(&degraded_result(false), &json!([]), "all_terms");
    let reversed = SearchDegradation::from_result(&degraded_result(true), &json!([]), "all_terms");

    assert!(!forward.backend_errors.is_empty());
    assert!(forward.backend_errors.len() <= MAX_BACKEND_ERROR_ENTRIES);
    assert!(forward.backend_errors.iter().all(|(backend, diagnostic)| {
        backend.chars().count() <= MAX_BACKEND_ERROR_KEY_CHARS
            && diagnostic.backend_id_truncated
            && diagnostic.message.chars().count() <= MAX_BACKEND_ERROR_MESSAGE_CHARS + 1
    }));
    assert_eq!(
        forward.missing_backends,
        forward.backend_errors.keys().cloned().collect::<Vec<_>>()
    );
    assert_eq!(
        forward.backend_errors_omitted,
        MAX_BACKEND_ERROR_ENTRIES + 9 - forward.backend_errors.len()
    );
    assert_eq!(forward.missing_backends, reversed.missing_backends);
    assert_eq!(
        backend_errors_value(&forward.backend_errors),
        backend_errors_value(&reversed.backend_errors)
    );
    assert!(search_diagnostic_wire_len(&forward) <= MAX_SEARCH_DIAGNOSTIC_BYTES_PER_OP);

    let envelope = ok_envelope(
        "search".to_string(),
        OpSuccess {
            result: json!([{"id": "11111111-1111-1111-1111-111111111111"}]),
            degradation: forward,
        },
    );
    assert_eq!(envelope["backend_errors_truncated"], true);
    assert!(envelope["backend_errors_omitted"].as_u64().unwrap() > 0);
    assert_eq!(
        envelope["missing_backends"].as_array().unwrap(),
        &envelope["backend_errors"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .map(Value::String)
            .collect::<Vec<_>>()
    );
}

/// A populated `vector_error` is itself proof the vector arm was
/// selected and failed. `vector_selected` on the backend can be a
/// registry miss (stale/absent metadata) — it must never hide a
/// recorded vector-arm failure.
#[test]
#[serial_test::serial(config_ledger)]
fn vector_error_reports_arm_failure_even_when_vector_selected_is_false() {
    let result = CoordSearchResult {
        entity_hits: Vec::new(),
        note_hits: Vec::new(),
        per_backend: vec![crate::coordinator::BackendSearchResult {
            backend_id: khive_runtime::BackendId::parse("main").expect("valid backend id"),
            entity_hits: Vec::new(),
            note_hits: Vec::new(),
            vector_selected: false,
            error: None,
            vector_error: Some("injected vector-arm failure".to_string()),
        }],
        partial: false,
        entity_kinds: std::collections::HashMap::new(),
        note_kinds: std::collections::HashMap::new(),
        entity_created_at: std::collections::HashMap::new(),
        entity_updated_at: std::collections::HashMap::new(),
        entity_versions: std::collections::HashMap::new(),
        note_created_at: std::collections::HashMap::new(),
        note_updated_at: std::collections::HashMap::new(),
        note_versions: std::collections::HashMap::new(),
        note_names: std::collections::HashMap::new(),
    };

    let degradation = SearchDegradation::from_result(&result, &json!([]), "all_terms");
    let arm_participation = degradation
        .arm_participation
        .expect("arm participation must be computed");
    assert_eq!(
        arm_participation.vector.status,
        SearchArmStatus::Error,
        "a recorded vector_error must report the vector arm as failed \
             regardless of the backend's vector_selected flag"
    );
}

#[test]
#[serial_test::serial(config_ledger)]
fn backend_id_credentials_are_absent_from_wire_and_warning() {
    let secret = format!("archive auth token sk_live_{}", "c".repeat(32));
    let result = CoordSearchResult {
        entity_hits: Vec::new(),
        note_hits: Vec::new(),
        per_backend: vec![crate::coordinator::BackendSearchResult {
            backend_id: khive_runtime::BackendId::parse(secret.clone()).expect("valid backend id"),
            entity_hits: Vec::new(),
            note_hits: Vec::new(),
            vector_selected: true,
            error: Some(BackendSearchFailure::backend("storage unavailable")),
            vector_error: None,
        }],
        partial: true,
        entity_kinds: std::collections::HashMap::new(),
        note_kinds: std::collections::HashMap::new(),
        entity_created_at: std::collections::HashMap::new(),
        entity_updated_at: std::collections::HashMap::new(),
        entity_versions: std::collections::HashMap::new(),
        note_created_at: std::collections::HashMap::new(),
        note_updated_at: std::collections::HashMap::new(),
        note_versions: std::collections::HashMap::new(),
        note_names: std::collections::HashMap::new(),
    };
    let captured = SearchCapturedLog::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_ansi(false)
        .without_time()
        .finish();
    let degradation = tracing::subscriber::with_default(subscriber, || {
        SearchDegradation::from_result(&result, &json!([]), "all_terms")
    });
    let wire = search_diagnostic_value(&degradation).to_string();
    let logs = captured.contents();

    assert!(!wire.contains("sk_live_"), "wire leaked backend credential");
    assert!(
        !logs.contains("sk_live_"),
        "warning leaked backend credential"
    );
    let backend = degradation
        .missing_backends
        .first()
        .expect("failed backend diagnostic retained");
    assert!(backend.contains("***MASKED***"));
    assert!(degradation.backend_errors[backend].backend_id_masked);
}

fn degraded_search_result(
    failures: impl IntoIterator<Item = (String, BackendSearchFailure)>,
) -> CoordSearchResult {
    CoordSearchResult {
        entity_hits: Vec::new(),
        note_hits: Vec::new(),
        per_backend: failures
            .into_iter()
            .map(
                |(backend_id, error)| crate::coordinator::BackendSearchResult {
                    backend_id: khive_runtime::BackendId::parse(backend_id)
                        .expect("valid backend id"),
                    entity_hits: Vec::new(),
                    note_hits: Vec::new(),
                    vector_selected: false,
                    error: Some(error),
                    vector_error: None,
                },
            )
            .collect(),
        partial: true,
        entity_kinds: std::collections::HashMap::new(),
        note_kinds: std::collections::HashMap::new(),
        entity_created_at: std::collections::HashMap::new(),
        entity_updated_at: std::collections::HashMap::new(),
        entity_versions: std::collections::HashMap::new(),
        note_created_at: std::collections::HashMap::new(),
        note_updated_at: std::collections::HashMap::new(),
        note_versions: std::collections::HashMap::new(),
        note_names: std::collections::HashMap::new(),
    }
}

#[test]
fn search_failure_classification_timeout_only_is_retryable_and_typed() {
    let result = degraded_search_result([(
        "archive".to_string(),
        BackendSearchFailure::timeout("backend search timed out after 5000ms"),
    )]);

    let diagnostic = search_diagnostic_value(&SearchDegradation::from_result(
        &result,
        &json!([]),
        "all_terms",
    ));

    assert_eq!(diagnostic["retryable"], json!(true));
    assert_eq!(diagnostic["retry_after_ms"], json!(2_000));
    assert_eq!(
        diagnostic["backend_errors"]["archive"]["kind"],
        json!("timeout")
    );
}

#[test]
fn search_retry_pace_scales_with_the_full_failed_backend_set_and_is_capped() {
    assert_eq!(search_retry_after_ms(1), 2_000);
    assert_eq!(search_retry_after_ms(2), 2_250);
    assert_eq!(search_retry_after_ms(9), 4_000);
    assert_eq!(search_retry_after_ms(usize::MAX), 10_000);
}

#[test]
fn search_failure_classification_does_not_parse_timeout_from_backend_error_text() {
    let result = degraded_search_result([(
        "archive".to_string(),
        BackendSearchFailure::backend("backend search timed out after 5000ms"),
    )]);

    let diagnostic = search_diagnostic_value(&SearchDegradation::from_result(
        &result,
        &json!([]),
        "all_terms",
    ));

    assert_eq!(diagnostic["retryable"], json!(false));
    assert!(diagnostic.get("retry_after_ms").is_none());
    assert_eq!(
        diagnostic["backend_errors"]["archive"]["kind"],
        json!("backend_error")
    );
}

#[test]
fn search_failure_classification_mixed_is_not_retryable_and_keeps_each_kind() {
    let result = degraded_search_result([
        (
            "archive".to_string(),
            BackendSearchFailure::timeout("backend search timed out after 5000ms"),
        ),
        (
            "main".to_string(),
            BackendSearchFailure::backend("storage unavailable"),
        ),
    ]);

    let diagnostic = search_diagnostic_value(&SearchDegradation::from_result(
        &result,
        &json!([]),
        "all_terms",
    ));

    assert_eq!(diagnostic["retryable"], json!(false));
    assert!(diagnostic.get("retry_after_ms").is_none());
    assert_eq!(
        diagnostic["backend_errors"]["archive"]["kind"],
        json!("timeout")
    );
    assert_eq!(
        diagnostic["backend_errors"]["main"]["kind"],
        json!("backend_error")
    );
}

#[test]
fn search_failure_classification_omitted_non_timeout_still_controls_retryability() {
    let mut failures: Vec<(String, BackendSearchFailure)> = (0..MAX_BACKEND_ERROR_ENTRIES + 4)
        .map(|index| {
            (
                format!("backend-{index:03}"),
                BackendSearchFailure::timeout("backend search timed out after 5000ms"),
            )
        })
        .collect();
    failures.push((
        "zzzz-hidden-backend".to_string(),
        BackendSearchFailure::backend("storage unavailable"),
    ));
    let degradation =
        SearchDegradation::from_result(&degraded_search_result(failures), &json!([]), "all_terms");
    let diagnostic = search_diagnostic_value(&degradation);

    assert!(degradation.backend_errors_omitted > 0);
    assert!(!degradation
        .backend_errors
        .contains_key("zzzz-hidden-backend"));
    assert!(degradation
        .backend_errors
        .values()
        .all(|error| error.kind == BackendSearchFailureKind::Timeout));
    assert_eq!(diagnostic["retryable"], json!(false));
}

#[test]
fn search_failure_classification_all_timeouts_stays_retryable_when_truncated() {
    let failures = (0..MAX_BACKEND_ERROR_ENTRIES + 5).map(|index| {
        (
            format!("backend-{index:03}"),
            BackendSearchFailure::timeout("backend search timed out after 5000ms"),
        )
    });
    let degradation =
        SearchDegradation::from_result(&degraded_search_result(failures), &json!([]), "all_terms");
    let diagnostic = search_diagnostic_value(&degradation);

    assert!(degradation.backend_errors_omitted > 0);
    assert_eq!(diagnostic["retryable"], json!(true));
    assert!(diagnostic["retry_after_ms"].as_u64().unwrap() > 2_000);
}

#[test]
#[serial_test::serial(config_ledger)]
fn frame_budget_omission_preserves_complete_search_status() {
    let registry = frame_budget_category_test_registry();
    let omitted = frame_budget_omission(
        &json!({
            "ok": true,
            "tool": "search",
            "result": "oversized",
            "status": "complete",
            "arm_participation": {
                "text": {"mode": "all_terms", "status": "ran", "candidate_count": 0},
                "vector": {"status": "skipped", "candidate_count": 0}
            },
        }),
        &registry,
    );

    assert_eq!(omitted["ok"], json!(false));
    assert!(omitted.get("status").is_none());
    assert!(omitted.get("arm_participation").is_none());
    assert!(omitted.get("partial").is_none());
    assert!(omitted.get("result").is_none());
    assert_eq!(omitted["error"]["search"]["status"], json!("complete"));
    assert_eq!(
        omitted["error"]["search"]["arm_participation"],
        json!({
            "text": {"mode": "all_terms", "status": "ran", "candidate_count": 0},
            "vector": {"status": "skipped", "candidate_count": 0}
        })
    );
    assert_eq!(
        omitted["error"]["code"],
        json!("response_frame_budget_exceeded")
    );
}

/// ADR-130 §Compatibility: the `search_incomplete` error is small and
/// typed — it must survive frame-budget omission untransformed, not
/// collapse to the generic omitted-error string.
#[test]
#[serial_test::serial(config_ledger)]
fn frame_budget_omission_preserves_search_incomplete_error_untransformed() {
    let registry = frame_budget_category_test_registry();
    let error = json!({
        "kind": "search_incomplete",
        "message": "no-match was not established because selected backends failed",
        "retryable": false,
        "arm_participation": {
            "text": {"mode": "all_terms", "status": "error", "candidate_count": 0},
            "vector": {"status": "error", "candidate_count": 0}
        },
        "missing_backends": ["archive"],
        "backend_errors": {
            "archive": {
                "kind": "backend_error",
                "message": "storage unavailable"
            }
        },
        "backend_errors_truncated": true,
        "backend_errors_omitted": 2,
    });
    let omitted = frame_budget_omission(
        &json!({
            "ok": false,
            "tool": "search",
            "error": error,
        }),
        &registry,
    );

    assert_eq!(omitted["ok"], json!(false));
    assert_eq!(omitted["error"], error);
}

#[test]
#[serial_test::serial(config_ledger)]
fn frame_budget_omission_still_collapses_other_large_errors() {
    let registry = frame_budget_category_test_registry();
    let omitted = frame_budget_omission(
        &json!({
            "ok": false,
            "tool": "create",
            "error": { "kind": "invalid_input", "message": "x".repeat(10_000) },
        }),
        &registry,
    );

    assert_eq!(omitted["ok"], json!(false));
    assert_eq!(
        omitted["error"],
        json!({
            "kind": "response_frame_budget_exceeded",
            "code": "response_frame_budget_exceeded",
            "message": "operation failed; error details omitted because the response frame budget was exceeded",
            "domain_disposition": "unknown",
            "max_frame_bytes": khive_runtime::daemon::MAX_FRAME_BYTES,
            "retryable": false
        })
    );
}

#[test]
fn frame_budget_omission_marks_commissive_verb_non_retryable() {
    let registry = frame_budget_category_test_registry();
    let omitted = frame_budget_omission(
        &json!({
            "ok": true,
            "tool": "create",
            "result": "oversized",
        }),
        &registry,
    );

    assert_eq!(omitted["ok"], json!(false));
    assert_eq!(omitted["executed"], json!(true));
    assert_eq!(omitted["error"]["retryable"], json!(false));
    assert_eq!(omitted["error"]["recoverable"], json!("read_outcome"));
    assert_eq!(
        omitted["error"]["kind"],
        json!("response_frame_budget_exceeded")
    );
}

#[test]
fn frame_budget_omission_marks_unknown_verb_non_retryable() {
    let registry = frame_budget_category_test_registry();
    let omitted = frame_budget_omission(
        &json!({
            "ok": true,
            "tool": "some_future_unregistered_verb",
            "result": "oversized",
        }),
        &registry,
    );

    assert_eq!(omitted["ok"], json!(false));
    assert_eq!(omitted["executed"], json!(true));
    assert_eq!(omitted["error"]["retryable"], json!(false));
    assert_eq!(omitted["error"]["recoverable"], json!("read_outcome"));
}

#[cfg(unix)]
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn daemon_batch_keeps_rendered_result_when_compact_result_exceeds_frame() {
    let server = large_result_test_server();
    let row_bytes = khive_runtime::daemon::MAX_FRAME_BYTES / 2;
    let response = dispatch_large_result_through_daemon(
        &server,
        format!("large_result(table_bytes={row_bytes})"),
        Some("auto".to_string()),
    )
    .await;

    let envelope: Value = serde_json::from_str(&response).expect("response envelope");
    let entry = &envelope["results"][0];
    assert_eq!(entry["ok"], true);
    assert!(entry.get("result_omitted").is_none());
    let rendered = entry["result"].as_str().expect("rendered table result");
    assert!(rendered.starts_with("| payload |"));
    assert!(rendered.len() < row_bytes);
    assert!(rendered_response_fits_daemon_frame(
        &response,
        &server.config_id
    ));
}

#[test]
#[serial_test::serial(config_ledger)]
fn daemon_frame_fitting_preserves_reason_when_error_body_is_omitted() {
    let entry = json!({
        "ok": false,
        "tool": "create",
        "error": "x".repeat(khive_runtime::daemon::MAX_FRAME_BYTES + 1_024),
        "reason": "gate-refusal",
    });
    let envelope = parallel_batch_envelope(vec![entry.clone()]);
    let registry = frame_budget_category_test_registry();
    let fitted = fit_rendered_batch_envelope(
        envelope.as_object().expect("batch envelope object"),
        std::slice::from_ref(&entry),
        vec![entry.clone()],
        "test-config",
        &registry,
    );
    let fitted = Value::Object(fitted);

    assert_eq!(fitted["results"][0]["ok"], false);
    assert_eq!(fitted["results"][0]["reason"], "gate-refusal");
    assert!(fitted["results"][0]["error"]["message"]
        .as_str()
        .is_some_and(|error| error.contains("frame budget was exceeded")));
    assert!(rendered_response_fits_daemon_frame(
        &serialize_response_value(&fitted),
        "test-config"
    ));
}

/// `envelope_escaped_len` claims that a batch envelope's daemon-frame
/// length can be derived from `envelope_metadata_escaped_len` (the
/// envelope with `results: []`) plus each entry's own escaped length
/// plus separators, without ever re-serializing the populated envelope.
/// Check that claim by direct measurement across shapes that stress the
/// escaping (nested containers, quotes, backslashes, control characters).
#[test]
fn envelope_escaped_len_matches_direct_daemon_frame_serialization() {
    let served_config_id = "escape-probe";
    let shapes: Vec<Vec<Value>> = vec![
        vec![],
        vec![json!({"ok": true, "tool": "t", "result": 1})],
        (0..5)
            .map(|i| json!({"ok": true, "tool": format!("t{i}"), "result": i}))
            .collect(),
        vec![
            json!({
                "ok": true,
                "tool": "t",
                "result": {"nested": [1, 2, [3, 4], "a\"b\\c\nline\ttab\u{1}ctrl\u{7f}del"]},
            }),
            json!({
                "ok": false,
                "tool": "t2",
                "error": "quote \" backslash \\ newline \n unicode \u{1}",
            }),
        ],
    ];

    for results in shapes {
        let entry_count = results.len();
        let envelope = parallel_batch_envelope(results.clone());
        let map = envelope.as_object().expect("batch envelope object");
        let metadata = envelope_metadata(map);
        let metadata_len = envelope_metadata_escaped_len(&metadata);
        let entry_lens: Vec<usize> = results.iter().map(entry_escaped_len).collect();

        let incremental = empty_rendered_daemon_frame_len(served_config_id)
            + envelope_escaped_len(&entry_lens, metadata_len);
        let direct = rendered_response_daemon_frame_len(
            &serialize_response_value(&envelope),
            served_config_id,
        );
        assert_eq!(
            incremental, direct,
            "shape with {entry_count} entries: incremental frame length must match \
                 direct serialization"
        );
    }
}

/// Reproduces a defect found while writing this fixture: the pre-incremental
/// `fit_rendered_batch_envelope` computed `rendered_frame_bytes -
/// compact_frame_bytes` as a plain (eagerly evaluated) argument to
/// `bool::then_some`, so the subtraction ran unconditionally — including
/// when `compact_frame_bytes >= rendered_frame_bytes`, i.e. exactly the
/// case Agent JSON reduction introduces (canonical retains `full_id`/
/// `namespace` and so can be the larger side). That underflowed and
/// panicked in a debug/test build; in a release build (no overflow
/// checks) it wrapped to a huge value that would have sorted the
/// oversized canonical form to the FRONT of the fallback queue instead
/// of excluding it. This test fails loudly (panic) if that pattern comes
/// back, and separately asserts the entry is left untouched rather than
/// silently swapped.
#[test]
#[serial_test::serial(config_ledger)]
fn fit_rendered_batch_envelope_never_falls_back_when_compact_is_larger() {
    let registry = frame_budget_category_test_registry();
    let rendered_kept = Value::String("kept-rendered".to_string());
    let compact_larger =
        Value::String("kept-rendered-with-extra-canonical-metadata-suffix".to_string());
    let oversized = Value::String("Z".repeat(9_000_000));

    let compact_results = vec![
        json!({"ok": true, "tool": "small-a", "result": compact_larger}),
        json!({"ok": true, "tool": "big-b", "result": oversized.clone()}),
    ];
    let out_results = vec![
        json!({"ok": true, "tool": "small-a", "result": rendered_kept.clone()}),
        json!({"ok": true, "tool": "big-b", "result": oversized}),
    ];
    let envelope = parallel_batch_envelope(compact_results.clone());
    let map = envelope.as_object().expect("batch envelope object");

    let fitted = fit_rendered_batch_envelope(
        map,
        &compact_results,
        out_results,
        "compact-larger-probe",
        &registry,
    );
    let fitted = Value::Object(fitted);

    assert_eq!(
        fitted["results"][0]["result"], rendered_kept,
        "entry whose canonical form is larger than its rendered form must never fall back"
    );
    assert_eq!(fitted["results"][1]["ok"], false);
    assert_eq!(
        fitted["results"][1]["error"]["kind"],
        "response_frame_budget_exceeded"
    );
}

/// Snapshot equivalence: this fixture avoids any canonical-larger-than-
/// rendered entry (see the dedicated test above for that case, which the
/// pre-incremental code could not even run without panicking) so it can
/// be run unmodified against the pre-incremental `fit_rendered_batch_envelope`
/// for a baseline. Captured before the incremental rewrite:
///
/// ```text
/// PROBE index=0 tool=big-a ok=Some(false) result_len=None error_kind=Some("response_frame_budget_exceeded")
/// PROBE index=1 tool=mid-b ok=Some(true) result_len=Some(2900000) error_kind=None
/// PROBE index=2 tool=small-c ok=Some(true) result_len=Some(5) error_kind=None
/// PROBE summary={"aborted":0,"failed":1,"succeeded":2,"total":3}
/// PROBE status="partial"
/// PROBE total_serialized_len=2900530
/// ```
///
/// i.e. the modest compact-fallback saving on `big-a` (150,000 bytes)
/// isn't enough alone, `mid-b`'s fallback is applied, and `big-a` is the
/// one omitted afterward (largest current entry) — never `mid-b` or
/// `small-c`.
#[test]
#[serial_test::serial(config_ledger)]
fn fit_rendered_batch_envelope_matches_pre_incremental_behavior_for_fixed_fixture() {
    let registry = frame_budget_category_test_registry();
    let rendered0 = Value::String("A".repeat(6_000_000));
    let compact0 = Value::String("A".repeat(5_900_000));
    let rendered1 = Value::String("B".repeat(3_000_000));
    let compact1 = Value::String("B".repeat(2_900_000));
    let small = Value::String("small".to_string());

    let compact_results = vec![
        json!({"ok": true, "tool": "big-a", "result": compact0}),
        json!({"ok": true, "tool": "mid-b", "result": compact1}),
        json!({"ok": true, "tool": "small-c", "result": small.clone()}),
    ];
    let out_results = vec![
        json!({"ok": true, "tool": "big-a", "result": rendered0}),
        json!({"ok": true, "tool": "mid-b", "result": rendered1}),
        json!({"ok": true, "tool": "small-c", "result": small}),
    ];
    let envelope = parallel_batch_envelope(compact_results.clone());
    let map = envelope.as_object().expect("batch envelope object");

    let fitted = fit_rendered_batch_envelope(
        map,
        &compact_results,
        out_results,
        "probe-config",
        &registry,
    );
    let fitted = Value::Object(fitted);

    assert_eq!(fitted["results"][0]["ok"], false);
    assert_eq!(
        fitted["results"][0]["error"]["kind"],
        "response_frame_budget_exceeded"
    );
    assert_eq!(fitted["results"][1]["ok"], true);
    assert_eq!(
        fitted["results"][1]["result"].as_str().map(str::len),
        Some(2_900_000)
    );
    assert_eq!(fitted["results"][2]["result"], json!("small"));
    assert_eq!(
        fitted["summary"],
        json!({"total": 3, "succeeded": 2, "failed": 1, "aborted": 0})
    );
    assert_eq!(fitted["status"], "partial");
    assert_eq!(
        fitted["results"][0]["error"]["domain_disposition"],
        "committed"
    );
    assert_eq!(fitted["results"][0]["domain_disposition"], "committed");
    // A3 and #2951 add matching nested and entry-level fields to the
    // historic byte snapshot; omission
    // selection, remaining payloads, and aggregate counts stay identical.
    assert_eq!(
        serialized_response_len(&fitted),
        2_900_530 + 2 * r#","domain_disposition":"committed""#.len()
    );
}

#[test]
#[serial_test::serial(config_ledger)]
fn auto_rendered_batch_stays_within_daemon_frame_cap() {
    // Auto renders a single record as compact JSON, so a lone object can
    // no longer balloon past its compact form (the kv-block renderer is
    // gone). The remaining inflation mode is a sparse record array: the
    // table materializes the full column set for every row, so records
    // with disjoint key sets inflate quadratically. The fixture sits
    // where the compact envelope fits the budget but the rendered table
    // exceeds the daemon frame, which is exactly the fallback under test.
    // Sized to the smallest sparse array whose render exceeds the frame
    // with margin: 400 rows × 8000 columns ≈ 3.2M cells at ~3 bytes of
    // separator each ≈ 9.6MB rendered vs the 8MB frame. Larger fixtures
    // (600×20 keys/row was 7.2M cells) only slow the suite.
    let records: Vec<Value> = (0..400)
        .map(|record_index| {
            let mut record = serde_json::Map::new();
            for key_index in 0..20 {
                record.insert(format!("r{record_index}k{key_index}"), json!(1));
            }
            Value::Object(record)
        })
        .collect();
    let result = json!({ "items": records });
    let envelope = parallel_batch_envelope(vec![json!({
        "ok": true,
        "tool": "probe",
        "result": result.clone(),
    })]);
    let compact_bytes = serde_json::to_vec(&envelope)
        .expect("compact envelope")
        .len();
    assert!(compact_bytes < BATCH_RESPONSE_BUDGET_BYTES);
    // Precondition: the rendered form alone must exceed the daemon frame,
    // otherwise this fixture no longer exercises the fallback.
    let rendered_probe_bytes =
        render_format(result, OutputFormat::Auto, PresentationMode::Agent).len();
    assert!(
        rendered_probe_bytes > khive_runtime::daemon::MAX_FRAME_BYTES,
        "fixture drifted: rendered entry ({rendered_probe_bytes} bytes) \
             no longer exceeds the daemon frame"
    );

    let rendered = render_result(
        envelope,
        OutputFormat::Auto,
        &None,
        PresentationMode::Agent,
        &None,
        &RenderContext {
            registry: &large_result_test_server().registry,
            content_scopes: &[],
        },
        Some("test"),
    );
    let rendered_value: Value = serde_json::from_str(&rendered).expect("response envelope");
    assert_eq!(rendered_value["status"], "success");
    assert_eq!(rendered_value["results"][0]["ok"], true);
    assert!(
        rendered_value["results"][0]["result"].is_object(),
        "oversized auto output must fall back to the truthful compact result"
    );
    let frame = khive_runtime::DaemonResponseFrame {
        ok: true,
        result: Some(rendered),
        error: None,
        error_detail: None,
        namespace_mismatch: false,
        config_mismatch: false,
        served_config_id: Some("test".to_string()),
        version_mismatch: false,
        daemon_protocol_version: khive_runtime::PROTOCOL_VERSION,
        metrics: None,
        request_id: None,
    };
    let frame_bytes = serde_json::to_vec(&frame).expect("daemon frame").len();
    assert!(
        frame_bytes <= khive_runtime::daemon::MAX_FRAME_BYTES,
        "rendered daemon frame was {frame_bytes} bytes"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn bounded_batch_op_error_does_not_abort_siblings() {
    let count = 5;
    let in_flight = Arc::new(AtomicUsize::new(0));
    let max_in_flight = Arc::new(AtomicUsize::new(0));
    let futures = (0..count).map(|index| {
        let entry = if index == 2 {
            json!({"ok": false, "tool": "probe", "error": "expected failure"})
        } else {
            json!({"ok": true, "tool": "probe", "result": {"index": index}})
        };
        batch_task(
            index,
            observed_batch_entry(
                index,
                (count - index) as u64,
                entry,
                in_flight.clone(),
                max_in_flight.clone(),
            ),
        )
    });

    let response = parallel_batch_envelope(
        execute_bounded_batch(futures, usize::MAX, MAX_BATCH_CONCURRENCY).await,
    );

    assert_eq!(
        response["summary"],
        json!({"total": 5, "succeeded": 4, "failed": 1, "aborted": 0})
    );
    assert_eq!(response["results"][2]["error"], "expected failure");
    assert!(response["results"][3]["ok"].as_bool().unwrap_or(false));
    assert!(response["results"][4]["ok"].as_bool().unwrap_or(false));
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn bounded_batch_never_exceeds_concurrency_limit() {
    let count = MAX_BATCH_CONCURRENCY * 3;
    let in_flight = Arc::new(AtomicUsize::new(0));
    let max_in_flight = Arc::new(AtomicUsize::new(0));
    let futures = (0..count).map(|index| {
        batch_task(
            index,
            observed_batch_entry(
                index,
                10,
                json!({"ok": true, "tool": "probe", "result": index}),
                in_flight.clone(),
                max_in_flight.clone(),
            ),
        )
    });

    let results = execute_bounded_batch(futures, usize::MAX, MAX_BATCH_CONCURRENCY).await;

    assert_eq!(results.len(), count);
    assert_eq!(max_in_flight.load(Ordering::SeqCst), MAX_BATCH_CONCURRENCY);
    assert_eq!(in_flight.load(Ordering::SeqCst), 0);
}

fn t(pack: &str, verb: &str, desc: &str) -> (String, String, String) {
    (pack.to_owned(), verb.to_owned(), desc.to_owned())
}

// ── serve_stdio handshake-mode decision (#714) ────────────────────────────

#[cfg(unix)]
#[test]
fn stdio_serve_mode_cold_start_uses_handshake() {
    assert_eq!(stdio_serve_mode_for(None), StdioServeMode::Handshake);
}

#[cfg(unix)]
#[test]
fn stdio_serve_mode_resumed_generation_skips_handshake() {
    assert_eq!(stdio_serve_mode_for(Some(1)), StdioServeMode::Resumed);
}

#[test]
#[serial_test::serial(config_ledger)]
fn single_pack_verbs_unchanged() {
    let catalog = build_verb_catalog([
        t("kg", "create", "Create an entity or note."),
        t("kg", "list", "List entities."),
    ]);
    assert_eq!(
        catalog,
        "  create — Create an entity or note.\n  list — List entities.\n"
    );
}

#[test]
#[serial_test::serial(config_ledger)]
fn duplicate_verb_concatenates_descriptions_with_pack_attribution() {
    let catalog = build_verb_catalog([
        t("kg", "create", "Create an entity or note."),
        t("gtd", "create", "Create a task."),
    ]);
    // Both pack descriptions must appear with attribution.
    assert!(catalog.contains("[kg] Create an entity or note."));
    assert!(catalog.contains("[gtd] Create a task."));
    // The verb name must appear exactly once in the catalog header.
    assert_eq!(catalog.matches("  create — ").count(), 1);
}

#[test]
fn instructions_carry_docs_address_and_guidance_pointers() {
    let instructions =
        build_instructions("  create — Create an entity or note.\n", "kg, gtd", "web");
    assert!(instructions.contains("https://ohdearquant.github.io/khive/"));
    assert!(instructions.contains("docs/configuration.md"));
    assert!(instructions.contains("docs/guide/tips-and-tricks.md"));
    // help=true / live-catalog-over-training-knowledge guidance present.
    assert!(instructions.contains("help=true"));
}

#[test]
fn the_packs_loaded_and_the_packs_merely_linked_are_named_separately() {
    let instructions = build_instructions(
        "  create — Create an entity.\n",
        "kg, gtd",
        "web, telemetry",
    );

    let loaded = instructions
        .split_once("Loaded on this server: ")
        .expect("the loaded clause is always present")
        .1
        .split_once('.')
        .expect("the loaded clause ends in a period")
        .0;
    assert_eq!(loaded, "kg, gtd");

    let selectable = instructions
        .split_once("until selected: ")
        .expect("a non-empty unloaded set produces the selectable clause")
        .1
        .split_once('.')
        .expect("the selectable clause ends in a period")
        .0;
    assert_eq!(selectable, "web, telemetry");

    // The defect: one list labelled "built-ins" carried both sets, so a
    // caller read every linked pack as callable. Spelling it out here
    // because a future rewording that re-merges them would otherwise
    // satisfy both assertions above.
    assert!(!instructions.contains("(built-ins:"));
}

#[test]
fn a_fully_loaded_binary_gets_no_selectable_clause() {
    let instructions = build_instructions("  create — Create an entity.\n", "kg, gtd", "");

    assert!(instructions.contains("Loaded on this server: kg, gtd."));
    assert!(!instructions.contains("Also linked into this binary"));
    assert!(!instructions.contains("until selected"));
    // The configure affordance survives an empty unloaded set: it is the
    // reason the linked list was in this string to begin with.
    assert!(instructions.contains("Configure packs via KHIVE_PACKS or --pack."));
}

#[tokio::test]
#[serial(config_ledger)]
async fn the_instructions_name_as_loaded_only_what_this_server_loaded() {
    let mut config = RuntimeConfig::no_embeddings();
    config.db_path = None;
    config.packs = vec!["kg".into()];
    let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
    let server = KhiveMcpServer::new(runtime).expect("the kg factory is linked");

    let info = rmcp::ServerHandler::get_info(&server);
    let instructions = info
        .instructions
        .expect("get_info always sets instructions");

    let loaded: Vec<&str> = instructions
        .split_once("Loaded on this server: ")
        .expect("the loaded clause is always present")
        .1
        .split_once('.')
        .expect("the loaded clause ends in a period")
        .0
        .split(", ")
        .collect();
    assert_eq!(loaded, vec!["kg"]);

    let selectable: Vec<&str> = instructions
        .split_once("until selected: ")
        .expect("selecting one pack leaves the rest of the binary unloaded")
        .1
        .split_once('.')
        .expect("the selectable clause ends in a period")
        .0
        .split(", ")
        .collect();

    // `web` is linked only with `pack-web`; naming it as loaded is the whole defect.
    assert_eq!(selectable.contains(&"web"), cfg!(feature = "pack-web"));
    assert!(!loaded.contains(&"web"));
    // The two sets partition the linked inventory: nothing is in both.
    for pack in &loaded {
        assert!(!selectable.contains(pack), "{pack} appears in both clauses");
    }
}

#[test]
#[serial_test::serial(config_ledger)]
fn catalog_is_sorted_alphabetically() {
    let catalog = build_verb_catalog([
        t("kg", "search", "Search."),
        t("kg", "assign", "Assign."),
        t("kg", "list", "List."),
    ]);
    let names: Vec<&str> = catalog
        .lines()
        .filter(|l| l.starts_with("  "))
        .map(|l| l.trim_start().split(' ').next().unwrap())
        .collect();
    assert_eq!(names, vec!["assign", "list", "search"]);
}

#[cfg(feature = "pack-telemetry")]
#[tokio::test]
#[serial(config_ledger)]
async fn telemetry_inventory_dispatch_preserves_configured_carriers() {
    use khive_runtime::{TelemetryCarrier, TelemetryChannelConfig, TelemetryFailurePosture};

    let mut config = RuntimeConfig::no_embeddings();
    config.db_path = None;
    config.packs = vec!["kg".into(), "telemetry".into()];
    config.telemetry.stream = "configured-events".into();
    config.telemetry.default_carrier = Some(TelemetryCarrier::Ephemeral);
    config.telemetry.channels.push(TelemetryChannelConfig {
        kinds: vec!["run.completed".into()],
        carrier: TelemetryCarrier::Durable,
        failure_posture: TelemetryFailurePosture::Stop,
    });
    let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
    let server = KhiveMcpServer::new(runtime).expect("linked telemetry factory");
    let durable = server
        .registry
        .dispatch(
            "telemetry.emit",
            json!({"kind":"run.completed","payload":{"count":1}}),
        )
        .await
        .expect("configured durable emit");
    assert_eq!(durable["carrier"], "durable");
    let ephemeral = server
        .registry
        .dispatch("telemetry.emit", json!({"kind":"new.event","payload":null}))
        .await
        .expect("default ephemeral emit");
    assert_eq!(ephemeral["carrier"], "ephemeral");
    assert_eq!(ephemeral["outcome"], "dropped");
    assert_eq!(ephemeral["classified"], false);
    assert!(ephemeral["receipt_id"].is_null());
    let page = server
        .registry
        .dispatch("stream.read", json!({"stream":"configured-events"}))
        .await
        .expect("existing stream read");
    assert_eq!(page["entries"].as_array().unwrap().len(), 1);
    assert_eq!(page["entries"][0]["record"]["kind"], "run.completed");
}

// ── #658 regression: brain dispatch hook wired into production builder ──

/// The hook (registered via `PackInstall::dispatch_hook`) and the pack
/// runtime the registry dispatches `brain.*` verbs to must be the same
/// `BrainPack` instance — otherwise the hook's posterior updates would be
/// invisible to `brain.state` reads. `brain.state` loads the default
/// namespace into the shared active slot as a side effect, so a
/// subsequent non-brain dispatch in the same namespace lands on
/// `ApplyTarget::ActiveSlot` and is immediately observable.
///
/// Uses the `local` namespace (rather than an arbitrary one) because
/// ADR-007 Rule 3b always pins the implicit write token to `local`
/// regardless of the registry's configured default namespace; using
/// `local` for both keeps the dispatched event's namespace and the
/// token's namespace identical, so the signal lands on the active slot
/// instead of the cold-namespace queue.
#[tokio::test]
#[serial(config_ledger)]
async fn brain_dispatch_hook_updates_state_visible_through_same_instance() {
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string(), "brain".to_string()],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
    let server = KhiveMcpServer::with_packs(runtime, &["kg".to_string(), "brain".to_string()])
        .expect("server builds with kg + brain");

    server
        .registry
        .dispatch("brain.state", serde_json::Value::Null)
        .await
        .expect("brain.state loads the default namespace into the active slot");

    server
        .registry
        .dispatch("stats", serde_json::json!({}))
        .await
        .expect("kg.stats dispatch succeeds");
    let after_irrelevant = server
        .registry
        .dispatch("brain.state", serde_json::Value::Null)
        .await
        .expect("brain.state dispatch after irrelevant stats");
    assert_eq!(after_irrelevant["balanced_recall"]["total_events"], 0);

    // Search is a relevant BrainSignal even when the corpus is empty.
    server
        .registry
        .dispatch(
            "search",
            serde_json::json!({"kind": "entity", "query": "hook-wiring-regression"}),
        )
        .await
        .expect("kg.search dispatch succeeds");

    let state = server
        .registry
        .dispatch("brain.state", serde_json::Value::Null)
        .await
        .expect("brain.state dispatch");
    let total_events = state["balanced_recall"]["total_events"]
        .as_u64()
        .unwrap_or(0);
    assert_eq!(
        total_events, 1,
        "dispatch hook must update the same BrainPack instance the registry \
             dispatches brain.* verbs to; got snapshot {state:?}"
    );
}

/// ADR-124 boot-occupancy regression: `has_note_write_validator` exists
/// specifically so a transport's own tests can assert, per boot path,
/// that the documented startup sequence actually filled the slot — but
/// nothing called it. Every prior ADR-124 test built its registry by
/// hand (`registry.call_register_note_write_validators(&rt)` in
/// `khive-pack-comm`'s integration tests), which proves the validator
/// works but proves nothing about whether `with_packs` — the single-
/// backend production boot path — installs it. Asserts occupancy
/// directly through the real builder, then proves the slot is not just
/// occupied but functioning: a generic `create` naming a forged
/// `from_actor` on a `message` note must come back derived to the
/// dispatching token's actor. Sensitivity verified by temporarily
/// commenting out the `registry.call_register_note_write_validators(&runtime);`
/// line in `KhiveMcpServer::with_packs` and re-running: both assertions
/// fail without it (occupancy false; forged value survives) and pass
/// with it restored.
#[tokio::test]
#[serial(config_ledger)]
async fn single_runtime_boot_installs_note_write_validator() {
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string(), "comm".to_string()],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
    let runtime_probe = runtime.clone();
    let server = KhiveMcpServer::with_packs(runtime, &["kg".to_string(), "comm".to_string()])
        .expect("server builds with kg + comm");

    assert!(
        runtime_probe.has_note_write_validator(),
        "with_packs (single-backend boot) must install the note-write \
             validator on the runtime it serves writes through"
    );

    let identity = khive_runtime::RequestIdentity {
        namespace: "local".to_string(),
        actor_id: Some("lambda:probe".to_string()),
        ..Default::default()
    };
    let created = server
        .registry
        .dispatch_with_identity(
            "create",
            serde_json::json!({
                "kind": "message",
                "content": "single-runtime boot occupancy probe",
                "properties": {"from_actor": "forged-actor"},
            }),
            Some(identity),
        )
        .await
        .expect("create must succeed");
    assert_eq!(
        created["properties"]["from_actor"], "lambda:probe",
        "a forged from_actor on a generic create must come back derived to \
             the dispatching token's actor, proving the installed validator is \
             not just present but wired into the write path; got {created:?}"
    );
}

// ── relative backend paths must not collide across projects ────────────

/// RAII guard: temporarily chdirs into `dir`, restoring the original cwd
/// on drop (even on panic/unwind). Process cwd is global state, so every
/// test using this guard is `#[serial]`.
struct CwdGuard {
    original: std::path::PathBuf,
}

impl CwdGuard {
    fn enter(dir: &std::path::Path) -> Self {
        let original = std::env::current_dir().expect("read cwd");
        std::env::set_current_dir(dir).expect("chdir into test project root");
        Self { original }
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.original);
    }
}

/// The security finding this guards: `compute_config_id`'s backend
/// topology fold used to embed the RAW relative path string declared in
/// `khive.toml`. Two different projects that happen to declare the same
/// relative string (e.g. `./data/main.db`) but resolve it against two
/// different working directories produced the SAME `config_id` despite
/// opening two different physical databases — a warm daemon started for
/// one project could then accept forwarded requests meant for the other,
/// serving or writing the wrong project's data.
#[test]
#[serial]
#[serial_test::serial(config_ledger)]
fn config_id_does_not_collide_across_projects_with_same_relative_backend_path() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    use khive_runtime::{BackendId, BackendKind, KhiveConfig, Namespace};

    let project_a = tempfile::tempdir().expect("project a tempdir");
    let project_b = tempfile::tempdir().expect("project b tempdir");

    let base_rt = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("local").unwrap(),
        embedding_model: None,
        packs: vec!["kg".to_string()],
        backend_id: BackendId::main(),
        ..RuntimeConfig::default()
    };

    let relative_backend_cfg = || KhiveConfig {
        backends: vec![khive_runtime::BackendConfig {
            name: "main".to_string(),
            kind: BackendKind::Sqlite,
            path: Some(std::path::PathBuf::from("./data/main.db")),
            cache_mb: None,
            journal_mode: None,
            wal_ceiling_bytes: None,
            disk_reserve_bytes: None,
            disk_guard_deadline_ms: None,
            served_kinds: None,
            read_only: false,
        }],
        ..KhiveConfig::default()
    };

    let id_a = {
        let _cwd = CwdGuard::enter(project_a.path());
        compute_config_id(&base_rt, Some(&relative_backend_cfg()))
    };
    let id_b = {
        let _cwd = CwdGuard::enter(project_b.path());
        compute_config_id(&base_rt, Some(&relative_backend_cfg()))
    };

    assert_ne!(
        id_a, id_b,
        "two projects declaring the same relative backend path string from \
             different working directories must not share a config_id; both \
             produced: {id_a}"
    );
}

include!("server/config_id_read_only_tests.rs");

/// The collision fix is deliberately conditional: ordinary topology
/// components that contain no reserved syntax keep the legacy spelling
/// of the path and mode fields.
#[test]
#[serial_test::serial(config_ledger)]
fn config_id_preserves_legacy_topology_spelling_when_delimiter_free() {
    use khive_runtime::{BackendConfig, BackendId, BackendKind, KhiveConfig, PackConfig};

    let dir = tempfile::tempdir().expect("legacy topology tempdir");
    let main_path = dir.path().join("main.db");
    let runtime = RuntimeConfig {
        db_path: Some(main_path.clone()),
        packs: vec!["kg".to_string()],
        backend_id: BackendId::main(),
        ..RuntimeConfig::no_embeddings()
    };
    let topology = KhiveConfig {
        backends: vec![BackendConfig {
            name: "main".to_string(),
            kind: BackendKind::Sqlite,
            path: Some(main_path.clone()),
            cache_mb: None,
            journal_mode: None,
            wal_ceiling_bytes: None,
            disk_reserve_bytes: None,
            disk_guard_deadline_ms: None,
            served_kinds: None,
            read_only: false,
        }],
        packs: std::collections::HashMap::from([(
            "kg".to_string(),
            PackConfig {
                backend: "main".to_string(),
                no_embed: false,
            },
        )]),
        ..KhiveConfig::default()
    };

    let expected_suffix = format!(
        ";backends=[main:Sqlite:{}:wal_ceiling_bytes=0];pack_backends=[kg=main]",
        canonical_fingerprint_path(&main_path)
    );
    let config_id = compute_config_id(&runtime, Some(&topology));
    // The disk-guard segment follows the topology segments for a writable SQLite backend.
    assert!(
        config_id.contains(&format!("{expected_suffix};sqlite_disk_guard=")),
        "delimiter-free topologies must retain the legacy field encoding; got {config_id}"
    );
}

#[test]
#[serial_test::serial(config_ledger)]
fn config_id_folds_effective_wal_ceiling_bytes() {
    use khive_runtime::{BackendConfig, BackendKind, KhiveConfig, WalCeilingSource};

    let dir = tempfile::tempdir().expect("WAL ceiling fingerprint tempdir");
    let main_path = dir.path().join("main.db");
    let archive_path = dir.path().join("archive.db");
    let default_runtime = RuntimeConfig {
        db_path: Some(main_path.clone()),
        packs: vec!["kg".to_string()],
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_env_raw: None,
        wal_ceiling_source: WalCeilingSource::Default,
        ..RuntimeConfig::no_embeddings()
    };
    let env_runtime = RuntimeConfig {
        wal_ceiling_bytes: 8192,
        wal_ceiling_configured_bytes: 8192,
        wal_ceiling_env_raw: Some("8192".to_string()),
        wal_ceiling_source: WalCeilingSource::Environment,
        ..default_runtime.clone()
    };
    let explicit_zero_runtime = RuntimeConfig {
        wal_ceiling_env_raw: Some("0".to_string()),
        wal_ceiling_source: WalCeilingSource::Environment,
        ..default_runtime.clone()
    };

    let implicit_zero = compute_config_id(&default_runtime, None);
    let implicit_nonzero = compute_config_id(&env_runtime, None);
    assert_eq!(
        implicit_zero,
        compute_config_id(&explicit_zero_runtime, None)
    );
    assert_ne!(implicit_zero, implicit_nonzero);
    assert!(implicit_zero.contains(&format!(
        "backend={:?}:wal_ceiling_bytes=0;outbound=",
        default_runtime.backend_id
    )));
    assert!(implicit_nonzero.contains(&format!(
        "backend={:?}:wal_ceiling_bytes=8192",
        default_runtime.backend_id
    )));
    assert!(!khive_runtime::daemon::config_ids_compatible(
        &implicit_zero,
        &implicit_nonzero
    ));

    let backend = |name: &str, path: std::path::PathBuf, wal_ceiling_bytes| BackendConfig {
        name: name.to_string(),
        kind: BackendKind::Sqlite,
        path: Some(path),
        cache_mb: None,
        journal_mode: None,
        wal_ceiling_bytes,
        disk_reserve_bytes: None,
        disk_guard_deadline_ms: None,
        served_kinds: None,
        read_only: false,
    };
    let env_topology = KhiveConfig {
        backends: vec![
            backend("archive", archive_path.clone(), None),
            backend("main", main_path.clone(), None),
        ],
        ..KhiveConfig::default()
    };
    let mut field_topology = env_topology.clone();
    for backend in &mut field_topology.backends {
        backend.wal_ceiling_bytes = Some(8192);
    }
    let from_env = compute_config_id(&env_runtime, Some(&env_topology));
    let from_fields = compute_config_id(&default_runtime, Some(&field_topology));
    assert_eq!(
        from_env, from_fields,
        "only effective values define identity"
    );
    assert!(from_fields.contains(&format!(
        "backend={:?}:wal_ceiling_bytes=8192",
        default_runtime.backend_id
    )));
    assert!(from_fields.contains("archive:Sqlite:"));
    assert!(from_fields.contains(":wal_ceiling_bytes=8192"));

    let mut reversed = field_topology.clone();
    reversed.backends.reverse();
    assert_eq!(
        from_fields,
        compute_config_id(&default_runtime, Some(&reversed))
    );

    let mut changed = field_topology.clone();
    changed.backends[0].wal_ceiling_bytes = Some(8193);
    let changed_id = compute_config_id(&default_runtime, Some(&changed));
    assert_ne!(from_fields, changed_id);
    assert!(!khive_runtime::daemon::config_ids_compatible(
        &from_fields,
        &changed_id
    ));

    let mut explicit_zero_topology = env_topology.clone();
    for backend in &mut explicit_zero_topology.backends {
        backend.wal_ceiling_bytes = Some(0);
    }
    assert_eq!(
        compute_config_id(&default_runtime, Some(&env_topology)),
        compute_config_id(&default_runtime, Some(&explicit_zero_topology))
    );

    let mut read_only = explicit_zero_topology.clone();
    read_only.backends[1].read_only = true;
    let read_only_zero = compute_config_id(&default_runtime, Some(&read_only));
    read_only.backends[1].wal_ceiling_bytes = Some(8192);
    let read_only_configured = compute_config_id(&default_runtime, Some(&read_only));
    assert_eq!(read_only_zero, read_only_configured);
    assert!(read_only_configured.contains(&format!(
        "backend={:?}:read_only:wal_ceiling_bytes=0;outbound=",
        default_runtime.backend_id
    )));
    assert!(read_only_configured.contains(&format!(
        "main:Sqlite:{}:read_only:wal_ceiling_bytes=0",
        canonical_fingerprint_path(&main_path)
    )));
    assert!(!read_only_configured.contains("wal_ceiling_bytes=8192"));
}

/// A disabled ceiling is an explicit policy, so its zero value is part of
/// daemon identity for the implicit main backend and for every named
/// backend, in both topology spellings.
#[test]
#[serial_test::serial(config_ledger)]
fn config_id_encodes_disabled_wal_ceiling_for_every_backend() {
    use khive_runtime::{BackendConfig, BackendKind, KhiveConfig, PackConfig};

    let dir = tempfile::tempdir().expect("WAL ceiling fingerprint tempdir");
    let main_path = dir.path().join("main.db");
    let archive_path = dir.path().join("archive.db");
    let runtime = RuntimeConfig {
        db_path: Some(main_path.clone()),
        packs: vec!["kg".to_string()],
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        ..RuntimeConfig::no_embeddings()
    };
    let main_backend = format!(
        "backend={:?}:wal_ceiling_bytes=0;outbound=",
        runtime.backend_id
    );

    let implicit = compute_config_id(&runtime, None);
    assert!(
        implicit.contains(&main_backend),
        "the implicit main backend must encode a disabled ceiling; got {implicit}"
    );
    assert_eq!(
        implicit.matches("wal_ceiling_bytes=").count(),
        1,
        "an implicit topology has exactly one ceiling component; got {implicit}"
    );

    let backend = |name: &str, path: Option<std::path::PathBuf>, kind| BackendConfig {
        name: name.to_string(),
        kind,
        path,
        cache_mb: None,
        journal_mode: None,
        wal_ceiling_bytes: None,
        disk_reserve_bytes: None,
        disk_guard_deadline_ms: None,
        served_kinds: None,
        read_only: false,
    };
    let packs = std::collections::HashMap::from([(
        "kg".to_string(),
        PackConfig {
            backend: "main".to_string(),
            no_embed: false,
        },
    )]);
    let legacy = KhiveConfig {
        backends: vec![
            backend("main", Some(main_path.clone()), BackendKind::Sqlite),
            backend("archive", Some(archive_path.clone()), BackendKind::Sqlite),
        ],
        packs: packs.clone(),
        ..KhiveConfig::default()
    };
    let legacy_id = compute_config_id(&runtime, Some(&legacy));
    assert!(legacy_id.contains(&main_backend), "{legacy_id}");
    assert!(
        legacy_id.contains(&format!(
            ";backends=[archive:Sqlite:{}:wal_ceiling_bytes=0,\
                 main:Sqlite:{}:wal_ceiling_bytes=0];pack_backends=[kg=main];sqlite_disk_guard=",
            canonical_fingerprint_path(&archive_path),
            canonical_fingerprint_path(&main_path),
        )),
        "every named backend must encode a disabled ceiling in topology order; got {legacy_id}"
    );

    // A name carrying reserved syntax takes the escaped encoding.
    let escaped = KhiveConfig {
        backends: vec![
            backend("main", Some(main_path.clone()), BackendKind::Sqlite),
            backend("ma:in", Some(archive_path.clone()), BackendKind::Sqlite),
        ],
        packs: std::collections::HashMap::new(),
        ..KhiveConfig::default()
    };
    let escaped_id = compute_config_id(&runtime, Some(&escaped));
    assert!(escaped_id.contains(&main_backend), "{escaped_id}");
    assert!(
        escaped_id.contains(";backends=[v2|ma%3ain:Sqlite:"),
        "a reserved-syntax name must take the escaped topology; got {escaped_id}"
    );
    assert_eq!(
        escaped_id.matches(":w:wal_ceiling_bytes=0").count(),
        2,
        "each escaped backend row must encode a disabled ceiling; got {escaped_id}"
    );

    // A backend that enforces no ceiling (memory, read-only) is zero, and
    // encodes zero rather than being omitted, whatever value was written.
    let mut memory = backend("main", None, BackendKind::Memory);
    memory.wal_ceiling_bytes = Some(8192);
    let mut read_only = backend("archive", Some(archive_path), BackendKind::Sqlite);
    read_only.read_only = true;
    read_only.wal_ceiling_bytes = Some(8192);
    let unenforced = KhiveConfig {
        backends: vec![memory, read_only],
        ..KhiveConfig::default()
    };
    let unenforced_id = compute_config_id(&runtime, Some(&unenforced));
    assert!(unenforced_id.contains(&main_backend), "{unenforced_id}");
    assert!(
        !unenforced_id.contains("wal_ceiling_bytes=8192"),
        "a ceiling no backend enforces must not appear; got {unenforced_id}"
    );
    assert_eq!(
        unenforced_id.matches("wal_ceiling_bytes=0").count(),
        3,
        "the implicit main and both named rows must encode zero; got {unenforced_id}"
    );
}

/// A disabled ceiling fingerprints identically whichever way it was
/// configured, and differs from any enabled value.
#[test]
#[serial_test::serial(config_ledger)]
fn config_id_disabled_wal_ceiling_is_independent_of_its_source() {
    use khive_runtime::{BackendConfig, BackendKind, KhiveConfig, WalCeilingSource};

    let dir = tempfile::tempdir().expect("WAL ceiling fingerprint tempdir");
    let main_path = dir.path().join("main.db");
    let default_runtime = RuntimeConfig {
        db_path: Some(main_path.clone()),
        packs: vec!["kg".to_string()],
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_env_raw: None,
        wal_ceiling_source: WalCeilingSource::Default,
        ..RuntimeConfig::no_embeddings()
    };
    let env_zero_runtime = RuntimeConfig {
        wal_ceiling_env_raw: Some("0".to_string()),
        wal_ceiling_source: WalCeilingSource::Environment,
        ..default_runtime.clone()
    };
    let env_enabled_runtime = RuntimeConfig {
        wal_ceiling_bytes: 8192,
        wal_ceiling_configured_bytes: 8192,
        wal_ceiling_env_raw: Some("8192".to_string()),
        wal_ceiling_source: WalCeilingSource::Environment,
        ..default_runtime.clone()
    };
    let topology = |wal_ceiling_bytes| KhiveConfig {
        backends: vec![BackendConfig {
            name: "main".to_string(),
            kind: BackendKind::Sqlite,
            path: Some(main_path.clone()),
            cache_mb: None,
            journal_mode: None,
            wal_ceiling_bytes,
            disk_reserve_bytes: None,
            disk_guard_deadline_ms: None,
            served_kinds: None,
            read_only: false,
        }],
        ..KhiveConfig::default()
    };

    // Implicit main backend: default and environment zero agree.
    let implicit_default = compute_config_id(&default_runtime, None);
    assert_eq!(implicit_default, compute_config_id(&env_zero_runtime, None));
    assert_ne!(
        implicit_default,
        compute_config_id(&env_enabled_runtime, None)
    );

    // Named backend: default, environment zero and a backend-field zero
    // that overrides an enabled environment value all agree.
    let named_default = compute_config_id(&default_runtime, Some(&topology(None)));
    assert_eq!(
        named_default,
        compute_config_id(&env_zero_runtime, Some(&topology(None)))
    );
    assert_eq!(
        named_default,
        compute_config_id(&default_runtime, Some(&topology(Some(0))))
    );
    assert_eq!(
        named_default,
        compute_config_id(&env_enabled_runtime, Some(&topology(Some(0))))
    );
    assert_ne!(
        named_default,
        compute_config_id(&env_enabled_runtime, Some(&topology(None)))
    );
    assert_ne!(
        named_default,
        compute_config_id(&default_runtime, Some(&topology(Some(8192))))
    );
}

#[test]
fn config_id_differs_when_backend_served_kinds_differ() {
    use khive_runtime::{BackendConfig, BackendId, BackendKind, KhiveConfig};
    use khive_types::SubstrateKind;

    let runtime = RuntimeConfig {
        db_path: None,
        packs: vec!["kg".to_string()],
        backend_id: BackendId::main(),
        ..RuntimeConfig::no_embeddings()
    };
    let topology_for = |served_kinds| KhiveConfig {
        backends: vec![BackendConfig {
            name: "main".to_string(),
            kind: BackendKind::Memory,
            path: None,
            cache_mb: None,
            journal_mode: None,
            wal_ceiling_bytes: None,
            disk_reserve_bytes: None,
            disk_guard_deadline_ms: None,
            served_kinds,
            read_only: false,
        }],
        ..KhiveConfig::default()
    };

    let legacy = topology_for(None);
    let entity_only = topology_for(Some(std::collections::BTreeSet::from([
        SubstrateKind::Entity,
    ])));
    assert_ne!(
        compute_config_id(&runtime, Some(&legacy)),
        compute_config_id(&runtime, Some(&entity_only)),
        "dispatch-shaping served-kind metadata must move daemon identity"
    );
}

/// The legacy and escaped topology encodings in `encode_backend_topology`
/// each formatted their own `:serves=` suffix independently. Drive the
/// same served-kinds set through both branches (a name containing `:`
/// forces the escaped branch; a plain name keeps the legacy branch) and
/// assert they agree, now that both call the shared formatter.
#[test]
fn served_kinds_suffix_matches_between_legacy_and_escaped_topology_encodings() {
    use khive_runtime::{BackendConfig, BackendKind, KhiveConfig};
    use khive_types::SubstrateKind;

    let served_kinds = Some(std::collections::BTreeSet::from([
        SubstrateKind::Entity,
        SubstrateKind::Note,
    ]));
    let base_backend = BackendConfig {
        name: "main".to_string(),
        kind: BackendKind::Memory,
        path: None,
        cache_mb: None,
        journal_mode: None,
        wal_ceiling_bytes: None,
        disk_reserve_bytes: None,
        disk_guard_deadline_ms: None,
        served_kinds: served_kinds.clone(),
        read_only: false,
    };

    let legacy_topology = KhiveConfig {
        backends: vec![base_backend.clone()],
        ..KhiveConfig::default()
    };
    // A `:` in the name is reserved syntax, forcing the escaped
    // (non-legacy-safe) encoding path instead.
    let escaped_topology = KhiveConfig {
        backends: vec![BackendConfig {
            name: "ma:in".to_string(),
            ..base_backend
        }],
        ..KhiveConfig::default()
    };

    let runtime = RuntimeConfig::no_embeddings();
    let legacy_encoded = encode_backend_topology(&legacy_topology, &runtime);
    let escaped_encoded = encode_backend_topology(&escaped_topology, &runtime);

    // `BTreeSet<SubstrateKind>` iterates in discriminant order (Note=0,
    // Entity=1), so the joined suffix is "note+entity", not input order.
    let expected_suffix = format_served_kinds_suffix(Some("note+entity"));
    assert!(
        legacy_encoded.contains(&expected_suffix),
        "legacy topology encoding must carry the served-kinds suffix: {legacy_encoded}"
    );
    assert!(
        escaped_encoded.contains(&expected_suffix),
        "escaped topology encoding must carry the SAME served-kinds suffix as the \
             legacy encoding, produced by the same formatter: {escaped_encoded}"
    );
}

/// `no_embed` changes runtime behavior (that pack's runtime carries zero
/// embedders), so two configs differing only in it must not share a
/// `config_id` — a shared id would let a daemon serve a client whose
/// embedding policy it does not implement. Absent/false keeps the
/// pre-existing spelling so already-deployed configs keep their id.
#[test]
#[serial_test::serial(config_ledger)]
fn config_id_differs_when_pack_no_embed_differs() {
    use khive_runtime::{BackendConfig, BackendId, BackendKind, KhiveConfig, PackConfig};

    let dir = tempfile::tempdir().expect("no_embed topology tempdir");
    let main_path = dir.path().join("main.db");
    let runtime = RuntimeConfig {
        db_path: Some(main_path.clone()),
        packs: vec!["comm".to_string()],
        backend_id: BackendId::main(),
        ..RuntimeConfig::no_embeddings()
    };
    let topology_for = |no_embed: bool| KhiveConfig {
        backends: vec![BackendConfig {
            name: "main".to_string(),
            kind: BackendKind::Sqlite,
            path: Some(main_path.clone()),
            cache_mb: None,
            journal_mode: None,
            wal_ceiling_bytes: None,
            disk_reserve_bytes: None,
            disk_guard_deadline_ms: None,
            served_kinds: None,
            read_only: false,
        }],
        packs: std::collections::HashMap::from([(
            "comm".to_string(),
            PackConfig {
                backend: "main".to_string(),
                no_embed,
            },
        )]),
        ..KhiveConfig::default()
    };

    let with_flag = compute_config_id(&runtime, Some(&topology_for(true)));
    let without_flag = compute_config_id(&runtime, Some(&topology_for(false)));
    assert_ne!(
        with_flag, without_flag,
        "configs differing only in no_embed must not share a config_id"
    );
    assert!(
        with_flag.contains("comm=main:no_embed"),
        "no_embed must appear in the pack fingerprint; got {with_flag}"
    );
    assert!(
        without_flag.contains("comm=main]"),
        "absent no_embed keeps the legacy pack spelling; got {without_flag}"
    );
}

#[test]
#[serial_test::serial(config_ledger)]
fn config_id_separates_effective_read_only_storage_modes() {
    use khive_runtime::{BackendId, BackendKind, KhiveConfig, Namespace};

    let dir = tempfile::tempdir().expect("config-mode tempdir");
    let runtime = RuntimeConfig {
        db_path: Some(dir.path().join("khive-config-mode.db")),
        default_namespace: Namespace::local(),
        embedding_model: None,
        packs: vec!["kg".to_string()],
        backend_id: BackendId::main(),
        ..RuntimeConfig::default()
    };

    let writable = compute_config_id(&runtime, None);
    assert_eq!(
        writable,
        compute_config_id_with_storage_mode(&runtime, None, false),
        "the writable fingerprint must remain byte-identical"
    );
    let detected_read_only = compute_config_id_with_storage_mode(&runtime, None, true);
    assert_ne!(
        writable, detected_read_only,
        "a chmod-detected snapshot must not reuse a write-capable warm daemon"
    );
    assert!(detected_read_only.contains(&format!("backend={:?}:read_only", runtime.backend_id)));

    let writable_topology = KhiveConfig {
        backends: vec![khive_runtime::BackendConfig {
            name: "main".to_string(),
            kind: BackendKind::Sqlite,
            path: runtime.db_path.clone(),
            cache_mb: None,
            journal_mode: None,
            wal_ceiling_bytes: None,
            disk_reserve_bytes: None,
            disk_guard_deadline_ms: None,
            served_kinds: None,
            read_only: false,
        }],
        ..KhiveConfig::default()
    };
    let mut read_only_topology = writable_topology.clone();
    read_only_topology.backends[0].read_only = true;
    assert_ne!(
        compute_config_id(&runtime, Some(&writable_topology)),
        compute_config_id(&runtime, Some(&read_only_topology)),
        "declared multi-backend read_only mode is part of backend topology"
    );
    assert_eq!(
        compute_config_id(&runtime, Some(&read_only_topology)),
        compute_config_id_with_storage_mode(&runtime, Some(&read_only_topology), true),
        "the pre-open client and opened read-only server must fingerprint identically"
    );
}

#[cfg(unix)]
#[test]
#[serial_test::serial(config_ledger)]
fn config_id_auto_detects_chmod_read_only_single_backend() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("config-mode tempdir");
    let path = dir.path().join("chmod-snapshot.db");
    std::fs::write(&path, b"snapshot identity fixture").expect("create fixture");
    let runtime = RuntimeConfig {
        db_path: Some(path.clone()),
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::default()
    };
    let writable = compute_config_id(&runtime, None);

    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o444);
    std::fs::set_permissions(&path, permissions).unwrap();
    freeze_snapshot_sidecars(&path);

    let detected = compute_config_id(&runtime, None);
    assert_ne!(writable, detected);
    assert!(
        detected.contains(&format!("backend={:?}:read_only", runtime.backend_id)),
        "{detected}"
    );
    assert_eq!(
        detected,
        compute_config_id_with_storage_mode(&runtime, None, true),
        "pre-open forwarding and opened-server identities must converge"
    );
}

#[cfg(unix)]
#[test]
#[serial_test::serial(config_ledger)]
fn runtime_owned_config_id_keeps_captured_writable_mode_after_post_open_chmod() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("runtime-mode tempdir");
    let path = dir.path().join("post-open-chmod.db");
    let config = RuntimeConfig {
        db_path: Some(path.clone()),
        embedding_model: None,
        packs: Vec::new(),
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).expect("open writable runtime");
    assert!(
        !runtime.is_read_only(),
        "runtime must capture writable mode"
    );
    let captured_writable_id = compute_config_id_with_runtime_policies(
        runtime.config(),
        None,
        runtime.ann_fresh_tail_enabled(),
        runtime.is_read_only(),
    );

    let original_mode = std::fs::metadata(&path).unwrap().permissions().mode();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o444);
    std::fs::set_permissions(&path, permissions).unwrap();
    freeze_snapshot_sidecars(&path);

    let pre_open_read_only_id = compute_config_id(runtime.config(), None);
    assert!(
        pre_open_read_only_id.contains(&format!(
            "backend={:?}:read_only",
            runtime.config().backend_id
        )),
        "a new runtime must detect the chmod-read-only snapshot: {pre_open_read_only_id}"
    );

    let server = KhiveMcpServer::new(runtime).expect("build server from opened runtime");
    assert_eq!(
        server.config_id(),
        captured_writable_id,
        "runtime-owned identity must trust the access mode captured when its SQLite pool opened"
    );
    assert_ne!(
        server.config_id(),
        pre_open_read_only_id,
        "an already-open writable engine must not advertise the pre-open read-only identity"
    );

    drop(server);
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(original_mode);
    std::fs::set_permissions(&path, permissions).unwrap();
}

/// The same collision, one layer up the resolution chain: `--db`/`KHIVE_DB`
/// resolves to a raw relative `PathBuf` (`resolve_db_anchor`) that lands in
/// `RuntimeConfig.db_path` unchanged. Before this fix, `compute_config_id`
/// fingerprinted that raw string directly, so two different projects both
/// running `KHIVE_DB=./data/main.db` produced the SAME `config_id` while
/// opening two different SQLite files — the single-backend route
/// (`KhiveMcpServer::with_packs`) would let a warm daemon started for one
/// project serve requests meant for the other's database.
#[test]
#[serial]
#[serial_test::serial(config_ledger)]
fn config_id_does_not_collide_across_projects_with_same_relative_db_override() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    use khive_runtime::Namespace;

    let project_a = tempfile::tempdir().expect("project a tempdir");
    let project_b = tempfile::tempdir().expect("project b tempdir");

    let rt_with_db = |db_path: Option<std::path::PathBuf>| RuntimeConfig {
        db_path,
        default_namespace: Namespace::parse("local").unwrap(),
        embedding_model: None,
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::default()
    };

    let relative_db = std::path::PathBuf::from("./data/main.db");

    let id_a = {
        let _cwd = CwdGuard::enter(project_a.path());
        compute_config_id(&rt_with_db(Some(relative_db.clone())), None)
    };
    let id_b = {
        let _cwd = CwdGuard::enter(project_b.path());
        compute_config_id(&rt_with_db(Some(relative_db.clone())), None)
    };

    assert_ne!(
        id_a, id_b,
        "two projects overriding KHIVE_DB with the same relative path from \
             different working directories must not share a config_id; both \
             produced: {id_a}"
    );

    let id_a_again = {
        let _cwd = CwdGuard::enter(project_a.path());
        compute_config_id(&rt_with_db(Some(relative_db.clone())), None)
    };
    assert_eq!(
        id_a, id_a_again,
        "resolving the same project's KHIVE_DB override twice must produce \
             the same config_id"
    );
}

// ── #823: runtime `$prev` result depth guard ────────────────────────────

/// Iteratively (no native recursion) wrap `leaf` in `depth` nested
/// single-key objects, a synthetic stand-in for a pathologically deep
/// handler result (e.g. from `traverse`/`context`) that would otherwise
/// overflow the stack when cloned into `$prev` chain context.
///
/// Builds each level via a direct `Map` insert rather than `json!` — the
/// `json!` object-literal arm calls `serde_json::to_value(&v)` on the
/// accumulated value, which would walk the whole tree built so far on
/// every iteration (recursing to the current depth each time) and
/// overflow the stack itself well before reaching `depth` large enough
/// to exercise the guard under test.
fn nest_object(depth: usize, leaf: Value) -> Value {
    let mut v = leaf;
    for _ in 0..depth {
        let mut map = serde_json::Map::with_capacity(1);
        map.insert("nested".to_string(), v);
        v = Value::Object(map);
    }
    v
}

#[test]
fn deep_nested_result_over_limit_is_flagged() {
    let deep = nest_object(
        khive_request::NESTING_DEPTH_LIMIT + 5,
        json!({"leaf": true}),
    );
    let result_obj = json!({ "ok": true, "tool": "traverse", "result": deep });
    assert!(
        result_exceeds_depth_limit(&result_obj),
        "result nested past NESTING_DEPTH_LIMIT must be flagged"
    );
}

#[test]
fn result_at_exactly_the_depth_limit_is_not_flagged() {
    // A scalar leaf (not a container) so the wrapping objects alone land
    // exactly at NESTING_DEPTH_LIMIT containers deep.
    let at_limit = nest_object(khive_request::NESTING_DEPTH_LIMIT, json!(true));
    let result_obj = json!({ "ok": true, "tool": "traverse", "result": at_limit });
    assert!(
        !result_exceeds_depth_limit(&result_obj),
        "result nested exactly at the limit must still be usable as $prev context"
    );
}

#[test]
fn shallow_result_is_not_flagged() {
    let shallow = json!({"a": {"b": {"c": 1}}});
    let result_obj = json!({ "ok": true, "tool": "get", "result": shallow });
    assert!(!result_exceeds_depth_limit(&result_obj));
}

#[test]
fn result_missing_field_is_not_flagged() {
    let result_obj = json!({ "ok": false, "tool": "get", "error": "not found" });
    assert!(!result_exceeds_depth_limit(&result_obj));
}

#[test]
fn chain_aggregation_seam_rejects_over_limit_result_via_iterative_drop() {
    // Directly exercises the post-hoc aggregation-loop guard in
    // `run_parsed`'s `Chain` arm (isolated as
    // `chain_aggregation_depth_reject`) with a value nested well past
    // NESTING_DEPTH_LIMIT. If this branch let the rejected `result_obj`
    // fall out of scope instead of routing it through
    // `drop_value_iteratively`, `Value`'s derived recursive `Drop` would
    // overflow the stack on a value this deep — so this test failing to
    // complete (rather than merely asserting wrong) is itself the
    // regression signal for #823's post-hoc-rejection finding.
    let deep = nest_object(khive_request::NESTING_DEPTH_LIMIT + 50_000, json!(true));
    // Built via direct `Map` inserts, not `json!({..., "result": deep})`:
    // the object-literal macro arm calls `serde_json::to_value(&deep)` on
    // the already-deep value, which would recurse over the whole tree
    // and overflow the stack while constructing the fixture itself,
    // before the guard under test ever runs (see `nest_object` above).
    let mut envelope = serde_json::Map::with_capacity(3);
    envelope.insert("ok".to_string(), Value::Bool(true));
    envelope.insert("tool".to_string(), Value::String("traverse".to_string()));
    envelope.insert("result".to_string(), deep);
    let result_obj = Value::Object(envelope);

    let err = chain_aggregation_depth_reject(result_obj)
        .expect_err("result nested past NESTING_DEPTH_LIMIT must be rejected");

    assert_eq!(err["ok"], json!(false));
    assert_eq!(err["tool"], json!("traverse"));
    assert_eq!(err["error"]["kind"], json!("result_too_deep"));
    // The error entry must never embed the oversized value itself.
    assert!(err.get("result").is_none());
}

#[test]
fn chain_aggregation_seam_accepts_result_within_limit_unchanged() {
    let shallow = json!({ "ok": true, "tool": "get", "result": {"a": {"b": 1}} });
    let accepted = chain_aggregation_depth_reject(shallow.clone())
        .expect("result within the limit must be passed through unchanged");
    assert_eq!(accepted, shallow);
}

// ── earliest-seam guard: raw handler `Value` before json!/present/clone ──
//
// These exercise `chain_ok_envelope_or_depth_error` and
// `present_ok_envelope_or_depth_error` directly with a synthetic
// over-limit `Value` — no DSL parsing involved, standing in for a mock
// handler whose result is pathologically deep regardless of how shallow
// the caller's own op args were. This is the earliest point in
// `dispatch_op` / `run_parsed`'s parallel closure where the raw value is
// available, strictly before it is ever cloned, presented, or passed
// through `json!`/`serde_json::to_value`.

#[test]
fn chain_seam_rejects_over_limit_result_before_envelope_build() {
    // Deep enough that native recursion (json!/to_value/present) over
    // this value would be a real stack risk; the guard must reject it
    // via the iterative checker without ever attempting that recursion.
    let pathological = nest_object(khive_request::NESTING_DEPTH_LIMIT + 50_000, json!(true));
    let err =
        chain_ok_envelope_or_depth_error("traverse".to_string(), OpSuccess::complete(pathological))
            .expect_err("over-limit result must be rejected, not enveloped");
    assert_eq!(err.tool, "traverse");
    assert_eq!(err.error["kind"], json!("result_too_deep"));
    // The error payload must never embed the oversized value itself.
    assert!(err.error.get("result").is_none());
    assert!(err.error.get("nested").is_none());
}

#[test]
fn chain_seam_accepts_at_limit_result_and_moves_value_without_reserializing() {
    let at_limit = nest_object(khive_request::NESTING_DEPTH_LIMIT, json!("leaf"));
    let envelope =
        chain_ok_envelope_or_depth_error("get".to_string(), OpSuccess::complete(at_limit.clone()))
            .expect("result at exactly the limit must be accepted");
    assert_eq!(envelope["ok"], json!(true));
    assert_eq!(envelope["tool"], json!("get"));
    assert_eq!(envelope["result"], at_limit);
}

#[test]
fn parallel_seam_rejects_over_limit_result_before_present() {
    let pathological = nest_object(khive_request::NESTING_DEPTH_LIMIT + 50_000, json!(true));
    let envelope = present_ok_envelope_or_depth_error(
        "context".to_string(),
        OpSuccess::complete(pathological),
        PresentationMode::Agent,
        0,
        khive_types::VerbPresentationPolicy::Standard,
        NoteContentScope::None,
    );
    assert_eq!(envelope["ok"], json!(false));
    assert_eq!(envelope["tool"], json!("context"));
    assert_eq!(envelope["error"]["kind"], json!("result_too_deep"));
    assert!(envelope["error"].get("result").is_none());
}

#[test]
fn parallel_seam_accepts_shallow_result_and_applies_presentation() {
    let shallow = json!({"id": "11111111-1111-1111-1111-111111111111"});
    let envelope = present_ok_envelope_or_depth_error(
        "get".to_string(),
        OpSuccess::complete(shallow),
        PresentationMode::Verbose,
        0,
        khive_types::VerbPresentationPolicy::Standard,
        NoteContentScope::None,
    );
    assert_eq!(envelope["ok"], json!(true));
    assert_eq!(
        envelope["result"]["id"],
        json!("11111111-1111-1111-1111-111111111111")
    );
}

#[test]
fn success_envelope_requires_typed_degradation_and_preserves_it_through_presentation() {
    let success = OpSuccess {
        result: json!([{"id": "11111111-1111-1111-1111-111111111111"}]),
        degradation: SearchDegradation {
            status: Some(SearchStatus::Partial),
            retryable: false,
            arm_participation: Some(SearchArmParticipation {
                text: SearchArmEvidence {
                    status: SearchArmStatus::Error,
                    candidate_count: 1,
                },
                vector: SearchArmEvidence {
                    status: SearchArmStatus::Error,
                    candidate_count: 0,
                },
                text_mode: "all_terms",
            }),
            retry_after_ms: None,
            missing_backends: vec!["archive".to_string()],
            backend_errors: BTreeMap::from([(
                "archive".to_string(),
                BackendErrorDiagnostic {
                    kind: BackendSearchFailureKind::BackendError,
                    message: "storage unavailable".to_string(),
                    backend_id_masked: false,
                    backend_id_truncated: false,
                    backend_id_chars: "archive".chars().count(),
                },
            )]),
            backend_errors_omitted: 0,
        },
    };
    let envelope = present_ok_envelope_or_depth_error(
        "search".to_string(),
        success,
        PresentationMode::Agent,
        0,
        khive_types::VerbPresentationPolicy::Standard,
        NoteContentScope::None,
    );

    assert_eq!(envelope["ok"], json!(true));
    assert_eq!(envelope["status"], json!("partial"));
    assert_eq!(envelope["partial"], json!(true));
    assert_eq!(envelope["missing_backends"], json!(["archive"]));
    assert_eq!(
        envelope["backend_errors"]["archive"]["message"],
        json!("storage unavailable")
    );
    assert!(envelope.get("result").is_some());
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn chain_with_deep_accumulated_prev_result_errors_cleanly() {
    // Real end-to-end reproduction: chain N `create` ops where each step's
    // `properties.inner` embeds the previous op's full `properties` via
    // `$prev.properties`. Each op's own DSL args stay shallow (well under
    // NESTING_DEPTH_LIMIT), but the accumulated *runtime result* nests one
    // level deeper per chain step, the exact CWE-674 shape the parser's
    // syntax-tree guard cannot see. Past the limit this must surface a
    // clean per-op `result_too_deep` error and abort the remaining chain,
    // never attempting to clone/serialize the unbounded value.
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
    let server = KhiveMcpServer::new(runtime).expect("server builds with kg");

    let steps = khive_request::NESTING_DEPTH_LIMIT + 6;
    let mut dsl = String::from(
        r#"create(kind="entity", entity_kind="concept", name="d0", properties={"n": 0})"#,
    );
    for i in 1..steps {
        dsl.push_str(&format!(
                r#" | create(kind="entity", entity_kind="concept", name="d{i}", properties={{"inner": $prev.properties}})"#
            ));
    }

    let parsed = parse_request(&dsl).expect("each op's own args stay shallow; DSL must parse");
    assert_eq!(parsed.mode, ExecutionMode::Chain);

    let (response, _) = server
        .run_parsed(
            parsed.ops,
            parsed.mode,
            parsed.ranges,
            PresentationMode::Verbose,
            None,
            RunParsedContext {
                enforce_response_budget: true,
                max_batch_concurrency: MAX_BATCH_CONCURRENCY,
                from_wire: false,
                identity: None,
            },
        )
        .await;

    let results = response["results"]
        .as_array()
        .expect("results must be an array");
    assert_eq!(results.len(), steps);

    let failure_idx = results
        .iter()
        .position(|r| r["ok"] == json!(false))
        .expect("accumulated nesting must trip the depth guard before the chain completes");
    assert_eq!(
        results[failure_idx]["error"]["kind"],
        json!("result_too_deep"),
        "unexpected failure shape at index {failure_idx}: {:?}",
        results[failure_idx]
    );

    // Every op after the failing one is marked aborted, not attempted,
    // proving the process kept running instead of crashing.
    for r in &results[failure_idx + 1..] {
        assert_eq!(
            r["aborted"],
            json!(true),
            "expected abort after the depth guard trips: {r:?}"
        );
    }
}

// ── request-boundary regression: raw controls survive wire decoding ─────

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn request_boundary_raw_control_bytes_reach_handler() {
    // Simulates the actual MCP wire: a JSON-RPC client sends the tool's
    // `ops` argument as a JSON string using the standard JSON `\n`
    // escape. Deserializing `RequestParams` decodes that escape into an
    // actual raw LF byte inside the DSL source — the exact shape
    // `normalize_quoted_string` (crates/khive-request/src/parser/scan.rs)
    // exists to accept. This confirms the decoded raw newline survives
    // parsing and dispatch all the way to the pack handler's result.
    let wire = "{\"ops\":\"create(kind=\\\"entity\\\", entity_kind=\\\"concept\\\", name=\\\"line1\\nline2\\\")\"}";
    let params: RequestParams = serde_json::from_str(wire).expect("wire JSON deserializes");
    assert!(
        params.ops.contains('\n'),
        "deserialized ops must carry a raw LF, not the two-char escape: {:?}",
        params.ops
    );

    let config = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
    let server = KhiveMcpServer::new(runtime).expect("server builds with kg");

    let parsed = parse_request(&params.ops).expect("literal newline inside quotes must parse");
    let (response, _) = server
        .run_parsed(
            parsed.ops,
            parsed.mode,
            parsed.ranges,
            PresentationMode::Verbose,
            None,
            RunParsedContext {
                enforce_response_budget: true,
                max_batch_concurrency: MAX_BATCH_CONCURRENCY,
                from_wire: false,
                identity: None,
            },
        )
        .await;

    let results = response["results"]
        .as_array()
        .expect("results must be an array");
    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0]["ok"],
        json!(true),
        "unexpected result: {response:?}"
    );
    assert_eq!(results[0]["result"]["name"], json!("line1\nline2"));
}

// ── MCP-AUD-002 regression: save_to must bypass daemon forwarding ────────

fn make_daemon_save_to_test_server(db_path: Option<std::path::PathBuf>) -> KhiveMcpServer {
    let config = RuntimeConfig {
        db_path,
        default_namespace: Namespace::parse("test").unwrap(),
        actor_id: Some("test".to_string()),
        embedding_model: None,
        additional_embedding_models: vec![],
        events_split: None,
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).expect("request fixture runtime");
    KhiveMcpServer::new(runtime).expect("server builds with kg")
}

fn clear_daemon_env() {
    std::env::remove_var("KHIVE_SOCKET");
    std::env::remove_var("KHIVE_PID");
    std::env::remove_var("KHIVE_NO_DAEMON");
    std::env::remove_var("KHIVE_LOCK");
    std::env::remove_var("KHIVE_PROCESS_REF");
}

fn stats_without_request_local_usage(raw: &str) -> Value {
    let mut envelope: Value = serde_json::from_str(raw).expect("stats response JSON");
    for entry in envelope["results"]
        .as_array_mut()
        .expect("stats results array")
    {
        entry
            .as_object_mut()
            .expect("stats result object")
            .remove("usage");
    }
    envelope
}

/// khive#948: `wire_daemon_frame` forwards `RequestParams::request_id`
/// onto the `DaemonRequestFrame` unchanged, and defaults to `None` when
/// the caller supplied none.
#[cfg(unix)]
#[test]
#[serial_test::serial(config_ledger)]
fn wire_daemon_frame_forwards_request_id() {
    let server = make_daemon_save_to_test_server(None);

    let with_id = RequestParams {
        ops: "stats()".to_string(),
        request_id: Some(123),
        ..Default::default()
    };
    let frame = server.wire_daemon_frame(&with_id);
    assert_eq!(frame.request_id, Some(123));

    let without_id = RequestParams {
        ops: "stats()".to_string(),
        ..Default::default()
    };
    let frame = server.wire_daemon_frame(&without_id);
    assert_eq!(frame.request_id, None);
}

/// Query every persisted audit event and find the one whose
/// `resource.request_id` matches `id`, if any.
async fn find_audit_event_with_request_id(
    store: &Arc<dyn khive_storage::EventStore>,
    id: u64,
) -> Option<khive_storage::Event> {
    let page = store
        .query_events(
            EventFilter::default(),
            PageRequest {
                limit: 50,
                offset: 0,
            },
        )
        .await
        .expect("query_events must succeed");
    page.items
        .into_iter()
        .find(|ev| ev.payload["resource"]["request_id"] == json!(id))
}

/// khive#948: `request_id` was previously dropped on the
/// `KHIVE_NO_DAEMON`/soft-fallback local dispatch path because
/// `dispatch_request_wire` always passed `identity = None`. This drives
/// `request()` end-to-end under `KHIVE_NO_DAEMON=1` and inspects the
/// persisted audit event, proving the id now survives to
/// `resource.request_id` on the local-dispatch path too, not just the
/// daemon-forward path.
#[tokio::test]
#[serial]
#[serial_test::serial(config_ledger)]
async fn request_no_daemon_fallback_preserves_request_id_in_audit_event() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    std::env::set_var("KHIVE_NO_DAEMON", "1");

    let dir = tempfile::tempdir().expect("request fixture directory");
    let server = make_daemon_save_to_test_server(Some(dir.path().join("main.db")));
    server
        .request(
            Parameters(RequestParams {
                // Explicit `namespace="test"` so the write lands in the
                // same namespace the server's audit `EventStore` handle is
                // scoped to at construction, matching
                // `find_audit_event_with_request_id`'s read scope.
                ops: "stats(namespace=\"test\")".to_string(),
                request_id: Some(9001),
                ..Default::default()
            }),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("request() must succeed via local dispatch under KHIVE_NO_DAEMON");

    let store = server
        .event_store()
        .expect("request runtime must configure an EventStore");
    let matched = find_audit_event_with_request_id(&store, 9001).await;
    assert!(
        matched.is_some(),
        "KHIVE_NO_DAEMON local dispatch must stamp request_id onto the persisted \
             audit event"
    );

    clear_daemon_env();
}

/// khive#948: the `save_to` bypass (MCP-AUD-002) also routes through
/// `dispatch_request_wire`'s local dispatch — this proves the id
/// survives that path too.
#[tokio::test]
#[serial]
#[serial_test::serial(config_ledger)]
async fn request_save_to_bypass_preserves_request_id_in_audit_event() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("KHIVE_SAVE_TO_ROOT", dir.path());

    let server = make_daemon_save_to_test_server(Some(dir.path().join("main.db")));
    let sink_path = dir.path().join("out.jsonl");
    server
        .request(
            Parameters(RequestParams {
                ops: "stats(namespace=\"test\")".to_string(),
                save_to: Some(sink_path.to_string_lossy().to_string()),
                request_id: Some(9002),
                ..Default::default()
            }),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("request() with save_to must succeed");

    let store = server
        .event_store()
        .expect("request runtime must configure an EventStore");
    let matched = find_audit_event_with_request_id(&store, 9002).await;
    assert!(
        matched.is_some(),
        "save_to local-dispatch bypass must stamp request_id onto the persisted \
             audit event"
    );

    clear_daemon_env();
    std::env::remove_var("KHIVE_SAVE_TO_ROOT");
}

#[cfg(unix)]
async fn connect_when_daemon_ready(sock: &std::path::Path) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if tokio::net::UnixStream::connect(sock).await.is_ok() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "daemon never bound {sock:?} within 5s"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Regression for MCP-AUD-002 / #440: `request()` must NOT forward a
/// `save_to`-bearing call to a warm daemon (whose wire frame has no
/// `save_to` field and would silently return the inline result instead of
/// writing the sink). With a real daemon reachable at `KHIVE_SOCKET`, a
/// `save_to` request must still take the local path and return the
/// manifest with the file actually written — proving the daemon was
/// bypassed rather than silently dropping the sink.
#[cfg(unix)]
#[tokio::test]
#[serial]
#[serial_test::serial(config_ledger)]
async fn request_save_to_bypasses_daemon_forwarding_and_writes_manifest() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    let pid = dir.path().join("khived.pid");
    std::env::set_var("KHIVE_SOCKET", &sock);
    std::env::set_var("KHIVE_PID", &pid);
    std::env::remove_var("KHIVE_NO_DAEMON");
    // save_to destinations must resolve inside the allowed export root
    // (crate::save_sink); scope it to this test's tempdir.
    std::env::set_var("KHIVE_SAVE_TO_ROOT", dir.path());

    let server = make_daemon_save_to_test_server(Some(dir.path().join("main.db")));
    let daemon_server = server.clone();
    let handle = tokio::spawn(async move {
        let _ = khive_runtime::daemon::run_daemon(daemon_server).await;
    });
    connect_when_daemon_ready(&sock).await;

    let sink_path = dir.path().join("out.jsonl");
    let resp = server
        .request(
            Parameters(RequestParams {
                plan: None,
                ops: "stats()".to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: Some(sink_path.to_string_lossy().to_string()),
                format: None,
                format_per_op: None,
                request_id: None,
            }),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("request with save_to must succeed even with a warm daemon reachable");

    let manifest: serde_json::Value =
        serde_json::from_str(&resp).expect("response must be the save_to manifest JSON");
    assert!(
        manifest.get("rows").is_some() && manifest.get("path").is_some(),
        "response must be the save_to manifest, not an inline daemon result; got: {resp}"
    );
    assert!(
        sink_path.exists(),
        "save_to file must be written even when a daemon is reachable"
    );
    let contents = std::fs::read_to_string(&sink_path).expect("read sink file");
    assert!(
        !contents.trim().is_empty(),
        "sink file must contain JSONL content"
    );

    handle.abort();
    let _ = handle.await;
    clear_daemon_env();
    std::env::remove_var("KHIVE_SAVE_TO_ROOT");
}

/// A malformed MCP request must be rejected with the same typed RPC error
/// even when a matching warm daemon is available. The bridge protocol has a
/// string-only error channel, so this specifically fences the parse-before-
/// forward preflight in `request()`.
#[cfg(unix)]
#[tokio::test]
#[serial]
#[serial_test::serial(config_ledger)]
async fn request_parse_error_stays_typed_with_warm_daemon_available() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    let pid = dir.path().join("khived.pid");
    std::env::set_var("KHIVE_SOCKET", &sock);
    std::env::set_var("KHIVE_PID", &pid);
    std::env::remove_var("KHIVE_NO_DAEMON");

    let server = make_daemon_save_to_test_server(Some(dir.path().join("main.db")));
    let daemon_server = server.clone();
    let handle = tokio::spawn(async move {
        let _ = khive_runtime::daemon::run_daemon(daemon_server).await;
    });
    connect_when_daemon_ready(&sock).await;

    let error = server
        .request(
            Parameters(RequestParams {
                ops: "stats(".to_string(),
                ..Default::default()
            }),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect_err("malformed DSL must be rejected before forwarding");
    assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
    assert_eq!(
        error.data.as_ref().and_then(|data| data["reason"].as_str()),
        Some("parse-error")
    );

    // Prove this was the normal warm-daemon environment, not a no-daemon
    // fallback that happened to retain the local error shape.
    server
        .request(
            Parameters(RequestParams {
                ops: "stats()".to_string(),
                ..Default::default()
            }),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("valid follow-up must dispatch through the warm daemon");

    handle.abort();
    let _ = handle.await;
    clear_daemon_env();
}

// ── #644 regression: ambiguous post-write outcome must not double-dispatch ──
//
// `request()`'s daemon-forward call site (`if let Some(res) = forward_or_spawn(...)
// .await { return res; }`) must return BOTH `Some(Ok(_))` and `Some(Err(_))`
// directly, never falling through to `dispatch_request_wire` on the `Err`
// arm. If a future edit narrowed that match to only short-circuit on
// success (e.g. `if let Some(Ok(res)) = ...`), a mutating op whose real
// frame was already written to a now-dead daemon would ALSO run through
// local dispatch — a duplicate execution of the exact case #644 exists to
// prevent. This forces that ambiguous outcome (a fake socket that reads
// the request then closes without responding, exactly as a daemon crash
// mid-dispatch would) and proves both that the caller sees the
// ambiguous-forward error verbatim AND that the mutating op never actually
// ran locally.
#[cfg(unix)]
#[tokio::test]
#[serial]
#[serial_test::serial(config_ledger)]
async fn request_returns_ambiguous_forward_error_without_local_double_dispatch() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    let pid = dir.path().join("khived.pid");
    std::env::set_var("KHIVE_SOCKET", &sock);
    std::env::set_var("KHIVE_PID", &pid);
    std::env::remove_var("KHIVE_NO_DAEMON");

    let config = RuntimeConfig {
        db_path: Some(dir.path().join("main.db")),
        default_namespace: Namespace::parse("test").unwrap(),
        actor_id: Some("test".to_string()),
        embedding_model: None,
        additional_embedding_models: vec![],
        events_split: None,
        packs: vec!["kg".to_string(), "comm".to_string()],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).expect("file-backed request runtime");
    let server = KhiveMcpServer::new(runtime).expect("server builds with kg + comm");

    // Fake "crashed daemon": accept exactly one connection, read the
    // request frame (the real write #644 cares about), then drop the
    // stream without writing a response.
    let listener = tokio::net::UnixListener::bind(&sock).expect("bind fake crash-daemon socket");
    let fake_handle = tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            let _ = khive_runtime::daemon::read_frame(&mut stream).await;
        }
    });

    let baseline = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        })
        .await
        .expect("baseline stats() must succeed");

    let resp = server
        .request(
            Parameters(RequestParams {
                plan: None,
                ops: "comm.send(to=\"bob\", content=\"double-forward-probe\")".to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            }),
            tokio_util::sync::CancellationToken::new(),
        )
        .await;

    match resp {
        Err(McpError { message, .. }) => {
            assert!(
                message
                    .contains("not retrying or locally dispatching to avoid duplicate execution"),
                "request() must surface forward_or_spawn's ambiguous-forward error \
                     verbatim, not a local dispatch result; got: {message}"
            );
        }
        Ok(v) => panic!(
            "request() must return the ambiguous-forward error directly, not fall \
                 through to local dispatch; got Ok({v})"
        ),
    }

    let after = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        })
        .await
        .expect("post-request stats() must succeed");

    assert_eq!(
        stats_without_request_local_usage(&after),
        stats_without_request_local_usage(&baseline),
        "the comm.send op must NEVER have run locally after the ambiguous \
             forward outcome — a double-dispatch would mutate local state here"
    );

    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), fake_handle).await;
    clear_daemon_env();
}

// ── #947 Medium regression: strict fallback lands as a per-op envelope ──
//
// Before this fix, `request()` returned `forward_or_spawn`'s strict-mode
// rejection as a raw `Err(McpError)`, bypassing the per-op `{ok, tool,
// result/error}` / `summary` wire contract every other failure mode goes
// through. This drives `request()` end to end with a genuinely
// unreachable daemon under `KHIVE_DAEMON_STRICT=1` and asserts: (1) the
// response is `Ok(envelope_json)`, never an RPC error; (2) each shape
// (single op, parallel batch, chain) reports the fallback reason as a
// normal failed-op `error`, with chain aborting the remaining ops exactly
// like a real op failure would (`run_parsed`'s `Chain` arm); (3) summary
// counts match `results`; and (4) none of the ops ever ran locally (a
// `stats()` snapshot taken via the trusted `dispatch_request_local` path
// is unchanged after all three calls).
#[cfg(unix)]
#[tokio::test]
#[serial]
#[serial_test::serial(config_ledger)]
async fn request_strict_fallback_lands_as_failed_op_envelope_not_rpc_error() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    crate::daemon::reset_fallback_counters();
    let dir = tempfile::tempdir().expect("tempdir");
    // Never bound by anything in this test. The spawned test harness exits
    // immediately on `mcp --daemon`, so #898 classifies this as a confirmed
    // respawn failure rather than the older generic `no_socket` fallback.
    std::env::set_var("KHIVE_SOCKET", dir.path().join("khived.sock"));
    std::env::set_var("KHIVE_PID", dir.path().join("khived.pid"));
    std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));
    std::env::remove_var("KHIVE_NO_DAEMON");
    std::env::set_var("KHIVE_DAEMON_STRICT", "1");

    let config = RuntimeConfig {
        db_path: Some(dir.path().join("main.db")),
        default_namespace: Namespace::parse("test").unwrap(),
        actor_id: Some("test".to_string()),
        embedding_model: None,
        additional_embedding_models: vec![],
        events_split: None,
        packs: vec!["kg".to_string(), "comm".to_string()],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).expect("file-backed request runtime");
    let server = KhiveMcpServer::new(runtime).expect("server builds with kg + comm");

    let baseline = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        })
        .await
        .expect("baseline stats() must succeed");

    fn assert_fallback_error(entry: &Value, tool: &str) {
        assert_eq!(entry["ok"], json!(false), "entry: {entry}");
        assert_eq!(entry["tool"], json!(tool), "entry: {entry}");
        let msg = entry["error"]["message"]
            .as_str()
            .expect("error must carry its message");
        assert!(
            msg.contains("KHIVE_DAEMON_STRICT"),
            "error must name the strict mode that rejected the fallback: {msg}"
        );
        assert!(
            msg.contains("respawn_failed"),
            "error must name the confirmed respawn failure: {msg}"
        );
        assert!(
            msg.contains("make local"),
            "error must include the safe respawn remediation: {msg}"
        );
    }

    // ── single op ──────────────────────────────────────────────────────
    let single_resp = server
        .request(
            Parameters(RequestParams {
                plan: None,
                ops: "comm.send(to=\"bob\", content=\"strict-single-probe\")".to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            }),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("strict fallback must land as a normal Ok(envelope), not Err(McpError)");
    let single: Value =
        serde_json::from_str(&single_resp).expect("response must be the request envelope");
    assert_eq!(
        single["results"].as_array().expect("results array").len(),
        1
    );
    assert_fallback_error(&single["results"][0], "comm.send");
    assert_eq!(
        single["summary"],
        json!({ "total": 1, "succeeded": 0, "failed": 1, "aborted": 0 })
    );

    // ── parallel batch ─────────────────────────────────────────────────
    let batch_resp = server
        .request(
            Parameters(RequestParams {
                plan: None,
                ops: "[comm.send(to=\"bob\", content=\"strict-batch-1\"), \
                       comm.send(to=\"bob\", content=\"strict-batch-2\")]"
                    .to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            }),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("strict fallback must land as a normal Ok(envelope), not Err(McpError)");
    let batch: Value =
        serde_json::from_str(&batch_resp).expect("response must be the request envelope");
    let batch_results = batch["results"].as_array().expect("results array");
    assert_eq!(batch_results.len(), 2);
    for entry in batch_results {
        assert_fallback_error(entry, "comm.send");
    }
    assert_eq!(
        batch["summary"],
        json!({ "total": 2, "succeeded": 0, "failed": 2, "aborted": 0 })
    );

    // ── chain (must abort remaining ops per the wire contract) ─────────
    let chain_resp = server
        .request(
            Parameters(RequestParams {
                plan: None,
                ops: "comm.send(to=\"bob\", content=\"strict-chain-1\") | \
                      comm.send(to=\"bob\", content=\"strict-chain-2\")"
                    .to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            }),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("strict fallback must land as a normal Ok(envelope), not Err(McpError)");
    let chain: Value =
        serde_json::from_str(&chain_resp).expect("response must be the request envelope");
    let chain_results = chain["results"].as_array().expect("results array");
    assert_eq!(chain_results.len(), 2);
    assert_fallback_error(&chain_results[0], "comm.send");
    assert_eq!(
        chain_results[1],
        json!({ "ok": false, "tool": "comm.send", "aborted": true, "domain_disposition":"not_committed" })
    );
    assert_eq!(
        chain["summary"],
        json!({ "total": 2, "succeeded": 0, "failed": 1, "aborted": 1 })
    );

    // ── no local dispatch ever happened for any of the three calls ─────
    let after = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: "stats()".to_string(),
            presentation: None,
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        })
        .await
        .expect("post-request stats() must succeed");
    assert_eq!(
        stats_without_request_local_usage(&after),
        stats_without_request_local_usage(&baseline),
        "no comm.send op must ever have run locally under strict-mode fallback \
             rejection — a local dispatch would mutate local state here"
    );

    crate::daemon::reset_fallback_counters();
    clear_daemon_env();
}

// ── #1220: top-level `status` distinguishes a partially-failed batch ──────

fn in_memory_kg_server() -> KhiveMcpServer {
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
    KhiveMcpServer::new(runtime).expect("server builds with kg")
}

/// ADR-130 §1: the KG single-backend (no coordinator) envelope must also
/// carry `status="complete"` on every successful `search` — both for a
/// genuine no-match and a populated result — with no possible "partial"
/// state for a lone backend. Other verbs must not gain a `status` field.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn single_backend_search_reports_status_complete() {
    let server = in_memory_kg_server();

    let resp = server
            .dispatch_request_local(RequestParams {
                plan: None,
                ops: r#"search(kind="entity", query="a deliberately long keyword dense query whose terms cannot all match any entity in this empty corpus")"#.to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("search dispatch must succeed");
    let parsed: Value = serde_json::from_str(&resp).expect("envelope must be JSON");
    let search = &parsed["results"][0];
    assert_eq!(search["ok"], json!(true), "unexpected response: {search}");
    assert_eq!(search["status"], json!("complete"));
    assert_eq!(search["result"], json!([]));
    assert_eq!(
        search["arm_participation"],
        json!({
            "text": {
                "mode": "all_terms",
                "status": "ran",
                "candidate_count": 0,
                "reason": "No text candidate survived matching, filtering, fusion, and the result limit. Plain text search combines normalized term groups conjunctively; try fewer terms."
            },
            "vector": {"status": "skipped", "candidate_count": 0}
        }),
        "a dense zero-hit query must prove that text ran without a match"
    );
    assert!(search.get("partial").is_none());

    // Chain (`|`), not a parallel batch: `search` must observe the
    // preceding `create`, which an independent-ops batch does not
    // guarantee (bounded-concurrency ops have no relative ordering).
    let resp = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: "create(kind=\"entity\", entity_kind=\"concept\", name=\"kg-search-status\") \
                       | search(kind=\"entity\", query=\"kg-search-status\")"
                .to_string(),
            presentation: None,
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        })
        .await
        .expect("chain dispatch must succeed");
    let parsed: Value = serde_json::from_str(&resp).expect("envelope must be JSON");
    let create = &parsed["results"][0];
    let search = &parsed["results"][1];
    assert!(
        create.get("status").is_none(),
        "non-search verbs must not gain a status field: {create}"
    );
    assert_eq!(search["ok"], json!(true), "unexpected response: {search}");
    assert_eq!(search["status"], json!("complete"));
    assert_eq!(
        search["arm_participation"],
        json!({
            "text": {"mode": "all_terms", "status": "ran", "candidate_count": 1},
            "vector": {"status": "skipped", "candidate_count": 0}
        }),
        "an exact-name presence check must expose its text-arm evidence"
    );
    assert!(
        search["result"]
            .as_array()
            .map(|items| !items.is_empty())
            .unwrap_or(false),
        "unexpected response: {search}"
    );
}

// ── MAJ-3: explicit-namespace narrowing arm of `coordinator_search_visibility` ──

fn registry_with_visible_namespaces(ns: Vec<khive_runtime::Namespace>) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.with_visible_namespaces(ns);
    builder.build().expect("build registry with no packs")
}

fn request_identity_with_visible_namespaces(ns: Vec<&str>) -> khive_runtime::RequestIdentity {
    khive_runtime::RequestIdentity {
        namespace: "local".to_string(),
        actor_id: None,
        visible_namespaces: ns.into_iter().map(str::to_string).collect(),
        process_ref: None,
        request_id: None,
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn scheduled_replay_reads_its_own_actor_namespace_and_never_the_daemons_visibility() {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::local(),
        actor_id: Some("lambda:daemon".to_string()),
        visible_namespaces: vec![Namespace::parse("daemon-visible").unwrap()],
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::default()
    })
    .expect("in-memory replay runtime");
    let server = KhiveMcpServer::new(runtime).expect("replay server");

    for actor_id in [Some("lambda:scheduled-replay"), None] {
        let verified_actor = actor_id
            .map(|actor| khive_runtime::VerifiedActor::new(actor).expect("verified creator"));
        let raw = server
            .dispatch_request_replay_as(
                RequestParams {
                    ops: "whoami()".to_string(),
                    presentation: Some("verbose".to_string()),
                    format: Some("json".to_string()),
                    ..Default::default()
                },
                "local",
                verified_actor,
            )
            .await
            .expect("scheduled replay dispatch");
        let envelope: Value = serde_json::from_str(&raw).expect("replay JSON envelope");
        assert_eq!(envelope["results"][0]["ok"], true, "{envelope}");
        let identity = &envelope["results"][0]["result"];
        assert_eq!(identity["actor_id"], actor_id.unwrap_or("local"));
        assert_eq!(
            identity["actor_kind"],
            if actor_id.is_some() {
                "actor"
            } else {
                "anonymous"
            }
        );
        assert_eq!(identity["unattributed"], actor_id.is_none());
        assert_eq!(identity["namespace"], "local");
        // A replay reads exactly what its verified actor reads on every
        // other path: `local` plus the actor's own namespace (ADR-007 Rev 4
        // Rule 3b, folded where the token is minted), and nothing from the
        // daemon's configured visibility. An anonymous replay keeps `local`.
        let expected = match actor_id {
            Some(actor) => json!(["local", actor]),
            None => json!(["local"]),
        };
        assert_eq!(
            identity["visible_namespaces"], expected,
            "replay inherits its own actor namespace and never the daemon's visibility: {identity}"
        );
        assert!(
            !identity["visible_namespaces"]
                .as_array()
                .map(|v| v.iter().any(|ns| ns == "daemon-visible"))
                .unwrap_or(true),
            "daemon visibility leaked into a scheduled replay: {identity}"
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn issue_2427_coordinator_search_consumes_normalized_identity_visibility() {
    use crate::coordinator::tests::MockCoordinator;

    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::local(),
        actor_id: Some("lambda:daemon".to_string()),
        visible_namespaces: vec![Namespace::parse("daemon-visible").unwrap()],
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::default()
    })
    .expect("in-memory coordinator runtime");
    let coordinator = MockCoordinator::multi_backend();
    let server = KhiveMcpServer::new(runtime)
        .expect("coordinator server")
        .with_coordinator(Arc::clone(&coordinator) as Arc<dyn CoordinatorService>);

    // Supply the boundary's normalized output directly; daemon-frame parsing
    // and transport coverage belong to the daemon tests, not this fixture.
    for (visible, explicit_namespace, mut expected) in [
        (
            vec!["lambda:request-actor", "client-visible"],
            false,
            vec!["lambda:request-actor", "client-visible", "local"],
        ),
        (vec![], false, vec!["local"]),
        // A reconstructed token carrying an explicit list is consumed as
        // given: the coordinator adds `local` and nothing else, so neither
        // the request actor's own namespace nor the daemon's configured
        // visibility is folded in behind the caller's back.
        (
            vec!["client-visible"],
            false,
            vec!["client-visible", "local"],
        ),
        (vec!["lambda:request-actor", "client-visible"], true, vec![]),
    ] {
        let mut identity = request_identity_with_visible_namespaces(visible);
        identity.actor_id = Some("lambda:request-actor".to_string());
        let ops = if explicit_namespace {
            r#"search(kind="entity", query="visibility", namespace="chosen")"#
        } else {
            r#"search(kind="entity", query="visibility")"#
        };
        coordinator.search_called.store(false, Ordering::SeqCst);
        let raw = server
            .dispatch_request_inner(
                RequestParams {
                    ops: ops.to_string(),
                    ..Default::default()
                },
                true,
                Some(identity),
                DispatchOrigin::Local,
            )
            .await
            .expect("coordinator dispatch");
        let envelope: Value = serde_json::from_str(&raw).expect("coordinator JSON envelope");
        assert_eq!(envelope["results"][0]["ok"], true, "{envelope}");
        assert!(
            coordinator.search_called.load(Ordering::SeqCst),
            "search must reach the coordinator, not the single-backend registry"
        );
        let mut actual: Vec<String> = coordinator
            .last_extra_visible
            .lock()
            .expect("captured coordinator visibility")
            .iter()
            .map(|namespace| namespace.as_str().to_string())
            .collect();
        actual.sort();
        expected.sort();
        assert_eq!(
            actual, expected,
            "coordinator must consume supplied visibility without widening internal identities"
        );
    }
}

/// No per-request identity: falls back to the registry's operator-baked
/// `visible_namespaces`, widened with `local` — mirrors the normal
/// registry dispatch path's default-case widening.
#[test]
#[serial_test::serial(config_ledger)]
fn coordinator_search_visibility_widens_to_registry_defaults_when_no_identity() {
    let registry = registry_with_visible_namespaces(vec![khive_runtime::Namespace::parse(
        "tenant-a",
    )
    .unwrap()]);
    let extra = coordinator_search_visibility(&registry, &json!({}), None);
    assert!(
        extra.contains(&khive_runtime::Namespace::parse("tenant-a").unwrap()),
        "must widen to the registry's baked visible_namespaces: {extra:?}"
    );
    assert!(
        extra.contains(&khive_runtime::Namespace::local()),
        "must always include local: {extra:?}"
    );
}

/// A per-request identity's `visible_namespaces` overrides the registry's
/// baked defaults entirely (ADR-096 Fork 1) — the registry's "tenant-a"
/// must NOT leak into a request identity scoped to "tenant-b" only.
#[test]
#[serial_test::serial(config_ledger)]
fn coordinator_search_visibility_widens_to_identity_visible_namespaces() {
    let registry = registry_with_visible_namespaces(vec![khive_runtime::Namespace::parse(
        "tenant-a",
    )
    .unwrap()]);
    let identity = request_identity_with_visible_namespaces(vec!["tenant-b"]);
    let extra = coordinator_search_visibility(&registry, &json!({}), Some(&identity));
    assert!(
        extra.contains(&khive_runtime::Namespace::parse("tenant-b").unwrap()),
        "must widen to the per-request identity's visible_namespaces: {extra:?}"
    );
    assert!(
        !extra.contains(&khive_runtime::Namespace::parse("tenant-a").unwrap()),
        "must NOT fall back to the registry's baked defaults when an identity is \
             present: {extra:?}"
    );
    assert!(
        extra.contains(&khive_runtime::Namespace::local()),
        "must always include local: {extra:?}"
    );
}

/// An explicit `namespace=` request argument intentionally narrows
/// visibility to that one namespace — the coordinator boundary must
/// return an unwidened empty extra-visibility set in that case, exactly
/// like the normal registry dispatch path's `explicit_namespace` branch.
///
/// RED before the fix: an explicit namespace still widened visibility to
/// the caller's full `visible_namespaces` set, silently overriding the
/// caller's intended narrowing.
#[test]
#[serial_test::serial(config_ledger)]
fn coordinator_search_visibility_narrows_to_empty_when_namespace_explicit() {
    let registry = registry_with_visible_namespaces(vec![khive_runtime::Namespace::parse(
        "tenant-a",
    )
    .unwrap()]);
    let identity = request_identity_with_visible_namespaces(vec!["tenant-b"]);
    let extra = coordinator_search_visibility(
        &registry,
        &json!({"namespace": "tenant-c"}),
        Some(&identity),
    );
    assert!(
        extra.is_empty(),
        "an explicit namespace= argument must narrow to an empty extra-visible \
             set, not widen: {extra:?}"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn unknown_verb_with_invalid_namespace_is_not_classified_as_verb_refused() {
    let server = in_memory_kg_server();
    let response = server
        .dispatch_request_local(RequestParams {
            ops: "not_loaded(namespace=5)".to_string(),
            ..Default::default()
        })
        .await
        .expect("dispatch failures remain in the per-operation envelope");
    let response: Value = serde_json::from_str(&response).expect("response envelope");
    assert!(
        response["results"][0]["error"]["message"]
            .as_str()
            .is_some_and(|error| error.contains("invalid namespace")),
        "unexpected error: {response}"
    );
    assert!(
        response["results"][0].get("reason").is_none(),
        "namespace validation is not an unknown-verb refusal: {response}"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn request_status_is_success_when_every_op_in_batch_succeeds() {
    let server = in_memory_kg_server();
    let resp = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: "[create(kind=\"entity\", entity_kind=\"concept\", name=\"status-ok-1\"), \
                       create(kind=\"entity\", entity_kind=\"concept\", name=\"status-ok-2\")]"
                .to_string(),
            presentation: None,
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        })
        .await
        .expect("batch dispatch must succeed");
    let parsed: Value = serde_json::from_str(&resp).expect("envelope must be JSON");
    assert_eq!(parsed["summary"]["failed"], 0);
    assert_eq!(
        parsed["status"], "success",
        "an all-succeeding batch must report status=success; got {parsed}"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn request_status_is_partial_when_a_batch_op_fails() {
    let server = in_memory_kg_server();
    // The second op targets an unknown kind and fails; the first succeeds.
    let resp = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: "[create(kind=\"entity\", entity_kind=\"concept\", name=\"status-partial-1\"), \
                       search(kind=\"not_a_real_kind\", query=\"x\")]"
                .to_string(),
            presentation: None,
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        })
        .await
        .expect("batch dispatch must succeed at the RPC level even with a failed op");
    let parsed: Value = serde_json::from_str(&resp).expect("envelope must be JSON");
    assert!(
        parsed["summary"]["failed"].as_u64().unwrap_or(0) > 0,
        "expected at least one failed op; got {parsed}"
    );
    assert_eq!(
        parsed["status"], "partial",
        "a batch with a failed op must report status=partial; got {parsed}"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn request_status_is_partial_when_a_chain_op_is_aborted() {
    let server = in_memory_kg_server();
    let resp = server
            .dispatch_request_local(RequestParams {
                plan: None,
                ops: "search(kind=\"not_a_real_kind\", query=\"x\") | \
                      create(kind=\"entity\", entity_kind=\"concept\", name=\"status-chain-aborted\")"
                    .to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("chain dispatch must succeed at the RPC level even with an aborted op");
    let parsed: Value = serde_json::from_str(&resp).expect("envelope must be JSON");
    assert!(
        parsed["summary"]["aborted"].as_u64().unwrap_or(0) > 0,
        "expected the second chain op to be aborted; got {parsed}"
    );
    assert_eq!(
        parsed["status"], "partial",
        "a chain with an aborted op must report status=partial; got {parsed}"
    );
}
#[test]
fn issue2757_note_scope_handles_persisted_kinds_without_payload_markers() {
    let server = large_result_test_server();
    let note = json!({
        "id":"11111111-1111-4111-8111-111111111111", "kind":"unloaded_pack_note",
        "version":1, "created_at":"2026-09-15T12:00:00Z", "updated_at":"2026-09-15T12:00:00Z",
        "content":null,
    });
    assert_eq!(
        note_content_scope(true, "get", &note, &server.registry),
        NoteContentScope::Record
    );
    let page =
        json!({"items":[note], "requested_limit":20, "effective_limit":20, "limit_clamped":false});
    assert_eq!(
        note_content_scope(true, "list", &page, &server.registry),
        NoteContentScope::Items
    );
    assert_eq!(
        note_content_scope(false, "list", &page, &server.registry),
        NoteContentScope::None
    );
    assert_eq!(
        note_content_scope(true, "search", &page, &server.registry),
        NoteContentScope::None
    );
    let entity = json!({"id":"entity", "kind":"concept", "properties":{
        "parse_content":true, "content":page, "kind":"observation", "version":1,
    }});
    assert_eq!(
        note_content_scope(true, "get", &entity, &server.registry),
        NoteContentScope::None
    );
}
