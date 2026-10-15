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
    let payload = br#"{"kind":"error","id":"op-1","code":"frame_too_large","message":"too big"}"#;
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
    let payload = br#"{"kind":"error","code":"unsupported_version","message":"no common version"}"#;
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
        Frame::Error {
            id: Some(OperationId::from("")),
            code: crate::error::WireErrorCode::Internal,
            message: "failure".to_string(),
            unrecognized_code: None,
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
fn encode_rejects_zero_protocol_version_in_both_handshake_kinds() {
    for frame in [
        Frame::Handshake {
            version: crate::version::ProtocolVersion::new(0),
        },
        Frame::HandshakeAck {
            version: crate::version::ProtocolVersion::new(0),
        },
    ] {
        match encode_frame(&frame).unwrap_err() {
            CodecError::InvalidFields { kind, detail } => {
                assert_eq!(kind, frame.kind());
                assert!(detail.contains("version 0"), "detail: {detail}");
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

// ── unknown wire code: fallback keeps the raw string ──

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
    let payload = br#"{"kind":"error","id":"op-1","code":"frame_too_large","message":"too big"}"#;
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

// ── fallback frames cannot be re-encoded by this relay ──

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

// ── decode_with_consumed ──

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

// ── serde_json feature posture probe ──

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

// ── CodecError → WireErrorCode canonical mapping ──

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
            WireErrorCode::Internal,
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
