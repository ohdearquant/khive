//! The closed set of application frame kinds (ADR-137, "Decision" and
//! "Protocol contract completeness").

use serde::de::{Error as DeError, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::version::ProtocolVersion;

/// A caller-generated operation id.
///
/// Unique across `request`, `subscribe`, and `unsubscribe` frames for the
/// lifetime of one connection. The server echoes it on the operation's
/// single terminal frame.
///
/// The codec rejects an EMPTY operation id at decode: an empty string can
/// never be a unique caller-generated id, so it is a frame-grammar
/// violation rather than a value the protocol has to give meaning to.
/// In-memory construction (`From<String>`, `From<&str>`) remains
/// unrestricted; only the wire form is validated.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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

impl Serialize for OperationId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for OperationId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        if value.is_empty() {
            return Err(D::Error::custom("operation id must be a non-empty string"));
        }
        Ok(Self(value))
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

/// The frame kinds a client may send to the server, in the direction each
/// is legal. The complement of this set within [`FRAME_KINDS`] —
/// `handshake_ack`, `response`, `error`, `subscribe_ack`, `unsubscribe_ack`,
/// and `event` — is server→client only; a server-side inbound gate rejects
/// them as grammar violations ([`crate::handshake::HandshakeGate`]).
pub const CLIENT_TO_SERVER_KINDS: &[&str] =
    &["handshake", "request", "cancel", "subscribe", "unsubscribe"];

/// The field set of a [`Frame::Handshake`] payload.
///
/// Strict within a protocol version: [`deny_unknown_fields`](serde) — a
/// payload carrying any field this struct does not declare is rejected at
/// decode (ADR-137's closed grammar; see the crate documentation's "Strict
/// field rejection" section).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandshakePayload {
    /// The protocol version the client wants to speak.
    pub version: ProtocolVersion,
}

/// The field set of a [`Frame::HandshakeAck`] payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandshakeAckPayload {
    /// The protocol version the connection now speaks.
    pub version: ProtocolVersion,
}

/// The field set of a [`Frame::Request`] payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestPayload {
    /// Caller-generated, connection-unique operation id.
    pub id: OperationId,
    /// The request DSL string (ADR-016's function-call or JSON form).
    pub ops: String,
    /// Optional deadline in milliseconds, measured from server receipt
    /// of this frame against the server's monotonic clock. Scopes the
    /// entire request frame (the whole DSL batch or chain).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
    /// Frame-level namespace override. Legal only on transports that
    /// accept caller-supplied identity context; a mapped transport
    /// (ADR-137's TCP transport) rejects any request carrying this with
    /// [`crate::error::WireErrorCode::ContextRejected`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// Frame-level actor override; see `namespace` above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<String>,
    /// Frame-level visible-namespace-set override; see `namespace`
    /// above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_namespaces: Option<Vec<String>>,
}

/// The field set of a [`Frame::Response`] payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponsePayload {
    /// Echoes the originating `request`'s operation id.
    pub id: OperationId,
    /// The verb-dispatch result, exactly as ADR-016's `request` verb
    /// surface returns it (an aggregate `{ok, tool, result}` /
    /// `{ok, summary, ...}` payload). Opaque to this crate.
    pub result: serde_json::Value,
}

/// The field set of a [`Frame::Error`] payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorPayload {
    /// The operation id this error terminates, for a request-scoped
    /// error. `None` for a connection-terminal error, which carries no
    /// operation id (ADR-137, "Operation correlation").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<OperationId>,
    /// The wire error code.
    pub code: crate::error::WireErrorCode,
    /// A human-readable detail message. Not part of the closed
    /// contract — callers must branch on `code`, never on this string.
    pub message: String,
}

/// The field set of a [`Frame::Cancel`] payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelPayload {
    /// The `request` operation id to cancel. A `cancel` naming a
    /// subscribe/unsubscribe id, or an unknown or already-terminal
    /// request id, is a no-op.
    pub id: OperationId,
}

/// The field set of a [`Frame::Subscribe`] payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubscribePayload {
    /// Caller-generated, connection-unique operation id.
    pub id: OperationId,
    /// The topic to subscribe to, `<domain>.<event>`.
    pub topic: String,
    /// Resume position. Absent starts delivery at new events only;
    /// present replays every retained event with a cursor greater than
    /// this value before delivering new events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_cursor: Option<Cursor>,
}

/// The field set of a [`Frame::SubscribeAck`] payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubscribeAckPayload {
    /// Echoes the originating `subscribe`'s operation id.
    pub id: OperationId,
    /// The subscribed topic.
    pub topic: String,
    /// The cursor position delivery begins after.
    pub start_cursor: Cursor,
}

/// The field set of a [`Frame::Unsubscribe`] payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnsubscribePayload {
    /// Caller-generated, connection-unique operation id.
    pub id: OperationId,
    /// The topic to unsubscribe from. Naming a topic with no active
    /// subscription is an idempotent no-op.
    pub topic: String,
}

/// The field set of a [`Frame::UnsubscribeAck`] payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnsubscribeAckPayload {
    /// Echoes the originating `unsubscribe`'s operation id.
    pub id: OperationId,
    /// The unsubscribed topic.
    pub topic: String,
}

/// The field set of a [`Frame::Event`] payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventPayload {
    /// The topic this event belongs to.
    pub topic: String,
    /// Server-assigned, per-topic, strictly increasing resumption
    /// cursor.
    pub cursor: Cursor,
    /// Server-assigned event time, RFC 3339.
    pub occurred_at: String,
    /// Topic-specific payload. Field-by-field shape is owned by the
    /// per-topic catalog (ADR-137, "Implementation-phase deliverables"),
    /// not by this crate.
    pub payload: serde_json::Value,
}

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
///
/// Serde is implemented by hand (rather than `#[serde(tag = "kind")]`) so
/// the decode path can enforce the closed grammar ADR-137 requires: the
/// codec checks the `"kind"` discriminant against [`FRAME_KINDS`] itself,
/// then hands the payload to the matching kind's payload struct
/// ([`HandshakePayload`], [`RequestPayload`], ...), every one of which
/// carries `#[serde(deny_unknown_fields)]`. A payload with any field its
/// kind does not declare is therefore rejected — never silently ignored —
/// and a missing or non-string `"kind"` is rejected before any kind is
/// matched. Encoding writes `"kind"` first, then the kind's fields in
/// declaration order, skipping absent optional fields; this is the exact
/// byte layout the golden fixtures pin.
#[derive(Debug, Clone, PartialEq)]
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
        deadline_ms: Option<u64>,
        /// Frame-level namespace override. Legal only on transports that
        /// accept caller-supplied identity context; a mapped transport
        /// (ADR-137's TCP transport) rejects any request carrying this with
        /// [`crate::error::WireErrorCode::ContextRejected`].
        namespace: Option<String>,
        /// Frame-level actor override; see `namespace` above.
        actor_id: Option<String>,
        /// Frame-level visible-namespace-set override; see `namespace`
        /// above.
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

/// Serialize one frame as its wire object: `"kind"` first, then the kind's
/// fields in declaration order, absent optional fields skipped. The byte
/// layout is pinned by the golden fixtures in `tests/fixtures/*.hex`.
impl Serialize for Frame {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let size_hint = match self {
            Frame::Handshake { .. } | Frame::HandshakeAck { .. } | Frame::Cancel { .. } => 2,
            Frame::Subscribe { resume_cursor, .. } => {
                if resume_cursor.is_some() {
                    4
                } else {
                    3
                }
            }
            Frame::Request {
                deadline_ms,
                namespace,
                actor_id,
                visible_namespaces,
                ..
            } => {
                3 + usize::from(deadline_ms.is_some())
                    + usize::from(namespace.is_some())
                    + usize::from(actor_id.is_some())
                    + usize::from(visible_namespaces.is_some())
            }
            Frame::Error { id, .. } => 3 + usize::from(id.is_some()),
            Frame::Response { .. } | Frame::Unsubscribe { .. } | Frame::UnsubscribeAck { .. } => 3,
            Frame::SubscribeAck { .. } => 4,
            Frame::Event { .. } => 5,
        };
        let mut map = serializer.serialize_map(Some(size_hint))?;
        map.serialize_entry("kind", self.kind())?;
        match self {
            Frame::Handshake { version } => {
                map.serialize_entry("version", version)?;
            }
            Frame::HandshakeAck { version } => {
                map.serialize_entry("version", version)?;
            }
            Frame::Request {
                id,
                ops,
                deadline_ms,
                namespace,
                actor_id,
                visible_namespaces,
            } => {
                map.serialize_entry("id", id)?;
                map.serialize_entry("ops", ops)?;
                if let Some(deadline_ms) = deadline_ms {
                    map.serialize_entry("deadline_ms", deadline_ms)?;
                }
                if let Some(namespace) = namespace {
                    map.serialize_entry("namespace", namespace)?;
                }
                if let Some(actor_id) = actor_id {
                    map.serialize_entry("actor_id", actor_id)?;
                }
                if let Some(visible_namespaces) = visible_namespaces {
                    map.serialize_entry("visible_namespaces", visible_namespaces)?;
                }
            }
            Frame::Response { id, result } => {
                map.serialize_entry("id", id)?;
                map.serialize_entry("result", result)?;
            }
            Frame::Error { id, code, message } => {
                if let Some(id) = id {
                    map.serialize_entry("id", id)?;
                }
                map.serialize_entry("code", code)?;
                map.serialize_entry("message", message)?;
            }
            Frame::Cancel { id } => {
                map.serialize_entry("id", id)?;
            }
            Frame::Subscribe {
                id,
                topic,
                resume_cursor,
            } => {
                map.serialize_entry("id", id)?;
                map.serialize_entry("topic", topic)?;
                if let Some(resume_cursor) = resume_cursor {
                    map.serialize_entry("resume_cursor", resume_cursor)?;
                }
            }
            Frame::SubscribeAck {
                id,
                topic,
                start_cursor,
            } => {
                map.serialize_entry("id", id)?;
                map.serialize_entry("topic", topic)?;
                map.serialize_entry("start_cursor", start_cursor)?;
            }
            Frame::Unsubscribe { id, topic } => {
                map.serialize_entry("id", id)?;
                map.serialize_entry("topic", topic)?;
            }
            Frame::UnsubscribeAck { id, topic } => {
                map.serialize_entry("id", id)?;
                map.serialize_entry("topic", topic)?;
            }
            Frame::Event {
                topic,
                cursor,
                occurred_at,
                payload,
            } => {
                map.serialize_entry("topic", topic)?;
                map.serialize_entry("cursor", cursor)?;
                map.serialize_entry("occurred_at", occurred_at)?;
                map.serialize_entry("payload", payload)?;
            }
        }
        map.end()
    }
}

/// The decode half of the closed grammar; see the type-level docs and the
/// crate documentation's "Strict field rejection" section. The codec's
/// [`crate::codec::decode_payload`] drives this via
/// `serde_json::from_value::<Frame>` after its own closed-set `"kind"`
/// check (which produces the finer-grained
/// [`crate::codec::CodecError::UnknownFrameKind`]).
impl<'de> Deserialize<'de> for Frame {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct FrameVisitor;

        impl<'de> Visitor<'de> for FrameVisitor {
            type Value = Frame;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a JSON object with a string \"kind\" discriminant field")
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Frame, A::Error> {
                // Buffer the whole object, then inspect `"kind"` before any
                // per-kind parsing: the closed-set check needs the
                // discriminant up front, and the per-kind payload structs
                // (all `deny_unknown_fields`) must never see the `"kind"`
                // key itself.
                let mut object: serde_json::Map<String, serde_json::Value> =
                    Deserialize::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
                let kind_value = object
                    .get("kind")
                    .ok_or_else(|| A::Error::custom("missing field `kind`"))?;
                let kind = kind_value
                    .as_str()
                    .ok_or_else(|| A::Error::custom("field `kind` must be a string"))?;
                let kind = kind.to_string();
                object.remove("kind");

                fn parse<T: serde::de::DeserializeOwned>(
                    object: &serde_json::Map<String, serde_json::Value>,
                ) -> Result<T, serde_json::Error> {
                    serde_json::from_value(serde_json::Value::Object(object.clone()))
                }

                match kind.as_str() {
                    "handshake" => {
                        let payload: HandshakePayload = parse(&object).map_err(A::Error::custom)?;
                        Ok(Frame::Handshake {
                            version: payload.version,
                        })
                    }
                    "handshake_ack" => {
                        let payload: HandshakeAckPayload =
                            parse(&object).map_err(A::Error::custom)?;
                        Ok(Frame::HandshakeAck {
                            version: payload.version,
                        })
                    }
                    "request" => {
                        let payload: RequestPayload = parse(&object).map_err(A::Error::custom)?;
                        Ok(Frame::Request {
                            id: payload.id,
                            ops: payload.ops,
                            deadline_ms: payload.deadline_ms,
                            namespace: payload.namespace,
                            actor_id: payload.actor_id,
                            visible_namespaces: payload.visible_namespaces,
                        })
                    }
                    "response" => {
                        let payload: ResponsePayload = parse(&object).map_err(A::Error::custom)?;
                        Ok(Frame::Response {
                            id: payload.id,
                            result: payload.result,
                        })
                    }
                    "error" => {
                        let payload: ErrorPayload = parse(&object).map_err(A::Error::custom)?;
                        Ok(Frame::Error {
                            id: payload.id,
                            code: payload.code,
                            message: payload.message,
                        })
                    }
                    "cancel" => {
                        let payload: CancelPayload = parse(&object).map_err(A::Error::custom)?;
                        Ok(Frame::Cancel { id: payload.id })
                    }
                    "subscribe" => {
                        let payload: SubscribePayload = parse(&object).map_err(A::Error::custom)?;
                        Ok(Frame::Subscribe {
                            id: payload.id,
                            topic: payload.topic,
                            resume_cursor: payload.resume_cursor,
                        })
                    }
                    "subscribe_ack" => {
                        let payload: SubscribeAckPayload =
                            parse(&object).map_err(A::Error::custom)?;
                        Ok(Frame::SubscribeAck {
                            id: payload.id,
                            topic: payload.topic,
                            start_cursor: payload.start_cursor,
                        })
                    }
                    "unsubscribe" => {
                        let payload: UnsubscribePayload =
                            parse(&object).map_err(A::Error::custom)?;
                        Ok(Frame::Unsubscribe {
                            id: payload.id,
                            topic: payload.topic,
                        })
                    }
                    "unsubscribe_ack" => {
                        let payload: UnsubscribeAckPayload =
                            parse(&object).map_err(A::Error::custom)?;
                        Ok(Frame::UnsubscribeAck {
                            id: payload.id,
                            topic: payload.topic,
                        })
                    }
                    "event" => {
                        let payload: EventPayload = parse(&object).map_err(A::Error::custom)?;
                        Ok(Frame::Event {
                            topic: payload.topic,
                            cursor: payload.cursor,
                            occurred_at: payload.occurred_at,
                            payload: payload.payload,
                        })
                    }
                    // The codec's closed-set check against `FRAME_KINDS`
                    // rejects unknown kinds with `UnknownFrameKind` before
                    // this point; this arm only fires for a direct
                    // `serde_json::from_value::<Frame>` call.
                    other => Err(A::Error::custom(format!("unknown frame kind: {other:?}"))),
                }
            }
        }

        deserializer.deserialize_map(FrameVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_operation_id_is_rejected_at_deserialization() {
        // Item 7: an empty operation id can never be a unique caller-
        // generated id, so the wire form rejects it. In-memory
        // construction stays unrestricted (checked below).
        let err = serde_json::from_str::<OperationId>(r#""""#).unwrap_err();
        assert!(
            err.to_string().contains("non-empty"),
            "unexpected error: {err}"
        );
        assert_eq!(OperationId::from(""), OperationId("".to_string()));
    }

    #[test]
    fn non_empty_operation_id_round_trips() {
        let id: OperationId = serde_json::from_str(r#""op-1""#).unwrap();
        assert_eq!(id, OperationId::from("op-1"));
        assert_eq!(serde_json::to_string(&id).unwrap(), r#""op-1""#);
    }

    #[test]
    fn unknown_kind_through_direct_serde_is_rejected() {
        let err = serde_json::from_str::<Frame>(r#"{"kind":"ping"}"#).unwrap_err();
        assert!(err.to_string().contains("unknown frame kind"));
    }
}
