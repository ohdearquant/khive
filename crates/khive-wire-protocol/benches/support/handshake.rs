use khive_wire_protocol::{Frame, FrameCodec, HandshakeGate, HandshakeOutcome, CURRENT_VERSION};
use std::hint::black_box;

#[derive(Clone, Copy)]
pub enum Operation {
    Gate,
    EncodedExchange,
}
impl Operation {
    pub const ALL: [Self; 2] = [Self::Gate, Self::EncodedExchange];
    pub fn name(self) -> &'static str {
        match self {
            Self::Gate => "gate_exchange",
            Self::EncodedExchange => "encoded_exchange",
        }
    }
    pub fn run(self, fixture: &Fixture, mut gate: HandshakeGate) -> Output {
        let (handshake, exchange) = match self {
            Self::Gate => (
                gate.admit(black_box(&fixture.handshake))
                    .expect("accept current version"),
                None,
            ),
            Self::EncodedExchange => {
                let handshake_wire = fixture
                    .codec
                    .encode(black_box(&fixture.handshake))
                    .expect("encode handshake");
                let received = fixture
                    .codec
                    .decode(black_box(&handshake_wire))
                    .expect("server decodes handshake");
                let accepted = gate
                    .admit(black_box(&received))
                    .expect("accept decoded handshake");
                let ack = match &accepted {
                    HandshakeOutcome::Accepted { ack, .. } => ack,
                    other => panic!("expected accepted handshake, got {other:?}"),
                };
                let ack_wire = fixture
                    .codec
                    .encode(black_box(ack))
                    .expect("encode server ACK");
                // The ACK goes to the client decoder, never back into the inbound gate.
                let client_ack = fixture
                    .codec
                    .decode(black_box(&ack_wire))
                    .expect("client decodes ACK");
                (
                    accepted,
                    Some((handshake_wire, received, ack_wire, client_ack)),
                )
            }
        };
        let admission = gate
            .admit(black_box(&fixture.request))
            .expect("first request follows handshake");
        Output {
            gate,
            handshake,
            admission,
            exchange,
        }
    }
}

pub struct Fixture {
    codec: FrameCodec,
    handshake: Frame,
    request: Frame,
    ack: Frame,
    handshake_wire: Vec<u8>,
    ack_wire: Vec<u8>,
}
impl Fixture {
    pub fn new() -> Self {
        let codec = FrameCodec::default();
        let handshake = Frame::Handshake {
            version: CURRENT_VERSION,
        };
        let ack = Frame::HandshakeAck {
            version: CURRENT_VERSION,
        };
        let handshake_wire = codec.encode(&handshake).expect("handshake fixture");
        let ack_wire = codec.encode(&ack).expect("ACK fixture");
        Self {
            codec,
            handshake,
            ack,
            handshake_wire,
            ack_wire,
            request: Frame::Request {
                id: "first-request".into(),
                ops: "stats()".into(),
                deadline_ms: None,
                namespace: None,
                actor_id: None,
                visible_namespaces: None,
            },
        }
    }
    pub fn fresh_gate(&self) -> HandshakeGate {
        let gate = HandshakeGate::default();
        assert!(!gate.is_complete());
        assert_eq!(gate.accepted_version(), None);
        gate
    }
    pub fn validate(&self, output: Output) {
        assert_eq!(
            output.handshake,
            HandshakeOutcome::Accepted {
                ack: self.ack.clone(),
                version: CURRENT_VERSION
            }
        );
        assert_eq!(output.admission, HandshakeOutcome::Admitted);
        assert!(output.gate.is_complete());
        assert_eq!(output.gate.accepted_version(), Some(CURRENT_VERSION));
        if let Some((handshake_wire, received, ack_wire, client_ack)) = output.exchange {
            assert_eq!(handshake_wire, self.handshake_wire);
            assert_eq!(received, self.handshake);
            assert_eq!(ack_wire, self.ack_wire);
            assert_eq!(client_ack, self.ack);
        }
    }
}
pub struct Output {
    gate: HandshakeGate,
    handshake: HandshakeOutcome,
    admission: HandshakeOutcome,
    exchange: Option<(Vec<u8>, Frame, Vec<u8>, Frame)>,
}
