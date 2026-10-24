use super::*;

#[test]
fn empty_operation_id_is_rejected_at_deserialization() {
    // An empty operation id can never be a unique caller-
    // generated id, so the wire form rejects it. In-memory
    // construction stays unrestricted (checked below).
    let err = serde_json::from_str::<OperationId>(r#""""#).unwrap_err();
    assert!(
        err.to_string().contains("non-empty"),
        "unexpected error: {err}"
    );
    assert_eq!(OperationId::from(""), OperationId("".to_string()));
}

#[test]
fn non_empty_operation_id_round_trips() {
    let id: OperationId = serde_json::from_str(r#""op-1""#).unwrap();
    assert_eq!(id, OperationId::from("op-1"));
    assert_eq!(serde_json::to_string(&id).unwrap(), r#""op-1""#);
}

#[test]
fn direct_serde_rejects_every_invalid_wire_shape() {
    let frames = [
        Frame::Handshake {
            version: ProtocolVersion::new(0),
        },
        Frame::HandshakeAck {
            version: ProtocolVersion::new(0),
        },
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
        Frame::Error {
            id: Some(OperationId::from("")),
            code: crate::error::WireErrorCode::Internal,
            message: "failure".to_string(),
            unrecognized_code: None,
        },
        Frame::Cancel {
            id: OperationId::from(""),
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
            id: Some(OperationId::from("op-1")),
            code: crate::error::WireErrorCode::FrameTooLarge,
            message: "too big".to_string(),
            unrecognized_code: None,
        },
        Frame::Error {
            id: None,
            code: crate::error::WireErrorCode::Cancelled,
            message: "cancelled".to_string(),
            unrecognized_code: None,
        },
        Frame::Error {
            id: None,
            code: crate::error::WireErrorCode::Internal,
            message: "future".to_string(),
            unrecognized_code: Some("future_code_xyz".to_string()),
        },
    ];

    for frame in frames {
        assert!(
            serde_json::to_vec(&frame).is_err(),
            "invalid frame {:?} serialized successfully",
            frame.kind()
        );
    }
}

#[test]
fn direct_serde_matches_encode_payload_for_a_valid_frame() {
    let frame = Frame::Request {
        id: OperationId::from("op-1"),
        ops: "stats()".to_string(),
        deadline_ms: Some(5000),
        namespace: Some("default".to_string()),
        actor_id: Some("actor".to_string()),
        visible_namespaces: Some(vec!["default".to_string()]),
    };
    let direct = serde_json::to_vec(&frame).unwrap();
    let encoded = crate::codec::encode_frame(&frame).unwrap();
    assert_eq!(direct, &encoded[crate::codec::LENGTH_PREFIX_BYTES..]);
    assert_eq!(serde_json::from_slice::<Frame>(&direct).unwrap(), frame);
}

#[test]
fn unknown_kind_through_direct_serde_is_rejected() {
    let err = serde_json::from_str::<Frame>(r#"{"kind":"ping"}"#).unwrap_err();
    assert!(err.to_string().contains("unknown frame kind"));
}

#[test]
fn client_to_server_kinds_are_all_frame_kinds() {
    for kind in CLIENT_TO_SERVER_KINDS {
        assert!(
            FRAME_KINDS.contains(kind),
            "client-to-server kind {kind:?} is missing from FRAME_KINDS"
        );
    }
}

#[test]
fn direct_serde_rejects_connection_terminal_error_carrying_an_id() {
    // The id/scope rule is enforced inside the visitor itself, so a
    // DIRECT serde decode — bypassing `decode_payload` entirely —
    // cannot represent an inconsistent error frame either.
    let err = serde_json::from_str::<Frame>(
        r#"{"kind":"error","id":"op-1","code":"frame_too_large","message":"too big"}"#,
    )
    .unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains("id/scope rule"),
        "unexpected error: {message}"
    );
    assert!(message.contains("frame_too_large"), "error: {message}");
    assert!(message.contains("op-1"), "error: {message}");
}

#[test]
fn direct_serde_rejects_request_terminal_error_without_an_id() {
    let err = serde_json::from_str::<Frame>(
        r#"{"kind":"error","code":"cancelled","message":"cancelled"}"#,
    )
    .unwrap_err();
    let message = err.to_string();
    assert!(
        message.contains("id/scope rule"),
        "unexpected error: {message}"
    );
    assert!(message.contains("cancelled"), "error: {message}");
}

#[test]
fn direct_serde_accepts_both_consistent_error_scopes() {
    let connection_terminal: Frame = serde_json::from_str(
        r#"{"kind":"error","code":"unsupported_version","message":"no common version"}"#,
    )
    .unwrap();
    assert!(matches!(connection_terminal, Frame::Error { id: None, .. }));

    let request_terminal: Frame = serde_json::from_str(
        r#"{"kind":"error","id":"op-9","code":"deadline_exceeded","message":"too slow"}"#,
    )
    .unwrap();
    match request_terminal {
        Frame::Error {
            id,
            code,
            unrecognized_code,
            ..
        } => {
            assert_eq!(id, Some(OperationId::from("op-9")));
            assert_eq!(code, crate::error::WireErrorCode::DeadlineExceeded);
            assert!(unrecognized_code.is_none());
        }
        other => panic!("expected an error frame, got {other:?}"),
    }
}

#[test]
fn direct_serde_fills_unrecognized_code_for_an_unknown_wire_code() {
    // ADR-137 Amendment 1 Decision 5 keeps this fallback only with an id;
    // the superseded id-less acceptance is now tested as a refusal below.
    let frame: Frame = serde_json::from_str(
        r#"{"kind":"error","id":"op-7","code":"future_code_xyz","message":"from newer peer"}"#,
    )
    .unwrap();
    assert_eq!(
        frame,
        Frame::Error {
            id: Some(OperationId::from("op-7")),
            code: crate::error::WireErrorCode::Internal,
            message: "from newer peer".to_string(),
            unrecognized_code: Some("future_code_xyz".to_string()),
        }
    );
    assert_eq!(
        crate::error::WireErrorCode::Internal.terminal_scope(),
        crate::error::TerminalScope::Request
    );
    assert!(serde_json::to_vec(&frame).is_err());
    assert_eq!(
        crate::codec::encode_frame(&frame).unwrap_err(),
        crate::codec::CodecError::FallbackFrameNotEncodable {
            code: "future_code_xyz".to_string(),
        }
    );
}

#[test]
fn direct_serde_rejects_unknown_wire_code_without_an_id() {
    // Decision 5 is enforced on Frame itself, independently of a client
    // or server gate. Both raw serde and Value-driven entry points agree.
    let json = r#"{"kind":"error","code":"future_code_xyz","message":"from newer peer"}"#;
    let value: serde_json::Value = serde_json::from_str(json).unwrap();
    let results = [
        ("raw serde", serde_json::from_str::<Frame>(json)),
        ("Value serde", serde_json::from_value::<Frame>(value)),
    ];
    for (entrypoint, result) in results {
        assert!(
            result.is_err(),
            "D5 {entrypoint} accepted an id-less unknown error code"
        );
        let error = result.unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("id/scope rule"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains("unknown error code requires an operation id"),
            "unexpected error: {message}"
        );
    }
}

#[test]
fn direct_serde_leaves_unrecognized_code_none_for_a_literal_internal() {
    // `"internal"` is a closed-set member, not a fallback: the
    // diagnostic must stay `None` so the encode side does not mistake
    // an honest `internal` for a decoded-fallback frame.
    let frame: Frame =
        serde_json::from_str(r#"{"kind":"error","id":"op-1","code":"internal","message":"boom"}"#)
            .unwrap();
    match frame {
        Frame::Error {
            unrecognized_code, ..
        } => assert!(unrecognized_code.is_none()),
        other => panic!("expected an error frame, got {other:?}"),
    }
}

#[test]
fn direct_serde_rejects_duplicate_envelope_keys_before_field_validation() {
    // Amendment 1 Decision 4 supersedes lossy root-map last-wins behavior.
    // Missing/unknown fields and invalid first values must not hide a duplicate.
    for raw in [
        r#"{"kind":"cancel","kind":"cancel","id":"op-1"}"#,
        r#"{"kind":"cancel","id":"","id":"op-1"}"#,
        r#"{"kind":"cancel","id":"op-1","\u0069d":"op-1"}"#,
        r#"{"kind":"cancel","id":"op-1","extra":1,"extra":2}"#,
        r#"{"extra":1,"extra":2}"#,
    ] {
        let error = serde_json::from_str::<Frame>(raw).unwrap_err();
        assert!(
            error.to_string().contains("duplicate envelope field"),
            "D4 must reject the duplicate before field validation: {raw}: {error}"
        );
    }
}

#[test]
fn direct_serde_distinguishes_omitted_optional_fields_from_null() {
    let cases = [
        (
            "deadline_ms",
            r#"{"kind":"request","id":"op-1","ops":"stats()","deadline_ms":null}"#,
            r#"{"kind":"request","id":"op-1","ops":"stats()"}"#,
        ),
        (
            "namespace",
            r#"{"kind":"request","id":"op-1","ops":"stats()","namespace":null}"#,
            r#"{"kind":"request","id":"op-1","ops":"stats()"}"#,
        ),
        (
            "actor_id",
            r#"{"kind":"request","id":"op-1","ops":"stats()","actor_id":null}"#,
            r#"{"kind":"request","id":"op-1","ops":"stats()"}"#,
        ),
        (
            "visible_namespaces",
            r#"{"kind":"request","id":"op-1","ops":"stats()","visible_namespaces":null}"#,
            r#"{"kind":"request","id":"op-1","ops":"stats()"}"#,
        ),
        (
            "resume_cursor",
            r#"{"kind":"subscribe","id":"op-1","topic":"a.b","resume_cursor":null}"#,
            r#"{"kind":"subscribe","id":"op-1","topic":"a.b"}"#,
        ),
        (
            "id",
            r#"{"kind":"error","id":null,"code":"unsupported_version","message":"unsupported"}"#,
            r#"{"kind":"error","code":"unsupported_version","message":"unsupported"}"#,
        ),
    ];
    for (field, raw, omitted) in cases {
        // Prove the otherwise equivalent omitted form is a valid frame first.
        let healthy: Frame = serde_json::from_str(omitted).unwrap();
        assert!(serde_json::to_value(healthy).unwrap().get(field).is_none());
        let error = serde_json::from_str::<Frame>(raw).unwrap_err();
        assert!(error.to_string().contains("null"), "D4 {field}: {error}");
    }

    // The same names, nulls and repeated members inside opaque JSON are data.
    let response: Frame = serde_json::from_str(
        r#"{"kind":"response","id":"op-1","result":{"namespace":null,"id":1,"id":2}}"#,
    )
    .unwrap();
    let Frame::Response { result, .. } = response else {
        panic!("expected response")
    };
    assert_eq!(result.get("namespace"), Some(&serde_json::Value::Null));
    let response_null: Frame =
        serde_json::from_str(r#"{"kind":"response","id":"op-1","result":null}"#).unwrap();
    assert!(matches!(
        response_null,
        Frame::Response {
            result: serde_json::Value::Null,
            ..
        }
    ));
    let event: Frame = serde_json::from_str(
        r#"{"kind":"event","topic":"a.b","cursor":1,"occurred_at":"2026-08-04T11:00:00Z","payload":{"resume_cursor":null,"id":1,"id":2}}"#,
    ).unwrap();
    let Frame::Event { payload, .. } = event else {
        panic!("expected event")
    };
    assert_eq!(payload.get("resume_cursor"), Some(&serde_json::Value::Null));
}

#[test]
fn direct_serde_requires_an_object_event_payload() {
    let healthy: Frame = serde_json::from_str(
        r#"{"kind":"event","topic":"a.b","cursor":1,"occurred_at":"2026-08-04T11:00:00Z","payload":{"nested":[null,true]}}"#,
    ).unwrap();
    assert!(matches!(healthy, Frame::Event { payload, .. } if payload.is_object()));

    for payload in ["[]", r#""text""#, "1", "true", "null"] {
        let raw = format!(
            r#"{{"kind":"event","topic":"a.b","cursor":1,"occurred_at":"2026-08-04T11:00:00Z","payload":{payload}}}"#
        );
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        for (entrypoint, result) in [
            ("raw serde", serde_json::from_str::<Frame>(&raw)),
            ("Value serde", serde_json::from_value::<Frame>(value)),
        ] {
            let error = result.unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("event payload must be a JSON object"),
                "D10 {entrypoint} returned an unrelated refusal: {error}"
            );
        }
    }
}

#[test]
fn direct_and_codec_serialization_require_an_object_event_payload() {
    // Invalid local frames are constructed directly. D10 decode must not be
    // used as setup for an encode-only refusal witness.
    let event = |payload| Frame::Event {
        topic: "a.b".to_string(),
        cursor: 1,
        occurred_at: "2026-08-04T11:00:00Z".to_string(),
        payload,
    };
    let healthy = event(serde_json::json!({"nested": [null, true]}));
    let direct = serde_json::to_vec(&healthy).unwrap();
    let wire = crate::codec::encode_frame(&healthy).unwrap();
    assert_eq!(direct, &wire[crate::codec::LENGTH_PREFIX_BYTES..]);
    let response_null = Frame::Response {
        id: OperationId::from("op-1"),
        result: serde_json::Value::Null,
    };
    serde_json::to_vec(&response_null).unwrap();
    crate::codec::encode_frame(&response_null).unwrap();

    for payload in [
        serde_json::json!([]),
        serde_json::json!("text"),
        serde_json::json!(1),
        serde_json::json!(true),
        serde_json::Value::Null,
    ] {
        let frame = event(payload);
        let direct_error = serde_json::to_vec(&frame).unwrap_err();
        assert!(
            direct_error
                .to_string()
                .contains("payload must be a JSON object"),
            "D10 direct serialization returned an unrelated refusal: {direct_error}"
        );
        match crate::codec::encode_frame(&frame).unwrap_err() {
            crate::codec::CodecError::InvalidFields { kind, detail } => {
                assert_eq!(kind, "event");
                assert_eq!(detail, "payload must be a JSON object");
            }
            other => panic!("D10 expected typed event payload refusal, got {other:?}"),
        }
    }
}
