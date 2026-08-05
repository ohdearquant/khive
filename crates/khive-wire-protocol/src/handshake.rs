//! The server-side handshake state machine (ADR-137, "Shared protocol crate
//! and handshake").
//!
//! The ADR requires: *"the first application frame on every connection must
//! be a `handshake` carrying a protocol version; the answer is
//! `handshake_ack` naming the accepted version, or the wire error
//! `unsupported_version` followed by connection close, before it accepts
//! any request or subscription frame."* [`HandshakeGate`] is a type the
//! server side feeds every inbound frame through: it is impossible to
//! obtain a [`Frame::Request`]/`Subscribe`/etc. admission decision without
//! having first driven a successful handshake through it, so "no request
//! frame before handshake completes" is enforced by the gate's API shape
//! rather than left to every call site to remember.

use crate::error::WireErrorCode;
use crate::frame::Frame;
use crate::version::{ProtocolVersion, SupportedVersions};

/// Why a gate moved to the terminal [`State::Closed`]. Stored at the
/// moment of closure so the Closed-state rejection can report the ACTUAL
/// cause instead of always blaming a handshake failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseReason {
    /// The handshake named no mutually supported version
    /// ([`HandshakeOutcome::Rejected`]).
    UnsupportedVersion,
    /// A non-`handshake` frame arrived before the handshake completed.
    NonHandshakeFirst,
    /// A second `handshake` arrived after one already completed.
    DuplicateHandshake,
    /// A server→client-only frame kind arrived on this inbound gate.
    ServerOnlyKind,
}

impl CloseReason {
    /// The human-readable cause used in the Closed-state rejection message.
    const fn phrase(self) -> &'static str {
        match self {
            CloseReason::UnsupportedVersion => "a rejected handshake (unsupported version)",
            CloseReason::NonHandshakeFirst => "a frame before the handshake",
            CloseReason::DuplicateHandshake => "a duplicate handshake",
            CloseReason::ServerOnlyKind => "a server-to-client frame on an inbound gate",
        }
    }
}

/// The gate's current state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// No handshake has completed yet on this connection.
    AwaitingHandshake,
    /// A handshake completed; the connection speaks this version.
    Completed(ProtocolVersion),
    /// A connection-terminal outcome was already produced (rejected
    /// handshake, or a protocol violation). The gate accepts no further
    /// frames; the carried reason records what closed it.
    Closed(CloseReason),
}

/// The outcome of feeding one frame to [`HandshakeGate::admit`].
#[derive(Debug, Clone, PartialEq)]
pub enum HandshakeOutcome {
    /// The frame was a valid `handshake` naming a mutually supported
    /// version. Send the returned `handshake_ack` frame; the connection may
    /// now accept `request`/`subscribe`/`unsubscribe`/`cancel` frames.
    Accepted {
        ack: Frame,
        version: ProtocolVersion,
    },
    /// The frame was a `handshake` naming no mutually supported version.
    /// Send the returned `error` frame (`unsupported_version`), then close
    /// the connection.
    Rejected { error: Frame },
    /// The connection already completed its handshake, this frame was not
    /// a handshake attempt, and its kind is one a client may send to the
    /// server; it is admitted for ordinary dispatch.
    Admitted,
}

/// A protocol violation the gate detected outside the handshake itself:
/// a non-`handshake` frame arriving before handshake completion, a second
/// `handshake` frame arriving after completion, or a server→client-only
/// frame kind arriving on the server's INBOUND gate after completion.
///
/// ADR-137 does not name a specific wire error code for either violation;
/// this crate maps both to [`WireErrorCode::MalformedFrame`] (connection
/// grammar violation) — see the crate documentation's "Contract choices the
/// ADR did not fix" note.
#[derive(Debug, Clone, PartialEq)]
pub struct HandshakeSequenceError {
    pub error: Box<Frame>,
}

impl std::fmt::Display for HandshakeSequenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.error.as_ref() {
            Frame::Error { code, message, .. } => {
                write!(f, "handshake sequence rejected ({code}): {message}")
            }
            frame => write!(f, "handshake sequence rejected by {} frame", frame.kind()),
        }
    }
}

impl std::error::Error for HandshakeSequenceError {}

/// Drives the per-connection handshake state machine.
///
/// Deliberately not `Clone`: the gate's entire guarantee is that its state
/// only ever moves forward for one connection. A copy of a completed gate
/// would admit requests on a connection that never handshook, and a copy of
/// an earlier state could be restored after a violation closed the original.
#[derive(Debug)]
pub struct HandshakeGate {
    state: State,
    supported: SupportedVersions,
}

impl HandshakeGate {
    /// A new gate for a connection that has not yet handshaken, accepting
    /// [`SupportedVersions::current`].
    pub fn new(supported: SupportedVersions) -> Self {
        Self {
            state: State::AwaitingHandshake,
            supported,
        }
    }

    /// True once a `handshake` has been accepted.
    pub fn is_complete(&self) -> bool {
        matches!(self.state, State::Completed(_))
    }

    /// The accepted protocol version, once the handshake has completed.
    pub fn accepted_version(&self) -> Option<ProtocolVersion> {
        match self.state {
            State::Completed(v) => Some(v),
            _ => None,
        }
    }

    /// Feed one inbound frame to the gate.
    ///
    /// - Before completion, a [`Frame::Handshake`] is evaluated against the
    ///   configured [`SupportedVersions`] and produces
    ///   [`HandshakeOutcome::Accepted`] or [`HandshakeOutcome::Rejected`].
    /// - Before completion, any other frame kind is a sequence violation
    ///   (`Err(`[`HandshakeSequenceError`]`)`) — the caller must never have
    ///   dispatched it to `request`/`subscribe`/etc. handling; this call is
    ///   what makes that guarantee enforceable rather than conventional.
    /// - After completion, client→server frames
    ///   ([`crate::frame::CLIENT_TO_SERVER_KINDS`] minus `handshake`) are
    ///   admitted ([`HandshakeOutcome::Admitted`]). A stray second
    ///   `handshake` is a sequence violation (the ADR fixes the handshake
    ///   to "the first application frame"), and a server→client-only kind
    ///   (`response`, `handshake_ack`, `subscribe_ack`, `unsubscribe_ack`,
    ///   `event`) is a direction violation — it can never be a legal
    ///   inbound frame on this gate — and closes the gate like any other
    ///   protocol violation.
    pub fn admit(&mut self, frame: &Frame) -> Result<HandshakeOutcome, HandshakeSequenceError> {
        match (&self.state, frame) {
            (State::AwaitingHandshake, Frame::Handshake { version }) => {
                if self.supported.contains(*version) {
                    self.state = State::Completed(*version);
                    Ok(HandshakeOutcome::Accepted {
                        ack: Frame::HandshakeAck { version: *version },
                        version: *version,
                    })
                } else {
                    self.state = State::Closed(CloseReason::UnsupportedVersion);
                    Ok(HandshakeOutcome::Rejected {
                        error: Frame::Error {
                            id: None,
                            code: WireErrorCode::UnsupportedVersion,
                            message: format!(
                                "unsupported protocol version {version}; server supports [{}, {}]",
                                self.supported.min(),
                                self.supported.max()
                            ),
                            unrecognized_code: None,
                        },
                    })
                }
            }
            (State::AwaitingHandshake, _) => {
                self.state = State::Closed(CloseReason::NonHandshakeFirst);
                Err(HandshakeSequenceError {
                    error: Box::new(Frame::Error {
                        id: None,
                        code: WireErrorCode::MalformedFrame,
                        message: format!(
                            "expected \"handshake\" as the first frame, got {:?}",
                            frame.kind()
                        ),
                        unrecognized_code: None,
                    }),
                })
            }
            (State::Completed(_), Frame::Handshake { .. }) => {
                self.state = State::Closed(CloseReason::DuplicateHandshake);
                Err(HandshakeSequenceError {
                    error: Box::new(Frame::Error {
                        id: None,
                        code: WireErrorCode::MalformedFrame,
                        message: "handshake already completed on this connection".to_string(),
                        unrecognized_code: None,
                    }),
                })
            }
            (State::Completed(_), _) => {
                // The gate is the server-side inbound admission point, so a
                // frame whose kind is only ever sent server→client is a
                // direction violation (frame grammar) no matter when it
                // arrives: reject it like any other grammar violation.
                if crate::frame::CLIENT_TO_SERVER_KINDS.contains(&frame.kind()) {
                    Ok(HandshakeOutcome::Admitted)
                } else {
                    self.state = State::Closed(CloseReason::ServerOnlyKind);
                    Err(HandshakeSequenceError {
                        error: Box::new(Frame::Error {
                            id: None,
                            code: WireErrorCode::MalformedFrame,
                            message: format!(
                                "frame kind {:?} is server-to-client only; a server never accepts it as an inbound frame",
                                frame.kind()
                            ),
                            unrecognized_code: None,
                        }),
                    })
                }
            }
            (State::Closed(reason), _) => Err(HandshakeSequenceError {
                error: Box::new(Frame::Error {
                    id: None,
                    code: WireErrorCode::MalformedFrame,
                    message: format!("connection already closed by {}", reason.phrase()),
                    unrecognized_code: None,
                }),
            }),
        }
    }
}

impl Default for HandshakeGate {
    fn default() -> Self {
        Self::new(SupportedVersions::current())
    }
}

#[cfg(test)]
mod tests {
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
                result: serde_json::json!({}),
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
                payload: serde_json::json!({}),
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
            result: serde_json::json!({}),
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
                    !message.contains("handshake failure")
                        && !message.contains("rejected handshake"),
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
            result: serde_json::json!({}),
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
}
