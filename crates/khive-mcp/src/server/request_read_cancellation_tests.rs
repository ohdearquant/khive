use super::{
    scope_mcp_request_read_cancellation, stdio_bridge_response_deadline_from_env, KhiveMcpServer,
};
use crate::tools::request::RequestParams;
use khive_runtime::{KhiveRuntime, RuntimeConfig};
use rmcp::{handler::server::wrapper::Parameters, ErrorData as McpError};
use serde_json::Value;
use std::{future::Future, sync::Arc, time::Duration};

#[derive(Clone)]
struct EofProbeServer {
    started: Arc<tokio::sync::Notify>,
    cancelled: Arc<tokio::sync::Notify>,
}

impl rmcp::ServerHandler for EofProbeServer {
    fn call_tool(
        &self,
        _request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> impl Future<Output = Result<rmcp::model::CallToolResult, McpError>> + Send + '_ {
        let started = self.started.clone();
        let cancelled = self.cancelled.clone();
        async move {
            started.notify_one();
            scope_mcp_request_read_cancellation(context.ct, async {
                khive_storage::wait_for_request_read_cancellation().await;
            })
            .await;
            cancelled.notify_one();
            Ok(rmcp::model::CallToolResult::success(Vec::new()))
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn stdio_eof_cancels_root_and_request_read_before_rmcp_drain() {
    use rmcp::transport::async_rw::AsyncRwTransport;
    use tokio::io::AsyncWriteExt;

    let started = Arc::new(tokio::sync::Notify::new());
    let cancelled = Arc::new(tokio::sync::Notify::new());
    let probe = EofProbeServer {
        started: started.clone(),
        cancelled: cancelled.clone(),
    };
    let root = tokio_util::sync::CancellationToken::new();
    let (server_io, mut client_io) = tokio::io::duplex(16 * 1024);
    let (read, write) = tokio::io::split(server_io);
    let transport = crate::transport::CancelOnEofTransport::with_idle_timeout(
        AsyncRwTransport::new_server(read, write),
        root.clone(),
        None,
        None,
        Some(Duration::from_secs(3600)),
    );
    let running = rmcp::service::serve_directly_with_ct(probe, transport, None, root.clone());

    client_io
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"probe\",\"arguments\":{}}}\n",
            )
            .await
            .expect("write one real JSON-RPC tool request");
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("rmcp never admitted the request handler");

    drop(client_io);

    tokio::time::timeout(Duration::from_secs(2), cancelled.notified())
        .await
        .expect("stdio EOF did not cancel the request read scope promptly");
    assert!(
        root.is_cancelled(),
        "EOF must cancel the exact root token passed into rmcp"
    );
    let reason = tokio::time::timeout(Duration::from_secs(2), running.waiting())
        .await
        .expect("rmcp remained in its five-second EOF drain")
        .expect("rmcp service task panicked");
    assert!(
        matches!(
            reason,
            rmcp::service::QuitReason::Closed | rmcp::service::QuitReason::Cancelled
        ),
        "unexpected rmcp quit reason after EOF: {reason:?}"
    );
}

/// An idle stdio bridge — pipe still open, no request sent — must
/// be reaped the same way a real EOF is, not held open indefinitely.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn stdio_idle_timeout_cancels_root_without_eof() {
    use rmcp::transport::async_rw::AsyncRwTransport;

    let started = Arc::new(tokio::sync::Notify::new());
    let cancelled = Arc::new(tokio::sync::Notify::new());
    let probe = EofProbeServer {
        started: started.clone(),
        cancelled: cancelled.clone(),
    };
    let root = tokio_util::sync::CancellationToken::new();
    let (server_io, client_io) = tokio::io::duplex(16 * 1024);
    let (read, write) = tokio::io::split(server_io);
    let transport = crate::transport::CancelOnEofTransport::with_idle_timeout(
        AsyncRwTransport::new_server(read, write),
        root.clone(),
        Some(Duration::from_millis(50)),
        None,
        Some(Duration::from_secs(3600)),
    );
    let running = rmcp::service::serve_directly_with_ct(probe, transport, None, root.clone());

    // Deliberately never write anything and never drop `client_io`: the
    // pipe stays open exactly like an abandoned bridge's client — this
    // must still be reaped once the idle timeout elapses.
    // Observe the cancellation BEFORE consuming the service, and it has to
    // be in this order. `RunningService` holds a `dg: DropGuard`
    // (rmcp 1.8.0, `src/service.rs:712`) and `waiting(mut self)` consumes
    // `self`, so the guard cancels this very token as `waiting` returns
    // whatever the transport did. Asserting `root.is_cancelled()` after
    // that await therefore passes even with the adapter's cancel deleted:
    // it measures rmcp's drop guard, not the idle path. Awaiting
    // `root.cancelled()` while the service is still alive is the only
    // ordering that can tell the two apart.
    tokio::time::timeout(Duration::from_secs(2), root.cancelled())
        .await
        .expect("idle timeout must cancel the exact root token passed into rmcp");

    let reason = tokio::time::timeout(Duration::from_secs(2), running.waiting())
        .await
        .expect("idle timeout never closed the session")
        .expect("rmcp service task panicked");
    assert!(
        matches!(
            reason,
            rmcp::service::QuitReason::Closed | rmcp::service::QuitReason::Cancelled
        ),
        "unexpected rmcp quit reason after idle timeout: {reason:?}"
    );
    drop(client_io);
}

#[derive(Clone)]
struct SlowProbeServer {
    started: Arc<tokio::sync::Notify>,
    delay: Duration,
}

impl rmcp::ServerHandler for SlowProbeServer {
    fn call_tool(
        &self,
        _request: rmcp::model::CallToolRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> impl Future<Output = Result<rmcp::model::CallToolResult, McpError>> + Send + '_ {
        let started = self.started.clone();
        let delay = self.delay;
        async move {
            started.notify_one();
            tokio::time::sleep(delay).await;
            Ok(rmcp::model::CallToolResult::success(Vec::new()))
        }
    }
}

/// Regression: two outstanding obligations under one request id are
/// refused, not resolved by guessing.
///
/// Retirement matches on request id, so two live entries sharing an id
/// make it ambiguous which one a completing response discharges — and both
/// resolutions are wrong in opposite directions. Removing the first match
/// can leave a *completed* request's instant as the newest entry, so the
/// freshness check defers past the older obligation's TTL: the unbounded
/// session this whole mechanism exists to close. Removing the last match
/// can leave the older instant, so the session closes out from under a
/// live handler. The transport cannot tell the two apart, so it refuses
/// the second admission instead.
///
/// Idle reaping is off and the pipe is deliberately never closed, so the
/// refusal is the only thing that can end this session. A build that
/// admitted the duplicate would leave `root` uncancelled and this test
/// would exhaust its bound.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn stdio_refuses_a_second_outstanding_obligation_under_one_request_id() {
    use rmcp::transport::async_rw::AsyncRwTransport;
    use tokio::io::AsyncWriteExt;

    let first_admitted = Arc::new(tokio::sync::Notify::new());
    let probe = OutOfOrderProbeServer {
        first_admitted: first_admitted.clone(),
        calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    };
    let root = tokio_util::sync::CancellationToken::new();
    let (server_io, client_io) = tokio::io::duplex(16 * 1024);
    let (server_read, server_write) = tokio::io::split(server_io);
    let transport = crate::transport::CancelOnEofTransport::with_idle_timeout(
        AsyncRwTransport::new_server(server_read, server_write),
        root.clone(),
        // No idle timer: only the duplicate-id refusal can close this.
        None,
        None,
        Some(Duration::from_secs(3600)),
    );
    let obligations = transport.in_flight_handle();
    let running = rmcp::service::serve_directly_with_ct(probe, transport, None, root.clone());
    let (_client_read, mut client_write) = tokio::io::split(client_io);

    let request = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\
                        \"params\":{\"name\":\"probe\",\"arguments\":{}}}\n";
    client_write
        .write_all(request)
        .await
        .expect("write the first request");
    tokio::time::timeout(Duration::from_secs(2), first_admitted.notified())
        .await
        .expect("rmcp never admitted the first request");

    // Premise: id 1 is genuinely still outstanding. Without this the second
    // request would just be a reuse after retirement, which is legitimate
    // and not what this test is about.
    assert_eq!(
        obligations.lock().expect("obligation queue poisoned").len(),
        1,
        "fixture premise: the first request must still be outstanding when the duplicate \
             arrives, otherwise nothing is ambiguous"
    );
    assert!(
        !root.is_cancelled(),
        "fixture premise: a single outstanding request must not have closed the session"
    );

    client_write
        .write_all(request)
        .await
        .expect("write the duplicate-id request");

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !root.is_cancelled() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        root.is_cancelled(),
        "a second outstanding obligation under an id that already has one must close the \
             session; admitting it leaves retirement to pick an entry arbitrarily, which \
             corrupts the freshness check in one direction or the other"
    );

    drop(client_write);
    let _ = tokio::time::timeout(Duration::from_secs(2), running.waiting()).await;
}

/// Regression: an id whose obligation has gone STALE is still a duplicate.
///
/// Staleness is a statement about the freshness check, not about the
/// handler: rmcp keeps a spawned handler alive independently of this
/// receive loop, so an entry past its TTL routinely names a request that
/// is still running and has simply not answered yet. If the staleness drop
/// runs before the duplicate scan, that entry is gone by the time the scan
/// looks, the reused id is admitted as a fresh obligation, and the first of
/// the two eventual responses retires the NEW entry by id match — leaving
/// the older live handler untracked and the idle branch free to close out
/// from under it. Scanning before pruning refuses the reuse instead.
///
/// This is the arm the earlier ordering passed: the plain duplicate test
/// above uses an hour-long TTL, so its entry is never stale and the two
/// orderings are indistinguishable there.
///
/// Idle reaping is off and the pipe is never closed, so the refusal is the
/// only thing that can end this session.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn stdio_refuses_a_reused_id_whose_obligation_is_already_stale() {
    use rmcp::transport::async_rw::AsyncRwTransport;
    use tokio::io::AsyncWriteExt;

    const OBLIGATION_TTL: Duration = Duration::from_millis(100);

    let first_admitted = Arc::new(tokio::sync::Notify::new());
    let probe = OutOfOrderProbeServer {
        first_admitted: first_admitted.clone(),
        // Parks forever on the first call: the handler outlives its TTL.
        calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    };
    let root = tokio_util::sync::CancellationToken::new();
    let (server_io, client_io) = tokio::io::duplex(16 * 1024);
    let (server_read, server_write) = tokio::io::split(server_io);
    let transport = crate::transport::CancelOnEofTransport::with_idle_timeout(
        AsyncRwTransport::new_server(server_read, server_write),
        root.clone(),
        // No idle timer: only the duplicate-id refusal can close this.
        None,
        None,
        Some(OBLIGATION_TTL),
    );
    let obligations = transport.in_flight_handle();
    let running = rmcp::service::serve_directly_with_ct(probe, transport, None, root.clone());
    let (_client_read, mut client_write) = tokio::io::split(client_io);

    let request = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\
                        \"params\":{\"name\":\"probe\",\"arguments\":{}}}\n";
    client_write
        .write_all(request)
        .await
        .expect("write the first request");
    tokio::time::timeout(Duration::from_secs(2), first_admitted.notified())
        .await
        .expect("rmcp never admitted the first request");

    tokio::time::sleep(OBLIGATION_TTL * 3).await;

    // Premise, asserted rather than assumed: the entry is still in the
    // queue AND the transport's own freshness rule already calls it stale.
    // Both halves matter — a test that only slept would pass against a
    // build that pruned the entry, since an empty queue also admits the
    // reuse without closing, which is the very defect under test.
    {
        let queue = obligations.lock().expect("obligation queue poisoned");
        let (id, admitted_at) = queue
            .front()
            .expect(
                "fixture premise: the unanswered obligation must still be queued when the \
                     reused id arrives; an empty queue means something pruned it and this test \
                     can no longer distinguish the orderings",
            )
            .clone();
        assert_eq!(
            id,
            rmcp::model::NumberOrString::Number(1),
            "fixture premise: the queued entry must be the request under test"
        );
        assert!(
            admitted_at.elapsed() >= OBLIGATION_TTL,
            "fixture premise: the entry must already be STALE by the transport's own rule, \
                 otherwise this is just the plain duplicate case"
        );
    }
    assert!(
        !root.is_cancelled(),
        "fixture premise: a single outstanding request must not have closed the session"
    );

    client_write
        .write_all(request)
        .await
        .expect("write the request reusing the stale id");

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !root.is_cancelled() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        root.is_cancelled(),
        "reusing an id whose obligation is stale but whose handler is still alive must close \
             the session. Admitting it lets the first response retire the wrong entry, which \
             leaves a live handler untracked and the idle close free to fire under it."
    );

    drop(client_write);
    let _ = tokio::time::timeout(Duration::from_secs(2), running.waiting()).await;
}

/// Regression: the obligation queue must not grow without bound.
///
/// Retirement is keyed by request id, so it only ever removes the entry
/// whose response was actually written. A request that never produces one
/// — a handler that panics, a request the peer cancels — leaves its entry
/// behind. Under the earlier oldest-first retirement any completion
/// removed *some* entry, so the queue tracked admissions minus
/// completions; keying by id means an unanswered request now holds its
/// slot for the life of the session unless something drops it.
///
/// Nothing here waits on the idle timer: it is disabled, so the only thing
/// that can bound this queue is the staleness drop.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn stdio_obligation_queue_drops_entries_past_their_ttl() {
    use rmcp::transport::async_rw::AsyncRwTransport;
    use tokio::io::AsyncWriteExt;

    const OBLIGATION_TTL: Duration = Duration::from_millis(100);
    // Longer than the TTL, so every earlier admission is already stale by
    // the time the next one arrives and the queue can never hold two.
    const ADMISSION_GAP: Duration = Duration::from_millis(150);
    const ADMISSIONS: u32 = 4;

    let probe = SlowProbeServer {
        started: Arc::new(tokio::sync::Notify::new()),
        // Never answers, so nothing is ever retired by id.
        delay: Duration::from_secs(3600),
    };
    let root = tokio_util::sync::CancellationToken::new();
    let (server_io, mut client_io) = tokio::io::duplex(16 * 1024);
    let (server_read, server_write) = tokio::io::split(server_io);
    let transport = crate::transport::CancelOnEofTransport::with_idle_timeout(
        AsyncRwTransport::new_server(server_read, server_write),
        root.clone(),
        // Idle reaping off: this test is about the queue, not the timer.
        None,
        None,
        Some(OBLIGATION_TTL),
    );
    let obligations = transport.in_flight_handle();
    let running = rmcp::service::serve_directly_with_ct(probe, transport, None, root.clone());

    for id in 1..=ADMISSIONS {
        client_io
            .write_all(
                format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"tools/call\",\
                         \"params\":{{\"name\":\"probe\",\"arguments\":{{}}}}}}\n"
                )
                .as_bytes(),
            )
            .await
            .expect("write request");
        tokio::time::sleep(ADMISSION_GAP).await;
    }

    let outstanding = obligations.lock().expect("obligation queue poisoned").len();
    assert!(
        outstanding <= 1,
        "an unanswered request must not hold its obligation slot past the TTL: after \
             {ADMISSIONS} admissions spaced {}ms apart against a {}ms TTL the queue holds \
             {outstanding} entries. Without the staleness drop it would hold {ADMISSIONS}, one \
             per admission, and would keep growing for as long as the session lives.",
        ADMISSION_GAP.as_millis(),
        OBLIGATION_TTL.as_millis(),
    );

    drop(client_io);
    let _ = tokio::time::timeout(Duration::from_secs(2), running.waiting()).await;
}

/// Regression: a peer that keeps sending requests without consuming
/// responses cannot make the transport's outstanding state grow without
/// limit. The third request is rejected before rmcp can spawn its handler.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn stdio_closes_when_outstanding_request_limit_is_reached() {
    use rmcp::transport::async_rw::AsyncRwTransport;
    use tokio::io::AsyncWriteExt;

    const MAX_OUTSTANDING: usize = 2;
    let probe = SlowProbeServer {
        started: Arc::new(tokio::sync::Notify::new()),
        delay: Duration::from_secs(3600),
    };
    let root = tokio_util::sync::CancellationToken::new();
    let (server_io, mut client_io) = tokio::io::duplex(16 * 1024);
    let (server_read, server_write) = tokio::io::split(server_io);
    let transport = crate::transport::CancelOnEofTransport::with_idle_timeout_and_max_outstanding(
        AsyncRwTransport::new_server(server_read, server_write),
        root.clone(),
        None,
        None,
        Some(Duration::from_secs(3600)),
        MAX_OUTSTANDING,
    );
    let obligations = transport.in_flight_handle();
    let running = rmcp::service::serve_directly_with_ct(probe, transport, None, root.clone());

    for id in 1..=MAX_OUTSTANDING + 1 {
        let request = format!(
            "{}\n",
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {"name": "probe", "arguments": {}}
            })
        );
        client_io
            .write_all(request.as_bytes())
            .await
            .expect("write request");
    }

    tokio::time::timeout(Duration::from_secs(2), root.cancelled())
        .await
        .expect("the transport must close after reaching its admission limit");
    assert_eq!(
        obligations.lock().expect("obligation queue poisoned").len(),
        MAX_OUTSTANDING,
        "the rejected request must not enter the outstanding tracker"
    );

    drop(client_io);
    let _ = tokio::time::timeout(Duration::from_secs(2), running.waiting()).await;
}

/// Parks forever on the first call and answers every later one promptly,
/// so a test can produce out-of-order response completion: the request
/// admitted first stays outstanding while a later one completes.
#[derive(Clone)]
struct OutOfOrderProbeServer {
    first_admitted: Arc<tokio::sync::Notify>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl rmcp::ServerHandler for OutOfOrderProbeServer {
    fn call_tool(
        &self,
        _request: rmcp::model::CallToolRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> impl Future<Output = Result<rmcp::model::CallToolResult, McpError>> + Send + '_ {
        let first_admitted = self.first_admitted.clone();
        let calls = self.calls.clone();
        async move {
            if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                first_admitted.notify_one();
                // Never answers. This is the older obligation, and it is
                // the one whose TTL must govern when the session closes.
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
            Ok(rmcp::model::CallToolResult::success(Vec::new()))
        }
    }
}

/// Distinguishes handler behavior by tool name: `"slow"` blocks for
/// `slow_delay` (keeping `in_flight` above zero across several idle
/// windows), `"quick"` (or anything else) completes immediately — used
/// to admit a second request while the first is still running.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn rmcp_cancellation_token_reaches_request_read_scope() {
    let token = tokio_util::sync::CancellationToken::new();
    let token_for_scope = token.clone();
    let observed = tokio::spawn(async move {
        scope_mcp_request_read_cancellation(token_for_scope, async {
            khive_storage::wait_for_request_read_cancellation().await;
            true
        })
        .await
    });

    token.cancel();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(250), observed)
            .await
            .expect("rmcp cancellation never reached the read scope")
            .expect("scope task panicked")
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn already_cancelled_rmcp_token_is_visible_without_yielding() {
    let token = tokio_util::sync::CancellationToken::new();
    token.cancel();

    let observed = scope_mcp_request_read_cancellation(token, async {
        khive_storage::request_read_is_cancelled()
    })
    .await;

    assert!(
        observed,
        "a synchronously-ready request raced past a pre-cancelled rmcp context"
    );
}

#[tokio::test]
#[serial_test::serial]
#[serial_test::serial(config_ledger)]
async fn request_tool_path_honors_an_already_cancelled_rmcp_token() {
    std::env::set_var("KHIVE_NO_DAEMON", "1");
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        default_namespace: khive_runtime::Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::default()
    })
    .expect("in-memory runtime");
    let server = KhiveMcpServer::new(runtime).expect("server builds with kg");
    let cancellation = tokio_util::sync::CancellationToken::new();
    cancellation.cancel();

    let response = server
        .request(
            Parameters(RequestParams {
                ops: "stats()".to_string(),
                ..Default::default()
            }),
            cancellation,
        )
        .await;
    std::env::remove_var("KHIVE_NO_DAEMON");

    match response {
        Err(error) => {
            let rendered = format!("{error:?}").to_ascii_lowercase();
            assert!(
                rendered.contains("timeout") || rendered.contains("cancel"),
                "cancelled request returned an unrelated RPC error: {rendered}"
            );
        }
        Ok(payload) => {
            let parsed: Value = serde_json::from_str(&payload).expect("JSON response envelope");
            assert!(
                parsed["summary"]["failed"].as_u64().unwrap_or(0) > 0,
                "the actual request tool path ignored its cancelled token: {parsed}"
            );
        }
    }
}

// ── KHIVE_BRIDGE_RESPONSE_DEADLINE_SECS=0 must not disable the bound ──

/// The response-delivery deadline used to treat `0` as "disable
/// the bound", mirroring `KHIVE_BRIDGE_IDLE_TIMEOUT_SECS=0`. Unlike the
/// idle timeout, an unbounded response-delivery deadline restores the
/// exact defect this deadline exists to close (a peer that admits a
/// request and stops reading pins the bridge's response write forever)
/// — so `0` is now a hard startup error instead of a supported opt-out.
#[test]
#[serial_test::serial]
fn response_deadline_from_env_rejects_zero() {
    std::env::set_var("KHIVE_BRIDGE_RESPONSE_DEADLINE_SECS", "0");
    let result = stdio_bridge_response_deadline_from_env();
    std::env::remove_var("KHIVE_BRIDGE_RESPONSE_DEADLINE_SECS");

    let error = result.expect_err(
        "KHIVE_BRIDGE_RESPONSE_DEADLINE_SECS=0 must be rejected, not silently accepted \
             as \"disable the bound\"",
    );
    let rendered = error.to_string();
    assert!(
        rendered.contains("KHIVE_BRIDGE_RESPONSE_DEADLINE_SECS"),
        "error must name the offending variable: {rendered}"
    );
    assert!(
        rendered.contains("=0"),
        "error must name the rejected value: {rendered}"
    );
    assert!(
        rendered.contains("1..=u64::MAX"),
        "error must state the accepted range: {rendered}"
    );
}

/// The response-delivery deadline had no test that ever let it elapse:
/// the only coverage rejected `0` at startup, which exercises the parser
/// and not the bound. A bound whose expiry is never observed is a claim,
/// so this drives a real write against a peer that has stopped reading.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn response_write_past_its_deadline_is_abandoned_and_closes_the_session() {
    use rmcp::transport::async_rw::AsyncRwTransport;
    use rmcp::transport::Transport;

    // A 16-byte pipe that nobody reads. `_client_io` is held, not dropped,
    // so this is a peer that is present and simply not reading — the case
    // the deadline exists for. Dropping it would produce a broken pipe
    // instead, which is a different failure and already handled.
    let (server_io, _client_io) = tokio::io::duplex(16);
    let (read, write) = tokio::io::split(server_io);
    let root = tokio_util::sync::CancellationToken::new();
    let mut transport = crate::transport::CancelOnEofTransport::with_idle_timeout(
        AsyncRwTransport::new_server(read, write),
        root.clone(),
        None,
        Some(Duration::from_millis(50)),
        None,
    );

    // An error response is a response for this purpose: `send` bounds
    // `Response` and `Error` alike, because either one can leave the write
    // pinned.
    let message = rmcp::model::JsonRpcMessage::Error(rmcp::model::JsonRpcError {
        jsonrpc: rmcp::model::JsonRpcVersion2_0,
        id: Some(rmcp::model::RequestId::Number(1)),
        error: rmcp::model::ErrorData::internal_error("x".repeat(4096), None),
    });

    let error = tokio::time::timeout(Duration::from_secs(2), transport.send(message))
        .await
        .expect("the deadline must resolve the write; it hung instead")
        .expect_err("a write that outlived its deadline must not report success");

    assert!(
        error.to_string().contains("deadline"),
        "the error must name the deadline that abandoned the write: {error}"
    );
    assert!(
        root.is_cancelled(),
        "a peer that stops reading must close the session, not just fail one write"
    );
}

/// Discriminating arm for the test above. Without this, that test would
/// also pass against a deadline that fired on every response regardless of
/// whether the peer was reading.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn response_write_inside_its_deadline_succeeds_and_leaves_the_session_open() {
    use rmcp::transport::async_rw::AsyncRwTransport;
    use rmcp::transport::Transport;
    use tokio::io::AsyncReadExt;

    let (server_io, mut client_io) = tokio::io::duplex(16 * 1024);
    let (read, write) = tokio::io::split(server_io);
    let root = tokio_util::sync::CancellationToken::new();
    let mut transport = crate::transport::CancelOnEofTransport::with_idle_timeout(
        AsyncRwTransport::new_server(read, write),
        root.clone(),
        None,
        Some(Duration::from_secs(30)),
        None,
    );

    // This peer reads.
    let reader = tokio::spawn(async move {
        let mut buf = vec![0u8; 8192];
        let _ = client_io.read(&mut buf).await;
        buf
    });

    let message = rmcp::model::JsonRpcMessage::Error(rmcp::model::JsonRpcError {
        jsonrpc: rmcp::model::JsonRpcVersion2_0,
        id: Some(rmcp::model::RequestId::Number(1)),
        error: rmcp::model::ErrorData::internal_error("read by the peer", None),
    });

    tokio::time::timeout(Duration::from_secs(2), transport.send(message))
        .await
        .expect("a write to a reading peer must not hit the 2s test bound")
        .expect("a write to a reading peer must succeed");

    assert!(
        !root.is_cancelled(),
        "a response delivered inside its deadline must not close the session"
    );
    let _ = reader.await;
}

/// The deadline bounds a write left PENDING. It says nothing about a write
/// that fails immediately, which is what a half-closed peer produces: one
/// that closes the side it reads from while keeping the side it writes to
/// open. rmcp does not close the session for us there — the response-send
/// task logs the error and returns (`rmcp-1.8.0` `src/service.rs:1105-1112`)
/// — the receive loop stays pending on the still-open read side, and idle
/// reaping is off by default, so before this the session outlived the
/// peer's ability to receive anything from it.
///
/// Two independent pipes, because a single `duplex` cannot be half-closed:
/// dropping either end closes both directions, and `tokio::io::split`
/// keeps the stream alive until both halves drop. Separate pipes for the
/// read source and the write sink are what let the peer close exactly one.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn response_write_that_fails_fast_closes_the_session() {
    use rmcp::transport::async_rw::AsyncRwTransport;
    use rmcp::transport::Transport;

    // peer -> server: held open and silent, so the receive side would stay
    // pending forever. This is what makes the missing cancel a leak rather
    // than a race with EOF.
    let (server_read, _peer_write_side) = tokio::io::duplex(1024);
    // server -> peer: the peer has closed the side it reads from, so every
    // write fails with BrokenPipe.
    let (server_write, peer_read_side) = tokio::io::duplex(1024);
    drop(peer_read_side);

    let root = tokio_util::sync::CancellationToken::new();
    let mut transport = crate::transport::CancelOnEofTransport::with_idle_timeout(
        AsyncRwTransport::new_server(server_read, server_write),
        root.clone(),
        None,
        // Long enough that the deadline cannot be what resolves this
        // write. If the fix were the deadline rather than the error path,
        // the 2s bound below would trip instead of the assertion.
        Some(Duration::from_secs(30)),
        None,
    );

    let message = rmcp::model::JsonRpcMessage::Error(rmcp::model::JsonRpcError {
        jsonrpc: rmcp::model::JsonRpcVersion2_0,
        id: Some(rmcp::model::RequestId::Number(1)),
        error: rmcp::model::ErrorData::internal_error("peer closed its read side", None),
    });

    let error = tokio::time::timeout(Duration::from_secs(2), transport.send(message))
        .await
        .expect("a write to a closed pipe must fail fast, not wait out its deadline")
        .expect_err("a write to a closed pipe must not report success");

    assert!(
        !error.to_string().contains("deadline"),
        "this must exercise the error path, not the deadline path: {error}"
    );
    assert!(
        root.is_cancelled(),
        "a response that could not be written must close the session; rmcp only logs it"
    );
}

/// A failed NOTIFICATION write closes the session too, and this test used
/// to assert the opposite. The rule was scoped to responses on the ground
/// that rmcp carried its own accounting for server-initiated messages. It
/// does carry accounting and the accounting does not close anything: a
/// failed notification send delivers `ServiceError::TransportSend` to the
/// notification's own responder (`rmcp-1.8.0` `src/service.rs:1074-1093`)
/// and a failed request send does the same to the caller's responder
/// (`:1066-1073`), while the serve loop's only exits are receive EOF, token
/// cancellation, and a send-task join error (`:1028-1062`). So the same
/// broken writer strands the session whatever class of message hit it.
///
/// The discriminating arm is now
/// `notification_write_to_a_reading_peer_leaves_the_session_open`: without
/// it, this test would pass against a transport that cancelled on every
/// write, successful ones included.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn notification_write_that_fails_fast_also_closes_the_session() {
    use rmcp::transport::async_rw::AsyncRwTransport;
    use rmcp::transport::Transport;

    let (server_read, _peer_write_side) = tokio::io::duplex(1024);
    let (server_write, peer_read_side) = tokio::io::duplex(1024);
    drop(peer_read_side);

    let root = tokio_util::sync::CancellationToken::new();
    let mut transport = crate::transport::CancelOnEofTransport::with_idle_timeout(
        AsyncRwTransport::new_server(server_read, server_write),
        root.clone(),
        None,
        Some(Duration::from_secs(30)),
        None,
    );

    let message = rmcp::model::JsonRpcMessage::Notification(rmcp::model::JsonRpcNotification {
        jsonrpc: rmcp::model::JsonRpcVersion2_0,
        notification: rmcp::model::ServerNotification::ProgressNotification(
            rmcp::model::Notification::new(rmcp::model::ProgressNotificationParam {
                progress_token: rmcp::model::ProgressToken(rmcp::model::NumberOrString::Number(1)),
                progress: 1.0,
                total: None,
                message: None,
            }),
        ),
    });

    let error = tokio::time::timeout(Duration::from_secs(2), transport.send(message))
        .await
        .expect("a write to a closed pipe must fail fast, not wait out its deadline")
        .expect_err("a write to a closed pipe must not report success");

    assert!(
        !error.to_string().contains("deadline"),
        "this must exercise the error path, not the deadline path: {error}"
    );
    assert!(
        root.is_cancelled(),
        "a notification that could not be written must close the session; rmcp hands the \
             error to the notification's own responder and never breaks the serve loop"
    );
}

/// Discriminating arm for the two fail-fast tests above. Without it, both
/// would pass against a transport that cancelled on every write rather than
/// on every FAILED write, which is a far worse rule than either.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn notification_write_to_a_reading_peer_leaves_the_session_open() {
    use rmcp::transport::async_rw::AsyncRwTransport;
    use rmcp::transport::Transport;
    use tokio::io::AsyncReadExt;

    let (server_read, _peer_write_side) = tokio::io::duplex(1024);
    let (server_write, mut peer_read_side) = tokio::io::duplex(16 * 1024);

    let root = tokio_util::sync::CancellationToken::new();
    let mut transport = crate::transport::CancelOnEofTransport::with_idle_timeout(
        AsyncRwTransport::new_server(server_read, server_write),
        root.clone(),
        None,
        Some(Duration::from_secs(30)),
        None,
    );

    // This peer reads.
    let reader = tokio::spawn(async move {
        let mut buf = vec![0u8; 8192];
        let _ = peer_read_side.read(&mut buf).await;
        buf
    });

    let message = rmcp::model::JsonRpcMessage::Notification(rmcp::model::JsonRpcNotification {
        jsonrpc: rmcp::model::JsonRpcVersion2_0,
        notification: rmcp::model::ServerNotification::ProgressNotification(
            rmcp::model::Notification::new(rmcp::model::ProgressNotificationParam {
                progress_token: rmcp::model::ProgressToken(rmcp::model::NumberOrString::Number(1)),
                progress: 1.0,
                total: None,
                message: None,
            }),
        ),
    });

    tokio::time::timeout(Duration::from_secs(2), transport.send(message))
        .await
        .expect("a write to a reading peer must not hit the 2s test bound")
        .expect("a write to a reading peer must succeed");

    assert!(
        !root.is_cancelled(),
        "a delivered notification must not close the session"
    );
    let _ = reader.await;
}

/// A writer whose first flush is interrupted and which works from then on.
///
/// This is the shape Tokio's blocking stdout adapter presents on EINTR: it
/// restores its idle state and puts the writer back before returning the
/// flush error (`tokio-1.52.4/src/io/blocking.rs:146-176`), and its
/// `uninterruptibly!` retry macro (`:183-192`) is not applied to that
/// branch. Writes are accepted and discarded, because what this arm asserts
/// is the session's fate, not the bytes.
struct InterruptOnceWriter {
    interrupted_yet: bool,
}

impl tokio::io::AsyncWrite for InterruptOnceWriter {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::task::Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.interrupted_yet {
            return std::task::Poll::Ready(Ok(()));
        }
        self.interrupted_yet = true;
        std::task::Poll::Ready(Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "flush interrupted by a signal",
        )))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

/// The exception to "every failed outbound write closes the session", and
/// it is narrower than the writer's health alone.
///
/// An interrupted flush leaves the writer usable, so closing on it would
/// trade a lost message for a lost session. The second send is what makes
/// that claim rather than assuming it: if the transport were dead the
/// assertion could not distinguish a correct decision from a lucky one.
///
/// This arm uses a NOTIFICATION because the exception is scoped to the
/// classes whose loss someone can observe. See
/// `an_interrupted_response_still_closes_the_session` for the other side of
/// that boundary, which is the arm that would go red if the scope were
/// dropped.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn an_interrupted_write_leaves_the_session_open_and_the_writer_usable() {
    use rmcp::transport::async_rw::AsyncRwTransport;
    use rmcp::transport::Transport;

    let (server_read, _peer_write_side) = tokio::io::duplex(1024);
    let writer = InterruptOnceWriter {
        interrupted_yet: false,
    };

    let root = tokio_util::sync::CancellationToken::new();
    let mut transport = crate::transport::CancelOnEofTransport::with_idle_timeout(
        AsyncRwTransport::new_server(server_read, writer),
        root.clone(),
        None,
        Some(Duration::from_secs(30)),
        None,
    );

    let notification = || {
        rmcp::model::JsonRpcMessage::Notification(rmcp::model::JsonRpcNotification {
            jsonrpc: rmcp::model::JsonRpcVersion2_0,
            notification: rmcp::model::ServerNotification::ProgressNotification(
                rmcp::model::Notification::new(rmcp::model::ProgressNotificationParam {
                    progress_token: rmcp::model::ProgressToken(
                        rmcp::model::NumberOrString::Number(1),
                    ),
                    progress: 1.0,
                    total: None,
                    message: None,
                }),
            ),
        })
    };

    let error = tokio::time::timeout(Duration::from_secs(2), transport.send(notification()))
        .await
        .expect("an interrupted flush must resolve, not hang")
        .expect_err("an interrupted flush must be reported as an error");
    assert_eq!(
        error.kind(),
        std::io::ErrorKind::Interrupted,
        "this arm must exercise the interrupted class, not some other failure: {error}"
    );
    assert!(
        !root.is_cancelled(),
        "an interrupted write must not close the session; the writer is still usable"
    );

    tokio::time::timeout(Duration::from_secs(2), transport.send(notification()))
        .await
        .expect("the second write must resolve, not hang")
        .expect("the writer is usable after an interrupted flush, so the next write succeeds");
    assert!(
        !root.is_cancelled(),
        "a successful write after an interrupted one must leave the session open"
    );
}

/// The boundary of that exception: a usable writer is not enough when the
/// message was a RESPONSE.
///
/// The interrupted flush leaves the writer able to carry the next message,
/// exactly as in the notification arm, so this test differs from that one
/// in the message class and nothing else. The outcome differs because rmcp
/// treats the classes differently. A failed notification or server-initiated
/// request send reaches a local responder (`rmcp-1.8.0`
/// `src/service.rs:1074-1093` and `:1066-1073`), so something in the process
/// learns the message was lost. A failed response send is only logged
/// (`:1095-1112`): nothing goes to the peer, no local caller is waiting, and
/// the serve loop keeps running. The client that asked the question would
/// wait on an answer that is not coming and could not tell that from a slow
/// one. Closing is what turns that into an EOF it can act on.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn an_interrupted_response_still_closes_the_session() {
    use rmcp::transport::async_rw::AsyncRwTransport;
    use rmcp::transport::Transport;

    let (server_read, _peer_write_side) = tokio::io::duplex(1024);
    let writer = InterruptOnceWriter {
        interrupted_yet: false,
    };

    let root = tokio_util::sync::CancellationToken::new();
    let mut transport = crate::transport::CancelOnEofTransport::with_idle_timeout(
        AsyncRwTransport::new_server(server_read, writer),
        root.clone(),
        None,
        // Long enough that the deadline cannot be what resolves this write,
        // so a pass here cannot come from the timeout path.
        Some(Duration::from_secs(30)),
        None,
    );

    let response = rmcp::model::JsonRpcMessage::Error(rmcp::model::JsonRpcError {
        jsonrpc: rmcp::model::JsonRpcVersion2_0,
        id: Some(rmcp::model::RequestId::Number(1)),
        error: rmcp::model::ErrorData::internal_error("answering a request", None),
    });

    let error = tokio::time::timeout(Duration::from_secs(2), transport.send(response))
        .await
        .expect("an interrupted flush must resolve, not hang")
        .expect_err("an interrupted flush must be reported as an error");
    assert_eq!(
        error.kind(),
        std::io::ErrorKind::Interrupted,
        "this arm must exercise the interrupted class, not some other failure: {error}"
    );
    assert!(
        root.is_cancelled(),
        "an interrupted RESPONSE must still close the session: the writer's health does not \
             help a peer that is waiting on an answer rmcp will only log the loss of"
    );
}
