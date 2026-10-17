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

impl HandshakeSequenceError {
    fn malformed(message: String) -> Self {
        Self {
            error: Box::new(Frame::Error {
                id: None,
                code: WireErrorCode::MalformedFrame,
                message,
                unrecognized_code: None,
            }),
        }
    }
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
                Err(HandshakeSequenceError::malformed(format!(
                    "expected \"handshake\" as the first frame, got {:?}",
                    frame.kind()
                )))
            }
            (State::Completed(_), Frame::Handshake { .. }) => {
                self.state = State::Closed(CloseReason::DuplicateHandshake);
                Err(HandshakeSequenceError::malformed(
                    "handshake already completed on this connection".to_string(),
                ))
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
                    Err(HandshakeSequenceError::malformed(format!(
                                "frame kind {:?} is server-to-client only; a server never accepts it as an inbound frame",
                                frame.kind()
                            )))
                }
            }
            (State::Closed(reason), _) => Err(HandshakeSequenceError::malformed(format!(
                "connection already closed by {}",
                reason.phrase()
            ))),
        }
    }
}

impl Default for HandshakeGate {
    fn default() -> Self {
        Self::new(SupportedVersions::current())
    }
}

#[cfg(test)]
#[path = "handshake_tests.rs"]
mod tests;
