//! The transport of the ADR-220 acceptance arms: a test computer served by
//! rmcp's streamable HTTP service and reached over an in-process pipe that
//! carries HTTP/1.1. No socket is bound, and the real status codes, the 404
//! for a lost session included, reach the client.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use futures::StreamExt;
use http::{HeaderName, HeaderValue, Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper_util::rt::TokioIo;
use hyper_util::service::TowerToHyperService;
use rmcp::handler::server::ServerHandler;
use rmcp::model::{ClientJsonRpcMessage, ServerJsonRpcMessage};
use rmcp::transport::common::client_side_sse::BoxedSseResponse;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClient, StreamableHttpClientTransportConfig, StreamableHttpError,
    StreamableHttpPostResponse,
};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{
    StreamableHttpClientTransport, StreamableHttpServerConfig, StreamableHttpService,
};
use sse_stream::SseStream;
use tokio::io::DuplexStream;
use tokio::task::JoinHandle;

use super::computer_client::{connect_transport, Connection, Connector};

/// The URL every test computer is configured with.
pub(super) const URL: &str = "http://localhost/mcp";
const SESSION_HEADER: &str = "mcp-session-id";
const LAST_EVENT_HEADER: &str = "last-event-id";
const EVENT_STREAM: &str = "text/event-stream";
const JSON_TYPE: &str = "application/json";

/// Why the pipe could not carry a request.
#[derive(Debug)]
pub(super) enum PipeError {
    Io(std::io::Error),
    Hyper(hyper::Error),
    Http(http::Error),
}

impl std::fmt::Display for PipeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "pipe: {error}"),
            Self::Hyper(error) => write!(formatter, "pipe: {error}"),
            Self::Http(error) => write!(formatter, "pipe: {error}"),
        }
    }
}

impl std::error::Error for PipeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Hyper(error) => Some(error),
            Self::Http(error) => Some(error),
        }
    }
}

/// Serves one connection on the server end of a pipe.
type Serve = Box<dyn Fn(DuplexStream) -> JoinHandle<()> + Send + Sync>;

/// The test computer: rmcp's streamable HTTP service, served by hyper on one
/// end of a fresh in-process pipe per request.
pub(super) struct PipeComputer {
    serve: Serve,
    authorization: Arc<StdMutex<Vec<String>>>,
    stopped: AtomicBool,
    connections: StdMutex<Vec<JoinHandle<()>>>,
}

impl PipeComputer {
    /// A computer serving `handler`, holding its sessions in `sessions`.
    pub(super) fn serve<H>(handler: H, sessions: Arc<LocalSessionManager>) -> Arc<Self>
    where
        H: ServerHandler + Clone + Send + Sync + 'static,
    {
        let service = StreamableHttpService::new(
            move || Ok(handler.clone()),
            sessions,
            StreamableHttpServerConfig::default(),
        );
        let authorization = Arc::new(StdMutex::new(Vec::new()));
        let seen = Arc::clone(&authorization);
        let serve: Serve = Box::new(move |stream| {
            let service = TowerToHyperService::new(service.clone());
            let seen = Arc::clone(&seen);
            let recording = hyper::service::service_fn(move |request: Request<Incoming>| {
                if let Some(value) = request.headers().get(http::header::AUTHORIZATION) {
                    seen.lock()
                        .expect("authorization")
                        .push(value.to_str().unwrap_or_default().to_owned());
                }
                hyper::service::Service::call(&service, request)
            });
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), recording)
                    .await;
            })
        });
        Arc::new(Self {
            serve,
            authorization,
            stopped: AtomicBool::new(false),
            connections: StdMutex::new(Vec::new()),
        })
    }

    async fn send(&self, request: Request<Full<Bytes>>) -> Result<Response<Incoming>, PipeError> {
        if self.stopped.load(Ordering::SeqCst) {
            return Err(PipeError::Io(ErrorKind::ConnectionRefused.into()));
        }
        let (client, server) = tokio::io::duplex(1 << 16);
        let connection = (self.serve)(server);
        self.connections
            .lock()
            .expect("connections")
            .push(connection);
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(client))
            .await
            .map_err(PipeError::Hyper)?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        sender.send_request(request).await.map_err(PipeError::Hyper)
    }

    /// Every `Authorization` header the computer received.
    pub(super) fn authorization(&self) -> Vec<String> {
        self.authorization.lock().expect("authorization").clone()
    }

    /// Refuse every new connection and end the open ones.
    pub(super) fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        for connection in self.connections.lock().expect("connections").drain(..) {
            connection.abort();
        }
    }

    /// Accept connections again after [`Self::stop`].
    pub(super) fn restart(&self) {
        self.stopped.store(false, Ordering::SeqCst);
    }
}

/// An HTTP client over the pipe, answering as rmcp's own clients do.
#[derive(Clone)]
struct PipeClient {
    computer: Arc<PipeComputer>,
}

fn request_to(
    method: Method,
    uri: &str,
    session_id: Option<&str>,
    auth_header: Option<String>,
    custom_headers: HashMap<HeaderName, HeaderValue>,
) -> http::request::Builder {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(http::header::HOST, "localhost")
        .header(http::header::ACCEPT, format!("{EVENT_STREAM}, {JSON_TYPE}"));
    if let Some(token) = auth_header {
        builder = builder.header(http::header::AUTHORIZATION, format!("Bearer {token}"));
    }
    for (name, value) in custom_headers {
        builder = builder.header(name, value);
    }
    if let Some(session_id) = session_id {
        builder = builder.header(SESSION_HEADER, session_id);
    }
    builder
}

fn content_type(response: &Response<Incoming>) -> String {
    response
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

impl StreamableHttpClient for PipeClient {
    type Error = PipeError;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<PipeError>> {
        let body = serde_json::to_vec(&message)?;
        let request = request_to(
            Method::POST,
            &uri,
            session_id.as_deref(),
            auth_header,
            custom_headers,
        )
        .header(http::header::CONTENT_TYPE, JSON_TYPE)
        .body(Full::new(Bytes::from(body)))
        .map_err(|error| StreamableHttpError::Client(PipeError::Http(error)))?;
        let response = self
            .computer
            .send(request)
            .await
            .map_err(StreamableHttpError::Client)?;
        let status = response.status();
        if matches!(status, StatusCode::ACCEPTED | StatusCode::NO_CONTENT) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if status == StatusCode::NOT_FOUND && session_id.is_some() {
            return Err(StreamableHttpError::SessionExpired);
        }
        if !status.is_success() {
            return Err(StreamableHttpError::UnexpectedServerResponse(
                format!("HTTP {status}").into(),
            ));
        }
        let session = response
            .headers()
            .get(SESSION_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let kind = content_type(&response);
        if kind.starts_with(EVENT_STREAM) {
            let events = SseStream::new(response.into_body()).boxed();
            Ok(StreamableHttpPostResponse::Sse(events, session))
        } else if kind.starts_with(JSON_TYPE) {
            let body = response
                .into_body()
                .collect()
                .await
                .map_err(|error| StreamableHttpError::Client(PipeError::Hyper(error)))?
                .to_bytes();
            match serde_json::from_slice::<ServerJsonRpcMessage>(&body) {
                Ok(message) => Ok(StreamableHttpPostResponse::Json(message, session)),
                Err(_) => Ok(StreamableHttpPostResponse::Accepted),
            }
        } else {
            Err(StreamableHttpError::UnexpectedContentType(Some(kind)))
        }
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<PipeError>> {
        let request = request_to(
            Method::DELETE,
            &uri,
            Some(&session_id),
            auth_header,
            custom_headers,
        )
        .body(Full::new(Bytes::new()))
        .map_err(|error| StreamableHttpError::Client(PipeError::Http(error)))?;
        self.computer
            .send(request)
            .await
            .map_err(StreamableHttpError::Client)?;
        Ok(())
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxedSseResponse, StreamableHttpError<PipeError>> {
        let mut builder = request_to(
            Method::GET,
            &uri,
            Some(&session_id),
            auth_header,
            custom_headers,
        );
        if let Some(last_event_id) = last_event_id {
            builder = builder.header(LAST_EVENT_HEADER, last_event_id);
        }
        let request = builder
            .body(Full::new(Bytes::new()))
            .map_err(|error| StreamableHttpError::Client(PipeError::Http(error)))?;
        let response = self
            .computer
            .send(request)
            .await
            .map_err(StreamableHttpError::Client)?;
        if response.status() == StatusCode::METHOD_NOT_ALLOWED {
            return Err(StreamableHttpError::ServerDoesNotSupportSse);
        }
        if !response.status().is_success() {
            return Err(StreamableHttpError::UnexpectedServerResponse(
                format!("get_stream returned {}", response.status()).into(),
            ));
        }
        Ok(SseStream::new(response.into_body()).boxed())
    }
}

/// Opens every session of a computer client over the pipe to `computer`.
pub(super) struct PipeConnector {
    pub(super) computer: Arc<PipeComputer>,
}

#[async_trait]
impl Connector for PipeConnector {
    async fn connect(
        &self,
        transport: StreamableHttpClientTransportConfig,
    ) -> Result<Connection, String> {
        let client = PipeClient {
            computer: Arc::clone(&self.computer),
        };
        connect_transport(StreamableHttpClientTransport::with_client(
            client, transport,
        ))
        .await
    }
}
