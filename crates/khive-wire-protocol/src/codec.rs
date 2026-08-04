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

use crate::error::TerminalScope;
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

    /// The declared frame length exceeds the configured maximum
    /// (payload-only; the 4-byte prefix never counts against it). `max`
    /// is always the configured bound that was exceeded.
    #[error("frame of {declared} bytes exceeds the {max} byte maximum")]
    FrameTooLarge { declared: usize, max: usize },

    /// The serialized payload exceeds the 4-byte length prefix's inherent
    /// `u32::MAX` byte capacity. Distinct from [`FrameTooLarge`]: this arm
    /// can only fire when the configured maximum is at least the payload
    /// length (otherwise [`FrameTooLarge`] fires first), so the configured
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
    /// documentation's "Strict field rejection" section).
    #[error("frame kind {kind:?}: {detail}")]
    InvalidFields { kind: String, detail: String },

    /// An `error` frame whose operation-id presence contradicts its code's
    /// terminal scope (ADR-137, "Operation correlation"): a
    /// connection-terminal code carries no operation id, and a
    /// request-terminal code echoes the one it terminates.
    #[error("error frame violates the id/scope rule: {detail}")]
    InconsistentErrorScope { detail: String },
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
pub fn encode_frame_with_max(frame: &Frame, max_frame_bytes: usize) -> Result<Vec<u8>, CodecError> {
    let payload = serde_json::to_vec(frame).map_err(|e| CodecError::InvalidJson(e.to_string()))?;
    check_encode_payload_len(payload.len(), max_frame_bytes)?;
    let mut buf = Vec::with_capacity(LENGTH_PREFIX_BYTES + payload.len());
    buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    buf.extend_from_slice(&payload);
    Ok(buf)
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
///
/// Enforces, beyond serde: the closed `"kind"` set
/// ([`CodecError::UnknownFrameKind`]), and — after the frame parses — the
/// ADR-137 id/scope consistency rule for `error` frames
/// ([`CodecError::InconsistentErrorScope`]). Strict field rejection (no
/// unknown fields) is carried by the per-kind payload structs in
/// [`crate::frame`].
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

    // For an `error` frame, capture the raw code string before `value` is
    // consumed: the scope/id consistency check applies only to codes in the
    // closed set (see `WIRE_ERROR_CODES`).
    let raw_error_code = value
        .as_object()
        .and_then(|obj| obj.get("code"))
        .and_then(|c| c.as_str())
        .map(str::to_string);

    let frame: Frame = serde_json::from_value(value).map_err(|e| CodecError::InvalidFields {
        kind,
        detail: e.to_string(),
    })?;

    // ADR-137, "Operation correlation": a connection-terminal error carries
    // no operation id, and a request-terminal error echoes the one it
    // terminates. An `error` frame violating that pairing is an inconsistent
    // wire state, rejected at decode rather than represented. Enforcement
    // covers the codes in the closed set — the table fixes their scopes. A
    // code outside the set fell back to `Internal` (its true scope is
    // unknown to this version), and ADR-137 directs the client to treat it
    // as `internal` rather than reject the frame.
    if let Frame::Error { id, code, .. } = &frame {
        let code_in_closed_set = raw_error_code
            .as_deref()
            .is_some_and(|c| crate::error::WIRE_ERROR_CODES.contains(&c));
        if !code_in_closed_set {
            return Ok(frame);
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

    Ok(frame)
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
