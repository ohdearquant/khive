//! The HTTP client the bridge reaches the caller's computer with (ADR-220
//! section 5): rmcp's streamable HTTP client protocol over reqwest, with every
//! response body bounded.
//!
//! rmcp's built-in reqwest client reads a non-success response body whole into
//! its error text and reads a JSON body with no bound (RUSTSEC-2026-0462). This
//! client counts the bytes of each body as they arrive and refuses a body that
//! passes its bound with [`ResponseTooLarge`]. A body is never cut short and
//! read as a result: it is either read whole within its bound or refused.
//!
//! - A successful `application/json` body: at most [`MAX_JSON_BODY`] bytes.
//! - The body of a non-success status: at most [`MAX_ERROR_BODY`] bytes.
//! - One event of a `text/event-stream` body: at most [`MAX_EVENT`] bytes.
//!
//! A `Content-Length` over the bound is refused before the body is read.
//! Redirects are not followed, so the key is never sent to another address.
#![cfg(unix)]

use std::borrow::Cow;
use std::collections::HashMap;
use std::error::Error as StdError;
use std::fmt;
use std::sync::Arc;

use futures::stream::{self, BoxStream, StreamExt};
use reqwest::header::{HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use reqwest::{Response, StatusCode};
use rmcp::model::{ClientJsonRpcMessage, JsonRpcMessage, ServerJsonRpcMessage};
use rmcp::transport::common::http_header::{
    EVENT_STREAM_MIME_TYPE, HEADER_LAST_EVENT_ID, HEADER_MCP_PROTOCOL_VERSION, HEADER_SESSION_ID,
    JSON_MIME_TYPE,
};
use rmcp::transport::streamable_http_client::{
    StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
};
use sse_stream::{Error as SseError, Sse, SseStream};

/// The largest successful `application/json` body read, in bytes.
pub(super) const MAX_JSON_BODY: usize = 16 * 1024 * 1024;
/// The largest body of a non-success status read, in bytes.
pub(super) const MAX_ERROR_BODY: usize = 64 * 1024;
/// The largest single event of an event stream read, in bytes. Line breaks
/// are not counted.
pub(super) const MAX_EVENT: usize = 16 * 1024 * 1024;

/// Which body passed its bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BodyKind {
    /// A successful `application/json` body.
    Json,
    /// The body of a non-success status.
    Error,
    /// One event of a `text/event-stream` body.
    Event,
}

/// A response body, or one event of it, that passed its bound. It was not
/// read past the bound and nothing of it is used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ResponseTooLarge {
    pub(super) kind: BodyKind,
    pub(super) limit: usize,
    /// The status the response carried.
    pub(super) status: u16,
}

impl fmt::Display for ResponseTooLarge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = match self.kind {
            BodyKind::Json => "a JSON response body",
            BodyKind::Error => "the body of an error response",
            BodyKind::Event => "an event of an event stream",
        };
        write!(
            f,
            "answered HTTP {} with {what} larger than {} bytes, which was not read",
            self.status, self.limit
        )
    }
}

impl StdError for ResponseTooLarge {}

/// An error of [`BoundedHttpClient`].
#[derive(Debug)]
pub(super) enum BoundedHttpError {
    Http(reqwest::Error),
    TooLarge(ResponseTooLarge),
}

impl fmt::Display for BoundedHttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(error) => write!(f, "{error}"),
            Self::TooLarge(error) => write!(f, "{error}"),
        }
    }
}

impl StdError for BoundedHttpError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Http(error) => Some(error),
            Self::TooLarge(error) => Some(error),
        }
    }
}

type ClientError = StreamableHttpError<BoundedHttpError>;

fn http_error(error: reqwest::Error) -> ClientError {
    StreamableHttpError::Client(BoundedHttpError::Http(error))
}

/// The streamable HTTP client of the computer bridge.
#[derive(Clone)]
pub(super) struct BoundedHttpClient {
    http: reqwest::Client,
}

impl BoundedHttpClient {
    pub(super) fn new() -> Result<Self, String> {
        reqwest::Client::builder()
            // An idle pooled connection whose last body was not read to the
            // end stalls the next request; rmcp's client pools none either.
            .pool_max_idle_per_host(0)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map(|http| Self { http })
            .map_err(|error| format!("the HTTP client could not be built: {error}"))
    }
}

/// Adds the headers the transport supplies. The ones this client sets itself
/// are refused, as rmcp's own client refuses them; the protocol version
/// header is the transport's to send.
fn with_custom_headers(
    mut request: reqwest::RequestBuilder,
    custom_headers: HashMap<HeaderName, HeaderValue>,
) -> Result<reqwest::RequestBuilder, ClientError> {
    for (name, value) in custom_headers {
        let reserved = ["accept", HEADER_SESSION_ID, HEADER_LAST_EVENT_ID]
            .iter()
            .any(|reserved| name.as_str().eq_ignore_ascii_case(reserved));
        if reserved
            && !name
                .as_str()
                .eq_ignore_ascii_case(HEADER_MCP_PROTOCOL_VERSION)
        {
            return Err(StreamableHttpError::ReservedHeaderConflict(
                name.to_string(),
            ));
        }
        request = request.header(name, value);
    }
    Ok(request)
}

fn content_type(response: &Response) -> Option<String> {
    response
        .headers()
        .get(CONTENT_TYPE)
        .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
}

fn is(content_type: Option<&str>, mime: &str) -> bool {
    content_type.is_some_and(|value| value.as_bytes().starts_with(mime.as_bytes()))
}

/// Reads the whole body of `response`, refusing it once it passes `limit`
/// bytes. A declared length over `limit` is refused before any of the body is
/// read.
async fn read_bounded(
    mut response: Response,
    kind: BodyKind,
    limit: usize,
) -> Result<Vec<u8>, ClientError> {
    let too_large = |status: StatusCode| {
        StreamableHttpError::Client(BoundedHttpError::TooLarge(ResponseTooLarge {
            kind,
            limit,
            status: status.as_u16(),
        }))
    };
    let status = response.status();
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(too_large(status));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(http_error)? {
        if chunk.len() > limit - body.len() {
            return Err(too_large(status));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// The bytes of the event an event stream is in the middle of, counted as
/// they arrive. An event ends at a blank line; line breaks are `\n`, `\r` or
/// `\r\n`.
#[derive(Debug, Default)]
struct EventBound {
    line: usize,
    event: usize,
    after_cr: bool,
}

impl EventBound {
    /// Whether `chunk` keeps every event within [`MAX_EVENT`].
    fn admit(&mut self, chunk: &[u8]) -> bool {
        for &byte in chunk {
            match byte {
                b'\n' if self.after_cr => self.after_cr = false,
                b'\n' | b'\r' => {
                    if self.line == 0 {
                        self.event = 0;
                    }
                    self.line = 0;
                    self.after_cr = byte == b'\r';
                }
                _ => {
                    self.after_cr = false;
                    self.line += 1;
                    self.event += 1;
                    if self.event > MAX_EVENT {
                        return false;
                    }
                }
            }
        }
        true
    }
}

/// The events of `response`, each refused once it passes [`MAX_EVENT`].
fn bounded_events(response: Response) -> BoxStream<'static, Result<Sse, SseError>> {
    let status = response.status().as_u16();
    let bytes = stream::try_unfold(
        (response, EventBound::default()),
        move |(mut response, mut bound)| async move {
            let Some(chunk) = response.chunk().await.map_err(BoundedHttpError::Http)? else {
                return Ok(None);
            };
            if !bound.admit(&chunk) {
                return Err(BoundedHttpError::TooLarge(ResponseTooLarge {
                    kind: BodyKind::Event,
                    limit: MAX_EVENT,
                    status,
                }));
            }
            Ok(Some((chunk, (response, bound))))
        },
    );
    SseStream::from_bytes_stream(bytes).boxed()
}

/// The JSON-RPC error a non-success body carries, if it is one.
fn json_rpc_error(body: &[u8]) -> Option<ServerJsonRpcMessage> {
    match serde_json::from_slice::<ServerJsonRpcMessage>(body) {
        Ok(message @ JsonRpcMessage::Error(_)) => Some(message),
        _ => None,
    }
}

impl StreamableHttpClient for BoundedHttpClient {
    type Error = BoundedHttpError;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, ClientError> {
        let mut request = self
            .http
            .post(uri.as_ref())
            .header(ACCEPT, [EVENT_STREAM_MIME_TYPE, JSON_MIME_TYPE].join(", "))
            .header(CONTENT_TYPE, JSON_MIME_TYPE);
        if let Some(token) = auth_header {
            request = request.bearer_auth(token);
        }
        request = with_custom_headers(request, custom_headers)?;
        let session_was_attached = session_id.is_some();
        if let Some(session_id) = session_id {
            request = request.header(HEADER_SESSION_ID, session_id.as_ref());
        }
        let body = serde_json::to_vec(&message)?;
        let response = request.body(body).send().await.map_err(http_error)?;

        let status = response.status();
        if matches!(status, StatusCode::ACCEPTED | StatusCode::NO_CONTENT) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if status == StatusCode::NOT_FOUND && session_was_attached {
            return Err(StreamableHttpError::SessionExpired);
        }
        let content_type = content_type(&response);
        let session_id = response
            .headers()
            .get(HEADER_SESSION_ID)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        // An empty 200 to a message that expects no answer is taken as 202.
        if status.is_success()
            && response.content_length() == Some(0)
            && matches!(
                message,
                ClientJsonRpcMessage::Notification(_)
                    | ClientJsonRpcMessage::Response(_)
                    | ClientJsonRpcMessage::Error(_)
            )
        {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if !status.is_success() {
            let body = read_bounded(response, BodyKind::Error, MAX_ERROR_BODY).await?;
            if is(content_type.as_deref(), JSON_MIME_TYPE) {
                if let Some(message) = json_rpc_error(&body) {
                    return Ok(StreamableHttpPostResponse::Json(message, session_id));
                }
            }
            return Err(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                format!("HTTP {status}: {}", String::from_utf8_lossy(&body)),
            )));
        }
        if is(content_type.as_deref(), EVENT_STREAM_MIME_TYPE) {
            return Ok(StreamableHttpPostResponse::Sse(
                bounded_events(response),
                session_id,
            ));
        }
        if is(content_type.as_deref(), JSON_MIME_TYPE) {
            let body = read_bounded(response, BodyKind::Json, MAX_JSON_BODY).await?;
            // As rmcp's own client does: a body that is not a JSON-RPC
            // message, such as a 200 to a notification, is taken as 202.
            return Ok(
                match serde_json::from_slice::<ServerJsonRpcMessage>(&body) {
                    Ok(message) => StreamableHttpPostResponse::Json(message, session_id),
                    Err(error) => {
                        tracing::warn!(
                            "computer answered JSON that is not a JSON-RPC message, taken as \
                             accepted: {error}"
                        );
                        StreamableHttpPostResponse::Accepted
                    }
                },
            );
        }
        Err(StreamableHttpError::UnexpectedContentType(content_type))
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), ClientError> {
        let mut request = self.http.delete(uri.as_ref());
        if let Some(token) = auth_header {
            request = request.bearer_auth(token);
        }
        request = request.header(HEADER_SESSION_ID, session_id.as_ref());
        request = with_custom_headers(request, custom_headers)?;
        let response = request.send().await.map_err(http_error)?;
        let status = response.status();
        if status == StatusCode::METHOD_NOT_ALLOWED || status.is_success() {
            return Ok(());
        }
        // The body of an error is not read: the status says enough.
        Err(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
            format!("HTTP {status} to the session's deletion"),
        )))
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, ClientError> {
        let mut request = self
            .http
            .get(uri.as_ref())
            .header(ACCEPT, [EVENT_STREAM_MIME_TYPE, JSON_MIME_TYPE].join(", "))
            .header(HEADER_SESSION_ID, session_id.as_ref());
        if let Some(last_event_id) = last_event_id {
            request = request.header(HEADER_LAST_EVENT_ID, last_event_id);
        }
        if let Some(token) = auth_header {
            request = request.bearer_auth(token);
        }
        request = with_custom_headers(request, custom_headers)?;
        let response = request.send().await.map_err(http_error)?;
        let status = response.status();
        if status == StatusCode::METHOD_NOT_ALLOWED {
            return Err(StreamableHttpError::ServerDoesNotSupportSse);
        }
        if !status.is_success() {
            // The body of an error is not read: the status says enough.
            return Err(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                format!("HTTP {status} to the event stream request"),
            )));
        }
        let content_type = content_type(&response);
        if !is(content_type.as_deref(), EVENT_STREAM_MIME_TYPE)
            && !is(content_type.as_deref(), JSON_MIME_TYPE)
        {
            return Err(StreamableHttpError::UnexpectedContentType(content_type));
        }
        Ok(bounded_events(response))
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;
    use reqwest::header::CONTENT_TYPE;
    use rmcp::transport::streamable_http_client::StreamableHttpError;
    use sse_stream::Error as SseError;

    use super::{
        bounded_events, read_bounded, BodyKind, BoundedHttpError, ClientError, EventBound,
        ResponseTooLarge, MAX_ERROR_BODY, MAX_EVENT, MAX_JSON_BODY,
    };

    fn response(status: u16, content_type: &str, body: Vec<u8>) -> reqwest::Response {
        http::Response::builder()
            .status(status)
            .header(CONTENT_TYPE, content_type)
            .body(body)
            .expect("a response")
            .into()
    }

    fn refusal(error: ClientError) -> ResponseTooLarge {
        match error {
            StreamableHttpError::Client(BoundedHttpError::TooLarge(refusal)) => refusal,
            other => panic!("not a refusal for size: {other}"),
        }
    }

    #[tokio::test]
    async fn a_json_body_at_its_bound_is_read_whole_and_one_byte_more_is_refused() {
        let at = response(200, "application/json", vec![b' '; MAX_JSON_BODY]);
        let body = read_bounded(at, BodyKind::Json, MAX_JSON_BODY)
            .await
            .expect("a body at the bound is read");
        assert_eq!(body.len(), MAX_JSON_BODY);

        let over = response(200, "application/json", vec![b' '; MAX_JSON_BODY + 1]);
        // Matched rather than `expect_err`, which would print the body.
        let Err(refused) = read_bounded(over, BodyKind::Json, MAX_JSON_BODY).await else {
            panic!("one byte over the bound was read");
        };
        assert_eq!(
            refusal(refused),
            ResponseTooLarge {
                kind: BodyKind::Json,
                limit: MAX_JSON_BODY,
                status: 200,
            }
        );
    }

    #[tokio::test]
    async fn an_error_body_past_its_bound_is_refused() {
        let at = response(500, "text/plain", vec![b'x'; MAX_ERROR_BODY]);
        let body = read_bounded(at, BodyKind::Error, MAX_ERROR_BODY)
            .await
            .expect("an error body at the bound is read");
        assert_eq!(body.len(), MAX_ERROR_BODY);

        let over = response(500, "text/plain", vec![b'x'; MAX_ERROR_BODY + 1]);
        let Err(refused) = read_bounded(over, BodyKind::Error, MAX_ERROR_BODY).await else {
            panic!("one byte over the bound was read");
        };
        let refused = refusal(refused);
        assert_eq!(refused.kind, BodyKind::Error);
        assert_eq!(refused.status, 500);
        let text = refused.to_string();
        assert!(text.contains("HTTP 500"), "{text}");
        assert!(text.contains(&MAX_ERROR_BODY.to_string()), "{text}");
    }

    fn event(data_len: usize) -> Vec<u8> {
        let mut body = b"data: ".to_vec();
        body.resize(body.len() + data_len, b'a');
        body.extend_from_slice(b"\n\n");
        body
    }

    #[tokio::test]
    async fn an_event_at_its_bound_is_parsed() {
        // "data: " is six bytes of the event.
        let at = response(200, "text/event-stream", event(MAX_EVENT - 6));
        let mut events = bounded_events(at);
        let first = events
            .next()
            .await
            .expect("one event")
            .expect("an event at the bound is parsed");
        assert_eq!(first.data.map(|data| data.len()), Some(MAX_EVENT - 6));
        assert!(events.next().await.is_none());
    }

    #[tokio::test]
    async fn an_event_past_its_bound_ends_the_stream_with_the_refusal() {
        let over = response(200, "text/event-stream", event(MAX_EVENT - 5));
        let mut events = bounded_events(over);
        let Some(Err(error)) = events.next().await else {
            panic!("an event past the bound was read");
        };
        let SseError::Body(inner) = error else {
            panic!("not a body error: {error}");
        };
        match inner.downcast_ref::<BoundedHttpError>() {
            Some(BoundedHttpError::TooLarge(refused)) => {
                assert_eq!(refused.kind, BodyKind::Event);
                assert_eq!(refused.limit, MAX_EVENT);
            }
            _ => panic!("not a refusal for size: {inner}"),
        }
        assert!(events.next().await.is_none(), "the stream ends");
    }

    #[test]
    fn an_event_at_the_bound_is_admitted_and_one_byte_more_is_not() {
        let mut bound = EventBound::default();
        let field = vec![b'a'; MAX_EVENT];
        assert!(bound.admit(&field));
        assert!(!bound.admit(b"a"));
    }

    #[test]
    fn a_blank_line_starts_a_new_event_for_every_line_break() {
        for blank in [&b"\n\n"[..], b"\r\r", b"\r\n\r\n"] {
            let mut bound = EventBound::default();
            assert!(bound.admit(&vec![b'a'; MAX_EVENT]));
            assert!(bound.admit(blank));
            assert!(bound.admit(&vec![b'b'; MAX_EVENT]), "{blank:?}");
        }
    }

    #[test]
    fn the_lines_of_one_event_count_together() {
        let mut bound = EventBound::default();
        let half = vec![b'a'; MAX_EVENT / 2];
        assert!(bound.admit(&half));
        assert!(bound.admit(b"\r\n"));
        assert!(bound.admit(&half));
        // A CRLF split across two chunks is still one line break.
        assert!(bound.admit(b"\r"));
        assert!(bound.admit(b"\n"));
        assert!(!bound.admit(b"a"));
    }
}
