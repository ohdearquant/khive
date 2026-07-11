//! Generic daemon-protocol test helpers (#544). See the `test_support`
//! module doc in `daemon.rs` for the gating rationale.

use std::path::Path;

use tokio::net::{UnixListener, UnixStream};

use super::{
    handle_conn, read_frame, write_frame, DaemonDispatch, DaemonRequestFrame, DaemonResponseFrame,
    PROTOCOL_VERSION,
};

/// A protocol-default request frame for `config_id`: no wire-only fields set,
/// this build's own [`PROTOCOL_VERSION`].
pub fn base_request_frame(config_id: &str) -> DaemonRequestFrame {
    DaemonRequestFrame {
        ops: String::new(),
        presentation: None,
        presentation_per_op: None,
        namespace: "local".to_string(),
        actor_id: None,
        visible_namespaces: Vec::new(),
        config_id: config_id.to_string(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: false,
    }
}

/// Poll `sock` until it accepts a connection or 5s elapse.
pub async fn connect_when_ready(sock: &Path) -> UnixStream {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Ok(s) = UnixStream::connect(sock).await {
            return s;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "daemon never bound {sock:?} within 5s"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Connect to `sock`, write `frame`, and decode the response frame.
pub async fn exchange(sock: &Path, frame: &DaemonRequestFrame) -> DaemonResponseFrame {
    let mut stream = UnixStream::connect(sock)
        .await
        .expect("connect to daemon socket");
    let payload = serde_json::to_vec(frame).expect("serialize request frame");
    write_frame(&mut stream, &payload)
        .await
        .expect("write request frame");
    let resp = read_frame(&mut stream).await.expect("read response frame");
    serde_json::from_slice(&resp).expect("decode response frame")
}

/// Accept exactly one connection, read its request frame (discarding it),
/// write back `response`, then stop accepting. Simulates a fixed-response fake
/// daemon (e.g. a stale/old-protocol peer) for exactly one exchange.
pub async fn serve_response_once(listener: UnixListener, response: DaemonResponseFrame) {
    if let Ok((mut stream, _)) = listener.accept().await {
        if read_frame(&mut stream).await.is_ok() {
            if let Ok(payload) = serde_json::to_vec(&response) {
                let _ = write_frame(&mut stream, &payload).await;
            }
        }
    }
    // Listener drops here; subsequent connection attempts see "connection refused".
}

/// Accept exactly one connection, read its request frame, then drop the
/// stream without writing any response, simulating a daemon crash mid-dispatch
/// (the request was fully written, but no response ever arrives).
pub async fn close_after_request(listener: UnixListener) {
    if let Ok((mut stream, _)) = listener.accept().await {
        let _ = read_frame(&mut stream).await;
    }
    // Listener drops here; subsequent connection attempts see "connection refused".
}

/// Drive `handle_conn` over an in-process `UnixStream::pair()` (no real
/// socket file needed) and decode the response frame it writes back. Generic
/// over any [`DaemonDispatch`] impl so both this crate's own daemon tests and
/// a downstream transport crate's dispatch fakes can share it (#544).
pub async fn round_trip<D: DaemonDispatch>(
    dispatcher: D,
    req: &DaemonRequestFrame,
) -> DaemonResponseFrame {
    let (mut client, server) = UnixStream::pair().expect("unix stream pair");
    let payload = serde_json::to_vec(req).expect("encode request frame");
    let handle = tokio::spawn(async move {
        handle_conn(server, dispatcher).await;
    });
    write_frame(&mut client, &payload)
        .await
        .expect("write request frame");
    let raw = read_frame(&mut client).await.expect("read response frame");
    handle.await.expect("handle_conn task panicked");
    serde_json::from_slice(&raw).expect("decode response frame")
}
