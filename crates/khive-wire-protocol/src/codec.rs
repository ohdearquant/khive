//! The length-prefixed framing codec.
//!
//! One wire frame is a 4-byte big-endian `u32` length prefix followed by
//! that many bytes of JSON, matching the existing Unix-domain-socket framing
//! this crate remains base-compatible with (`crates/khive-runtime/src/daemon.rs`,
//! `read_frame`/`write_frame`). This crate defines what the JSON payload
//! inside that framing is; it performs no I/O itself — [`decode_frame`] and
//! [`encode_frame`] operate on in-memory byte slices, and a transport crate
//! is responsible for reading/writing those bytes from a socket and for
//! buffering partial reads until a complete frame is available.

use crate::frame::Frame;

/// Length of the big-endian `u32` frame-length prefix, in bytes.
pub const LENGTH_PREFIX_BYTES: usize = 4;

/// The fixed wording every id/scope rejection carries, whichever decode
/// path produced it: the serde visitor in [`crate::frame`] (which
/// [`decode_payload`] drives, and which a direct
/// `serde_json::from_str::<Frame>` reaches too) reports the rule through
/// the deserializer's error type, and [`decode_payload`] re-classifies a
/// message carrying this prefix into
/// [`CodecError::InconsistentErrorScope`]. Keeping one wording lets both
/// decode paths enforce the ADR-137 rule without duplicating its logic.
pub(crate) const INCONSISTENT_SCOPE_ERROR_PREFIX: &str = "error frame violates the id/scope rule: ";

/// Default maximum frame size: 8 MiB.
///
/// Chosen to match the existing Unix-domain-socket transport's
/// `MAX_FRAME_BYTES` (`crates/khive-runtime/src/daemon.rs:38`) so that
/// migrating a connection from that framing to this crate's codec does not
/// silently tighten or loosen the limit. ADR-137 assigns the exact default
/// to this crate as an implementation-phase deliverable ("Implementation-phase
/// deliverables"); deployments that need a different bound configure
/// [`FrameCodec::max_frame_bytes`] explicitly.
pub const DEFAULT_MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// A codec failure, distinguishing every malformed-frame case this crate
/// can identify. Every variant maps onto the wire error taxonomy —
/// [`crate::error::WireErrorCode::MalformedFrame`] or
/// [`crate::error::WireErrorCode::FrameTooLarge`] — canonically via
/// [`wire_code`](Self::wire_code) (and the `From<&CodecError>` impl), so
/// servers need no hand-maintained mapping; this crate keeps the
/// finer-grained variant so a caller (including a test) can assert the
/// specific failure rather than only "decoding failed".
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CodecError {
    /// Fewer than [`LENGTH_PREFIX_BYTES`] bytes were supplied — the 4-byte
    /// length prefix itself is truncated.
    #[error("truncated length prefix: got {available} of {LENGTH_PREFIX_BYTES} bytes")]
    TruncatedLengthPrefix { available: usize },

    /// The length prefix names a payload longer than what was supplied.
    #[error("truncated payload: declared {declared} bytes, got {available}")]
    TruncatedPayload { declared: usize, available: usize },

    /// The declared frame length exceeds the configured maximum
    /// (payload-only; the 4-byte prefix never counts against it). `max`
    /// is always the configured bound that was exceeded.
    #[error("frame of {declared} bytes exceeds the {max} byte maximum")]
    FrameTooLarge { declared: usize, max: usize },

    /// The serialized payload exceeds the 4-byte length prefix's inherent
    /// `u32::MAX` byte capacity. Distinct from [`FrameTooLarge`](Self::FrameTooLarge): this arm
    /// can only fire when the configured maximum is at least the payload
    /// length (otherwise [`FrameTooLarge`](Self::FrameTooLarge) fires first), so the configured
    /// maximum was NOT the binding limit — the prefix capacity was, and
    /// the error names it.
    #[error("frame of {declared} bytes exceeds the u32 length prefix's {max} byte capacity")]
    U32PrefixLimitExceeded { declared: usize, max: usize },

    /// Decode side: the payload bytes are not valid JSON at all. Encode
    /// side: the frame failed to serialize (only reachable for opaque
    /// payloads that cannot be represented, e.g. a map with non-string
    /// keys smuggled into `response.result`).
    #[error("payload is not valid JSON: {0}")]
    InvalidJson(String),

    /// The payload is valid JSON but not a JSON object, or has no `"kind"`
    /// string field (absent or not a string).
    #[error("payload has no string \"kind\" discriminant field")]
    MissingKind,

    /// The `"kind"` field names a value outside the closed
    /// [`crate::frame::FRAME_KINDS`] set.
    #[error("unknown frame kind: {0:?}")]
    UnknownFrameKind(String),

    /// The frame's `"kind"` was recognized but a field required by that
    /// frame kind's shape is missing, has the wrong type, or is unknown to
    /// the closed grammar (strict field rejection — see the crate
    /// documentation's "Strict field rejection" section). Encode side: an
    /// operation id field carries the empty string, which the wire grammar
    /// forbids ([`crate::frame::OperationId`]).
    #[error("frame kind {kind:?}: {detail}")]
    InvalidFields { kind: String, detail: String },

    /// An `error` frame whose operation-id presence contradicts its code's
    /// terminal scope (ADR-137, "Operation correlation"): a
    /// connection-terminal code carries no operation id, and a
    /// request-terminal code echoes the one it terminates. Enforced at
    /// decode — inside [`crate::frame::Frame`]'s serde visitor, so BOTH
    /// the codec's `decode_payload` and a direct
    /// `serde_json::from_str::<Frame>` reject it (the codec re-classifies
    /// the visitor's message into this typed variant) — AND at encode
    /// ([`encode_frame_with_max`]).
    #[error("error frame violates the id/scope rule: {detail}")]
    InconsistentErrorScope { detail: String },

    /// Encode side: the frame is a decoded unknown-code fallback — a
    /// [`crate::frame::Frame::Error`] carrying a `Some`
    /// `unrecognized_code`, which only a decode path sets when the wire
    /// carried a code outside the closed set. Fallback frames are
    /// TERMINAL FOR RELAY: re-encoding one would emit the fallback code
    /// (`internal`) and silently discard the newer code the peer sent, so
    /// the encode path rejects the frame outright rather than corrupt it.
    /// If a relay must pass unknown codes through, it has to operate on
    /// the raw frame bytes, not on a decoded-and-re-encoded frame.
    #[error(
        "fallback error frame (unrecognized code {code:?}) is not re-encodable: \
         re-encoding would emit \"internal\" and discard the newer wire code"
    )]
    FallbackFrameNotEncodable { code: String },
}

impl CodecError {
    /// The wire error code this failure maps to under ADR-137's taxonomy:
    /// [`FrameTooLarge`](Self::FrameTooLarge) and
    /// [`U32PrefixLimitExceeded`](Self::U32PrefixLimitExceeded) — the size
    /// failures — map to [`WireErrorCode::FrameTooLarge`]; every other
    /// variant maps to [`WireErrorCode::MalformedFrame`]. This is the
    /// canonical codec→wire-error mapping: servers should use it (or the
    /// `From<&CodecError>` impl) rather than hand-maintain their own.
    ///
    /// [`WireErrorCode::FrameTooLarge`]: crate::error::WireErrorCode::FrameTooLarge
    /// [`WireErrorCode::MalformedFrame`]: crate::error::WireErrorCode::MalformedFrame
    pub const fn wire_code(&self) -> crate::error::WireErrorCode {
        match self {
            CodecError::FrameTooLarge { .. } | CodecError::U32PrefixLimitExceeded { .. } => {
                crate::error::WireErrorCode::FrameTooLarge
            }
            CodecError::TruncatedLengthPrefix { .. }
            | CodecError::TruncatedPayload { .. }
            | CodecError::InvalidJson(_)
            | CodecError::MissingKind
            | CodecError::UnknownFrameKind(_)
            | CodecError::InvalidFields { .. }
            | CodecError::InconsistentErrorScope { .. }
            | CodecError::FallbackFrameNotEncodable { .. } => {
                crate::error::WireErrorCode::MalformedFrame
            }
        }
    }
}

impl From<&CodecError> for crate::error::WireErrorCode {
    fn from(err: &CodecError) -> Self {
        err.wire_code()
    }
}

/// A configured framing codec.
///
/// Stateless beyond the configured [`max_frame_bytes`](Self::max_frame_bytes)
/// bound: [`encode`](Self::encode) and [`decode`](Self::decode) operate on
/// one frame at a time and hold no connection state (handshake sequencing
/// is [`crate::handshake::HandshakeGate`]'s job, not the codec's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameCodec {
    max_frame_bytes: usize,
}

impl FrameCodec {
    /// A codec with the given maximum frame size (the JSON payload length,
    /// not counting the 4-byte prefix).
    pub const fn new(max_frame_bytes: usize) -> Self {
        Self { max_frame_bytes }
    }

    pub const fn max_frame_bytes(&self) -> usize {
        self.max_frame_bytes
    }

    /// Encode one frame as a complete length-prefixed wire buffer: 4-byte
    /// BE length followed by the JSON payload, guarded by this codec's own
    /// configured maximum so encode and decode share one bound.
    pub fn encode(&self, frame: &Frame) -> Result<Vec<u8>, CodecError> {
        encode_frame_with_max(frame, self.max_frame_bytes)
    }

    /// Decode one complete length-prefixed frame from `buf`.
    ///
    /// `buf` must contain at least the full frame (prefix + payload); this
    /// function does not support partial/streaming input. Bytes in `buf`
    /// beyond the decoded frame, if any, are ignored — a transport that
    /// reads a stream is responsible for slicing exactly one frame's bytes
    /// (using the length prefix) before calling this, or for using
    /// [`decode_with_consumed`](Self::decode_with_consumed) to learn where
    /// the decoded frame ends.
    pub fn decode(&self, buf: &[u8]) -> Result<Frame, CodecError> {
        decode_frame(buf, self.max_frame_bytes)
    }

    /// Decode one complete length-prefixed frame from `buf` and report the
    /// number of bytes consumed: the 4-byte length prefix plus the
    /// declared payload length. The remainder `buf[consumed..]` — if any —
    /// is the next frame's bytes, letting a transport split a buffered
    /// stream into frames without re-parsing the length prefix itself.
    /// The frame itself is decoded exactly as [`decode`](Self::decode).
    pub fn decode_with_consumed(&self, buf: &[u8]) -> Result<(Frame, usize), CodecError> {
        decode_frame_with_consumed(buf, self.max_frame_bytes)
    }
}

impl Default for FrameCodec {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_FRAME_BYTES)
    }
}

/// Encode one frame as a complete length-prefixed wire buffer, using
/// [`DEFAULT_MAX_FRAME_BYTES`] as the encode-side size guard.
///
/// Use [`encode_frame_with_max`] to encode against a different bound; a
/// [`FrameCodec`] always encodes against its own configured maximum so that
/// both directions of one codec share a single bound.
pub fn encode_frame(frame: &Frame) -> Result<Vec<u8>, CodecError> {
    encode_frame_with_max(frame, DEFAULT_MAX_FRAME_BYTES)
}

/// Encode one frame against an explicit `max_frame_bytes` bound.
///
/// `max_frame_bytes` is PAYLOAD-ONLY: the maximum JSON payload length, not
/// counting the 4-byte length prefix — the same bound [`decode_frame`]
/// checks against.
///
/// Before serializing, the frame is validated against the same wire rules
/// the decode side enforces (`validate_frame_for_wire`, private): an empty
/// operation id in any id field ([`CodecError::InvalidFields`]), an
/// `error` frame whose id presence contradicts its code's terminal scope
/// ([`CodecError::InconsistentErrorScope`]), and a decoded unknown-code
/// fallback `error` frame ([`CodecError::FallbackFrameNotEncodable`]) are
/// all rejected here, so a frame this function accepts is one any
/// conforming decoder accepts. This keeps
/// encode and decode symmetric: a locally constructed frame that violates
/// the grammar can never leave this crate as wire bytes.
///
/// Cost note: the size guard runs on the SERIALIZED payload, so the frame
/// is fully serialized (and its bytes allocated) before the bound is
/// checked; an oversized frame therefore pays one full serialization before
/// [`CodecError::FrameTooLarge`] is returned. This is the accepted cost of
/// a local, non-I/O encode: no cheaper pre-estimate of the serialized size
/// exists without serializing.
pub fn encode_frame_with_max(frame: &Frame, max_frame_bytes: usize) -> Result<Vec<u8>, CodecError> {
    validate_frame_for_wire(frame)?;
    let payload = serde_json::to_vec(frame).map_err(|e| CodecError::InvalidJson(e.to_string()))?;
    check_encode_payload_len(payload.len(), max_frame_bytes)?;
    let mut buf = Vec::with_capacity(LENGTH_PREFIX_BYTES + payload.len());
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(&payload);
    Ok(buf)
}

/// The encode-side mirror of the decode-side grammar checks: rejects a
/// frame that [`decode_payload`] would reject, so encode and decode are
/// symmetric and a locally constructed frame can never serialize into wire
/// bytes no conforming decoder would accept.
///
/// Two rules are enforced here:
///
/// 1. No operation id field may carry the empty string — an empty string
///    can never be a unique caller-generated id. Decode enforces this in
///    `OperationId`'s `Deserialize` impl; the encode side checks the
///    in-memory value directly, because [`crate::frame::OperationId`]
///    construction is deliberately unrestricted.
/// 2. An `error` frame's operation-id presence must agree with its code's
///    terminal scope (ADR-137, "Operation correlation"), exactly as
///    [`decode_payload`] enforces via [`CodecError::InconsistentErrorScope`].
///    Every [`crate::error::WireErrorCode`] variant is in the closed set,
///    so the decode side's unknown-code exemption never applies here.
/// 3. An `error` frame carrying a `Some` `unrecognized_code` — the
///    decoded unknown-code fallback marker, which only a decode path sets
///    — is rejected with [`CodecError::FallbackFrameNotEncodable`].
///    Fallback frames are terminal for relay: re-encoding one would emit
///    the fallback code (`internal`) and silently discard the newer code
///    the peer sent, so this crate refuses to corrupt it.
fn validate_frame_for_wire(frame: &Frame) -> Result<(), CodecError> {
    use crate::error::TerminalScope;

    fn check_id(kind: &str, id: &crate::frame::OperationId) -> Result<(), CodecError> {
        if id.0.is_empty() {
            return Err(CodecError::InvalidFields {
                kind: kind.to_string(),
                detail: "operation id must be a non-empty string".to_string(),
            });
        }
        Ok(())
    }

    match frame {
        Frame::Handshake { .. } | Frame::HandshakeAck { .. } | Frame::Event { .. } => {}
        Frame::Request { id, .. } => check_id("request", id)?,
        Frame::Response { id, .. } => check_id("response", id)?,
        Frame::Cancel { id } => check_id("cancel", id)?,
        Frame::Subscribe { id, .. } => check_id("subscribe", id)?,
        Frame::SubscribeAck { id, .. } => check_id("subscribe_ack", id)?,
        Frame::Unsubscribe { id, .. } => check_id("unsubscribe", id)?,
        Frame::UnsubscribeAck { id, .. } => check_id("unsubscribe_ack", id)?,
        Frame::Error {
            id,
            code,
            unrecognized_code,
            ..
        } => {
            // A decoded-fallback frame is terminal for relay; see rule 3.
            if let Some(raw_code) = unrecognized_code {
                return Err(CodecError::FallbackFrameNotEncodable {
                    code: raw_code.clone(),
                });
            }
            if let Some(id) = id {
                check_id("error", id)?;
            }
            match (code.terminal_scope(), id) {
                (TerminalScope::Connection, Some(id)) => {
                    return Err(CodecError::InconsistentErrorScope {
                        detail: format!(
                            "connection-terminal code {code} must not carry an operation id, got {id}"
                        ),
                    });
                }
                (TerminalScope::Request, None) => {
                    return Err(CodecError::InconsistentErrorScope {
                        detail: format!(
                            "request-terminal code {code} must echo the operation id it terminates"
                        ),
                    });
                }
                (TerminalScope::Connection, None) | (TerminalScope::Request, Some(_)) => {}
            }
        }
    }
    Ok(())
}

/// The encode-side size decision, split out from
/// [`encode_frame_with_max`] so both guards are testable without
/// serializing a >4 GiB payload.
fn check_encode_payload_len(payload_len: usize, max_frame_bytes: usize) -> Result<(), CodecError> {
    if payload_len > max_frame_bytes {
        return Err(CodecError::FrameTooLarge {
            declared: payload_len,
            max: max_frame_bytes,
        });
    }
    // The 4-byte length prefix is a `u32`; without this guard a caller who
    // configured `max_frame_bytes > u32::MAX` could admit a payload whose
    // length would silently truncate in the cast to the prefix, writing a
    // prefix that does not match the payload. This arm only fires when the
    // configured maximum was NOT exceeded (that is the check above), so the
    // error names the prefix capacity as the binding limit rather than the
    // configured maximum.
    if payload_len > u32::MAX as usize {
        return Err(CodecError::U32PrefixLimitExceeded {
            declared: payload_len,
            max: u32::MAX as usize,
        });
    }
    Ok(())
}

/// Decode one complete length-prefixed frame from `buf` against an explicit
/// `max_frame_bytes` bound. See [`FrameCodec::decode`] for the contract.
///
/// `max_frame_bytes` is PAYLOAD-ONLY: the maximum JSON payload length, not
/// counting the 4-byte prefix. Passing a total wire length here is a caller
/// bug — it admits payloads 4 bytes over the intended bound.
pub fn decode_frame(buf: &[u8], max_frame_bytes: usize) -> Result<Frame, CodecError> {
    decode_frame_with_consumed(buf, max_frame_bytes).map(|(frame, _)| frame)
}

/// Decode one complete length-prefixed frame from `buf` and report the
/// number of bytes consumed: [`LENGTH_PREFIX_BYTES`] plus the declared
/// payload length. Bytes beyond the consumed prefix — if any — belong to
/// the next frame, letting a transport split a buffered stream into frames
/// without re-parsing the length prefix itself. [`decode_frame`] is this
/// function with the consumed count discarded.
///
/// `max_frame_bytes` is PAYLOAD-ONLY, as in [`decode_frame`].
///
/// **Decode errors are connection-terminal.** An error from this function
/// carries NO consumed count: once a frame fails to decode, this crate
/// cannot say where the failed frame ends, so the stream position is
/// unrecoverable. A transport must map the error through
/// [`CodecError::wire_code`], send the corresponding wire error, and close
/// the connection — never attempt to resynchronize and keep reading.
pub fn decode_frame_with_consumed(
    buf: &[u8],
    max_frame_bytes: usize,
) -> Result<(Frame, usize), CodecError> {
    if buf.len() < LENGTH_PREFIX_BYTES {
        return Err(CodecError::TruncatedLengthPrefix {
            available: buf.len(),
        });
    }
    let mut len_bytes = [0u8; LENGTH_PREFIX_BYTES];
    len_bytes.copy_from_slice(&buf[..LENGTH_PREFIX_BYTES]);
    let declared = u32::from_be_bytes(len_bytes) as usize;

    if declared > max_frame_bytes {
        return Err(CodecError::FrameTooLarge {
            declared,
            max: max_frame_bytes,
        });
    }

    let available = buf.len() - LENGTH_PREFIX_BYTES;
    if available < declared {
        return Err(CodecError::TruncatedPayload {
            declared,
            available,
        });
    }

    let payload = &buf[LENGTH_PREFIX_BYTES..LENGTH_PREFIX_BYTES + declared];
    let frame = decode_payload(payload)?;
    Ok((frame, LENGTH_PREFIX_BYTES + declared))
}

/// Decode one frame's JSON payload (without the length prefix).
///
/// Crate-internal split of [`decode_frame`]'s payload half; the public
/// surface is the length-prefixed [`decode_frame`] / [`FrameCodec::decode`],
/// which apply the `max_frame_bytes` size guard. Visible to unit tests in
/// this module only. This function applies NO
/// size check of its own: its only caller inside the crate is
/// [`decode_frame_with_consumed`], which has already enforced the bound
/// against the declared length before handing the payload over.
///
/// Enforces, beyond serde: the closed `"kind"` set
/// ([`CodecError::UnknownFrameKind`]). The ADR-137 id/scope consistency
/// rule for `error` frames and the unknown-code fallback diagnostic are
/// enforced inside [`crate::frame::Frame`]'s serde visitor — the same
/// visitor a direct serde decode drives — so both decode paths agree; this
/// function re-classifies the visitor's id/scope rejection into the typed
/// [`CodecError::InconsistentErrorScope`]. Strict field rejection (no
/// unknown fields) is carried by the per-kind payload structs in
/// [`crate::frame`]. For an `error` frame whose wire code is outside the
/// closed set, the raw code string is preserved in
/// [`Frame::Error`](crate::frame::Frame)'s `unrecognized_code` diagnostic
/// field.
pub(crate) fn decode_payload(payload: &[u8]) -> Result<Frame, CodecError> {
    let value: serde_json::Value =
        serde_json::from_slice(payload).map_err(|e| CodecError::InvalidJson(e.to_string()))?;

    let kind = value
        .as_object()
        .and_then(|obj| obj.get("kind"))
        .and_then(|k| k.as_str())
        .ok_or(CodecError::MissingKind)?
        .to_string();

    if !crate::frame::FRAME_KINDS.contains(&kind.as_str()) {
        return Err(CodecError::UnknownFrameKind(kind));
    }

    serde_json::from_value(value).map_err(|e| {
        // The visitor reports an id/scope violation through the
        // deserializer's error type; lift it back into the typed variant
        // so callers can branch on the specific failure.
        let detail = e.to_string();
        match detail.strip_prefix(INCONSISTENT_SCOPE_ERROR_PREFIX) {
            Some(scope_detail) => CodecError::InconsistentErrorScope {
                detail: scope_detail.to_string(),
            },
            None => CodecError::InvalidFields { kind, detail },
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::OperationId;

    /// One representative frame per kind, with every optional field
    /// present, for round-trip and closed-set controls.
    fn sample_frames() -> Vec<Frame> {
        vec![
            Frame::Handshake {
                version: crate::version::CURRENT_VERSION,
            },
            Frame::HandshakeAck {
                version: crate::version::CURRENT_VERSION,
            },
            Frame::Request {
                id: OperationId::from("op-1"),
                ops: "stats()".to_string(),
                deadline_ms: Some(5000),
                namespace: Some("research".to_string()),
                actor_id: Some("lambda".to_string()),
                visible_namespaces: Some(vec!["research".to_string(), "ops".to_string()]),
            },
            Frame::Response {
                id: OperationId::from("op-1"),
                result: serde_json::json!({"ok": true, "tool": "stats", "result": {"entities": 3}}),
            },
            Frame::Error {
                id: Some(OperationId::from("op-1")),
                code: crate::error::WireErrorCode::PeerClassDenied,
                message: "denied".to_string(),
                unrecognized_code: None,
            },
            Frame::Cancel {
                id: OperationId::from("op-1"),
            },
            Frame::Subscribe {
                id: OperationId::from("op-2"),
                topic: "comm.message_created".to_string(),
                resume_cursor: Some(42),
            },
            Frame::SubscribeAck {
                id: OperationId::from("op-2"),
                topic: "comm.message_created".to_string(),
                start_cursor: 42,
            },
            Frame::Unsubscribe {
                id: OperationId::from("op-3"),
                topic: "comm.message_created".to_string(),
            },
            Frame::UnsubscribeAck {
                id: OperationId::from("op-3"),
                topic: "comm.message_created".to_string(),
            },
            Frame::Event {
                topic: "comm.message_created".to_string(),
                cursor: 43,
                occurred_at: "2026-08-04T11:00:00Z".to_string(),
                payload: serde_json::json!({"message_id": "m-1"}),
            },
        ]
    }

    #[test]
    fn round_trips_a_cancel_frame() {
        let frame = Frame::Cancel {
            id: OperationId::from("op-1"),
        };
        let codec = FrameCodec::default();
        let wire = codec.encode(&frame).unwrap();
        assert_eq!(codec.decode(&wire).unwrap(), frame);
    }

    #[test]
    fn rejects_truncated_length_prefix() {
        let codec = FrameCodec::default();
        assert_eq!(
            codec.decode(&[0u8, 1]).unwrap_err(),
            CodecError::TruncatedLengthPrefix { available: 2 }
        );
    }

    #[test]
    fn rejects_truncated_payload_with_declared_and_available() {
        // A valid 4-byte prefix declaring 64 payload bytes, followed by
        // only 5 actual payload bytes: `available < declared` must surface
        // as `TruncatedPayload` with both values correct.
        let declared: u32 = 64;
        let mut wire = declared.to_be_bytes().to_vec();
        wire.extend_from_slice(b"part!"); // 5 bytes
        assert_eq!(
            decode_frame(&wire, DEFAULT_MAX_FRAME_BYTES).unwrap_err(),
            CodecError::TruncatedPayload {
                declared: 64,
                available: 5
            }
        );
    }

    #[test]
    fn rejects_truncated_payload_when_prefix_declares_exactly_one_byte_more() {
        // Boundary: one byte short is still truncated.
        let frame = Frame::Cancel {
            id: OperationId::from("op-1"),
        };
        let wire = encode_frame(&frame).unwrap();
        assert_eq!(
            decode_frame(&wire[..wire.len() - 1], DEFAULT_MAX_FRAME_BYTES).unwrap_err(),
            CodecError::TruncatedPayload {
                declared: wire.len() - LENGTH_PREFIX_BYTES,
                available: wire.len() - LENGTH_PREFIX_BYTES - 1
            }
        );
    }

    #[test]
    fn rejects_oversized_frame() {
        let codec = FrameCodec::new(4);
        let frame = Frame::Cancel {
            id: OperationId::from("op-1"),
        };
        let wire = encode_frame(&frame).unwrap();
        assert_eq!(
            codec.decode(&wire).unwrap_err(),
            CodecError::FrameTooLarge {
                declared: wire.len() - LENGTH_PREFIX_BYTES,
                max: 4
            }
        );
    }

    #[test]
    fn rejects_unknown_frame_kind() {
        let payload = br#"{"kind":"ping"}"#;
        assert_eq!(
            decode_payload(payload).unwrap_err(),
            CodecError::UnknownFrameKind("ping".to_string())
        );
    }

    #[test]
    fn rejects_non_json_payload() {
        let payload = b"not json";
        match decode_payload(payload).unwrap_err() {
            CodecError::InvalidJson(_) => {}
            other => panic!("expected InvalidJson, got {other:?}"),
        }
    }

    #[test]
    fn rejects_zero_length_payload() {
        let codec = FrameCodec::default();
        match codec.decode(&0u32.to_be_bytes()).unwrap_err() {
            CodecError::InvalidJson(_) => {}
            other => panic!("expected InvalidJson, got {other:?}"),
        }
    }

    #[test]
    fn rejects_missing_required_field() {
        let payload = br#"{"kind":"cancel"}"#;
        match decode_payload(payload).unwrap_err() {
            CodecError::InvalidFields { kind, .. } => assert_eq!(kind, "cancel"),
            other => panic!("expected InvalidFields, got {other:?}"),
        }
    }

    #[test]
    fn rejects_unknown_top_level_field() {
        // Strict closed grammar: no field a frame kind does not declare may
        // appear in the payload, however plausible it looks.
        let payload = br#"{"kind":"cancel","id":"op-1","unexpected":true}"#;
        match decode_payload(payload).unwrap_err() {
            CodecError::InvalidFields { kind, detail } => {
                assert_eq!(kind, "cancel");
                assert!(
                    detail.contains("unexpected"),
                    "detail should name the offending field: {detail}"
                );
            }
            other => panic!("expected InvalidFields, got {other:?}"),
        }
    }

    #[test]
    fn rejects_unknown_field_alongside_optional_fields() {
        // Same rule with the optional fields present: the extra field is
        // still rejected, and the rejection names it.
        let payload =
            br#"{"kind":"subscribe","id":"op-2","topic":"a.b","resume_cursor":1,"extra":{"nested":true}}"#;
        match decode_payload(payload).unwrap_err() {
            CodecError::InvalidFields { kind, detail } => {
                assert_eq!(kind, "subscribe");
                assert!(detail.contains("extra"), "detail: {detail}");
            }
            other => panic!("expected InvalidFields, got {other:?}"),
        }
    }

    #[test]
    fn opaque_payload_values_do_not_reject_unknown_keys() {
        // The strict grammar covers the fields each frame KIND declares.
        // `result` and `payload` are opaque JSON values: unknown keys
        // inside them are data, not grammar violations, and must decode.
        let payload = br#"{"kind":"event","topic":"a.b","cursor":1,"occurred_at":"2026-08-04T11:00:00Z","payload":{"anything":{"goes":true}}}"#;
        decode_payload(payload).unwrap();
    }

    #[test]
    fn every_frame_kind_round_trips_with_strict_decoding() {
        // Must-KEEP control: strict field rejection must not break any
        // known frame kind. One representative per kind, every optional
        // field populated, must survive encode -> decode unchanged.
        let frames = sample_frames();
        assert_eq!(frames.len(), crate::frame::FRAME_KINDS.len());
        for frame in frames {
            let wire = encode_frame(&frame).unwrap();
            let decoded = decode_frame(&wire, DEFAULT_MAX_FRAME_BYTES)
                .unwrap_or_else(|e| panic!("kind {:?} failed to decode: {e}", frame.kind()));
            assert_eq!(decoded, frame, "kind {:?} did not round-trip", frame.kind());
        }
    }

    #[test]
    fn rejects_connection_terminal_error_carrying_an_id() {
        // `frame_too_large` is connection-terminal: it must not echo an
        // operation id (ADR-137, "Operation correlation").
        let payload =
            br#"{"kind":"error","id":"op-1","code":"frame_too_large","message":"too big"}"#;
        match decode_payload(payload).unwrap_err() {
            CodecError::InconsistentErrorScope { detail } => {
                assert!(detail.contains("frame_too_large"), "detail: {detail}");
                assert!(detail.contains("op-1"), "detail: {detail}");
            }
            other => panic!("expected InconsistentErrorScope, got {other:?}"),
        }
    }

    #[test]
    fn rejects_request_terminal_error_without_an_id() {
        // `cancelled` is request-terminal: it must echo the operation id it
        // terminates.
        let payload = br#"{"kind":"error","code":"cancelled","message":"cancelled"}"#;
        match decode_payload(payload).unwrap_err() {
            CodecError::InconsistentErrorScope { detail } => {
                assert!(detail.contains("cancelled"), "detail: {detail}");
            }
            other => panic!("expected InconsistentErrorScope, got {other:?}"),
        }
    }

    #[test]
    fn accepts_connection_terminal_error_without_an_id() {
        // Valid arm: connection-terminal scope, no id.
        let payload =
            br#"{"kind":"error","code":"unsupported_version","message":"no common version"}"#;
        let frame = decode_payload(payload).unwrap();
        assert!(matches!(frame, Frame::Error { id: None, .. }));
    }

    #[test]
    fn accepts_request_terminal_error_with_an_id() {
        // Valid arm: request-terminal scope, echoing the id it terminates.
        let payload =
            br#"{"kind":"error","id":"op-9","code":"deadline_exceeded","message":"too slow"}"#;
        let frame = decode_payload(payload).unwrap();
        match frame {
            Frame::Error { id, code, .. } => {
                assert_eq!(id, Some(OperationId::from("op-9")));
                assert_eq!(code, crate::error::WireErrorCode::DeadlineExceeded);
            }
            other => panic!("expected an error frame, got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_operation_id() {
        // An empty string can never be a unique caller-generated id.
        let payload = br#"{"kind":"cancel","id":""}"#;
        match decode_payload(payload).unwrap_err() {
            CodecError::InvalidFields { kind, detail } => {
                assert_eq!(kind, "cancel");
                assert!(detail.contains("non-empty"), "detail: {detail}");
            }
            other => panic!("expected InvalidFields, got {other:?}"),
        }
    }

    #[test]
    fn rejects_valid_json_that_is_not_an_object() {
        for payload in [
            b"[1, 2, 3]".as_slice(),
            b"\"cancel\"".as_slice(),
            b"42".as_slice(),
            b"null".as_slice(),
        ] {
            assert_eq!(
                decode_payload(payload).unwrap_err(),
                CodecError::MissingKind,
                "payload {payload:?}"
            );
        }
    }

    #[test]
    fn rejects_payload_with_no_kind_field() {
        let payload = br#"{"id":"op-1"}"#;
        assert_eq!(
            decode_payload(payload).unwrap_err(),
            CodecError::MissingKind
        );
    }

    #[test]
    fn rejects_non_string_kind() {
        for payload in [
            br#"{"kind":42}"#.as_slice(),
            br#"{"kind":null}"#.as_slice(),
            br#"{"kind":["cancel"]}"#.as_slice(),
        ] {
            assert_eq!(
                decode_payload(payload).unwrap_err(),
                CodecError::MissingKind,
                "payload {payload:?}"
            );
        }
    }

    #[test]
    fn rejects_wrong_typed_field() {
        // `id` must be a string, not a number.
        let payload = br#"{"kind":"cancel","id":7}"#;
        match decode_payload(payload).unwrap_err() {
            CodecError::InvalidFields { kind, detail } => {
                assert_eq!(kind, "cancel");
                assert!(detail.contains("id"), "detail: {detail}");
            }
            other => panic!("expected InvalidFields, got {other:?}"),
        }

        // `start_cursor` must be a number, not a string.
        let payload = br#"{"kind":"subscribe_ack","id":"op-2","topic":"a.b","start_cursor":"42"}"#;
        match decode_payload(payload).unwrap_err() {
            CodecError::InvalidFields { kind, .. } => assert_eq!(kind, "subscribe_ack"),
            other => panic!("expected InvalidFields, got {other:?}"),
        }
    }

    #[test]
    fn opaque_payloads_are_preserved_semantically_not_byte_for_byte() {
        // Pins the REAL opaque-payload guarantee under this workspace's
        // serde_json feature set (crates/Cargo.toml: `serde_json = "1.0"` —
        // default features only; `preserve_order` and `arbitrary_precision`
        // are NOT enabled):
        //
        // 1. Object keys are NOT preserved in input order; the re-encoded
        //    form sorts keys (serde_json's BTreeMap-backed Map). Equality
        //    is semantic, not byte-for-byte.
        // 2. Integers within u64/i64 range round-trip exactly (they parse
        //    as u64/i64, never f64) — 2^53+1 below is exact.
        // 3. Integers outside u64/i64 range (e.g. 2^64) parse as f64 and
        //    lose precision; there is no `arbitrary_precision` fallback.
        let raw = br#"{"kind":"response","id":"op-1","result":{"zeta":1,"alpha":{"n":9007199254740993},"huge":18446744073709551616,"neg":-9007199254740993}}"#;
        let frame = decode_payload(raw).unwrap();
        let Frame::Response { result, .. } = &frame else {
            panic!("expected a response frame");
        };

        // (2) 2^53+1 is within u64 range and survives exactly.
        assert_eq!(result["alpha"]["n"], serde_json::json!(9007199254740993u64));
        assert_eq!(result["neg"], serde_json::json!(-9007199254740993i64));

        // (3) 2^64 exceeds u64 range: parsed as f64, precision semantics
        // change (2^64 happens to be exactly representable as f64, but it
        // is no longer an integer value on the serde_json type level).
        assert!(result["huge"].is_f64());
        assert!(!result["huge"].is_u64());
        assert_eq!(result["huge"].as_f64().unwrap(), 2.0f64.powi(64));

        // (1) Re-encode keeps semantic equality but reorders keys: input
        // had "zeta" first; the re-encoded payload sorts "alpha" first.
        let wire = encode_frame(&frame).unwrap();
        assert_eq!(decode_payload(&wire[LENGTH_PREFIX_BYTES..]).unwrap(), frame);
        let payload = std::str::from_utf8(&wire[LENGTH_PREFIX_BYTES..]).unwrap();
        let alpha = payload.find("\"alpha\"").unwrap();
        let zeta = payload.find("\"zeta\"").unwrap();
        assert!(
            alpha < zeta,
            "expected sorted key order in re-encoded payload: {payload}"
        );
    }

    // ── encode-side validation: the mirror of the decode checks above ──

    #[test]
    fn encode_rejects_empty_operation_id() {
        // Encode arm of `rejects_empty_operation_id`: `From<&str>` is
        // deliberately unrestricted, so the empty id can exist in memory —
        // the codec must refuse to put it on the wire in any id field.
        let frame = Frame::Cancel {
            id: OperationId::from(""),
        };
        match encode_frame(&frame).unwrap_err() {
            CodecError::InvalidFields { kind, detail } => {
                assert_eq!(kind, "cancel");
                assert!(detail.contains("non-empty"), "detail: {detail}");
            }
            other => panic!("expected InvalidFields, got {other:?}"),
        }
    }

    #[test]
    fn encode_rejects_empty_operation_id_in_every_id_field() {
        // The empty-id guard covers every frame kind with an operation id,
        // not just the one the decode test exercises.
        let frames = [
            Frame::Request {
                id: OperationId::from(""),
                ops: "stats()".to_string(),
                deadline_ms: None,
                namespace: None,
                actor_id: None,
                visible_namespaces: None,
            },
            Frame::Response {
                id: OperationId::from(""),
                result: serde_json::json!({}),
            },
            Frame::Subscribe {
                id: OperationId::from(""),
                topic: "a.b".to_string(),
                resume_cursor: None,
            },
            Frame::SubscribeAck {
                id: OperationId::from(""),
                topic: "a.b".to_string(),
                start_cursor: 0,
            },
            Frame::Unsubscribe {
                id: OperationId::from(""),
                topic: "a.b".to_string(),
            },
            Frame::UnsubscribeAck {
                id: OperationId::from(""),
                topic: "a.b".to_string(),
            },
        ];
        for frame in frames {
            match encode_frame(&frame).unwrap_err() {
                CodecError::InvalidFields { detail, .. } => {
                    assert!(detail.contains("non-empty"), "detail: {detail}");
                }
                other => panic!(
                    "kind {:?}: expected InvalidFields, got {other:?}",
                    frame.kind()
                ),
            }
        }
    }

    #[test]
    fn encode_rejects_connection_terminal_error_carrying_an_id() {
        // Encode arm of `rejects_connection_terminal_error_carrying_an_id`:
        // `frame_too_large` is connection-terminal and must not echo an
        // operation id (ADR-137, "Operation correlation").
        let frame = Frame::Error {
            id: Some(OperationId::from("op-1")),
            code: crate::error::WireErrorCode::FrameTooLarge,
            message: "too big".to_string(),
            unrecognized_code: None,
        };
        match encode_frame(&frame).unwrap_err() {
            CodecError::InconsistentErrorScope { detail } => {
                assert!(detail.contains("frame_too_large"), "detail: {detail}");
                assert!(detail.contains("op-1"), "detail: {detail}");
            }
            other => panic!("expected InconsistentErrorScope, got {other:?}"),
        }
    }

    #[test]
    fn encode_rejects_request_terminal_error_without_an_id() {
        // Encode arm of `rejects_request_terminal_error_without_an_id`:
        // `cancelled` is request-terminal and must echo the id it
        // terminates.
        let frame = Frame::Error {
            id: None,
            code: crate::error::WireErrorCode::Cancelled,
            message: "cancelled".to_string(),
            unrecognized_code: None,
        };
        match encode_frame(&frame).unwrap_err() {
            CodecError::InconsistentErrorScope { detail } => {
                assert!(detail.contains("cancelled"), "detail: {detail}");
            }
            other => panic!("expected InconsistentErrorScope, got {other:?}"),
        }
    }

    #[test]
    fn encode_accepts_both_consistent_error_scopes() {
        // Happy-path control for the encode-side scope check: both legal
        // pairings encode and round-trip through decode unchanged.
        let consistent = [
            Frame::Error {
                id: None,
                code: crate::error::WireErrorCode::MalformedFrame,
                message: "connection scope".to_string(),
                unrecognized_code: None,
            },
            Frame::Error {
                id: Some(OperationId::from("op-1")),
                code: crate::error::WireErrorCode::DeadlineExceeded,
                message: "request scope".to_string(),
                unrecognized_code: None,
            },
        ];
        for frame in consistent {
            let wire = encode_frame(&frame).expect("consistent scope must encode");
            assert_eq!(decode_frame(&wire, DEFAULT_MAX_FRAME_BYTES).unwrap(), frame);
        }
    }

    // ── unknown wire code: fallback keeps the raw string (item 3) ──

    #[test]
    fn unknown_code_fallback_preserves_the_raw_string_with_an_id() {
        // A newer peer may send a code this version does not know. The
        // frame must still decode (forward compatibility), the code falls
        // back to `internal`, the id/scope pairing is NOT enforced for it
        // (its true scope is unknown), and the raw string survives in the
        // `unrecognized_code` diagnostic instead of being discarded.
        let payload =
            br#"{"kind":"error","id":"op-7","code":"future_code_xyz","message":"from newer peer"}"#;
        let frame = decode_payload(payload).unwrap();
        match frame {
            Frame::Error {
                id,
                code,
                unrecognized_code,
                ..
            } => {
                assert_eq!(code, crate::error::WireErrorCode::Internal);
                assert_eq!(id, Some(OperationId::from("op-7")));
                assert_eq!(unrecognized_code.as_deref(), Some("future_code_xyz"));
            }
            other => panic!("expected an error frame, got {other:?}"),
        }
    }

    #[test]
    fn unknown_code_fallback_preserves_the_raw_string_without_an_id() {
        let payload = br#"{"kind":"error","code":"future_code_xyz","message":"from newer peer"}"#;
        match decode_payload(payload).unwrap() {
            Frame::Error {
                id,
                code,
                unrecognized_code,
                ..
            } => {
                assert_eq!(code, crate::error::WireErrorCode::Internal);
                assert!(id.is_none());
                assert_eq!(unrecognized_code.as_deref(), Some("future_code_xyz"));
            }
            other => panic!("expected an error frame, got {other:?}"),
        }
    }

    #[test]
    fn closed_set_code_never_populates_unrecognized_code() {
        // The diagnostic stays `None` for every code in the closed set: it
        // records only the serde-other fallback, never a recognized code.
        let payload =
            br#"{"kind":"error","id":"op-9","code":"deadline_exceeded","message":"too slow"}"#;
        match decode_payload(payload).unwrap() {
            Frame::Error {
                unrecognized_code, ..
            } => assert!(unrecognized_code.is_none()),
            other => panic!("expected an error frame, got {other:?}"),
        }
    }

    #[test]
    fn decode_scope_rejection_detail_is_reclassified_without_the_shared_prefix() {
        // The visitor reports the id/scope rule with a fixed prefix so
        // every decode path uses one wording; `decode_payload` must strip
        // that prefix when re-classifying into the typed variant.
        let payload =
            br#"{"kind":"error","id":"op-1","code":"frame_too_large","message":"too big"}"#;
        match decode_payload(payload).unwrap_err() {
            CodecError::InconsistentErrorScope { detail } => {
                assert!(
                    !detail.contains(INCONSISTENT_SCOPE_ERROR_PREFIX),
                    "detail must not repeat the shared prefix: {detail}"
                );
                assert!(detail.starts_with("connection-terminal code"));
            }
            other => panic!("expected InconsistentErrorScope, got {other:?}"),
        }
    }

    // ── fallback frames are terminal for relay (item 2) ──

    #[test]
    fn encode_rejects_a_fallback_frame_with_an_id() {
        // A decoded unknown-code fallback re-encoded as-is would emit
        // `internal` and silently discard the newer code the peer sent —
        // silent code loss. The encode path must reject it outright, in
        // both id shapes. Here: WITH an id (the request-terminal fallback
        // shape a newer peer would send).
        let payload =
            br#"{"kind":"error","id":"op-7","code":"future_code_xyz","message":"from newer peer"}"#;
        let frame = decode_payload(payload).unwrap();
        match encode_frame(&frame).unwrap_err() {
            CodecError::FallbackFrameNotEncodable { code } => {
                assert_eq!(code, "future_code_xyz");
            }
            other => panic!("expected FallbackFrameNotEncodable, got {other:?}"),
        }
    }

    #[test]
    fn encode_rejects_a_fallback_frame_without_an_id() {
        // Same rule for the id-less fallback shape: decode accepts it (the
        // pairing is not enforced for an unknown code), encode rejects it.
        let payload = br#"{"kind":"error","code":"future_code_xyz","message":"from newer peer"}"#;
        let frame = decode_payload(payload).unwrap();
        match encode_frame(&frame).unwrap_err() {
            CodecError::FallbackFrameNotEncodable { code } => {
                assert_eq!(code, "future_code_xyz");
            }
            other => panic!("expected FallbackFrameNotEncodable, got {other:?}"),
        }
    }

    #[test]
    fn encode_accepts_an_honest_internal_error_frame() {
        // The rejection keys on the `unrecognized_code` MARKER, not on the
        // `internal` code: a locally constructed (or closed-set-decoded)
        // `internal` frame has `unrecognized_code: None` and encodes fine.
        let frame = Frame::Error {
            id: Some(OperationId::from("op-1")),
            code: crate::error::WireErrorCode::Internal,
            message: "boom".to_string(),
            unrecognized_code: None,
        };
        let wire = encode_frame(&frame).expect("honest internal must encode");
        let decoded = decode_frame(&wire, DEFAULT_MAX_FRAME_BYTES).unwrap();
        assert_eq!(decoded, frame);
    }

    // ── decode_with_consumed (item 4) ──

    #[test]
    fn decode_with_consumed_reports_consumed_length_on_a_two_frame_buffer() {
        // Two concatenated frames: the consumed count must end exactly at
        // the first frame's last byte, and the remainder must be the
        // second frame, decodable without re-slicing by hand.
        let first = Frame::Cancel {
            id: OperationId::from("op-1"),
        };
        let second = Frame::Handshake {
            version: crate::version::CURRENT_VERSION,
        };
        let codec = FrameCodec::default();
        let mut buf = codec.encode(&first).unwrap();
        let first_len = buf.len();
        buf.extend_from_slice(&codec.encode(&second).unwrap());

        let (decoded, consumed) = codec.decode_with_consumed(&buf).unwrap();
        assert_eq!(decoded, first);
        assert_eq!(consumed, first_len);

        let (decoded2, consumed2) = codec.decode_with_consumed(&buf[consumed..]).unwrap();
        assert_eq!(decoded2, second);
        assert_eq!(consumed + consumed2, buf.len());

        // `decode` keeps its documented contract on the same buffer.
        assert_eq!(codec.decode(&buf).unwrap(), first);
    }

    #[test]
    fn decode_with_consumed_errors_without_reporting_length_on_truncation() {
        let codec = FrameCodec::default();
        let wire = codec
            .encode(&Frame::Cancel {
                id: OperationId::from("op-1"),
            })
            .unwrap();
        let err = codec
            .decode_with_consumed(&wire[..wire.len() - 1])
            .unwrap_err();
        assert!(matches!(err, CodecError::TruncatedPayload { .. }));
    }

    // ── serde_json feature posture probe (item 2) ──

    #[test]
    fn serde_json_feature_posture_is_default_keys_serialize_sorted() {
        // Golden byte-exactness (tests/golden_frames.rs) and the semantic
        // opaque-payload guarantee above both assume serde_json's DEFAULT
        // feature posture: no `preserve_order` (the Map is BTreeMap-backed,
        // so keys serialize in sorted order regardless of insertion) and no
        // `arbitrary_precision`. Cargo features are ADDITIVE across the
        // workspace: if any crate in the dependency graph enables
        // `preserve_order` or `arbitrary_precision`, serde_json unifies on
        // it for this crate too, silently changing key order (or number
        // handling) and invalidating the golden fixtures. This probe pins
        // the assumed posture at test time: if it fails, the workspace
        // feature set drifted — reconcile the workspace or the fixtures,
        // do not weaken this assertion.
        let mut map = serde_json::Map::new();
        map.insert("zeta".to_string(), serde_json::json!(1));
        map.insert("alpha".to_string(), serde_json::json!(2));
        map.insert("mid".to_string(), serde_json::json!(3));
        let wire = serde_json::to_string(&serde_json::Value::Object(map)).unwrap();
        assert_eq!(
            wire, r#"{"alpha":2,"mid":3,"zeta":1}"#,
            "serde_json keys did not serialize in sorted order — \
             `preserve_order` may have been enabled workspace-wide"
        );
    }

    // ── CodecError → WireErrorCode canonical mapping (item 8) ──

    #[test]
    fn codec_errors_map_to_their_wire_error_codes() {
        use crate::error::WireErrorCode;

        let cases: &[(CodecError, WireErrorCode)] = &[
            (
                CodecError::TruncatedLengthPrefix { available: 2 },
                WireErrorCode::MalformedFrame,
            ),
            (
                CodecError::TruncatedPayload {
                    declared: 8,
                    available: 3,
                },
                WireErrorCode::MalformedFrame,
            ),
            (
                CodecError::FrameTooLarge {
                    declared: 10,
                    max: 4,
                },
                WireErrorCode::FrameTooLarge,
            ),
            (
                CodecError::U32PrefixLimitExceeded {
                    declared: 10,
                    max: 4,
                },
                WireErrorCode::FrameTooLarge,
            ),
            (
                CodecError::InvalidJson("x".to_string()),
                WireErrorCode::MalformedFrame,
            ),
            (CodecError::MissingKind, WireErrorCode::MalformedFrame),
            (
                CodecError::UnknownFrameKind("ping".to_string()),
                WireErrorCode::MalformedFrame,
            ),
            (
                CodecError::InvalidFields {
                    kind: "cancel".to_string(),
                    detail: "x".to_string(),
                },
                WireErrorCode::MalformedFrame,
            ),
            (
                CodecError::InconsistentErrorScope {
                    detail: "x".to_string(),
                },
                WireErrorCode::MalformedFrame,
            ),
            (
                CodecError::FallbackFrameNotEncodable {
                    code: "future_code_xyz".to_string(),
                },
                WireErrorCode::MalformedFrame,
            ),
        ];
        for (err, expected) in cases {
            assert_eq!(
                err.wire_code(),
                *expected,
                "{err:?} mapped to {:?}, expected {expected:?}",
                err.wire_code()
            );
            assert_eq!(
                WireErrorCode::from(err),
                *expected,
                "From<&CodecError> disagrees"
            );
        }
    }
}

#[cfg(test)]
mod configured_max_tests {
    use super::*;
    use crate::frame::Frame;

    fn oversized_frame() -> Frame {
        Frame::Request {
            id: crate::frame::OperationId("x".into()),
            ops: "a".repeat(4096),
            deadline_ms: None,
            namespace: None,
            actor_id: None,
            visible_namespaces: None,
        }
    }

    #[test]
    fn codec_encode_honors_its_own_maximum_not_the_default() {
        let codec = FrameCodec::new(1024);
        let err = codec.encode(&oversized_frame()).unwrap_err();
        match err {
            CodecError::FrameTooLarge { max, .. } => assert_eq!(max, 1024),
            other => panic!("expected FrameTooLarge with the configured max, got {other:?}"),
        }
    }

    #[test]
    fn codec_encode_accepts_a_frame_within_its_own_maximum() {
        let codec = FrameCodec::new(1024 * 1024);
        let wire = codec
            .encode(&oversized_frame())
            .expect("within configured max");
        assert_eq!(codec.decode(&wire).unwrap(), oversized_frame());
    }

    #[test]
    fn free_function_encode_still_uses_the_default_maximum() {
        let wire = encode_frame(&oversized_frame()).expect("well under 8 MiB");
        assert!(wire.len() > 4096);
    }

    #[test]
    fn codec_configured_above_u32_capacity_still_encodes_normal_frames() {
        // What the old `encode_never_exceeds_the_u32_prefix_capacity` test
        // actually verified (and all it could verify without a >4 GiB
        // payload): configuring a codec above the u32 prefix capacity does
        // not disturb ordinary encoding. It did NOT exercise the u32 guard
        // itself — that decision logic is tested directly below.
        assert!(encode_frame_with_max(&oversized_frame(), usize::MAX).is_ok());
    }

    #[test]
    fn encode_size_guard_reports_the_configured_max_when_it_is_binding() {
        let err = check_encode_payload_len(32, 16).unwrap_err();
        assert_eq!(
            err,
            CodecError::FrameTooLarge {
                declared: 32,
                max: 16
            }
        );
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn encode_size_guard_reports_the_u32_prefix_capacity_when_it_is_binding() {
        // The u32 guard fires only when the configured maximum admitted the
        // payload (payload_len <= max) but the 4-byte prefix cannot name
        // it (payload_len > u32::MAX). The configured maximum was NOT
        // exceeded at that point, so the error must name the prefix
        // capacity as the binding limit — reporting the configured max
        // would claim an exceedance that did not happen.
        let err = check_encode_payload_len(u32::MAX as usize + 1, usize::MAX).unwrap_err();
        assert_eq!(
            err,
            CodecError::U32PrefixLimitExceeded {
                declared: u32::MAX as usize + 1,
                max: u32::MAX as usize
            }
        );
        // Exactly at the prefix capacity is still encodable in principle.
        assert!(check_encode_payload_len(u32::MAX as usize, usize::MAX).is_ok());
    }
}
