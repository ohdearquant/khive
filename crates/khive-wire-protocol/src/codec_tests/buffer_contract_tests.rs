// ── decode_with_consumed ──

#[test]
fn decode_with_consumed_reports_consumed_length_on_a_two_frame_buffer() {
    // Two concatenated frames: the consumed count must end exactly at
    // the first frame's last byte, and the remainder must be the
    // second frame, decodable without re-slicing by hand.
    let first = Frame::Cancel {
        id: OperationId::from("op-1"),
    };
    let second = Frame::Handshake {
        version: crate::version::CURRENT_VERSION,
    };
    let codec = FrameCodec::default();
    let mut buf = codec.encode(&first).unwrap();
    let first_len = buf.len();
    buf.extend_from_slice(&codec.encode(&second).unwrap());

    let (decoded, consumed) = codec.decode_with_consumed(&buf).unwrap();
    assert_eq!(decoded, first);
    assert_eq!(consumed, first_len);

    let (decoded2, consumed2) = codec.decode_with_consumed(&buf[consumed..]).unwrap();
    assert_eq!(decoded2, second);
    assert_eq!(consumed + consumed2, buf.len());

    // `decode` keeps its documented contract on the same buffer.
    assert_eq!(codec.decode(&buf).unwrap(), first);
}

#[test]
fn decode_with_consumed_errors_without_reporting_length_on_truncation() {
    let codec = FrameCodec::default();
    let wire = codec
        .encode(&Frame::Cancel {
            id: OperationId::from("op-1"),
        })
        .unwrap();
    let err = codec
        .decode_with_consumed(&wire[..wire.len() - 1])
        .unwrap_err();
    assert!(matches!(err, CodecError::TruncatedPayload { .. }));
}

// ── serde_json feature posture probe ──

#[test]
fn serde_json_feature_posture_is_default_keys_serialize_sorted() {
    // Golden byte-exactness (tests/golden_frames.rs) and the semantic
    // opaque-payload guarantee above both assume serde_json's DEFAULT
    // feature posture: no `preserve_order` (the Map is BTreeMap-backed,
    // so keys serialize in sorted order regardless of insertion) and no
    // `arbitrary_precision`. Cargo features are ADDITIVE across the
    // workspace: if any crate in the dependency graph enables
    // `preserve_order` or `arbitrary_precision`, serde_json unifies on
    // it for this crate too, silently changing key order (or number
    // handling) and invalidating the golden fixtures. This probe pins
    // the assumed posture at test time: if it fails, the workspace
    // feature set drifted — reconcile the workspace or the fixtures,
    // do not weaken this assertion.
    let mut map = serde_json::Map::new();
    map.insert("zeta".to_string(), serde_json::json!(1));
    map.insert("alpha".to_string(), serde_json::json!(2));
    map.insert("mid".to_string(), serde_json::json!(3));
    let wire = serde_json::to_string(&serde_json::Value::Object(map)).unwrap();
    assert_eq!(
        wire, r#"{"alpha":2,"mid":3,"zeta":1}"#,
        "serde_json keys did not serialize in sorted order — \
             `preserve_order` may have been enabled workspace-wide"
    );
}

// ── CodecError → WireErrorCode canonical mapping ──

#[test]
fn codec_errors_map_to_their_wire_error_codes() {
    use crate::error::WireErrorCode;

    let cases: &[(CodecError, WireErrorCode)] = &[
        (
            CodecError::TruncatedLengthPrefix { available: 2 },
            WireErrorCode::MalformedFrame,
        ),
        (
            CodecError::TruncatedPayload {
                declared: 8,
                available: 3,
            },
            WireErrorCode::MalformedFrame,
        ),
        (
            CodecError::FrameTooLarge {
                declared: 10,
                max: 4,
            },
            WireErrorCode::FrameTooLarge,
        ),
        (
            CodecError::U32PrefixLimitExceeded {
                declared: 10,
                max: 4,
            },
            WireErrorCode::FrameTooLarge,
        ),
        (
            CodecError::InvalidJson("x".to_string()),
            WireErrorCode::MalformedFrame,
        ),
        (CodecError::MissingKind, WireErrorCode::MalformedFrame),
        (
            CodecError::UnknownFrameKind("ping".to_string()),
            WireErrorCode::MalformedFrame,
        ),
        (
            CodecError::InvalidFields {
                kind: "cancel".to_string(),
                detail: "x".to_string(),
            },
            WireErrorCode::MalformedFrame,
        ),
        (
            CodecError::InconsistentErrorScope {
                detail: "x".to_string(),
            },
            WireErrorCode::MalformedFrame,
        ),
        (
            CodecError::FallbackFrameNotEncodable {
                code: "future_code_xyz".to_string(),
            },
            WireErrorCode::Internal,
        ),
    ];
    for (err, expected) in cases {
        assert_eq!(
            err.wire_code(),
            *expected,
            "{err:?} mapped to {:?}, expected {expected:?}",
            err.wire_code()
        );
        assert_eq!(
            WireErrorCode::from(err),
            *expected,
            "From<&CodecError> disagrees"
        );
    }
}
