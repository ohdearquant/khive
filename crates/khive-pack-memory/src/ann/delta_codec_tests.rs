fn chunk(values: Vec<f32>) -> (Vec<u8>, Uuid, Uuid, [u8; 32]) {
    let nonce = Uuid::from_u128(1);
    let id = Uuid::from_u128(2);
    let base = [3; 32];
    let batch = DeltaBatch {
        applied_seq: 9,
        raw_count: 1,
        ops: vec![(id, Some(values))],
    };
    (
        encode_chunk(&base, nonce, Uuid::nil(), &batch),
        nonce,
        id,
        base,
    )
}

fn resign(bytes: &mut [u8]) {
    let hash = checksum(&bytes[..96], &bytes[CHUNK_LEN..]);
    bytes[96..CHUNK_LEN].copy_from_slice(&hash);
}

#[test]
fn delta_vector_codec_preserves_portable_bytes_and_finite_bits() {
    let bits = [0, 0x8000_0000, 1, 0x3f80_0000, 0xc020_0000];
    let (bytes, nonce, id, base) = chunk(bits.map(f32::from_bits).to_vec());
    assert_eq!(
        &bytes[CHUNK_LEN + 25..],
        &[0, 0, 0, 0, 0, 0, 0, 0x80, 1, 0, 0, 0, 0, 0, 0x80, 0x3f, 0, 0, 0x20, 0xc0]
    );
    let (previous, decoded) = parse_chunk(&bytes, nonce, &base, bits.len()).unwrap();
    assert!(previous.is_nil());
    assert_eq!((decoded.applied_seq, decoded.raw_count), (9, 1));
    assert_eq!(decoded.ops.len(), 1);
    assert_eq!(decoded.ops[0].0, id);
    assert_eq!(
        decoded.ops[0]
            .1
            .as_ref()
            .unwrap()
            .iter()
            .map(|v| v.to_bits())
            .collect::<Vec<_>>(),
        bits
    );
}

#[test]
fn delta_vector_policy_still_rejects_nonfinite_values() {
    for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let (bytes, nonce, _, base) = chunk(vec![1.0, value, 0.0]);
        assert_eq!(
            parse_chunk(&bytes, nonce, &base, 3).err().unwrap(),
            "memory delta vector has non-finite value"
        );
    }
}

#[test]
fn delta_vector_framing_rejects_dimensions_truncation_and_length_overflow() {
    let (bytes, nonce, _, base) = chunk(vec![1.0, 0.0]);
    assert_eq!(
        parse_chunk(&bytes, nonce, &base, 3).err().unwrap(),
        "memory delta vector dimensions mismatch"
    );
    for missing in 1..=4 {
        let mut truncated = bytes[..bytes.len() - missing].to_vec();
        resign(&mut truncated);
        assert_eq!(
            parse_chunk(&truncated, nonce, &base, 2).err().unwrap(),
            "memory delta is truncated"
        );
    }
    let mut oversized = bytes;
    oversized[CHUNK_LEN + 17..CHUNK_LEN + 25].copy_from_slice(&(usize::MAX as u64).to_le_bytes());
    resign(&mut oversized);
    assert_eq!(
        parse_chunk(&oversized, nonce, &base, usize::MAX)
            .err()
            .unwrap(),
        "memory delta vector length overflow"
    );
}
