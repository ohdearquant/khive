//! Client-side handshake admission, per ADR-137 Amendment 1, Decisions 5 and 6.
//!
//! A transport records the version it actually sent, then feeds every decoded
//! inbound frame through this gate before handling it. This module performs no
//! I/O, and it does not replace the codec's frame-field validation.

use crate::error::{TerminalScope, WireErrorCode};
use crate::frame::Frame;
use crate::handshake::HandshakeSequenceError;
use crate::version::ProtocolVersion;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CloseReason {
    UnsolicitedFrame,
    MismatchedAcknowledgment,
    UnexpectedDuringHandshake,
    WrongDirectionOrRepeatedHandshake,
    InvalidErrorEnvelope,
    ServerTerminalError,
}

impl CloseReason {
    const fn phrase(self) -> &'static str {
        match self {
            Self::UnsolicitedFrame => "an unsolicited frame before a handshake was sent",
            Self::MismatchedAcknowledgment => "an acknowledgment of a version that was not offered",
            Self::UnexpectedDuringHandshake => "an unexpected frame during the handshake",
            Self::WrongDirectionOrRepeatedHandshake => {
                "a direction or repeated-handshake violation"
            }
            Self::InvalidErrorEnvelope => "an invalid server error frame",
            Self::ServerTerminalError => "a connection-terminal server error",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Fresh,
    Outstanding(ProtocolVersion),
    Completed(ProtocolVersion),
    Closed(CloseReason),
}

/// A local misuse of [`ClientHandshakeGate::record_handshake_sent`].
///
/// These errors do not carry a wire error code or close a connection: no inbound
/// frame caused them, and the gate's state is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ClientHandshakeStartError {
    /// A conforming encoder cannot emit a handshake naming version zero.
    #[error("cannot record a sent handshake for protocol version zero")]
    VersionZero,
    /// This connection already has an outstanding or completed handshake.
    #[error("a handshake has already been sent on this connection")]
    AlreadyStarted,
    /// A terminal outcome already closed this gate.
    #[error("the client handshake gate is permanently closed")]
    Closed,
}

/// The result of admitting one decoded server frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientHandshakeOutcome {
    /// The acknowledgment matches the exact version the client offered.
    Completed { version: ProtocolVersion },
    /// A server application frame was admitted after handshake completion.
    Admitted,
    /// A connection-terminal server error was admitted and closed the gate.
    ///
    /// The caller must surface the original inbound [`Frame::Error`] and close
    /// the transport without fallback. The borrowed frame passed to `admit`
    /// remains available, including its original code and message.
    ServerTerminated,
}

/// Enforces the inbound handshake sequence and frame direction for one client
/// connection.
///
/// Record the exact offered version only after the transport successfully sends
/// its handshake. Before that, every inbound frame is unsolicited. An exact
/// acknowledgment completes the handshake; a server connection-terminal error
/// is surfaced without fallback. Every sequence or direction violation closes
/// the gate permanently with an id-less `malformed_frame`.
///
/// Deliberately not `Clone`: copying an earlier state must not allow a terminal
/// connection to resume. There is no reset or unchecked-completion operation.
#[derive(Debug)]
pub struct ClientHandshakeGate {
    state: State,
}

impl ClientHandshakeGate {
    /// A fresh connection on which no handshake has been sent.
    pub fn new() -> Self {
        Self {
            state: State::Fresh,
        }
    }

    /// Record the exact version in the handshake the transport just sent.
    ///
    /// The caller constructs, encodes and writes its existing [`Frame::Handshake`]
    /// first, then records that version before reading inbound frames. This
    /// method does no I/O. Preparation, encoding or write failure must not call
    /// it; discard the failed connection instead. A local error leaves the state
    /// unchanged. The caller selects a version whose grammar it implements,
    /// normally [`crate::CURRENT_VERSION`]; this gate adds no range negotiation.
    pub fn record_handshake_sent(
        &mut self,
        offered: ProtocolVersion,
    ) -> Result<(), ClientHandshakeStartError> {
        match self.state {
            State::Fresh => {
                if offered.get() == 0 {
                    return Err(ClientHandshakeStartError::VersionZero);
                }
                self.state = State::Outstanding(offered);
                Ok(())
            }
            State::Outstanding(_) | State::Completed(_) => {
                Err(ClientHandshakeStartError::AlreadyStarted)
            }
            State::Closed(_) => Err(ClientHandshakeStartError::Closed),
        }
    }

    /// Whether this connection currently has a completed handshake.
    pub fn is_complete(&self) -> bool {
        matches!(self.state, State::Completed(_))
    }

    /// The outstanding offer, before its matching acknowledgment arrives.
    pub fn offered_version(&self) -> Option<ProtocolVersion> {
        match self.state {
            State::Outstanding(version) => Some(version),
            _ => None,
        }
    }

    /// The accepted version while the connection remains completed and usable.
    pub fn accepted_version(&self) -> Option<ProtocolVersion> {
        match self.state {
            State::Completed(version) => Some(version),
            _ => None,
        }
    }

    /// Whether a protocol violation or admitted server error permanently closed
    /// this gate.
    pub fn is_closed(&self) -> bool {
        matches!(self.state, State::Closed(_))
    }

    /// Admit one already-decoded inbound frame, before the caller handles it.
    ///
    /// A fresh gate rejects every kind. With an offer outstanding, only an exact
    /// acknowledgment or a connection-terminal server error is admissible. After
    /// completion, server application kinds are admitted; another acknowledgment
    /// or any client-to-server kind is a violation. An admitted terminal error
    /// yields [`ClientHandshakeOutcome::ServerTerminated`], leaving the original
    /// frame available to surface before closing the transport.
    ///
    /// The codec remains role-agnostic and owns field/grammar validation. Every
    /// sequence rejection here is id-less `malformed_frame` and permanently
    /// closes the gate; even a later otherwise-valid frame cannot recover it.
    pub fn admit(
        &mut self,
        frame: &Frame,
    ) -> Result<ClientHandshakeOutcome, HandshakeSequenceError> {
        match (self.state, frame) {
            (State::Fresh, _) => self.reject(
                CloseReason::UnsolicitedFrame,
                format!("received {:?} before the client sent a handshake", frame.kind()),
            ),
            (State::Outstanding(offered), Frame::HandshakeAck { version }) => {
                if *version == offered {
                    self.state = State::Completed(offered);
                    Ok(ClientHandshakeOutcome::Completed { version: offered })
                } else {
                    self.reject(
                        CloseReason::MismatchedAcknowledgment,
                        format!("handshake acknowledgment names version {version}, but the client offered {offered}"),
                    )
                }
            }
            (State::Completed(_), Frame::Error { id: None, unrecognized_code: Some(_), .. }) => self.reject(
                CloseReason::InvalidErrorEnvelope,
                "a server error with an unrecognized code must identify an operation".to_string(),
            ),
            (State::Completed(_), Frame::Error { id: Some(_), code, .. })
                if code.terminal_scope() == TerminalScope::Connection =>
            {
                self.reject(
                    CloseReason::InvalidErrorEnvelope,
                    "a connection-terminal server error must not identify an operation".to_string(),
                )
            }
            (State::Completed(_), Frame::Error { id: None, code, .. })
                if code.terminal_scope() == TerminalScope::Request =>
            {
                self.reject(
                    CloseReason::InvalidErrorEnvelope,
                    "a request-terminal server error must identify an operation".to_string(),
                )
            }
            (State::Outstanding(_) | State::Completed(_), Frame::Error { id: None, code, .. })
                if code.terminal_scope() == TerminalScope::Connection =>
            {
                self.state = State::Closed(CloseReason::ServerTerminalError);
                Ok(ClientHandshakeOutcome::ServerTerminated)
            }
            (State::Outstanding(_), _) => self.reject(
                CloseReason::UnexpectedDuringHandshake,
                format!("expected the offered-version acknowledgment or a connection-terminal error, got {:?}", frame.kind()),
            ),
            (State::Completed(_), Frame::Response { .. }
                | Frame::Error { .. }
                | Frame::SubscribeAck { .. }
                | Frame::UnsubscribeAck { .. }
                | Frame::Event { .. }) => Ok(ClientHandshakeOutcome::Admitted),
            (State::Completed(_), _) => self.reject(
                CloseReason::WrongDirectionOrRepeatedHandshake,
                format!("frame kind {:?} is not admissible at a client after handshake completion", frame.kind()),
            ),
            (State::Closed(reason), _) => Err(sequence_error(format!(
                "connection already closed by {}",
                reason.phrase()
            ))),
        }
    }

    fn reject(
        &mut self,
        reason: CloseReason,
        message: String,
    ) -> Result<ClientHandshakeOutcome, HandshakeSequenceError> {
        self.state = State::Closed(reason);
        Err(sequence_error(message))
    }
}

fn sequence_error(message: String) -> HandshakeSequenceError {
    HandshakeSequenceError {
        error: Box::new(Frame::Error {
            id: None,
            code: WireErrorCode::MalformedFrame,
            message,
            unrecognized_code: None,
        }),
    }
}

impl Default for ClientHandshakeGate {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[path = "client_handshake_tests.rs"]
mod tests;
