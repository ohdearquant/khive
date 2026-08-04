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

/// The gate's current state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// No handshake has completed yet on this connection.
    AwaitingHandshake,
    /// A handshake completed; the connection speaks this version.
    Completed(ProtocolVersion),
    /// A connection-terminal outcome was already produced (rejected
    /// handshake, or a protocol violation). The gate accepts no further
    /// frames.
    Closed,
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
    /// The connection already completed its handshake; this frame was not
    /// a handshake attempt and is admitted for ordinary dispatch.
    Admitted,
}

/// A protocol violation the gate detected outside the handshake itself:
/// a non-`handshake` frame arriving before handshake completion, or a
/// second `handshake` frame arriving after completion.
///
/// ADR-137 does not name a specific wire error code for either violation;
/// this crate maps both to [`WireErrorCode::MalformedFrame`] (connection
/// grammar violation) — see the crate documentation's "Contract choices the
/// ADR did not fix" note.
#[derive(Debug, Clone, PartialEq)]
pub struct HandshakeSequenceError {
    pub error: Box<Frame>,
}

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
    /// - After completion, every frame (including a stray second
    ///   `handshake`) is a sequence violation except frames the caller
    ///   routes elsewhere; a second `Handshake` frame specifically is
    ///   rejected here too, since the ADR fixes the handshake to "the first
    ///   application frame".
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
                    self.state = State::Closed;
                    Ok(HandshakeOutcome::Rejected {
                        error: Frame::Error {
                            id: None,
                            code: WireErrorCode::UnsupportedVersion,
                            message: format!(
                                "unsupported protocol version {version}; server supports [{}, {}]",
                                self.supported.min(),
                                self.supported.max()
                            ),
                        },
                    })
                }
            }
            (State::AwaitingHandshake, _) => {
                self.state = State::Closed;
                Err(HandshakeSequenceError {
                    error: Box::new(Frame::Error {
                        id: None,
                        code: WireErrorCode::MalformedFrame,
                        message: format!(
                            "expected \"handshake\" as the first frame, got {:?}",
                            frame.kind()
                        ),
                    }),
                })
            }
            (State::Completed(_), Frame::Handshake { .. }) => {
                self.state = State::Closed;
                Err(HandshakeSequenceError {
                    error: Box::new(Frame::Error {
                        id: None,
                        code: WireErrorCode::MalformedFrame,
                        message: "handshake already completed on this connection".to_string(),
                    }),
                })
            }
            (State::Completed(_), _) => Ok(HandshakeOutcome::Admitted),
            (State::Closed, _) => Err(HandshakeSequenceError {
                error: Box::new(Frame::Error {
                    id: None,
                    code: WireErrorCode::MalformedFrame,
                    message: "connection already closed by a prior handshake failure".to_string(),
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
}
