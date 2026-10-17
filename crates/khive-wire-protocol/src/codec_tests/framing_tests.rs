#[test]
fn round_trips_a_cancel_frame() {
    let frame = Frame::Cancel {
        id: OperationId::from("op-1"),
    };
    let codec = FrameCodec::default();
    let wire = codec.encode(&frame).unwrap();
    assert_eq!(codec.decode(&wire).unwrap(), frame);
}

#[test]
fn rejects_truncated_length_prefix() {
    let codec = FrameCodec::default();
    assert_eq!(
        codec.decode(&[0u8, 1]).unwrap_err(),
        CodecError::TruncatedLengthPrefix { available: 2 }
    );
}

#[test]
fn rejects_truncated_payload_with_declared_and_available() {
    // A valid 4-byte prefix declaring 64 payload bytes, followed by
    // only 5 actual payload bytes: `available < declared` must surface
    // as `TruncatedPayload` with both values correct.
    let declared: u32 = 64;
    let mut wire = declared.to_be_bytes().to_vec();
    wire.extend_from_slice(b"part!"); // 5 bytes
    assert_eq!(
        decode_frame(&wire, DEFAULT_MAX_FRAME_BYTES).unwrap_err(),
        CodecError::TruncatedPayload {
            declared: 64,
            available: 5
        }
    );
}

#[test]
fn rejects_truncated_payload_when_prefix_declares_exactly_one_byte_more() {
    // Boundary: one byte short is still truncated.
    let frame = Frame::Cancel {
        id: OperationId::from("op-1"),
    };
    let wire = encode_frame(&frame).unwrap();
    assert_eq!(
        decode_frame(&wire[..wire.len() - 1], DEFAULT_MAX_FRAME_BYTES).unwrap_err(),
        CodecError::TruncatedPayload {
            declared: wire.len() - LENGTH_PREFIX_BYTES,
            available: wire.len() - LENGTH_PREFIX_BYTES - 1
        }
    );
}

#[test]
fn rejects_oversized_frame() {
    let codec = FrameCodec::new(4);
    let frame = Frame::Cancel {
        id: OperationId::from("op-1"),
    };
    let wire = encode_frame(&frame).unwrap();
    assert_eq!(
        codec.decode(&wire).unwrap_err(),
        CodecError::FrameTooLarge {
            declared: wire.len() - LENGTH_PREFIX_BYTES,
            max: 4
        }
    );
}
