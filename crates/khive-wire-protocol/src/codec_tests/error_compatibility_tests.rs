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
    // Same rule for the id-less shape. Decode refuses an id-less unknown
    // code, so only a caller building the frame directly can hold one;
    // encode must still reject it.
    let frame = Frame::Error {
        id: None,
        code: crate::error::WireErrorCode::Internal,
        message: "from newer peer".to_string(),
        unrecognized_code: Some("future_code_xyz".to_string()),
    };
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
