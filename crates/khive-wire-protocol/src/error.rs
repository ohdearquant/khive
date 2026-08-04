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
/// The wire (`snake_case`) names of every code in the closed set for
/// protocol version 1, in ADR-137's "Wire error taxonomy" table order.
///
/// The codec's decode-time error-scope check applies only to codes in this
/// set: their terminal scopes are fixed by the table. A code outside the
/// set fell back to [`WireErrorCode::Internal`] via `#[serde(other)]`; its
/// true scope is unknown to this protocol version, and ADR-137 directs the
/// client to treat it as `internal` rather than reject the frame.
pub const WIRE_ERROR_CODES: &[&str] = &[
    "unsupported_version",
    "identity_rejected",
    "malformed_frame",
    "frame_too_large",
    "subscriber_overflow",
    "subscription_revoked",
    "context_rejected",
    "peer_class_denied",
    "subscription_denied",
    "already_subscribed",
    "cursor_expired",
    "in_flight_limit_exceeded",
    "deadline_exceeded",
    "cancelled",
    "shutting_down",
    "internal",
];

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unrecognized_code_falls_back_to_internal() {
        // ADR-137: a code this client does not recognize must decode as
        // `internal` — request-terminal — rather than failing to decode.
        let code: WireErrorCode = serde_json::from_str(r#""bogus""#).unwrap();
        assert_eq!(code, WireErrorCode::Internal);
        assert_eq!(code.terminal_scope(), TerminalScope::Request);
    }

    #[test]
    fn unrecognized_code_inside_an_error_frame_falls_back_to_internal() {
        let payload = br#"{"kind":"error","code":"bogus","message":"future code"}"#;
        let frame = crate::codec::decode_payload(payload).unwrap();
        match frame {
            crate::frame::Frame::Error { id, code, .. } => {
                assert_eq!(code, WireErrorCode::Internal);
                assert!(id.is_none());
            }
            other => panic!("expected an error frame, got {other:?}"),
        }
    }

    #[test]
    fn wire_error_codes_matches_the_code_enum() {
        // `WIRE_ERROR_CODES` gates the codec's decode-time error-scope
        // check: a code missing from the set silently takes the
        // unknown-code exemption and its scope pairing is never enforced.
        // The set must therefore cover every `WireErrorCode` variant
        // exactly.
        //
        // Set side, fully mechanical: every entry must round-trip through
        // serde to a variant whose `as_str` equals the entry. A typo'd or
        // stale entry deserializes to `Internal` via `#[serde(other)]` and
        // fails the `as_str` comparison.
        for entry in WIRE_ERROR_CODES {
            let json = format!("\"{entry}\"");
            let parsed: WireErrorCode = serde_json::from_str(&json).unwrap();
            assert_eq!(
                parsed.as_str(),
                *entry,
                "WIRE_ERROR_CODES entry {entry:?} names no variant \
                 (deserialized to {parsed:?})"
            );
        }
        let mut deduped: Vec<&str> = WIRE_ERROR_CODES.to_vec();
        deduped.sort_unstable();
        deduped.dedup();
        assert_eq!(
            deduped.len(),
            WIRE_ERROR_CODES.len(),
            "WIRE_ERROR_CODES contains duplicates"
        );

        // Variant side: the list below mirrors the enum; the wildcard-free
        // match forces whoever adds a variant to visit this site (the
        // compiler cannot force the array itself). Each variant must be in
        // the set, and equal counts close the loop.
        use WireErrorCode::*;
        let variants = [
            UnsupportedVersion,
            IdentityRejected,
            MalformedFrame,
            FrameTooLarge,
            SubscriberOverflow,
            SubscriptionRevoked,
            ContextRejected,
            PeerClassDenied,
            SubscriptionDenied,
            AlreadySubscribed,
            CursorExpired,
            InFlightLimitExceeded,
            DeadlineExceeded,
            Cancelled,
            ShuttingDown,
            Internal,
        ];
        for code in variants {
            match code {
                UnsupportedVersion
                | IdentityRejected
                | MalformedFrame
                | FrameTooLarge
                | SubscriberOverflow
                | SubscriptionRevoked
                | ContextRejected
                | PeerClassDenied
                | SubscriptionDenied
                | AlreadySubscribed
                | CursorExpired
                | InFlightLimitExceeded
                | DeadlineExceeded
                | Cancelled
                | ShuttingDown
                | Internal => {}
            }
            assert!(
                WIRE_ERROR_CODES.contains(&code.as_str()),
                "WIRE_ERROR_CODES is missing {code:?} ({}); its scope pairing \
                 would silently go unenforced at decode",
                code.as_str()
            );
        }
        assert_eq!(
            WIRE_ERROR_CODES.len(),
            variants.len(),
            "WIRE_ERROR_CODES and the variant list disagree on count"
        );
    }
}
