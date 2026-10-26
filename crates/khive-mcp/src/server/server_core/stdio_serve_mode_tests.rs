// Copyright 2026 Haiyang Li. Licensed under Apache-2.0.
//
use std::time::Duration;

use rmcp::model::{
    ClientCapabilities, ClientJsonRpcMessage, Implementation, InitializeRequestParams,
};
use rmcp::transport::{async_rw::AsyncRwTransport, Transport};
use rmcp::RoleServer;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};

use super::{stdio_serve_mode_for, StdioServeMode};

const TEST_TIMEOUT: Duration = Duration::from_secs(5);
type ServerWire = AsyncRwTransport<RoleServer, ReadHalf<DuplexStream>, WriteHalf<DuplexStream>>;

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
        assert_ne!(size, 0, "transport closed before replying");
        serde_json::from_str(&line).expect("server must emit complete JSON-RPC")
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
        Implementation::new("stdio-mode-test", "1"),
    );
    json!({"jsonrpc": "2.0", "id": id, "method": "initialize", "params": params})
}

fn assert_same_message(actual: ClientJsonRpcMessage, expected: Value) {
    let expected: ClientJsonRpcMessage =
        serde_json::from_value(expected).expect("SDK must parse the test input");
    assert_eq!(
        serde_json::to_value(actual).expect("serialize received SDK message"),
        serde_json::to_value(expected).expect("serialize expected SDK message")
    );
}

#[tokio::test]
async fn resumed_mode_forwards_request_without_initialization() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let (wire, mut client) = wire_pair();
        let mode = stdio_serve_mode_for(Some(1));
        assert_eq!(mode, StdioServeMode::Resumed);
        let mut transport = mode.wrap_transport(wire);
        let probe = json!({"jsonrpc": "2.0", "id": "resumed-probe", "method": "server/discover"});
        client.send(probe.clone()).await;
        // If this mode accidentally enables filtering, receive returns this
        // sentinel rather than the original request and the equality fails.
        client
            .send(json!({"jsonrpc": "2.0", "id": 2, "method": "ping"}))
            .await;
        assert_same_message(transport.receive().await.expect("resumed request"), probe);
    })
    .await
    .expect("resumed mode must forward the first request without waiting for initialize");
}

#[tokio::test]
async fn handshake_mode_refuses_request_before_initialization() {
    tokio::time::timeout(TEST_TIMEOUT, async {
        let (wire, mut client) = wire_pair();
        let mode = stdio_serve_mode_for(None);
        assert_eq!(mode, StdioServeMode::Handshake);
        let mut transport = mode.wrap_transport(wire);
        client
            .send(json!({"jsonrpc": "2.0", "id": 7, "method": "server/discover"}))
            .await;
        let init = initialize(json!(8));
        client.send(init.clone()).await;
        assert_same_message(transport.receive().await.expect("initialize"), init);

        let refusal = client.receive().await;
        assert_eq!(refusal["jsonrpc"], "2.0");
        assert_eq!(refusal["id"], 7);
        assert_eq!(refusal["error"]["code"], -32601);
        assert_eq!(
            refusal["error"]["message"],
            "Method not found before initialize: server/discover"
        );
        assert!(refusal.get("result").is_none());

        let after = json!({"jsonrpc": "2.0", "id": "after", "method": "server/discover"});
        client.send(after.clone()).await;
        assert_same_message(
            transport.receive().await.expect("post-initialize request"),
            after,
        );
    })
    .await
    .expect("handshake mode must refuse the probe and keep reading");
}
