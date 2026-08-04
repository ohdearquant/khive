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

/// A decode failure, distinguishing every malformed-frame case this crate
/// can identify. All of these correspond to the wire error
/// [`crate::error::WireErrorCode::MalformedFrame`] or
/// [`crate::error::WireErrorCode::FrameTooLarge`] once mapped onto the wire
/// error taxonomy by a server; this crate keeps the finer-grained variant
/// so a caller (including a test) can assert the specific failure rather
/// than only "decoding failed".
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CodecError {
    /// Fewer than [`LENGTH_PREFIX_BYTES`] bytes were supplied — the 4-byte
    /// length prefix itself is truncated.
    #[error("truncated length prefix: got {available} of {LENGTH_PREFIX_BYTES} bytes")]
    TruncatedLengthPrefix { available: usize },

    /// The length prefix names a payload longer than what was supplied.
    #[error("truncated payload: declared {declared} bytes, got {available}")]
    TruncatedPayload { declared: usize, available: usize },

    /// The declared frame length exceeds the configured maximum.
    #[error("frame of {declared} bytes exceeds the {max} byte maximum")]
    FrameTooLarge { declared: usize, max: usize },

    /// The payload bytes are not valid JSON at all.
    #[error("payload is not valid JSON: {0}")]
    InvalidJson(String),

    /// The payload is valid JSON but not a JSON object, or has no `"kind"`
    /// string field.
    #[error("payload has no string \"kind\" discriminant field")]
    MissingKind,

    /// The `"kind"` field names a value outside the closed
    /// [`crate::frame::FRAME_KINDS`] set.
    #[error("unknown frame kind: {0:?}")]
    UnknownFrameKind(String),

    /// The frame's `"kind"` was recognized but a field required by that
    /// frame kind's shape is missing or has the wrong type.
    #[error("frame kind {kind:?}: {detail}")]
    InvalidFields { kind: String, detail: String },
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
    /// BE length followed by the JSON payload.
    pub fn encode(&self, frame: &Frame) -> Result<Vec<u8>, CodecError> {
        encode_frame(frame)
    }

    /// Decode one complete length-prefixed frame from `buf`.
    ///
    /// `buf` must contain at least the full frame (prefix + payload); this
    /// function does not support partial/streaming input. Bytes in `buf`
    /// beyond the decoded frame, if any, are ignored — a transport that
    /// reads a stream is responsible for slicing exactly one frame's bytes
    /// (using the length prefix) before calling this.
    pub fn decode(&self, buf: &[u8]) -> Result<Frame, CodecError> {
        decode_frame(buf, self.max_frame_bytes)
    }
}

impl Default for FrameCodec {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_FRAME_BYTES)
    }
}

/// Encode one frame as a complete length-prefixed wire buffer, using
/// [`DEFAULT_MAX_FRAME_BYTES`] as the encode-side size guard.
pub fn encode_frame(frame: &Frame) -> Result<Vec<u8>, CodecError> {
    let payload = serde_json::to_vec(frame).map_err(|e| CodecError::InvalidJson(e.to_string()))?;
    if payload.len() > DEFAULT_MAX_FRAME_BYTES {
        return Err(CodecError::FrameTooLarge {
            declared: payload.len(),
            max: DEFAULT_MAX_FRAME_BYTES,
        });
    }
    let mut buf = Vec::with_capacity(LENGTH_PREFIX_BYTES + payload.len());
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(&payload);
    Ok(buf)
}

/// Decode one complete length-prefixed frame from `buf` against an explicit
/// `max_frame_bytes` bound. See [`FrameCodec::decode`] for the contract.
pub fn decode_frame(buf: &[u8], max_frame_bytes: usize) -> Result<Frame, CodecError> {
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
    decode_payload(payload)
}

/// Decode one frame's JSON payload (without the length prefix).
///
/// Split out from [`decode_frame`] so a caller that already has the exact
/// payload bytes (for example, from fixture files that store payload-only
/// JSON) does not have to synthesize a length prefix first.
pub fn decode_payload(payload: &[u8]) -> Result<Frame, CodecError> {
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

    serde_json::from_value(value).map_err(|e| CodecError::InvalidFields {
        kind,
        detail: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::OperationId;

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
    fn rejects_missing_required_field() {
        let payload = br#"{"kind":"cancel"}"#;
        match decode_payload(payload).unwrap_err() {
            CodecError::InvalidFields { kind, .. } => assert_eq!(kind, "cancel"),
            other => panic!("expected InvalidFields, got {other:?}"),
        }
    }
}
