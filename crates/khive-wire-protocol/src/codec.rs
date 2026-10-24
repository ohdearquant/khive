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

/// A codec failure, distinguishing every frame-level case this crate can
/// identify. Every variant maps onto the wire error taxonomy —
/// [`crate::error::WireErrorCode::MalformedFrame`],
/// [`crate::error::WireErrorCode::Internal`], or
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
    /// forbids ([`crate::frame::OperationId`]), or an Event payload is not
    /// a JSON object (Amendment 1 Decision 10).
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
    /// carried a code outside the closed set. Re-encoding one would emit
    /// the fallback code (`internal`) and silently discard the newer code
    /// the peer sent, so the encode path rejects this local relay attempt;
    /// the connection remains healthy, but this relay cannot re-encode the
    /// decoded frame. If a relay must pass unknown codes through, it has to
    /// operate on the raw frame bytes, not on a decoded-and-re-encoded frame.
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
    /// failures — map to [`WireErrorCode::FrameTooLarge`]. A decoded fallback
    /// that this local relay cannot re-encode maps to
    /// [`WireErrorCode::Internal`]; the connection is healthy and only the
    /// relay attempt failed. Every other variant maps to
    /// [`WireErrorCode::MalformedFrame`]. This is the canonical
    /// codec→wire-error mapping: servers should use it (or the
    /// `From<&CodecError>` impl) rather than hand-maintain their own.
    ///
    /// [`WireErrorCode::FrameTooLarge`]: crate::error::WireErrorCode::FrameTooLarge
    /// [`WireErrorCode::MalformedFrame`]: crate::error::WireErrorCode::MalformedFrame
    /// [`WireErrorCode::Internal`]: crate::error::WireErrorCode::Internal
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
            | CodecError::InconsistentErrorScope { .. } => {
                crate::error::WireErrorCode::MalformedFrame
            }
            CodecError::FallbackFrameNotEncodable { .. } => crate::error::WireErrorCode::Internal,
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
/// operation id in any id field ([`CodecError::InvalidFields`]), a handshake
/// or handshake acknowledgment naming version 0
/// ([`CodecError::InvalidFields`]), an
/// `error` frame whose id presence contradicts its code's terminal scope
/// ([`CodecError::InconsistentErrorScope`]), a decoded unknown-code
/// fallback `error` frame ([`CodecError::FallbackFrameNotEncodable`]), and
/// a non-object Event payload ([`CodecError::InvalidFields`]) are all
/// rejected here, so a frame this function accepts is one any
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
/// These rules are enforced here:
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
/// 3. A handshake or handshake acknowledgment may not name version 0;
///    version 0 does not exist in this protocol.
/// 4. An `error` frame carrying a `Some` `unrecognized_code` — the
///    decoded unknown-code fallback marker, which only a decode path sets
///    — is rejected with [`CodecError::FallbackFrameNotEncodable`].
///    This relay cannot re-encode a fallback: doing so would emit the
///    fallback code (`internal`) and silently discard the newer code the
///    peer sent, so this crate refuses to corrupt it.
/// 5. An Event payload must be a JSON object (Amendment 1 Decision 10).
///    Response results remain arbitrary JSON.
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
        Frame::Handshake { version } => {
            if version.get() == 0 {
                return Err(CodecError::InvalidFields {
                    kind: "handshake".to_string(),
                    detail: "protocol version 0 does not exist".to_string(),
                });
            }
        }
        Frame::HandshakeAck { version } => {
            if version.get() == 0 {
                return Err(CodecError::InvalidFields {
                    kind: "handshake_ack".to_string(),
                    detail: "protocol version 0 does not exist".to_string(),
                });
            }
        }
        Frame::Event { payload, .. } => {
            if !payload.is_object() {
                return Err(CodecError::InvalidFields {
                    kind: "event".to_string(),
                    detail: "payload must be a JSON object".to_string(),
                });
            }
        }
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
            // A decoded-fallback frame cannot be re-encoded by this relay;
            // see rule 4.
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
    // Decision 4 requires the Frame visitor to see raw envelope members.
    // A Value-first success path would erase duplicate keys before it runs.
    let error = match serde_json::from_slice::<Frame>(payload) {
        Ok(frame) => return Ok(frame),
        Err(error) => error,
    };

    // This second parse classifies an already-refused payload only. It
    // must never deserialize the Value back into an accepted Frame.
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

    // Preserve the visitor's original refusal, including duplicate keys
    // and present-null fields, while retaining typed id/scope diagnostics.
    let detail = error.to_string();
    Err(match detail.strip_prefix(INCONSISTENT_SCOPE_ERROR_PREFIX) {
        Some(scope_detail) => CodecError::InconsistentErrorScope {
            detail: scope_detail.to_string(),
        },
        None => CodecError::InvalidFields { kind, detail },
    })
}

#[cfg(test)]
#[path = "codec_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "codec_configured_max_tests.rs"]
mod configured_max_tests;
