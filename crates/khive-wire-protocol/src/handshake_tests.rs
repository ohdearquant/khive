use super::*;
use crate::version::CURRENT_VERSION;

#[test]
fn accepts_current_version() {
    let mut gate = HandshakeGate::default();
    let outcome = gate
        .admit(&Frame::Handshake {
            version: CURRENT_VERSION,
        })
        .unwrap();
    assert!(matches!(outcome, HandshakeOutcome::Accepted { .. }));
    assert!(gate.is_complete());
    assert_eq!(gate.accepted_version(), Some(CURRENT_VERSION));
}

#[test]
fn rejects_unsupported_version() {
    let mut gate = HandshakeGate::default();
    let outcome = gate
        .admit(&Frame::Handshake {
            version: ProtocolVersion::new(9999),
        })
        .unwrap();
    match outcome {
        HandshakeOutcome::Rejected { error } => match error {
            Frame::Error { code, .. } => assert_eq!(code, WireErrorCode::UnsupportedVersion),
            _ => panic!("expected an error frame"),
        },
        other => panic!("expected Rejected, got {other:?}"),
    }
}

#[test]
fn rejects_request_before_handshake() {
    let mut gate = HandshakeGate::default();
    let result = gate.admit(&Frame::Cancel {
        id: crate::frame::OperationId::from("op-1"),
    });
    assert!(result.is_err());
}

#[test]
fn sequence_error_displays_the_contained_refusal_reason() {
    let mut gate = HandshakeGate::default();
    let error = gate
        .admit(&Frame::Cancel {
            id: crate::frame::OperationId::from("op-1"),
        })
        .unwrap_err();
    assert!(error.to_string().contains("malformed_frame"));
    assert!(error.to_string().contains("expected \"handshake\""));

    fn assert_error<T: std::error::Error>() {}
    assert_error::<HandshakeSequenceError>();
}

#[test]
fn admits_ordinary_frames_after_handshake() {
    let mut gate = HandshakeGate::default();
    gate.admit(&Frame::Handshake {
        version: CURRENT_VERSION,
    })
    .unwrap();
    let outcome = gate
        .admit(&Frame::Cancel {
            id: crate::frame::OperationId::from("op-1"),
        })
        .unwrap();
    assert_eq!(outcome, HandshakeOutcome::Admitted);
}

#[test]
fn rejects_second_handshake() {
    let mut gate = HandshakeGate::default();
    gate.admit(&Frame::Handshake {
        version: CURRENT_VERSION,
    })
    .unwrap();
    let result = gate.admit(&Frame::Handshake {
        version: CURRENT_VERSION,
    });
    assert!(result.is_err());
}

fn completed_gate() -> HandshakeGate {
    let mut gate = HandshakeGate::default();
    gate.admit(&Frame::Handshake {
        version: CURRENT_VERSION,
    })
    .unwrap();
    gate
}

#[test]
fn rejects_server_only_kinds_on_the_inbound_gate() {
    // After handshake completion the gate must not admit kinds that are
    // only ever sent server→client; every client→server kind stays
    // admitted (checked in the next test).
    let server_only_frames = [
        Frame::HandshakeAck {
            version: CURRENT_VERSION,
        },
        Frame::Response {
            id: crate::frame::OperationId::from("op-1"),
            result: serde_json::json!({}).into(),
        },
        Frame::Error {
            id: None,
            code: WireErrorCode::Internal,
            message: "x".to_string(),
            unrecognized_code: None,
        },
        Frame::SubscribeAck {
            id: crate::frame::OperationId::from("op-2"),
            topic: "a.b".to_string(),
            start_cursor: 1,
        },
        Frame::UnsubscribeAck {
            id: crate::frame::OperationId::from("op-3"),
            topic: "a.b".to_string(),
        },
        Frame::Event {
            topic: "a.b".to_string(),
            cursor: 1,
            occurred_at: "2026-08-04T11:00:00Z".to_string(),
            payload: serde_json::json!({}).into(),
        },
    ];
    for frame in server_only_frames {
        let mut gate = completed_gate();
        assert!(
            gate.admit(&frame).is_err(),
            "server-only kind {:?} must be rejected on the inbound gate",
            frame.kind()
        );
    }
}

#[test]
fn rejects_a_server_only_kind_with_the_gates_rejection_shape() {
    let mut gate = completed_gate();
    let response = Frame::Response {
        id: crate::frame::OperationId::from("op-1"),
        result: serde_json::json!({}).into(),
    };
    let err = gate.admit(&response).unwrap_err();
    match err.error.as_ref() {
        Frame::Error {
            id, code, message, ..
        } => {
            assert_eq!(id, &None);
            assert_eq!(*code, WireErrorCode::MalformedFrame);
            assert!(message.contains("server-to-client"), "message: {message}");
        }
        other => panic!("expected an error frame, got {other:?}"),
    }
    // The direction violation closes the gate like any other protocol
    // violation: subsequent frames are rejected too.
    let err = gate
        .admit(&Frame::Cancel {
            id: crate::frame::OperationId::from("op-1"),
        })
        .unwrap_err();
    match err.error.as_ref() {
        Frame::Error { code, .. } => assert_eq!(*code, WireErrorCode::MalformedFrame),
        other => panic!("expected an error frame, got {other:?}"),
    }
}

#[test]
fn admits_every_client_kind_after_handshake() {
    // Direction-gate control arm: every kind a client may send stays
    // admitted after the handshake (a repeat `handshake` keeps its
    // existing rejection handling and is checked separately).
    let client_frames = [
        Frame::Request {
            id: crate::frame::OperationId::from("op-1"),
            ops: "stats()".to_string(),
            deadline_ms: None,
            namespace: None,
            actor_id: None,
            visible_namespaces: None,
        },
        Frame::Cancel {
            id: crate::frame::OperationId::from("op-1"),
        },
        Frame::Subscribe {
            id: crate::frame::OperationId::from("op-2"),
            topic: "a.b".to_string(),
            resume_cursor: None,
        },
        Frame::Unsubscribe {
            id: crate::frame::OperationId::from("op-3"),
            topic: "a.b".to_string(),
        },
    ];
    for frame in client_frames {
        let mut gate = completed_gate();
        let outcome = gate
            .admit(&frame)
            .unwrap_or_else(|_| panic!("kind {:?} should be admitted", frame.kind()));
        assert_eq!(
            outcome,
            HandshakeOutcome::Admitted,
            "kind {:?}",
            frame.kind()
        );
    }
}

#[test]
fn stays_closed_after_a_rejected_handshake() {
    // A rejected handshake produces a connection-terminal
    // `unsupported_version` error; the gate must stay closed to every
    // later frame, reporting the sequence-violation code AND the
    // preserved closure reason (a rejected handshake — not some other
    // cause).
    let mut gate = HandshakeGate::default();
    let outcome = gate
        .admit(&Frame::Handshake {
            version: ProtocolVersion::new(9999),
        })
        .unwrap();
    assert!(matches!(outcome, HandshakeOutcome::Rejected { .. }));

    for frame in [
        Frame::Handshake {
            version: CURRENT_VERSION,
        },
        Frame::Request {
            id: crate::frame::OperationId::from("op-1"),
            ops: "stats()".to_string(),
            deadline_ms: None,
            namespace: None,
            actor_id: None,
            visible_namespaces: None,
        },
    ] {
        let err = gate.admit(&frame).unwrap_err();
        match err.error.as_ref() {
            Frame::Error { code, message, .. } => {
                assert_eq!(*code, WireErrorCode::MalformedFrame);
                assert!(
                    message.contains("closed by a rejected handshake"),
                    "message: {message}"
                );
            }
            other => panic!("expected an error frame, got {other:?}"),
        }
    }
}

#[test]
fn closed_state_reports_a_sequence_violation_reason_not_handshake() {
    // Closure caused by a non-handshake first frame: the Closed-state
    // rejection must report THAT cause, not blame a handshake failure.
    let mut gate = HandshakeGate::default();
    gate.admit(&Frame::Cancel {
        id: crate::frame::OperationId::from("op-1"),
    })
    .unwrap_err();

    let err = gate
        .admit(&Frame::Handshake {
            version: CURRENT_VERSION,
        })
        .unwrap_err();
    match err.error.as_ref() {
        Frame::Error { code, message, .. } => {
            assert_eq!(*code, WireErrorCode::MalformedFrame);
            assert!(
                message.contains("closed by a frame before the handshake"),
                "message: {message}"
            );
            assert!(
                !message.contains("handshake failure") && !message.contains("rejected handshake"),
                "sequence-violation closure must not blame a handshake: {message}"
            );
        }
        other => panic!("expected an error frame, got {other:?}"),
    }
}

#[test]
fn closed_state_reports_a_duplicate_handshake_reason() {
    let mut gate = completed_gate();
    gate.admit(&Frame::Handshake {
        version: CURRENT_VERSION,
    })
    .unwrap_err();

    let err = gate
        .admit(&Frame::Cancel {
            id: crate::frame::OperationId::from("op-1"),
        })
        .unwrap_err();
    match err.error.as_ref() {
        Frame::Error { message, .. } => {
            assert!(
                message.contains("closed by a duplicate handshake"),
                "message: {message}"
            );
        }
        other => panic!("expected an error frame, got {other:?}"),
    }
}

#[test]
fn closed_state_reports_a_server_only_kind_reason() {
    let mut gate = completed_gate();
    gate.admit(&Frame::Response {
        id: crate::frame::OperationId::from("op-1"),
        result: serde_json::json!({}).into(),
    })
    .unwrap_err();

    let err = gate
        .admit(&Frame::Cancel {
            id: crate::frame::OperationId::from("op-1"),
        })
        .unwrap_err();
    match err.error.as_ref() {
        Frame::Error { message, .. } => {
            assert!(
                message.contains("closed by a server-to-client frame"),
                "message: {message}"
            );
        }
        other => panic!("expected an error frame, got {other:?}"),
    }
}
