//! The closed set of application frame kinds (ADR-137, "Decision" and
//! "Protocol contract completeness").

use serde::{Deserialize, Serialize};

use crate::version::ProtocolVersion;

/// A caller-generated operation id.
///
/// Unique across `request`, `subscribe`, and `unsubscribe` frames for the
/// lifetime of one connection. The server echoes it on the operation's
/// single terminal frame.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OperationId(pub String);

impl From<String> for OperationId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for OperationId {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl std::fmt::Display for OperationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A per-topic, server-assigned, strictly increasing resumption cursor.
pub type Cursor = u64;

/// The names of every frame kind, in the order ADR-137 lists them. Used to
/// drive the closed-set unknown-kind check in the codec, and exposed so a
/// caller can enumerate the protocol's frame vocabulary without matching on
/// [`Frame`] itself.
pub const FRAME_KINDS: &[&str] = &[
    "handshake",
    "handshake_ack",
    "request",
    "response",
    "error",
    "cancel",
    "subscribe",
    "subscribe_ack",
    "unsubscribe",
    "unsubscribe_ack",
    "event",
];

/// One application frame.
///
/// Every variant corresponds to exactly one entry in [`FRAME_KINDS`] and is
/// carried on the wire as a JSON object with a `"kind"` discriminant field
/// holding that variant's `snake_case` name, followed by the variant's own
/// fields flattened into the same object. This is the wire framing's
/// internally tagged encoding; see the crate documentation for a worked
/// example of the exact bytes.
///
/// The set is closed within a protocol version (ADR-137, "Decision"): a
/// decoder that encounters a `"kind"` value outside [`FRAME_KINDS`] must
/// reject the frame ([`crate::codec::CodecError::UnknownFrameKind`]), never
/// skip or ignore it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Frame {
    /// The first application frame on every connection. Names the protocol
    /// version the client supports.
    Handshake {
        /// The protocol version the client wants to speak.
        version: ProtocolVersion,
    },

    /// The server's acceptance of a [`Frame::Handshake`], naming the
    /// accepted protocol version.
    HandshakeAck {
        /// The protocol version the connection now speaks.
        version: ProtocolVersion,
    },

    /// A caller-issued operation: a DSL batch or chain (ADR-016) to execute.
    Request {
        /// Caller-generated, connection-unique operation id.
        id: OperationId,
        /// The request DSL string (ADR-016's function-call or JSON form).
        ops: String,
        /// Optional deadline in milliseconds, measured from server receipt
        /// of this frame against the server's monotonic clock. Scopes the
        /// entire request frame (the whole DSL batch or chain).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        deadline_ms: Option<u64>,
        /// Frame-level namespace override. Legal only on transports that
        /// accept caller-supplied identity context; a mapped transport
        /// (ADR-137's TCP transport) rejects any request carrying this with
        /// [`crate::error::WireErrorCode::ContextRejected`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        namespace: Option<String>,
        /// Frame-level actor override; see `namespace` above.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        actor_id: Option<String>,
        /// Frame-level visible-namespace-set override; see `namespace`
        /// above.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        visible_namespaces: Option<Vec<String>>,
    },

    /// The successful terminal frame for a `request`.
    Response {
        /// Echoes the originating `request`'s operation id.
        id: OperationId,
        /// The verb-dispatch result, exactly as ADR-016's `request` verb
        /// surface returns it (an aggregate `{ok, tool, result}` /
        /// `{ok, summary, ...}` payload). Opaque to this crate.
        result: serde_json::Value,
    },

    /// A wire-level failure terminal frame.
    Error {
        /// The operation id this error terminates, for a request-scoped
        /// error. `None` for a connection-terminal error, which carries no
        /// operation id (ADR-137, "Operation correlation").
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<OperationId>,
        /// The wire error code.
        code: crate::error::WireErrorCode,
        /// A human-readable detail message. Not part of the closed
        /// contract — callers must branch on `code`, never on this string.
        message: String,
    },

    /// Asks the server to terminate an in-flight `request`.
    Cancel {
        /// The `request` operation id to cancel. A `cancel` naming a
        /// subscribe/unsubscribe id, or an unknown or already-terminal
        /// request id, is a no-op.
        id: OperationId,
    },

    /// Opens delivery for one topic on the connection.
    Subscribe {
        /// Caller-generated, connection-unique operation id.
        id: OperationId,
        /// The topic to subscribe to, `<domain>.<event>`.
        topic: String,
        /// Resume position. Absent starts delivery at new events only;
        /// present replays every retained event with a cursor greater than
        /// this value before delivering new events.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume_cursor: Option<Cursor>,
    },

    /// The successful terminal frame for a `subscribe`.
    SubscribeAck {
        /// Echoes the originating `subscribe`'s operation id.
        id: OperationId,
        /// The subscribed topic.
        topic: String,
        /// The cursor position delivery begins after.
        start_cursor: Cursor,
    },

    /// Ends delivery for one topic on the connection.
    Unsubscribe {
        /// Caller-generated, connection-unique operation id.
        id: OperationId,
        /// The topic to unsubscribe from. Naming a topic with no active
        /// subscription is an idempotent no-op.
        topic: String,
    },

    /// The terminal frame for an `unsubscribe`.
    UnsubscribeAck {
        /// Echoes the originating `unsubscribe`'s operation id.
        id: OperationId,
        /// The unsubscribed topic.
        topic: String,
    },

    /// A server-pushed state-change delivery for a subscribed topic.
    ///
    /// Carries no operation id; correlated by topic and ordered by cursor
    /// instead.
    Event {
        /// The topic this event belongs to.
        topic: String,
        /// Server-assigned, per-topic, strictly increasing resumption
        /// cursor.
        cursor: Cursor,
        /// Server-assigned event time, RFC 3339.
        occurred_at: String,
        /// Topic-specific payload. Field-by-field shape is owned by the
        /// per-topic catalog (ADR-137, "Implementation-phase deliverables"),
        /// not by this crate.
        payload: serde_json::Value,
    },
}

impl Frame {
    /// The `snake_case` frame-kind name of this frame, matching its `"kind"`
    /// discriminant on the wire.
    pub const fn kind(&self) -> &'static str {
        match self {
            Frame::Handshake { .. } => "handshake",
            Frame::HandshakeAck { .. } => "handshake_ack",
            Frame::Request { .. } => "request",
            Frame::Response { .. } => "response",
            Frame::Error { .. } => "error",
            Frame::Cancel { .. } => "cancel",
            Frame::Subscribe { .. } => "subscribe",
            Frame::SubscribeAck { .. } => "subscribe_ack",
            Frame::Unsubscribe { .. } => "unsubscribe",
            Frame::UnsubscribeAck { .. } => "unsubscribe_ack",
            Frame::Event { .. } => "event",
        }
    }
}
