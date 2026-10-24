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
            result: serde_json::json!({}).into(),
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
