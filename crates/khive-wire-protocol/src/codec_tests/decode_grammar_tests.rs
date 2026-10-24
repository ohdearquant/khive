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

#[test]
fn rejects_raw_duplicate_envelope_keys_without_value_fallback_acceptance() {
    let codec = FrameCodec::default();
    let healthy = Frame::Cancel {
        id: OperationId::from("op-1"),
    };
    assert_eq!(
        codec.decode(&codec.encode(&healthy).unwrap()).unwrap(),
        healthy
    );
    for payload in [
        br#"{"kind":"cancel","id":"op-1","id":"op-1"}"#.as_slice(),
        br#"{"kind":"cancel","id":"","id":"op-1"}"#.as_slice(),
        br#"{"kind":"cancel","id":"op-1","\u0069d":"op-1"}"#.as_slice(),
        br#"{"kind":"cancel","kind":"cancel","id":"op-1"}"#.as_slice(),
        br#"{"kind":"handshake","version":0,"version":1}"#.as_slice(),
        br#"{"kind":"cancel","id":"op-1","extra":1,"extra":2}"#.as_slice(),
    ] {
        let mut wire = u32::try_from(payload.len()).unwrap().to_be_bytes().to_vec();
        wire.extend_from_slice(payload);
        match codec.decode_with_consumed(&wire).unwrap_err() {
            CodecError::InvalidFields { detail, .. } => assert!(
                detail.contains("duplicate envelope field"),
                "D4 raw duplicate was not the refusal: {detail}"
            ),
            other => panic!("expected a duplicate-envelope refusal, got {other:?}"),
        }
    }
}
