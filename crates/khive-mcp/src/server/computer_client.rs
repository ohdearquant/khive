//! The MCP client that carries routed requests to the caller's computer
//! (ADR-220 sections 3, 5 and 10).
//!
//! One session per bridge process, opened by the first routed request: the
//! key is read from `[computer].key_file`, an MCP session is initialized over
//! streamable HTTP, and `whoami` on the computer must answer with the
//! caller's `[actor].id` before anything is routed. A request the computer
//! answers with HTTP 404 for a lost session did not run (section 10): the
//! client opens a new session, checks `whoami` again and sends that request
//! once more. No other failure sends a request again.
#![cfg(unix)]

use std::error::Error as StdError;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use khive_runtime::{
    read_computer_key, ComputerKey, ComputerKeyError, ComputerSectionConfig, DomainDisposition,
    ShareTargetConfig,
};
use rmcp::model::{CallToolRequestParams, CallToolResult, ErrorData as McpError, JsonObject};
use rmcp::service::{RunningService, ServiceError};
use rmcp::transport::streamable_http_client::{
    StreamableHttpClient, StreamableHttpClientTransportConfig, StreamableHttpError,
};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::computer_http::{BoundedHttpClient, NotJsonRpc, ResponseTooLarge};
use super::computer_routing::{HomeOutcome, COMPUTER_PROTOCOL};

/// Failure kind for a computer that cannot be reached or did not answer.
pub(super) const COMPUTER_UNREACHABLE: &str = "computer_unreachable";
/// Refusal kind for a computer that answers `whoami` for another actor.
pub(super) const COMPUTER_IDENTITY_MISMATCH: &str = "computer_identity_mismatch";
/// The one tool a routed request calls on the computer.
const REQUEST_TOOL: &str = "request";

/// The caller's options that travel with a routed request (section 10).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ForwardedOptions {
    pub(super) presentation: Option<String>,
    pub(super) presentation_per_op: Option<Vec<Option<String>>>,
    pub(super) format: Option<String>,
    pub(super) format_per_op: Option<Vec<Option<String>>>,
    pub(super) request_id: Option<u64>,
}

impl ForwardedOptions {
    fn arguments(&self, ops: String) -> JsonObject {
        let mut arguments = JsonObject::new();
        arguments.insert("ops".into(), Value::String(ops));
        let mut put = |name: &str, value: Value| {
            if !value.is_null() {
                arguments.insert(name.into(), value);
            }
        };
        put("presentation", json!(self.presentation));
        put("presentation_per_op", json!(self.presentation_per_op));
        put("format", json!(self.format));
        put("format_per_op", json!(self.format_per_op));
        put("request_id", json!(self.request_id));
        arguments
    }
}

/// A routed request that produced no envelope: every operation it carried
/// fails with `error`, and `disposition` says whether the computer may have
/// run it.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct RoutedFailure {
    pub(super) error: Value,
    pub(super) disposition: DomainDisposition,
}

impl From<RoutedFailure> for HomeOutcome {
    fn from(failure: RoutedFailure) -> Self {
        HomeOutcome::Failed {
            error: failure.error,
            disposition: failure.disposition,
        }
    }
}

fn failure(kind: &str, message: String, disposition: DomainDisposition) -> RoutedFailure {
    RoutedFailure {
        error: json!({ "kind": kind, "message": message }),
        disposition,
    }
}

/// What a failed send says about the request it carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TransportFailure {
    /// HTTP 404 for a session the computer no longer holds: the request did
    /// not run (section 10).
    SessionExpired,
    /// No connection was made, so the request never left this process.
    NotSent,
    /// The computer answered with a body this client does not use (see
    /// `computer_http`): over its bound, or JSON answering a request that is
    /// not a JSON-RPC message. The request may have run.
    Unusable,
    /// The request may have reached the computer.
    Unknown,
}

type Classifier = fn(&(dyn StdError + 'static)) -> TransportFailure;

/// An initialized MCP session to the computer, with the reader of its
/// transport's errors.
pub(super) struct Connection {
    service: RunningService<RoleClient, ()>,
    classify: Classifier,
}

/// Initializes an MCP client session over `transport`.
pub(super) async fn connect_transport<C: StreamableHttpClient>(
    transport: StreamableHttpClientTransport<C>,
) -> Result<Connection, String> {
    let service = ().serve(transport).await.map_err(|error| error.to_string())?;
    Ok(Connection {
        service,
        classify: classify::<C>,
    })
}

fn classify<C: StreamableHttpClient>(error: &(dyn StdError + 'static)) -> TransportFailure {
    // `StreamableHttpError::Client` does not report the client's error as its
    // source, so the chain is walked from that error itself.
    let behind = match error.downcast_ref::<StreamableHttpError<C::Error>>() {
        Some(StreamableHttpError::SessionExpired) => return TransportFailure::SessionExpired,
        Some(StreamableHttpError::Client(client)) => client as &(dyn StdError + 'static),
        _ => error,
    };
    if in_chain::<ResponseTooLarge>(behind) || in_chain::<NotJsonRpc>(behind) {
        TransportFailure::Unusable
    } else if never_connected(behind) {
        TransportFailure::NotSent
    } else {
        TransportFailure::Unknown
    }
}

/// Whether `error` or an error behind it is a connection that could not be
/// opened: refused, no such socket, no such address, a name that did not
/// resolve, or a TLS handshake that failed. Each of these ends before the
/// first byte of the request is written.
fn never_connected(error: &(dyn StdError + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(error) = current {
        if error
            .downcast_ref::<reqwest::Error>()
            .is_some_and(reqwest::Error::is_connect)
        {
            return true;
        }
        if let Some(io) = error.downcast_ref::<std::io::Error>() {
            if matches!(
                io.kind(),
                ErrorKind::ConnectionRefused | ErrorKind::NotFound | ErrorKind::AddrNotAvailable
            ) {
                return true;
            }
        }
        current = error.source();
    }
    false
}

/// Whether `error` or an error behind it is a `T`.
fn in_chain<T: StdError + 'static>(error: &(dyn StdError + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(error) = current {
        if error.is::<T>() {
            return true;
        }
        current = error.source();
    }
    false
}

/// Opens the HTTP transport for one session. Production connects to `url`
/// over TCP or TLS; the acceptance arms connect over an in-process pipe.
#[async_trait]
pub(super) trait Connector: Send + Sync {
    async fn connect(
        &self,
        transport: StreamableHttpClientTransportConfig,
    ) -> Result<Connection, String>;
}

/// Connects to `url` with the bounded HTTP client of `computer_http`.
pub(super) struct HttpConnector;

#[async_trait]
impl Connector for HttpConnector {
    async fn connect(
        &self,
        transport: StreamableHttpClientTransportConfig,
    ) -> Result<Connection, String> {
        let client = BoundedHttpClient::new()?;
        connect_transport(StreamableHttpClientTransport::with_client(
            client, transport,
        ))
        .await
    }
}

struct Session {
    connection: Connection,
    generation: u64,
    /// Kept only to scrub its bytes from error text.
    key: ComputerKey,
}

enum CallOutcome {
    Text(String),
    SessionExpired,
    Failed {
        failure: RoutedFailure,
        drop_session: bool,
    },
}

/// The client of one caller's computer.
pub(super) struct ComputerClient {
    connector: Arc<dyn Connector>,
    url: String,
    key_file: PathBuf,
    call_timeout: Duration,
    /// `name (host:port)`: the only way a message names the computer.
    label: String,
    /// The caller's `[actor].id`, which `whoami` on the computer must report.
    actor: String,
    session: Mutex<Option<Arc<Session>>>,
    next_generation: AtomicU64,
}

impl ComputerClient {
    pub(super) fn new(
        config: &ComputerSectionConfig,
        label: String,
        actor: String,
        connector: Arc<dyn Connector>,
    ) -> Self {
        Self {
            connector,
            url: config.url.clone(),
            key_file: config.key_file.clone(),
            call_timeout: Duration::from_secs(config.call_timeout_s),
            label,
            actor,
            session: Mutex::new(None),
            next_generation: AtomicU64::new(0),
        }
    }

    /// The client of one `[[share.targets]]` entry (ADR-220 Amendment 1),
    /// held to the same key, session and `whoami` rules as the caller's
    /// computer.
    pub(super) fn for_share_target(
        target: &ShareTargetConfig,
        label: String,
        actor: String,
        connector: Arc<dyn Connector>,
    ) -> Self {
        Self {
            connector,
            url: target.url.clone(),
            key_file: target.key_file.clone(),
            call_timeout: Duration::from_secs(target.call_timeout_s),
            label,
            actor,
            session: Mutex::new(None),
            next_generation: AtomicU64::new(0),
        }
    }

    /// Sends `ops` to the computer as one `tools/call` of `request` and
    /// returns the text of its answer, which is the khive envelope.
    pub(super) async fn send(
        &self,
        ops: String,
        options: &ForwardedOptions,
    ) -> Result<String, RoutedFailure> {
        let arguments = options.arguments(ops);
        let session = self.session(None).await?;
        let outcome = match self.call(&session, arguments.clone()).await {
            CallOutcome::SessionExpired => {
                let renewed = self.session(Some(session.generation)).await?;
                let outcome = self.call(&renewed, arguments).await;
                return self.finish(outcome, &renewed).await;
            }
            outcome => outcome,
        };
        self.finish(outcome, &session).await
    }

    async fn finish(
        &self,
        outcome: CallOutcome,
        session: &Session,
    ) -> Result<String, RoutedFailure> {
        match outcome {
            CallOutcome::Text(text) => match envelope_problem(&text) {
                None => Ok(text),
                Some(problem) => Err(failure(
                    COMPUTER_PROTOCOL,
                    format!(
                        "computer {} answered with a response that is not the khive envelope: \
                         {problem}",
                        self.label
                    ),
                    DomainDisposition::Unknown,
                )),
            },
            CallOutcome::SessionExpired => {
                self.discard(session.generation).await;
                Err(failure(
                    COMPUTER_UNREACHABLE,
                    format!(
                        "computer {} lost the session again on the new session; the request \
                         was not run",
                        self.label
                    ),
                    DomainDisposition::NotCommitted,
                ))
            }
            CallOutcome::Failed {
                failure,
                drop_session,
            } => {
                if drop_session {
                    self.discard(session.generation).await;
                }
                Err(failure)
            }
        }
    }

    /// The live session, or a new one when there is none or when `expired`
    /// names the live one.
    async fn session(&self, expired: Option<u64>) -> Result<Arc<Session>, RoutedFailure> {
        let mut slot = self.session.lock().await;
        if let Some(current) = slot.as_ref() {
            if expired != Some(current.generation) {
                return Ok(Arc::clone(current));
            }
        }
        *slot = None;
        let session = Arc::new(self.open().await?);
        *slot = Some(Arc::clone(&session));
        Ok(session)
    }

    async fn discard(&self, generation: u64) {
        let mut slot = self.session.lock().await;
        if slot
            .as_ref()
            .is_some_and(|current| current.generation == generation)
        {
            *slot = None;
        }
    }

    async fn open(&self) -> Result<Session, RoutedFailure> {
        let key = read_computer_key(&self.key_file).map_err(|error: ComputerKeyError| {
            failure(
                ComputerKeyError::KIND,
                error.to_string(),
                DomainDisposition::NotCommitted,
            )
        })?;
        let transport = StreamableHttpClientTransportConfig::with_uri(self.url.as_str())
            .auth_header(key.token())
            .reinit_on_expired_session(false);
        let unreachable = |detail: String| {
            failure(
                COMPUTER_UNREACHABLE,
                format!(
                    "computer {} could not be reached: {}",
                    self.label,
                    scrub(detail, &key)
                ),
                DomainDisposition::NotCommitted,
            )
        };
        let connection = match tokio::time::timeout(
            self.call_timeout,
            self.connector.connect(transport),
        )
        .await
        {
            Ok(Ok(connection)) => connection,
            Ok(Err(detail)) => return Err(unreachable(detail)),
            Err(_) => {
                return Err(unreachable(format!(
                    "no session within {} s",
                    self.call_timeout.as_secs()
                )))
            }
        };
        let session = Session {
            connection,
            generation: self.next_generation.fetch_add(1, Ordering::Relaxed),
            key,
        };
        self.check_identity(&session).await?;
        Ok(session)
    }

    /// Section 3: the computer must answer `whoami` for this caller's actor.
    async fn check_identity(&self, session: &Session) -> Result<(), RoutedFailure> {
        let not_checked = |failure: RoutedFailure| RoutedFailure {
            disposition: DomainDisposition::NotCommitted,
            ..failure
        };
        let arguments = ForwardedOptions {
            format: Some("json".into()),
            ..ForwardedOptions::default()
        }
        .arguments("whoami()".into());
        let text = match self.call(session, arguments).await {
            CallOutcome::Text(text) => text,
            CallOutcome::SessionExpired => {
                return Err(failure(
                    COMPUTER_UNREACHABLE,
                    format!(
                        "computer {} dropped the new session before whoami was answered",
                        self.label
                    ),
                    DomainDisposition::NotCommitted,
                ))
            }
            CallOutcome::Failed { failure, .. } => return Err(not_checked(failure)),
        };
        let reported = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|envelope| {
                envelope
                    .pointer("/results/0/result/actor_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
        match reported {
            Some(actor) if actor == self.actor => Ok(()),
            Some(actor) => Err(failure(
                COMPUTER_IDENTITY_MISMATCH,
                format!(
                    "computer {} acts for {actor:?}, but this caller is {:?}; no routed \
                     operation was run",
                    self.label, self.actor
                ),
                DomainDisposition::NotCommitted,
            )),
            None => Err(failure(
                COMPUTER_PROTOCOL,
                format!(
                    "computer {} answered whoami without an actor id; no routed operation was run",
                    self.label
                ),
                DomainDisposition::NotCommitted,
            )),
        }
    }

    async fn call(&self, session: &Session, arguments: JsonObject) -> CallOutcome {
        let params = CallToolRequestParams::new(REQUEST_TOOL).with_arguments(arguments);
        let answer = tokio::time::timeout(
            self.call_timeout,
            session.connection.service.call_tool(params),
        )
        .await;
        let unreachable = |detail: String, disposition, drop_session| CallOutcome::Failed {
            failure: failure(
                COMPUTER_UNREACHABLE,
                format!("computer {} {}", self.label, scrub(detail, &session.key)),
                disposition,
            ),
            drop_session,
        };
        match answer {
            Err(_) => unreachable(
                format!(
                    "did not answer within {} s; the request may have run",
                    self.call_timeout.as_secs()
                ),
                DomainDisposition::Unknown,
                false,
            ),
            Ok(Ok(result)) => match result_text(result) {
                Some(text) => CallOutcome::Text(text),
                None => CallOutcome::Failed {
                    failure: failure(
                        COMPUTER_PROTOCOL,
                        format!(
                            "computer {} answered with no text content; the request may have run",
                            self.label
                        ),
                        DomainDisposition::Unknown,
                    ),
                    drop_session: false,
                },
            },
            Ok(Err(ServiceError::TransportSend(error))) => {
                match (session.connection.classify)(error.error.as_ref()) {
                    TransportFailure::SessionExpired => CallOutcome::SessionExpired,
                    TransportFailure::NotSent => unreachable(
                        format!("could not be reached: {}; the request was not run", error),
                        DomainDisposition::NotCommitted,
                        true,
                    ),
                    TransportFailure::Unusable => CallOutcome::Failed {
                        failure: failure(
                            COMPUTER_PROTOCOL,
                            format!(
                                "computer {} {}; the request may have run",
                                self.label,
                                scrub(error.to_string(), &session.key)
                            ),
                            DomainDisposition::Unknown,
                        ),
                        drop_session: true,
                    },
                    TransportFailure::Unknown => unreachable(
                        format!("failed after the request was sent: {error}; it may have run"),
                        DomainDisposition::Unknown,
                        true,
                    ),
                }
            }
            Ok(Err(ServiceError::McpError(error))) => CallOutcome::Failed {
                failure: self.refusal(error, &session.key),
                drop_session: false,
            },
            Ok(Err(other)) => unreachable(
                format!("failed after the request was sent: {other}; it may have run"),
                DomainDisposition::Unknown,
                true,
            ),
        }
    }

    /// A JSON-RPC error from the computer, for every operation of the request.
    fn refusal(&self, error: McpError, key: &ComputerKey) -> RoutedFailure {
        let data = error.data.unwrap_or(Value::Null);
        let kind = data
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("computer_error")
            .to_owned();
        let disposition = match data.get("domain_disposition").and_then(Value::as_str) {
            Some("not_committed") => DomainDisposition::NotCommitted,
            Some("committed") => DomainDisposition::Committed,
            _ => DomainDisposition::Unknown,
        };
        RoutedFailure {
            error: json!({
                "kind": kind,
                "message": format!(
                    "computer {} refused the request: {}",
                    self.label,
                    scrub(error.message.into_owned(), key)
                ),
                "code": error.code.0,
            }),
            disposition,
        }
    }
}

fn result_text(result: CallToolResult) -> Option<String> {
    result
        .content
        .into_iter()
        .find_map(|content| content.as_text().map(|text| text.text.clone()))
}

/// Why `text` is not a khive envelope, if it is not one.
fn envelope_problem(text: &str) -> Option<&'static str> {
    match serde_json::from_str::<Value>(text) {
        Err(_) => Some("the response is not JSON"),
        Ok(Value::Object(envelope)) if envelope.get("results").is_some_and(Value::is_array) => None,
        Ok(Value::Object(_)) => Some("the response has no results array"),
        Ok(_) => Some("the response is not a JSON object"),
    }
}

/// `text` with every copy of the key replaced, so an error that quotes a
/// request cannot carry it into a message.
fn scrub(text: String, key: &ComputerKey) -> String {
    if text.contains(key.token()) {
        text.replace(key.token(), "<redacted>")
    } else {
        text
    }
}

#[cfg(all(test, unix))]
#[path = "computer_client_tests.rs"]
mod tests;
