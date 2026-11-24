//! The computer client against a computer served over streamable HTTP
//! (ADR-220 acceptance: the connector seam). The client reaches it over an
//! in-process pipe carrying HTTP/1.1, so no socket is bound and the real status
//! codes, the 404 for a lost session included, reach the client. The arms at
//! the end read how a failed send is classified; two of them need a loopback
//! socket and run in the hosted checks.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use khive_runtime::{ComputerSectionConfig, DomainDisposition};
use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, ErrorData as McpError, JsonObject,
    ServerCapabilities, ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_client::StreamableHttpError;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::session::SessionManager;
use rmcp::RoleServer;
use serde_json::{json, Value};

use super::super::computer_http::{BoundedHttpClient, BoundedHttpError};
use super::super::computer_pipe_tests::{PipeComputer, PipeConnector, URL};
use super::{
    classify, ComputerClient, ForwardedOptions, RoutedFailure, TransportFailure,
    COMPUTER_IDENTITY_MISMATCH, COMPUTER_UNREACHABLE,
};

const ACTOR: &str = "lambda:test-caller";
// Assembled from parts so that no literal in the source reads as a credential.
const KEY: &str = concat!("computer-", "fixture-", "token");

/// A computer that answers `whoami` for `actor` and records every call.
#[derive(Clone)]
struct FakeComputer {
    actor: String,
    calls: Arc<StdMutex<Vec<JsonObject>>>,
}

impl ServerHandler for FakeComputer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let arguments = request.arguments.unwrap_or_default();
        let ops = arguments
            .get("ops")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        self.calls.lock().expect("calls").push(arguments);
        let tool = ops.split('(').next().unwrap_or_default().to_owned();
        let result = if tool == "whoami" {
            json!({ "actor_id": self.actor })
        } else {
            json!({ "ran": ops })
        };
        let envelope = json!({
            "results": [{ "ok": true, "tool": tool, "result": result }],
            "summary": { "total": 1, "succeeded": 1, "failed": 0, "aborted": 0 },
            "status": "success",
        });
        Ok(CallToolResult::success(vec![ContentBlock::text(
            envelope.to_string(),
        )]))
    }
}

struct TestComputer {
    dir: tempfile::TempDir,
    computer: Arc<PipeComputer>,
    calls: Arc<StdMutex<Vec<JsonObject>>>,
    sessions: Arc<LocalSessionManager>,
}

impl TestComputer {
    fn start(actor: &str) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        write_key(&dir.path().join("computer.key"), 0o600);
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let sessions = Arc::new(LocalSessionManager::default());
        let handler = FakeComputer {
            actor: actor.to_owned(),
            calls: Arc::clone(&calls),
        };
        let computer = PipeComputer::serve(handler, Arc::clone(&sessions));
        Self {
            dir,
            computer,
            calls,
            sessions,
        }
    }

    fn key_file(&self) -> PathBuf {
        self.dir.path().join("computer.key")
    }

    fn client(&self, actor: &str) -> ComputerClient {
        client_for(Arc::clone(&self.computer), self.key_file(), actor)
    }

    fn ops(&self) -> Vec<String> {
        self.calls
            .lock()
            .expect("calls")
            .iter()
            .map(|arguments| {
                arguments
                    .get("ops")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect()
    }

    fn arguments(&self, index: usize) -> JsonObject {
        self.calls.lock().expect("calls")[index].clone()
    }

    fn authorization(&self) -> Vec<String> {
        self.computer.authorization()
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
        self.computer.stop();
    }
}

fn write_key(path: &Path, mode: u32) {
    std::fs::write(path, format!("{KEY}\n")).expect("write the key file");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

fn client_for(computer: Arc<PipeComputer>, key_file: PathBuf, actor: &str) -> ComputerClient {
    let config = ComputerSectionConfig {
        name: "build-1".into(),
        url: URL.into(),
        key_file,
        packs: vec!["exec".into()],
        call_timeout_s: 30,
    };
    ComputerClient::new(
        &config,
        "build-1 (localhost:80)".into(),
        actor.into(),
        Arc::new(PipeConnector { computer }),
    )
}

fn assert_refused(failure: &RoutedFailure, kind: &str, disposition: DomainDisposition) {
    assert_eq!(failure.error["kind"], kind, "{failure:?}");
    assert_eq!(failure.disposition, disposition, "{failure:?}");
    let message = failure.error["message"].as_str().expect("a message");
    assert!(message.contains("build-1"), "{message}");
    assert!(!failure.error.to_string().contains(KEY), "{failure:?}");
}

#[tokio::test]
async fn a_routed_request_runs_once_after_whoami_with_the_key_and_the_options() {
    let computer = TestComputer::start(ACTOR);
    let client = computer.client(ACTOR);
    let options = ForwardedOptions {
        presentation: Some("verbose".into()),
        request_id: Some(7),
        ..ForwardedOptions::default()
    };

    let text = client
        .send(r#"exec.run(tool="sh")"#.into(), &options)
        .await
        .expect("the request is answered");

    let envelope: Value = serde_json::from_str(&text).expect("the computer's envelope");
    assert_eq!(envelope["results"][0]["tool"], "exec.run");
    assert_eq!(computer.ops(), ["whoami()", r#"exec.run(tool="sh")"#]);
    let forwarded = computer.arguments(1);
    assert_eq!(forwarded["presentation"], "verbose");
    assert_eq!(forwarded["request_id"], 7);
    assert!(!forwarded.contains_key("format"));
    let authorization = computer.authorization();
    assert!(!authorization.is_empty());
    assert!(
        authorization
            .iter()
            .all(|value| value == &format!("Bearer {KEY}")),
        "{authorization:?}"
    );

    client
        .send("exec.runs()".into(), &ForwardedOptions::default())
        .await
        .expect("a second request on the same session");
    assert_eq!(
        computer.ops(),
        ["whoami()", r#"exec.run(tool="sh")"#, "exec.runs()"],
        "one session, so whoami is asked once"
    );
}

#[tokio::test]
async fn a_lost_session_reopens_checks_whoami_and_sends_the_request_once() {
    let computer = TestComputer::start(ACTOR);
    let client = computer.client(ACTOR);
    client
        .send("exec.runs()".into(), &ForwardedOptions::default())
        .await
        .expect("first request");

    computer.expire_sessions().await;
    client
        .send(
            r#"exec.run(tool="sh")"#.into(),
            &ForwardedOptions::default(),
        )
        .await
        .expect("the request is sent on the new session");

    assert_eq!(
        computer.ops(),
        [
            "whoami()",
            "exec.runs()",
            "whoami()",
            r#"exec.run(tool="sh")"#
        ],
        "the computer ran the request once, after whoami on the new session"
    );
}

#[tokio::test]
async fn a_computer_acting_for_another_actor_is_refused_before_anything_runs() {
    let computer = TestComputer::start("lambda:someone-else");
    let client = computer.client(ACTOR);

    let failure = client
        .send(
            r#"exec.run(tool="sh")"#.into(),
            &ForwardedOptions::default(),
        )
        .await
        .expect_err("refused");

    assert_refused(
        &failure,
        COMPUTER_IDENTITY_MISMATCH,
        DomainDisposition::NotCommitted,
    );
    let message = failure.error["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("lambda:someone-else") && message.contains(ACTOR),
        "{message}"
    );
    assert_eq!(computer.ops(), ["whoami()"]);
}

#[tokio::test]
async fn a_computer_that_is_not_listening_is_unreachable_and_nothing_ran() {
    let computer = TestComputer::start(ACTOR);
    computer.computer.stop();
    let client = computer.client(ACTOR);

    let failure = client
        .send(
            r#"exec.run(tool="sh")"#.into(),
            &ForwardedOptions::default(),
        )
        .await
        .expect_err("unreachable");

    assert_refused(
        &failure,
        COMPUTER_UNREACHABLE,
        DomainDisposition::NotCommitted,
    );
    assert!(computer.ops().is_empty());
}

#[tokio::test]
async fn a_computer_that_stops_between_requests_is_unreachable_and_the_request_did_not_run() {
    let computer = TestComputer::start(ACTOR);
    let client = computer.client(ACTOR);
    client
        .send("exec.runs()".into(), &ForwardedOptions::default())
        .await
        .expect("first request");

    computer.computer.stop();
    let failure = client
        .send(
            r#"exec.run(tool="sh")"#.into(),
            &ForwardedOptions::default(),
        )
        .await
        .expect_err("unreachable");

    assert_refused(
        &failure,
        COMPUTER_UNREACHABLE,
        DomainDisposition::NotCommitted,
    );
    assert_eq!(computer.ops(), ["whoami()", "exec.runs()"]);
}

#[tokio::test]
async fn a_key_file_open_to_others_is_refused_before_the_computer_is_reached() {
    let computer = TestComputer::start(ACTOR);
    write_key(&computer.key_file(), 0o644);
    let client = computer.client(ACTOR);

    let failure = client
        .send(
            r#"exec.run(tool="sh")"#.into(),
            &ForwardedOptions::default(),
        )
        .await
        .expect_err("refused");

    assert_eq!(failure.error["kind"], "computer_key_unavailable");
    assert_eq!(failure.disposition, DomainDisposition::NotCommitted);
    let message = failure.error["message"].as_str().unwrap_or_default();
    assert!(message.contains("[computer].key_file"), "{message}");
    assert!(
        !message.contains(computer.key_file().to_str().unwrap_or_default()),
        "{message}"
    );
    assert!(computer.ops().is_empty());
    assert!(computer.authorization().is_empty());
}

// ---- what a failed send says about the request ----
//
// The classifier read with the errors the HTTP client really returns. The
// name lookup binds no socket. The two loopback listeners do, so those arms
// run in the hosted checks.

/// The error the HTTP client returns for a POST to `url`, as the bounded
/// client hands it on.
async fn send_failure(url: &str) -> StreamableHttpError<BoundedHttpError> {
    let error = reqwest::Client::new()
        .post(url)
        .body("{}")
        .send()
        .await
        .expect_err("the POST fails");
    StreamableHttpError::Client(BoundedHttpError::Http(error))
}

#[tokio::test]
async fn a_name_that_does_not_resolve_was_not_sent() {
    // The `invalid` top-level domain never resolves (RFC 2606).
    let error = send_failure("http://computer.invalid/mcp").await;
    assert_eq!(
        classify::<BoundedHttpClient>(&error),
        TransportFailure::NotSent,
        "{error}"
    );
}

#[tokio::test]
async fn a_tls_handshake_that_fails_was_not_sent() {
    // The listener answers in plain HTTP where a TLS handshake was expected.
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
    let port = listener.local_addr().expect("its address").port();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("one connection");
        let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\r\n");
    });

    let error = send_failure(&format!("https://127.0.0.1:{port}/mcp")).await;
    server.join().expect("the listener thread");
    assert_eq!(
        classify::<BoundedHttpClient>(&error),
        TransportFailure::NotSent,
        "{error}"
    );
}

#[tokio::test]
async fn a_connection_closed_after_the_request_was_written_may_have_run() {
    // The listener reads the request and closes without an answer: the
    // connection was made and the request written, so this is not a
    // connection that could not be opened.
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener");
    let port = listener.local_addr().expect("its address").port();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("one connection");
        let mut request = [0_u8; 4096];
        let _ = stream.read(&mut request);
    });

    let error = send_failure(&format!("http://127.0.0.1:{port}/mcp")).await;
    server.join().expect("the listener thread");
    assert_eq!(
        classify::<BoundedHttpClient>(&error),
        TransportFailure::Unknown,
        "{error}"
    );
}
