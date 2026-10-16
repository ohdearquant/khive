use super::*;

include!("tests/wire_values_tests.rs");

const SENTINEL_TOKEN: &str = "SENTINEL-TOKEN-DO-NOT-LEAK-abcdef123456";

/// A local address nothing is listening on: the bound listener is
/// dropped immediately, so a subsequent connect attempt gets a prompt
/// connection-refused instead of hanging.
fn refused_port_base_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}")
}

/// A minimal one-shot raw-socket HTTP server: accepts a single
/// connection, drains the request, and writes back a fixed response
/// body with `200 OK` and a matching `Content-Length`. No HTTP
/// framework dependency needed for these connector-level tests.
fn spawn_one_shot_server(response_body: &'static str) -> String {
    spawn_one_shot_server_with_status("200 OK", response_body)
}

fn spawn_one_shot_server_with_status(
    status_line: &'static str,
    response_body: &'static str,
) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            use std::io::{Read, Write};
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let response = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn send_message_carries_retry_after_from_rate_limit() {
    let base_url = spawn_one_shot_server_with_status(
        "429 Too Many Requests",
        r#"{"ok":false,"error_code":429,"description":"Too Many Requests","parameters":{"retry_after":2}}"#,
    );
    let connector = LiveTelegramConnector::with_base_url("token".to_string(), base_url);

    let error = connector.send_message(555, "hi").await.unwrap_err();
    assert!(matches!(
        &error,
        ChannelError::RateLimited { retry_after, .. }
            if *retry_after == std::time::Duration::from_secs(2)
    ));
    assert_eq!(
        error.delivery_failure_class(),
        khive_channel::DeliveryFailureClass::Transient
    );
}

#[tokio::test]
async fn send_message_rate_limit_without_retry_after_stays_transient() {
    let base_url = spawn_one_shot_server_with_status(
        "429 Too Many Requests",
        r#"{"ok":false,"error_code":429,"description":"Too Many Requests"}"#,
    );
    let connector = LiveTelegramConnector::with_base_url("token".to_string(), base_url);

    let error = connector.send_message(555, "hi").await.unwrap_err();
    assert!(matches!(error, ChannelError::Transport(_)));
}

#[tokio::test]
async fn send_message_classifies_api_client_error_as_permanent() {
    let base_url =
        spawn_one_shot_server(r#"{"ok":false,"error_code":400,"description":"Bad Request"}"#);
    let connector = LiveTelegramConnector::with_base_url("token".to_string(), base_url);

    let error = connector.send_message(555, "hi").await.unwrap_err();
    assert!(matches!(error, ChannelError::PermanentTransport(_)));
    assert_eq!(
        error.delivery_failure_class(),
        khive_channel::DeliveryFailureClass::Permanent
    );
}

#[tokio::test]
async fn send_message_classifies_client_error_with_unparseable_body_as_permanent() {
    // A 400 with a body that is not the Bot API's JSON shape (an intermediate
    // proxy's HTML error page, or an empty body) must still classify as
    // permanent from the HTTP status alone -- requiring the body to parse
    // before the status is consulted would silently retry a rejection.
    let base_url = spawn_one_shot_server_with_status(
        "400 Bad Request",
        "<html><body>400 Bad Request</body></html>",
    );
    let connector = LiveTelegramConnector::with_base_url("token".to_string(), base_url);

    let error = connector.send_message(555, "hi").await.unwrap_err();
    assert!(matches!(error, ChannelError::PermanentTransport(_)));
    assert_eq!(
        error.delivery_failure_class(),
        khive_channel::DeliveryFailureClass::Permanent
    );
}

/// Same as [`spawn_one_shot_server`], but also captures the request
/// body it received back to the caller via a channel, so the test can
/// assert on the JSON this connector actually sent.
fn spawn_capturing_server(
    response_body: &'static str,
) -> (String, std::sync::mpsc::Receiver<String>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            use std::io::{Read, Write};
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let body = request
                .split("\r\n\r\n")
                .nth(1)
                .unwrap_or_default()
                .to_string();
            let _ = tx.send(body);
            let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body
                );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    (format!("http://{addr}"), rx)
}

#[tokio::test]
async fn send_message_request_failure_does_not_leak_token() {
    let connector =
        LiveTelegramConnector::with_base_url(SENTINEL_TOKEN.to_string(), refused_port_base_url());
    let err = connector.send_message(555, "hi").await.unwrap_err();
    let msg = err.to_string();
    assert!(
        !msg.contains(SENTINEL_TOKEN),
        "error must not leak the bot token: {msg}"
    );
}

#[tokio::test]
async fn send_message_decode_failure_does_not_leak_token() {
    let base_url = spawn_one_shot_server("not valid json");
    let connector = LiveTelegramConnector::with_base_url(SENTINEL_TOKEN.to_string(), base_url);
    let err = connector.send_message(555, "hi").await.unwrap_err();
    let msg = err.to_string();
    assert!(
        !msg.contains(SENTINEL_TOKEN),
        "error must not leak the bot token: {msg}"
    );
}

#[tokio::test]
async fn get_updates_request_failure_does_not_leak_token() {
    let connector =
        LiveTelegramConnector::with_base_url(SENTINEL_TOKEN.to_string(), refused_port_base_url());
    let err = connector.get_updates(None).await.unwrap_err();
    let msg = err.to_string();
    assert!(
        !msg.contains(SENTINEL_TOKEN),
        "error must not leak the bot token: {msg}"
    );
}

#[tokio::test]
async fn get_updates_decode_failure_does_not_leak_token() {
    let base_url = spawn_one_shot_server("not valid json");
    let connector = LiveTelegramConnector::with_base_url(SENTINEL_TOKEN.to_string(), base_url);
    let err = connector.get_updates(None).await.unwrap_err();
    let msg = err.to_string();
    assert!(
        !msg.contains(SENTINEL_TOKEN),
        "error must not leak the bot token: {msg}"
    );
}

#[tokio::test]
async fn get_updates_sends_positive_long_poll_timeout_and_offset() {
    let (base_url, request_rx) = spawn_capturing_server(r#"{"ok":true,"result":[]}"#);
    let connector = LiveTelegramConnector::with_base_url("token".to_string(), base_url);

    let updates = connector.get_updates(Some(42)).await.unwrap();
    assert!(updates.is_empty());

    let captured_body = request_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("server must capture a request body");
    let parsed: serde_json::Value = serde_json::from_str(&captured_body).unwrap();
    assert_eq!(parsed["offset"], 42);
    let timeout = parsed["timeout"]
        .as_i64()
        .expect("timeout field must be present and numeric");
    assert!(
        timeout > 0,
        "getUpdates must use long polling, not timeout=0"
    );
}
