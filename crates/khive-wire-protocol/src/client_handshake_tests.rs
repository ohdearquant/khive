use crate::{
    encode_frame, ClientHandshakeGate, ClientHandshakeOutcome, ClientHandshakeStartError,
    CodecError, Frame, HandshakeSequenceError, OperationId, ProtocolVersion, TerminalScope,
    WireErrorCode, CURRENT_VERSION, FRAME_KINDS,
};

const CONNECTION_CODES: [WireErrorCode; 6] = [
    WireErrorCode::UnsupportedVersion,
    WireErrorCode::IdentityRejected,
    WireErrorCode::MalformedFrame,
    WireErrorCode::FrameTooLarge,
    WireErrorCode::SubscriberOverflow,
    WireErrorCode::SubscriptionRevoked,
];

const REQUEST_CODES: [WireErrorCode; 10] = [
    WireErrorCode::ContextRejected,
    WireErrorCode::PeerClassDenied,
    WireErrorCode::SubscriptionDenied,
    WireErrorCode::AlreadySubscribed,
    WireErrorCode::CursorExpired,
    WireErrorCode::InFlightLimitExceeded,
    WireErrorCode::DeadlineExceeded,
    WireErrorCode::Cancelled,
    WireErrorCode::ShuttingDown,
    WireErrorCode::Internal,
];

fn ack(version: u32) -> Frame {
    Frame::HandshakeAck {
        version: ProtocolVersion::new(version),
    }
}

fn error_frame(code: WireErrorCode, id: Option<&str>) -> Frame {
    Frame::Error {
        id: id.map(OperationId::from),
        code,
        message: format!("original server diagnostic: {code}"),
        unrecognized_code: None,
    }
}

fn unknown_request_error() -> Frame {
    Frame::Error {
        id: Some(OperationId::from("request-unknown")),
        code: WireErrorCode::Internal,
        message: "original newer-server diagnostic".to_string(),
        unrecognized_code: Some("newer_request_error".to_string()),
    }
}

fn all_kinds() -> Vec<Frame> {
    let frames = vec![
        Frame::Handshake {
            version: CURRENT_VERSION,
        },
        ack(CURRENT_VERSION.get()),
        Frame::Request {
            id: OperationId::from("request-1"),
            ops: "stats()".to_string(),
            deadline_ms: None,
            namespace: None,
            actor_id: None,
            visible_namespaces: None,
        },
        Frame::Response {
            id: OperationId::from("request-1"),
            result: serde_json::json!({"value": 7}),
        },
        error_frame(WireErrorCode::PeerClassDenied, Some("request-1")),
        Frame::Cancel {
            id: OperationId::from("request-1"),
        },
        Frame::Subscribe {
            id: OperationId::from("subscribe-1"),
            topic: "domain.event".to_string(),
            resume_cursor: None,
        },
        Frame::SubscribeAck {
            id: OperationId::from("subscribe-1"),
            topic: "domain.event".to_string(),
            start_cursor: 4,
        },
        Frame::Unsubscribe {
            id: OperationId::from("unsubscribe-1"),
            topic: "domain.event".to_string(),
        },
        Frame::UnsubscribeAck {
            id: OperationId::from("unsubscribe-1"),
            topic: "domain.event".to_string(),
        },
        Frame::Event {
            topic: "domain.event".to_string(),
            cursor: 5,
            occurred_at: "2026-10-24T00:00:00Z".to_string(),
            payload: serde_json::json!({"value": 8}),
        },
    ];
    assert_eq!(frames.len(), 11);
    assert_eq!(frames.len(), FRAME_KINDS.len());
    for kind in FRAME_KINDS {
        assert_eq!(
            frames.iter().filter(|frame| frame.kind() == *kind).count(),
            1
        );
    }
    frames
}

fn outstanding(version: u32) -> ClientHandshakeGate {
    let mut gate = ClientHandshakeGate::new();
    gate.record_handshake_sent(ProtocolVersion::new(version))
        .unwrap();
    assert_eq!(gate.offered_version(), Some(ProtocolVersion::new(version)));
    assert!(!gate.is_complete());
    assert!(!gate.is_closed());
    gate
}

fn completed() -> ClientHandshakeGate {
    let mut gate = outstanding(CURRENT_VERSION.get());
    assert_eq!(
        gate.admit(&ack(CURRENT_VERSION.get())).unwrap(),
        ClientHandshakeOutcome::Completed {
            version: CURRENT_VERSION
        }
    );
    assert!(gate.is_complete());
    assert_eq!(gate.accepted_version(), Some(CURRENT_VERSION));
    gate
}

fn assert_malformed(error: HandshakeSequenceError) {
    match *error.error {
        Frame::Error {
            id,
            code,
            unrecognized_code,
            ..
        } => {
            assert_eq!(
                id, None,
                "a sequence failure must not identify an operation"
            );
            assert_eq!(code, WireErrorCode::MalformedFrame);
            assert_eq!(code.terminal_scope(), TerminalScope::Connection);
            assert_eq!(unrecognized_code, None);
        }
        other => panic!("expected id-less malformed_frame, got {other:?}"),
    }
}

fn assert_closed(gate: &mut ClientHandshakeGate) {
    assert!(
        gate.is_closed(),
        "terminal admission must permanently close the gate"
    );
    assert!(!gate.is_complete());
    assert_eq!(gate.accepted_version(), None);
    assert_eq!(gate.offered_version(), None);
    assert_malformed(gate.admit(&ack(CURRENT_VERSION.get())).unwrap_err());
    assert_eq!(
        gate.record_handshake_sent(CURRENT_VERSION),
        Err(ClientHandshakeStartError::Closed),
        "a closed connection cannot be reoffered"
    );
    assert!(gate.is_closed());
}

#[test]
fn fresh_rejects_every_inbound_kind_and_stays_closed() {
    let mut frames = all_kinds();
    frames.extend(CONNECTION_CODES.map(|code| error_frame(code, None)));
    for frame in frames {
        let mut gate = ClientHandshakeGate::new();
        assert_eq!(gate.offered_version(), None);
        assert_malformed(gate.admit(&frame).unwrap_err());
        assert_closed(&mut gate);
    }
}

#[test]
fn record_sent_offer_is_exact_and_zero_is_local_misuse() {
    let mut gate = ClientHandshakeGate::default();
    assert!(!gate.is_complete());
    assert!(!gate.is_closed());
    assert_eq!(gate.offered_version(), None);
    assert_eq!(gate.accepted_version(), None);
    assert_eq!(
        gate.record_handshake_sent(ProtocolVersion::new(0)),
        Err(ClientHandshakeStartError::VersionZero)
    );
    assert!(!gate.is_closed());
    assert!(!gate.is_complete());
    assert_eq!(gate.offered_version(), None);
    gate.record_handshake_sent(CURRENT_VERSION).unwrap();
    assert_eq!(gate.offered_version(), Some(CURRENT_VERSION));

    for version in [1, 99, u32::MAX] {
        let gate = outstanding(version);
        assert_eq!(gate.offered_version(), Some(ProtocolVersion::new(version)));
    }
    for frame in [
        Frame::Handshake {
            version: ProtocolVersion::new(0),
        },
        ack(0),
    ] {
        assert!(matches!(
            encode_frame(&frame),
            Err(CodecError::InvalidFields { .. })
        ));
    }
}

#[test]
fn record_sent_offer_is_single_use_and_cannot_reset() {
    let mut gate = outstanding(CURRENT_VERSION.get());
    assert_eq!(
        gate.record_handshake_sent(ProtocolVersion::new(99)),
        Err(ClientHandshakeStartError::AlreadyStarted)
    );
    assert_eq!(gate.offered_version(), Some(CURRENT_VERSION));
    assert!(!gate.is_closed());
    assert_eq!(
        gate.admit(&ack(CURRENT_VERSION.get())).unwrap(),
        ClientHandshakeOutcome::Completed {
            version: CURRENT_VERSION
        }
    );
    assert_eq!(
        gate.record_handshake_sent(CURRENT_VERSION),
        Err(ClientHandshakeStartError::AlreadyStarted)
    );
    assert_eq!(gate.accepted_version(), Some(CURRENT_VERSION));
    assert!(gate.is_complete());
    assert_malformed(
        gate.admit(&Frame::Handshake {
            version: CURRENT_VERSION,
        })
        .unwrap_err(),
    );
    assert_closed(&mut gate);
    assert_eq!(
        gate.record_handshake_sent(ProtocolVersion::new(0)),
        Err(ClientHandshakeStartError::Closed)
    );
}

#[test]
fn matching_ack_completes_with_the_offered_version() {
    let mut gate = outstanding(CURRENT_VERSION.get());
    assert_eq!(
        gate.admit(&ack(CURRENT_VERSION.get())).unwrap(),
        ClientHandshakeOutcome::Completed {
            version: CURRENT_VERSION
        }
    );
    assert_eq!(gate.accepted_version(), Some(CURRENT_VERSION));
    assert_eq!(gate.offered_version(), None);
    assert!(gate.is_complete());
    assert!(!gate.is_closed());
}

#[test]
fn mismatched_ack_is_malformed_and_permanently_terminal() {
    for version in [0, 2, u32::MAX] {
        let mut gate = outstanding(1);
        assert_malformed(gate.admit(&ack(version)).unwrap_err());
        assert_closed(&mut gate);
        // A correct answer to the original offer cannot recover the connection.
        assert_malformed(gate.admit(&ack(1)).unwrap_err());
    }
}

#[test]
fn outstanding_admits_connection_errors_then_closes() {
    for code in CONNECTION_CODES {
        let mut gate = outstanding(CURRENT_VERSION.get());
        let frame = error_frame(code, None);
        let original = frame.clone();
        assert_eq!(
            gate.admit(&frame).unwrap(),
            ClientHandshakeOutcome::ServerTerminated
        );
        assert_eq!(
            frame, original,
            "the original server rejection must remain available"
        );
        if let Frame::Error {
            code: actual,
            message,
            ..
        } = &frame
        {
            assert_eq!(*actual, code);
            assert_eq!(message, &format!("original server diagnostic: {code}"));
        } else {
            panic!("server error fixture lost its error frame");
        }
        assert_closed(&mut gate);
    }
}

#[test]
fn outstanding_rejects_request_errors_and_every_other_kind() {
    let mut request_errors: Vec<Frame> = REQUEST_CODES
        .into_iter()
        .map(|code| error_frame(code, Some("request-1")))
        .collect();
    request_errors.push(unknown_request_error());
    for frame in request_errors {
        let mut gate = outstanding(CURRENT_VERSION.get());
        assert_malformed(gate.admit(&frame).unwrap_err());
        assert_closed(&mut gate);
    }
    for frame in all_kinds() {
        if matches!(&frame, Frame::HandshakeAck { .. } | Frame::Error { .. }) {
            continue;
        }
        let mut gate = outstanding(CURRENT_VERSION.get());
        assert_malformed(gate.admit(&frame).unwrap_err());
        assert_closed(&mut gate);
    }
}

#[test]
fn completed_admits_server_frames_and_keeps_request_errors_usable() {
    let mut server_frames: Vec<Frame> = all_kinds()
        .into_iter()
        .filter(|frame| {
            matches!(
                frame,
                Frame::Response { .. }
                    | Frame::SubscribeAck { .. }
                    | Frame::UnsubscribeAck { .. }
                    | Frame::Event { .. }
            )
        })
        .collect();
    server_frames.extend(REQUEST_CODES.map(|code| error_frame(code, Some("request-1"))));
    server_frames.push(unknown_request_error());
    let mut gate = completed();
    for frame in server_frames {
        let original = frame.clone();
        assert_eq!(
            gate.admit(&frame).unwrap(),
            ClientHandshakeOutcome::Admitted
        );
        assert_eq!(frame, original);
        assert!(gate.is_complete());
        assert!(!gate.is_closed());
        assert_eq!(gate.accepted_version(), Some(CURRENT_VERSION));
    }
}

#[test]
fn completed_connection_error_is_admitted_then_terminal() {
    for code in CONNECTION_CODES {
        let mut gate = completed();
        let frame = error_frame(code, None);
        assert_eq!(
            gate.admit(&frame).unwrap(),
            ClientHandshakeOutcome::ServerTerminated
        );
        assert_closed(&mut gate);
        let response = Frame::Response {
            id: OperationId::from("request-1"),
            result: serde_json::json!({}),
        };
        assert_malformed(gate.admit(&response).unwrap_err());
    }
}

#[test]
fn completed_rejects_idless_unrecognized_errors_and_stays_closed() {
    for code in [WireErrorCode::Internal, WireErrorCode::UnsupportedVersion] {
        let mut gate = completed();
        let frame = Frame::Error {
            id: None,
            code,
            message: "original newer-server diagnostic".to_string(),
            unrecognized_code: Some("newer_error".to_string()),
        };
        let original = frame.clone();
        assert_malformed(
            gate.admit(&frame)
                .expect_err("an id-less unrecognized error must be refused"),
        );
        assert_eq!(frame, original);
        assert_closed(&mut gate);
    }
}

#[test]
fn completed_rejects_connection_errors_with_ids_and_stays_closed() {
    for code in CONNECTION_CODES {
        let mut gate = completed();
        let frame = error_frame(code, Some("request-1"));
        let original = frame.clone();
        assert_malformed(
            gate.admit(&frame)
                .expect_err("a connection-terminal error with an operation id must be refused"),
        );
        assert_eq!(frame, original);
        assert_closed(&mut gate);
    }
}

#[test]
fn completed_rejects_request_errors_without_ids_and_stays_closed() {
    for code in REQUEST_CODES {
        let mut gate = completed();
        let frame = error_frame(code, None);
        let original = frame.clone();
        assert_malformed(
            gate.admit(&frame)
                .expect_err("a request-terminal error without an operation id must be refused"),
        );
        assert_eq!(frame, original);
        assert_closed(&mut gate);
    }
}

#[test]
fn completed_rejects_ack_and_every_client_frame() {
    for frame in all_kinds() {
        if !matches!(
            &frame,
            Frame::Handshake { .. }
                | Frame::HandshakeAck { .. }
                | Frame::Request { .. }
                | Frame::Cancel { .. }
                | Frame::Subscribe { .. }
                | Frame::Unsubscribe { .. }
        ) {
            continue;
        }
        let mut gate = completed();
        assert_malformed(gate.admit(&frame).unwrap_err());
        assert_closed(&mut gate);
    }
}

#[test]
fn closed_rejects_every_kind_and_cannot_restart() {
    let mut protocol_closed = outstanding(1);
    assert_malformed(protocol_closed.admit(&ack(2)).unwrap_err());
    let mut server_closed = outstanding(1);
    assert_eq!(
        server_closed
            .admit(&error_frame(WireErrorCode::UnsupportedVersion, None))
            .unwrap(),
        ClientHandshakeOutcome::ServerTerminated
    );
    for gate in [&mut protocol_closed, &mut server_closed] {
        assert_closed(gate);
        for frame in all_kinds() {
            assert_malformed(gate.admit(&frame).unwrap_err());
            assert!(gate.is_closed());
            assert_eq!(
                gate.record_handshake_sent(CURRENT_VERSION),
                Err(ClientHandshakeStartError::Closed)
            );
        }
    }
}
