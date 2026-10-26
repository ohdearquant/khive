// Copyright 2026 Haiyang Li. Licensed under Apache-2.0.
//
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::model::{
    ClientCapabilities, ClientJsonRpcMessage, ClientRequest, Implementation,
    InitializeRequestParams,
};
use rmcp::service::serve_server;
use rmcp::transport::{async_rw::AsyncRwTransport, Transport};
use rmcp::{RoleServer, ServerHandler};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};
use tokio_util::sync::CancellationToken;

use super::{CancelOnEofTransport, OutstandingRequests, PreInitializeRequestTransport};

const TEST_TIMEOUT: Duration = Duration::from_secs(5);
type ServerWire = AsyncRwTransport<RoleServer, ReadHalf<DuplexStream>, WriteHalf<DuplexStream>>;
type Tracker = Arc<Mutex<OutstandingRequests>>;

struct TestServer;

impl ServerHandler for TestServer {}

struct WirePeer {
    read: BufReader<ReadHalf<DuplexStream>>,
    write: WriteHalf<DuplexStream>,
}

impl WirePeer {
    async fn send(&mut self, message: Value) {
        let mut line = serde_json::to_vec(&message).expect("serialize client message");
        line.push(b'\n');
        self.write
            .write_all(&line)
            .await
            .expect("write client message");
    }

    async fn receive(&mut self) -> Value {
        let mut line = String::new();
        let size = self
            .read
            .read_line(&mut line)
            .await
            .expect("read server message");
        assert_ne!(size, 0, "server closed before replying");
        serde_json::from_str(&line).expect("server must emit a complete JSON-RPC message")
    }
}

fn wire_pair() -> (ServerWire, WirePeer) {
    let (client, server) = tokio::io::duplex(8192);
    let (server_read, server_write) = tokio::io::split(server);
    let (client_read, client_write) = tokio::io::split(client);
    (
        AsyncRwTransport::new_server(server_read, server_write),
        WirePeer {
            read: BufReader::new(client_read),
            write: client_write,
        },
    )
}

fn initialize(id: Value) -> Value {
    let params = InitializeRequestParams::new(
        ClientCapabilities::default(),
        Implementation::new("pre-initialize-test", "1"),
    );
    json!({"jsonrpc": "2.0", "id": id, "method": "initialize", "params": params})
}

fn request(id: Value, method: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method})
}

fn assert_method_not_found(reply: &Value, id: Value, method: &str) {
    assert_eq!(reply["jsonrpc"], "2.0");
    assert_eq!(reply["id"], id);
    assert_eq!(reply["error"]["code"], -32601);
    assert_eq!(
        reply["error"]["message"],
        format!("Method not found before initialize: {method}")
    );
    assert!(reply.get("result").is_none());
}

fn assert_initialized(reply: &Value, id: Value) {
    assert_eq!(reply["id"], id);
    assert!(reply.get("error").is_none());
    assert!(reply["result"]["protocolVersion"].is_string());
    assert!(reply["result"]["serverInfo"].is_object());
}

fn assert_ping(reply: &Value, id: Value) {
    assert_eq!(reply, &json!({"jsonrpc": "2.0", "id": id, "result": {}}));
}

fn assert_retired(tracker: &Tracker) {
    assert_eq!(tracker.lock().expect("tracker lock").len(), 0);
}

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(TEST_TIMEOUT, future)
        .await
        .expect("in-memory handshake must finish without waiting for a peer timeout")
}

async fn fresh_session<F, Fut>(scenario: F)
where
    F: FnOnce(WirePeer, Tracker) -> Fut,
    Fut: Future<Output = ()>,
{
    let (wire, client) = wire_pair();
    let root = CancellationToken::new();
    // A rejection must retire its actual ID before another request is read.
    // Capacity one makes a leaked pre-init obligation break the next request.
    let inner = CancelOnEofTransport::with_idle_timeout_and_max_outstanding(
        wire,
        root,
        None,
        Some(TEST_TIMEOUT),
        None,
        1,
    );
    let tracker = inner.in_flight_handle();
    let transport = PreInitializeRequestTransport::new(inner);
    let server = async move {
        let service = serve_server(TestServer, transport)
            .await
            .unwrap_or_else(|error| panic!("real SDK initialization failed: {error:?}"));
        service
            .waiting()
            .await
            .expect("SDK service task must finish");
    };
    // Poll the real serve call alongside the peer, so a bypass reports
    // ExpectedInitializeRequest directly instead of only timing out a read.
    bounded(async move {
        tokio::join!(server, scenario(client, tracker));
    })
    .await;
}

#[tokio::test]
async fn discover_before_initialize_replies_and_keeps_reading() {
    fresh_session(|mut client, tracker| async move {
        client.send(request(json!(41), "server/discover")).await;
        assert_method_not_found(&client.receive().await, json!(41), "server/discover");
        assert_retired(&tracker);

        client.send(initialize(json!(42))).await;
        assert_initialized(&client.receive().await, json!(42));
        client.send(request(json!(43), "ping")).await;
        assert_ping(&client.receive().await, json!(43));
    })
    .await;
}

#[tokio::test]
async fn two_pre_initialize_requests_keep_their_ids_and_escaped_methods() {
    fresh_session(|mut client, tracker| async move {
        for (id, method) in [
            (json!("discover-id"), "server/discover"),
            (json!(7), "probe/\"quoted\"\\method"),
        ] {
            client.send(request(id.clone(), method)).await;
            assert_method_not_found(&client.receive().await, id, method);
            assert_retired(&tracker);
        }
        client.send(initialize(json!("initialize-id"))).await;
        assert_initialized(&client.receive().await, json!("initialize-id"));
        client.send(request(json!(8), "ping")).await;
        assert_ping(&client.receive().await, json!(8));
    })
    .await;
}

#[tokio::test]
async fn initialize_first_preserves_handshake_and_post_initialize_routing() {
    fresh_session(|mut client, _tracker| async move {
        client.send(initialize(json!("first"))).await;
        assert_initialized(&client.receive().await, json!("first"));

        // No initialized notification is required by rmcp 2.2.0 here.
        client.send(request(json!(2), "tools/list")).await;
        let tools = client.receive().await;
        assert_eq!(tools["id"], 2);
        assert_eq!(tools["result"]["tools"], json!([]));
        assert!(tools.get("error").is_none());

        client
            .send(request(json!("after"), "server/discover"))
            .await;
        let reply = client.receive().await;
        assert_eq!(reply["id"], "after");
        assert_eq!(reply["error"]["code"], -32601);
        // This is the real default SDK custom-request handler's message,
        // rather than the adapter's pre-initialize refusal.
        assert_eq!(reply["error"]["message"], "server/discover");
        client.send(request(json!(4), "ping")).await;
        assert_ping(&client.receive().await, json!(4));
    })
    .await;
}

#[tokio::test]
async fn pre_initialize_ping_passes_without_disabling_request_filtering() {
    fresh_session(|mut client, tracker| async move {
        client.send(request(json!(1), "ping")).await;
        assert_ping(&client.receive().await, json!(1));
        assert_retired(&tracker);
        client.send(request(json!(2), "server/discover")).await;
        assert_method_not_found(&client.receive().await, json!(2), "server/discover");
        assert_retired(&tracker);
        client.send(initialize(json!(3))).await;
        assert_initialized(&client.receive().await, json!(3));
    })
    .await;
}

fn assert_same_client_message(actual: ClientJsonRpcMessage, expected: &Value) {
    let expected: ClientJsonRpcMessage =
        serde_json::from_value(expected.clone()).expect("SDK must parse the test input");
    assert_eq!(
        serde_json::to_value(actual).expect("serialize received SDK message"),
        serde_json::to_value(expected).expect("serialize expected SDK message")
    );
}

#[tokio::test]
async fn notification_passes_through_without_ending_pre_initialize_filtering() {
    bounded(async {
        let (wire, mut client) = wire_pair();
        let mut transport = PreInitializeRequestTransport::new(wire);
        let notification = json!({
            "jsonrpc": "2.0", "method": "notifications/custom", "params": {"token": 9}
        });
        client.send(notification.clone()).await;
        client.send(request(json!(5), "server/discover")).await;
        let init = initialize(json!(6));
        client.send(init.clone()).await;

        // This checks adapter passthrough only. The SDK itself rejects a
        // notification as the first message of an uninitialized session.
        assert_same_client_message(
            transport.receive().await.expect("notification"),
            &notification,
        );
        assert_same_client_message(transport.receive().await.expect("initialize"), &init);
        assert_method_not_found(&client.receive().await, json!(5), "server/discover");
    })
    .await;
}

#[tokio::test]
async fn non_request_messages_pass_through_before_initialize() {
    bounded(async {
        let (wire, mut client) = wire_pair();
        let mut transport = PreInitializeRequestTransport::new(wire);
        for message in [
            json!({"jsonrpc": "2.0", "id": "response", "result": {}}),
            json!({"jsonrpc": "2.0", "id": 4, "error": {"code": -32603, "message": "peer error"}}),
        ] {
            client.send(message.clone()).await;
            assert_same_client_message(transport.receive().await.expect("non-request"), &message);
        }
        let ping = request(json!(5), "ping");
        client.send(ping.clone()).await;
        assert_same_client_message(transport.receive().await.expect("ping"), &ping);
    })
    .await;
}

#[tokio::test]
async fn malformed_initialize_custom_request_does_not_end_filtering() {
    bounded(async {
        let (wire, mut client) = wire_pair();
        let mut transport = PreInitializeRequestTransport::new(wire);
        let malformed = json!({
            "jsonrpc": "2.0", "id": "malformed", "method": "initialize", "params": {}
        });
        let decoded: ClientJsonRpcMessage =
            serde_json::from_value(malformed.clone()).expect("SDK custom-request fallback");
        assert!(matches!(decoded, ClientJsonRpcMessage::Request(request)
            if matches!(request.request, ClientRequest::CustomRequest(_))));
        client.send(malformed).await;
        client.send(request(json!(10), "server/discover")).await;
        let init = initialize(json!(11));
        client.send(init.clone()).await;
        assert_same_client_message(transport.receive().await.expect("typed initialize"), &init);
        assert_method_not_found(&client.receive().await, json!("malformed"), "initialize");
        assert_method_not_found(&client.receive().await, json!(10), "server/discover");
    })
    .await;
}

#[tokio::test]
async fn failed_rejection_write_ends_receive_and_retires_obligation() {
    bounded(async {
        let (wire, mut client) = wire_pair();
        let root = CancellationToken::new();
        let inner = CancelOnEofTransport::with_idle_timeout_and_max_outstanding(
            wire,
            root.clone(),
            None,
            Some(TEST_TIMEOUT),
            None,
            1,
        );
        let tracker = inner.in_flight_handle();
        let mut transport = PreInitializeRequestTransport::new(inner);
        client.send(request(json!(99), "server/discover")).await;
        // Duplex retains the already-written input, but its peer can no
        // longer receive the rejection. The real AsyncRw send must fail.
        drop(client);
        assert!(transport.receive().await.is_none());
        assert!(root.is_cancelled());
        assert_retired(&tracker);
    })
    .await;
}
