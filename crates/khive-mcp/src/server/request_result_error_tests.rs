use super::{mark_all_failed_request_result, KhiveMcpServer};
use khive_runtime::{KhiveRuntime, RuntimeConfig};
use rmcp::model::{CallToolResult, Content};
use std::time::Duration;

fn result_with_summary(succeeded: u64, failed: u64, aborted: u64) -> CallToolResult {
    CallToolResult::success(vec![Content::text(format!(
            "{{\"results\":[],\"summary\":{{\"succeeded\":{succeeded},\"failed\":{failed},\"aborted\":{aborted}}},\"status\":\"partial\"}}"
        ))])
}

#[test]
fn all_failed_request_sets_mcp_is_error_without_changing_envelope() {
    let mut result = result_with_summary(0, 1, 2);
    let original = result.content.clone();
    mark_all_failed_request_result(&mut result);
    assert_eq!(result.is_error, Some(true));
    assert_eq!(result.content, original);
}

#[test]
fn mixed_request_does_not_set_mcp_is_error() {
    let mut result = result_with_summary(1, 1, 0);
    mark_all_failed_request_result(&mut result);
    assert_eq!(result.is_error, Some(false));
}

#[test]
fn plan_response_without_summary_keeps_its_original_mcp_status() {
    let mut result = CallToolResult::success(vec![Content::text("{\"parsed\":false}")]);
    mark_all_failed_request_result(&mut result);
    assert_eq!(result.is_error, Some(false));
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn all_failed_request_is_error_on_tools_call_wire() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::default()
    })
    .expect("in-memory runtime");
    let server = KhiveMcpServer::new(runtime).expect("server with kg pack");
    let root = tokio_util::sync::CancellationToken::new();
    let (server_io, client_io) = tokio::io::duplex(16 * 1024);
    let (server_read, server_write) = tokio::io::split(server_io);
    let transport = crate::transport::CancelOnEofTransport::with_idle_timeout(
        rmcp::transport::async_rw::AsyncRwTransport::new_server(server_read, server_write),
        root.clone(),
        None,
        Some(Duration::from_secs(2)),
        None,
    );
    let running = rmcp::service::serve_directly_with_ct(server, transport, None, root.clone());
    let (client_read, mut client_write) = tokio::io::split(client_io);
    let mut client_read = tokio::io::BufReader::new(client_read);
    client_write
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"request\",\"arguments\":{\"ops\":\"not_loaded()\"}}}\n",
            )
            .await
            .expect("send request tool call");
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(2), client_read.read_line(&mut line))
        .await
        .expect("request tool response deadline")
        .expect("read request tool response");
    let wire: serde_json::Value = serde_json::from_str(&line).expect("JSON-RPC response");
    assert_eq!(wire["result"]["isError"], true, "response: {wire}");
    let envelope: serde_json::Value = serde_json::from_str(
        wire["result"]["content"][0]["text"]
            .as_str()
            .expect("request result text"),
    )
    .expect("unchanged request envelope");
    assert_eq!(envelope["summary"]["succeeded"], 0);
    assert_eq!(envelope["summary"]["failed"], 1);

    client_write
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"request\",\"arguments\":{\"ops\":\"[stats(), not_loaded()]\"}}}\n",
            )
            .await
            .expect("send mixed request tool call");
    line.clear();
    tokio::time::timeout(Duration::from_secs(2), client_read.read_line(&mut line))
        .await
        .expect("mixed request response deadline")
        .expect("read mixed request response");
    let wire: serde_json::Value = serde_json::from_str(&line).expect("mixed JSON-RPC response");
    assert_eq!(wire["result"]["isError"], false, "response: {wire}");
    let envelope: serde_json::Value = serde_json::from_str(
        wire["result"]["content"][0]["text"]
            .as_str()
            .expect("mixed request result text"),
    )
    .expect("mixed request envelope");
    assert_eq!(envelope["status"], "partial");
    assert_eq!(envelope["summary"]["succeeded"], 1);
    assert_eq!(envelope["summary"]["failed"], 1);
    root.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), running.waiting()).await;
}
