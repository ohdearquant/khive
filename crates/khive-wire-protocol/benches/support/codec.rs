use khive_wire_protocol::{decode_frame, encode_frame, Frame, FrameCodec};
use serde_json::Value;
use std::hint::black_box;

pub const SIZES: [usize; 3] = [64, 4_096, 1_048_576];

#[derive(Clone, Copy)]
pub enum Operation {
    Encode,
    Decode,
    DecodeConsumed,
    FreeEncode,
    FreeDecode,
    RoundTrip,
}

impl Operation {
    pub const ALL: [Self; 6] = [
        Self::Encode,
        Self::Decode,
        Self::DecodeConsumed,
        Self::FreeEncode,
        Self::FreeDecode,
        Self::RoundTrip,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Encode => "encode",
            Self::Decode => "decode",
            Self::DecodeConsumed => "decode_with_consumed",
            Self::FreeEncode => "encode_frame",
            Self::FreeDecode => "decode_frame",
            Self::RoundTrip => "round_trip",
        }
    }

    pub fn run(self, fixture: &Fixture) -> Output {
        match self {
            Self::Encode => Output::Encoded(
                fixture
                    .codec
                    .encode(black_box(&fixture.frame))
                    .expect("encode valid fixture"),
            ),
            Self::Decode => Output::Decoded(
                fixture
                    .codec
                    .decode(black_box(&fixture.wire))
                    .expect("decode valid fixture"),
            ),
            Self::DecodeConsumed => {
                let (frame, consumed) = fixture
                    .codec
                    .decode_with_consumed(black_box(&fixture.concatenated))
                    .expect("decode exactly first frame");
                Output::Consumed(frame, consumed)
            }
            Self::FreeEncode => Output::Encoded(
                encode_frame(black_box(&fixture.frame)).expect("free encode valid fixture"),
            ),
            Self::FreeDecode => Output::Decoded(
                decode_frame(black_box(&fixture.wire), fixture.codec.max_frame_bytes())
                    .expect("free decode valid fixture"),
            ),
            Self::RoundTrip => {
                let bytes = fixture
                    .codec
                    .encode(black_box(&fixture.frame))
                    .expect("round-trip encode");
                let frame = fixture
                    .codec
                    .decode(black_box(&bytes))
                    .expect("round-trip decode");
                Output::RoundTrip(bytes, frame)
            }
        }
    }
}

pub enum Output {
    Encoded(Vec<u8>),
    Decoded(Frame),
    Consumed(Frame, usize),
    RoundTrip(Vec<u8>, Frame),
}

pub struct Fixture {
    codec: FrameCodec,
    frame: Frame,
    pub wire: Vec<u8>,
    concatenated: Vec<u8>,
    tail_wire: Vec<u8>,
    tail_frame: Frame,
    pub payload_bytes: usize,
}

impl Fixture {
    pub fn new(payload_bytes: usize) -> Self {
        assert!(SIZES.contains(&payload_bytes));
        let codec = FrameCodec::default();
        let mut frame = Frame::Response {
            id: "bench".into(),
            result: Value::String(String::new()),
        };
        let overhead = codec.encode(&frame).expect("empty result frame").len() - 4;
        let padding_len = payload_bytes
            .checked_sub(overhead)
            .expect("requested size fits frame grammar");
        let alphabet = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        let mut state = 0x5672_c0de_u64;
        let padding: String = (0..padding_len)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                alphabet[((state >> 32) as usize) % alphabet.len()] as char
            })
            .collect();
        if let Frame::Response { result, .. } = &mut frame {
            *result = Value::String(padding);
        } else {
            unreachable!("constructed response fixture");
        }
        let wire = codec.encode(&frame).expect("sized response frame");
        assert_eq!(wire.len(), payload_bytes + 4);
        assert_eq!(
            u32::from_be_bytes(wire[..4].try_into().expect("four-byte prefix")) as usize,
            payload_bytes
        );
        assert!(payload_bytes <= codec.max_frame_bytes());
        assert_eq!(codec.decode(&wire).expect("fixture round trip"), frame);
        assert_eq!(encode_frame(&frame).expect("equivalent free encoder"), wire);
        let tail_frame = Frame::Cancel {
            id: "next-frame".into(),
        };
        let tail_wire = codec.encode(&tail_frame).expect("trailing frame");
        let mut concatenated = Vec::with_capacity(wire.len() + tail_wire.len());
        concatenated.extend_from_slice(&wire);
        concatenated.extend_from_slice(&tail_wire);
        Self {
            codec,
            frame,
            wire,
            concatenated,
            tail_wire,
            tail_frame,
            payload_bytes,
        }
    }

    pub fn validate(&self, output: Output) {
        match output {
            Output::Encoded(bytes) => assert_eq!(bytes, self.wire),
            Output::Decoded(frame) => assert_eq!(frame, self.frame),
            Output::Consumed(frame, consumed) => {
                assert_eq!(frame, self.frame);
                assert_eq!(consumed, self.wire.len());
                assert_eq!(&self.concatenated[consumed..], self.tail_wire);
                assert_eq!(
                    self.codec
                        .decode(&self.concatenated[consumed..])
                        .expect("untouched next frame"),
                    self.tail_frame
                );
            }
            Output::RoundTrip(bytes, frame) => {
                assert_eq!(bytes, self.wire);
                assert_eq!(frame, self.frame);
            }
        }
    }
}
