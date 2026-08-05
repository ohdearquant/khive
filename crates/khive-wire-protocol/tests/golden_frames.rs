//! Byte-exact golden-frame fixtures for every frame kind, round-tripped
//! both directions, plus malformed-frame decode cases.
//!
//! Fixtures live in `tests/fixtures/*.hex`: each file holds the full wire
//! representation (4-byte BE length prefix + JSON payload) of one canonical
//! frame, as a single line of lowercase hex. A test both decodes the
//! fixture into a [`Frame`] and re-encodes that `Frame`, asserting the
//! re-encoded bytes match the fixture byte-for-byte — the round trip is
//! checked in both directions, not just that decode succeeds.
//!
//! Byte-exactness here DEPENDS on the workspace's serde_json feature
//! posture (crates/Cargo.toml: `serde_json = "1.0"`, default features —
//! no `preserve_order`, no `arbitrary_precision`). Cargo features unify
//! additively across the workspace, so a `preserve_order` enabled by any
//! other crate would silently change key order and break these fixtures.
//! The `serde_json_feature_posture_is_default_keys_serialize_sorted` probe
//! in `src/codec.rs` pins that posture at test time.

use khive_wire_protocol::frame::OperationId;
use khive_wire_protocol::{
    decode_frame, encode_frame, CodecError, Frame, ProtocolVersion, WireErrorCode,
};

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/{name}.hex", env!("CARGO_MANIFEST_DIR"));
    let hex = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {path}: {e}"));
    let hex = hex.trim();
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

fn assert_golden(name: &str, expected: &Frame) {
    let wire = fixture(name);
    let decoded = decode_frame(&wire, khive_wire_protocol::DEFAULT_MAX_FRAME_BYTES)
        .unwrap_or_else(|e| panic!("decoding fixture {name}: {e}"));
    assert_eq!(
        &decoded, expected,
        "fixture {name} decoded to an unexpected frame"
    );

    let re_encoded = encode_frame(expected).unwrap();
    assert_eq!(
        re_encoded, wire,
        "fixture {name} does not round-trip byte-for-byte"
    );
}

#[test]
fn handshake() {
    assert_golden(
        "handshake",
        &Frame::Handshake {
            version: ProtocolVersion::new(1),
        },
    );
}

#[test]
fn handshake_ack() {
    assert_golden(
        "handshake_ack",
        &Frame::HandshakeAck {
            version: ProtocolVersion::new(1),
        },
    );
}

#[test]
fn request() {
    assert_golden(
        "request",
        &Frame::Request {
            id: OperationId::from("op-1"),
            ops: "stats()".to_string(),
            deadline_ms: Some(5000),
            namespace: None,
            actor_id: None,
            visible_namespaces: None,
        },
    );
}

#[test]
fn response() {
    assert_golden(
        "response",
        &Frame::Response {
            id: OperationId::from("op-1"),
            result: serde_json::json!({"ok": true, "tool": "stats", "result": {"entities": 3}}),
        },
    );
}

#[test]
fn error() {
    assert_golden(
        "error",
        &Frame::Error {
            id: Some(OperationId::from("op-1")),
            code: WireErrorCode::PeerClassDenied,
            message: "verb 'delete' is outside the mapped class allowlist".to_string(),
            unrecognized_code: None,
        },
    );
}

#[test]
fn connection_terminal_error_carries_no_id() {
    // A connection-terminal error (e.g. a rejected handshake) omits `id`
    // entirely; it must round-trip byte-exactly without the field.
    let frame = Frame::Error {
        id: None,
        code: WireErrorCode::UnsupportedVersion,
        message: "unsupported protocol version 9999; server supports [1, 1]".to_string(),
        unrecognized_code: None,
    };
    let wire = encode_frame(&frame).unwrap();
    let payload = br#"{"kind":"error","code":"unsupported_version","message":"unsupported protocol version 9999; server supports [1, 1]"}"#;
    assert_eq!(&wire[4..], payload.as_slice());
    // `max_frame_bytes` is payload-only by definition: pass the payload
    // length, not the total wire length (which would admit payloads 4
    // bytes over the bound).
    let payload_len = wire.len() - khive_wire_protocol::codec::LENGTH_PREFIX_BYTES;
    assert_eq!(decode_frame(&wire, payload_len).unwrap(), frame);

    // Decoding a raw payload that lacks the `id` field yields `id: None`.
    let decoded = khive_wire_protocol::codec::decode_payload(payload).unwrap();
    assert_eq!(decoded, frame);
}

#[test]
fn cancel() {
    assert_golden(
        "cancel",
        &Frame::Cancel {
            id: OperationId::from("op-1"),
        },
    );
}

#[test]
fn subscribe() {
    assert_golden(
        "subscribe",
        &Frame::Subscribe {
            id: OperationId::from("op-2"),
            topic: "comm.message_created".to_string(),
            resume_cursor: Some(42),
        },
    );
}

#[test]
fn subscribe_ack() {
    assert_golden(
        "subscribe_ack",
        &Frame::SubscribeAck {
            id: OperationId::from("op-2"),
            topic: "comm.message_created".to_string(),
            start_cursor: 42,
        },
    );
}

#[test]
fn unsubscribe() {
    assert_golden(
        "unsubscribe",
        &Frame::Unsubscribe {
            id: OperationId::from("op-3"),
            topic: "comm.message_created".to_string(),
        },
    );
}

#[test]
fn unsubscribe_ack() {
    assert_golden(
        "unsubscribe_ack",
        &Frame::UnsubscribeAck {
            id: OperationId::from("op-3"),
            topic: "comm.message_created".to_string(),
        },
    );
}

#[test]
fn event() {
    assert_golden(
        "event",
        &Frame::Event {
            topic: "comm.message_created".to_string(),
            cursor: 43,
            occurred_at: "2026-08-04T11:00:00Z".to_string(),
            payload: serde_json::json!({"message_id": "m-1"}),
        },
    );
}

// ── malformed-frame decode cases ────────────────────────────────────────

#[test]
fn malformed_truncated_length_prefix() {
    // Only 2 of the required 4 length-prefix bytes.
    let buf = [0x00u8, 0x00];
    let err = decode_frame(&buf, khive_wire_protocol::DEFAULT_MAX_FRAME_BYTES).unwrap_err();
    assert_eq!(err, CodecError::TruncatedLengthPrefix { available: 2 });
}

#[test]
fn malformed_length_exceeding_max() {
    // A well-formed cancel frame, decoded against a max smaller than its
    // declared length.
    let wire = fixture("cancel");
    let declared = u32::from_be_bytes(wire[0..4].try_into().unwrap()) as usize;
    let max = declared - 1;
    let err = decode_frame(&wire, max).unwrap_err();
    assert_eq!(err, CodecError::FrameTooLarge { declared, max });
}

#[test]
fn malformed_non_json_payload() {
    let payload = b"this is not json";
    let mut wire = (payload.len() as u32).to_be_bytes().to_vec();
    wire.extend_from_slice(payload);
    let err = decode_frame(&wire, khive_wire_protocol::DEFAULT_MAX_FRAME_BYTES).unwrap_err();
    match err {
        CodecError::InvalidJson(_) => {}
        other => panic!("expected InvalidJson, got {other:?}"),
    }
}

#[test]
fn malformed_unknown_frame_kind() {
    let payload = br#"{"kind":"ping","id":"op-1"}"#;
    let mut wire = (payload.len() as u32).to_be_bytes().to_vec();
    wire.extend_from_slice(payload);
    let err = decode_frame(&wire, khive_wire_protocol::DEFAULT_MAX_FRAME_BYTES).unwrap_err();
    assert_eq!(err, CodecError::UnknownFrameKind("ping".to_string()));
}

#[test]
fn malformed_missing_required_field() {
    // "subscribe" without its required "topic" field.
    let payload = br#"{"kind":"subscribe","id":"op-1"}"#;
    let mut wire = (payload.len() as u32).to_be_bytes().to_vec();
    wire.extend_from_slice(payload);
    let err = decode_frame(&wire, khive_wire_protocol::DEFAULT_MAX_FRAME_BYTES).unwrap_err();
    match err {
        CodecError::InvalidFields { kind, detail } => {
            assert_eq!(kind, "subscribe");
            assert!(detail.contains("topic"), "detail was: {detail}");
        }
        other => panic!("expected InvalidFields, got {other:?}"),
    }
}

#[test]
fn trailing_bytes_beyond_the_first_frame_are_ignored() {
    // The documented contract: `decode_frame` consumes exactly one frame
    // and ignores any bytes beyond it; slicing a stream into frames is the
    // transport's job.
    let first = fixture("cancel");
    let second = fixture("handshake");
    let mut buf = first.clone();
    buf.extend_from_slice(&second);
    let decoded = decode_frame(&buf, khive_wire_protocol::DEFAULT_MAX_FRAME_BYTES).unwrap();
    assert_eq!(
        decoded,
        Frame::Cancel {
            id: OperationId::from("op-1"),
        }
    );
}

#[test]
fn frame_kinds_matches_the_frame_enum() {
    // `FRAME_KINDS` must cover every `Frame` variant exactly; otherwise the
    // codec's closed-set check would reject (or admit) the wrong kinds.
    let variants = [
        Frame::Handshake {
            version: ProtocolVersion::new(1),
        },
        Frame::HandshakeAck {
            version: ProtocolVersion::new(1),
        },
        Frame::Request {
            id: OperationId::from("op-1"),
            ops: "stats()".to_string(),
            deadline_ms: None,
            namespace: None,
            actor_id: None,
            visible_namespaces: None,
        },
        Frame::Response {
            id: OperationId::from("op-1"),
            result: serde_json::json!({}),
        },
        // A connection-terminal code with no id: a scope-consistent
        // pairing the decode-time check accepts (request-terminal codes
        // must echo an id; see `codec::InconsistentErrorScope`).
        Frame::Error {
            id: None,
            code: WireErrorCode::MalformedFrame,
            message: String::new(),
            unrecognized_code: None,
        },
        Frame::Cancel {
            id: OperationId::from("op-1"),
        },
        Frame::Subscribe {
            id: OperationId::from("op-1"),
            topic: "domain.event".to_string(),
            resume_cursor: None,
        },
        Frame::SubscribeAck {
            id: OperationId::from("op-1"),
            topic: "domain.event".to_string(),
            start_cursor: 0,
        },
        Frame::Unsubscribe {
            id: OperationId::from("op-1"),
            topic: "domain.event".to_string(),
        },
        Frame::UnsubscribeAck {
            id: OperationId::from("op-1"),
            topic: "domain.event".to_string(),
        },
        Frame::Event {
            topic: "domain.event".to_string(),
            cursor: 0,
            occurred_at: "2026-08-04T11:00:00Z".to_string(),
            payload: serde_json::json!({}),
        },
    ];
    assert_eq!(
        variants.len(),
        khive_wire_protocol::FRAME_KINDS.len(),
        "FRAME_KINDS and the Frame enum have drifted"
    );
    for variant in &variants {
        let kind = variant.kind();
        assert!(
            khive_wire_protocol::FRAME_KINDS.contains(&kind),
            "FRAME_KINDS is missing {kind:?}"
        );
        // Each kind must actually decode through the closed-set check.
        let wire = encode_frame(variant).unwrap();
        decode_frame(&wire, khive_wire_protocol::DEFAULT_MAX_FRAME_BYTES).unwrap();
    }
    for &kind in khive_wire_protocol::FRAME_KINDS {
        assert!(
            variants.iter().any(|v| v.kind() == kind),
            "FRAME_KINDS entry {kind:?} has no Frame variant"
        );
    }
}
