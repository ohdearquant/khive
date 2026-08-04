//! The wire error taxonomy from ADR-137's "Wire error taxonomy" section.
//!
//! Wire-level errors are distinct from verb-level DSL errors (ADR-016): a
//! wire error is returned before or instead of DSL dispatch, while a
//! DSL-level per-operation error is a separate failure mode carried inside a
//! successful [`crate::frame::Frame::Response`] frame's payload.

use serde::{Deserialize, Serialize};

/// Where a wire error terminates: the whole connection, or just the
/// operation id it echoes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalScope {
    /// The error is followed by connection close. It carries no operation
    /// id.
    Connection,
    /// The error terminates only the operation id it echoes (a request,
    /// subscribe, or unsubscribe id); the connection stays usable.
    Request,
}

/// The closed set of wire-level error codes for protocol version 1
/// (ADR-137, "Wire error taxonomy").
///
/// The set is closed within a protocol version: adding a code requires a
/// protocol version bump ([`crate::version`]). Serialized names are
/// `snake_case` and match the ADR exactly.
///
/// A decoder that encounters a code string it does not recognize falls back
/// to [`WireErrorCode::Internal`] per the ADR: *"a client that receives a
/// code it does not recognize must treat it as `internal` — request-terminal,
/// retriable only under the caller's own policy — rather than inventing
/// semantics for it."* This is implemented with `#[serde(other)]`, so an
/// older client talking to a server that has gained a new code under a later
/// protocol version degrades to this documented behavior instead of a decode
/// failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireErrorCode {
    /// Handshake named no mutually supported protocol version. Connection.
    UnsupportedVersion,
    /// Whois lookup failed, no or invalid node mapping, or native-frame
    /// ingress attempted by a class without native access. Connection.
    IdentityRejected,
    /// A frame cannot be decoded or violates the frame grammar. Connection.
    MalformedFrame,
    /// A frame exceeds the configured maximum size. Connection.
    FrameTooLarge,
    /// The connection's bounded outbound event queue reached its limit.
    /// Connection.
    SubscriberOverflow,
    /// A live mapping or peer-class change short of mapping deletion removed
    /// authorization for a topic with an active subscription on this
    /// connection. Connection.
    SubscriptionRevoked,
    /// A frame-level attempt to supply `namespace`, `actor_id`, or
    /// `visible_namespaces` on a mapped transport. Request.
    ContextRejected,
    /// A parsed operation names a verb outside the mapped class allowlist.
    /// Request.
    PeerClassDenied,
    /// A `subscribe` names a topic outside the mapped class's allowed-topic
    /// set. Request.
    SubscriptionDenied,
    /// A `subscribe` names a topic that already has an active subscription
    /// on this connection. Request.
    AlreadySubscribed,
    /// A `subscribe` resume cursor is older than the topic's retention
    /// window. Request.
    CursorExpired,
    /// A `request` arrives while the per-connection in-flight limit is
    /// reached. Request.
    InFlightLimitExceeded,
    /// The server abandoned the request at its deadline. Request.
    DeadlineExceeded,
    /// The request was terminated by a `cancel` frame before its normal
    /// completion. Request.
    Cancelled,
    /// The server is draining and refuses new work. Request.
    ShuttingDown,
    /// An unclassified server-side transport failure. Request. Also the
    /// fallback for an error code this client does not recognize.
    #[serde(other)]
    Internal,
}

impl WireErrorCode {
    /// The terminal scope defined for this code in ADR-137's error table.
    pub const fn terminal_scope(self) -> TerminalScope {
        use WireErrorCode::*;
        match self {
            UnsupportedVersion | IdentityRejected | MalformedFrame | FrameTooLarge
            | SubscriberOverflow | SubscriptionRevoked => TerminalScope::Connection,
            ContextRejected
            | PeerClassDenied
            | SubscriptionDenied
            | AlreadySubscribed
            | CursorExpired
            | InFlightLimitExceeded
            | DeadlineExceeded
            | Cancelled
            | ShuttingDown
            | Internal => TerminalScope::Request,
        }
    }

    /// The wire (`snake_case`) name of this code, as it appears on the wire
    /// and in ADR-137's table.
    pub const fn as_str(self) -> &'static str {
        use WireErrorCode::*;
        match self {
            UnsupportedVersion => "unsupported_version",
            IdentityRejected => "identity_rejected",
            MalformedFrame => "malformed_frame",
            FrameTooLarge => "frame_too_large",
            SubscriberOverflow => "subscriber_overflow",
            SubscriptionRevoked => "subscription_revoked",
            ContextRejected => "context_rejected",
            PeerClassDenied => "peer_class_denied",
            SubscriptionDenied => "subscription_denied",
            AlreadySubscribed => "already_subscribed",
            CursorExpired => "cursor_expired",
            InFlightLimitExceeded => "in_flight_limit_exceeded",
            DeadlineExceeded => "deadline_exceeded",
            Cancelled => "cancelled",
            ShuttingDown => "shutting_down",
            Internal => "internal",
        }
    }
}

impl std::fmt::Display for WireErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
