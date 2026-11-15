use super::*;

#[test]
fn unrecognized_code_falls_back_to_internal() {
    // ADR-137: a code this client does not recognize must decode as
    // `internal` — request-terminal — rather than failing to decode.
    let code: WireErrorCode = serde_json::from_str(r#""bogus""#).unwrap();
    assert_eq!(code, WireErrorCode::Internal);
    assert_eq!(code.terminal_scope(), TerminalScope::Request);
}

#[test]
fn unrecognized_code_inside_an_error_frame_falls_back_to_internal() {
    // ADR-137 Amendment 1 Decision 5 retains the with-id fallback; the
    // previous id-less positive is replaced by the decode refusal below.
    let payload = br#"{"kind":"error","id":"op-7","code":"bogus","message":"future code"}"#;
    let frame = crate::codec::decode_payload(payload).unwrap();
    assert_eq!(
        frame,
        crate::frame::Frame::Error {
            id: Some(crate::frame::OperationId::from("op-7")),
            code: WireErrorCode::Internal,
            message: "future code".to_string(),
            unrecognized_code: Some("bogus".to_string()),
        }
    );
    assert_eq!(
        WireErrorCode::Internal.terminal_scope(),
        TerminalScope::Request
    );
    assert_eq!(
        crate::codec::encode_frame(&frame).unwrap_err(),
        crate::codec::CodecError::FallbackFrameNotEncodable {
            code: "bogus".to_string(),
        }
    );
}

#[test]
fn unknown_error_code_without_an_id_is_malformed_at_decode() {
    // These are role-agnostic decode APIs: neither a client nor a server
    // may receive an accepted id-less unknown-code Frame from any of them.
    for raw_code in ["bogus", "future_code_xyz", "Cancelled"] {
        let payload = format!(r#"{{"kind":"error","code":"{raw_code}","message":"future code"}}"#);
        let mut wire = u32::try_from(payload.len()).unwrap().to_be_bytes().to_vec();
        wire.extend_from_slice(payload.as_bytes());
        let results = [
            (
                "decode_payload",
                crate::codec::decode_payload(payload.as_bytes()).map(|_| ()),
            ),
            (
                "decode_frame",
                crate::codec::decode_frame(&wire, crate::codec::DEFAULT_MAX_FRAME_BYTES)
                    .map(|_| ()),
            ),
            (
                "decode_with_consumed",
                crate::codec::FrameCodec::default()
                    .decode_with_consumed(&wire)
                    .map(|_| ()),
            ),
        ];
        for (entrypoint, result) in results {
            assert!(
                result.is_err(),
                "D5 {entrypoint} accepted an id-less unknown error code {raw_code}"
            );
            let error = result.unwrap_err();
            assert!(
                matches!(
                    &error,
                    crate::codec::CodecError::InconsistentErrorScope { .. }
                ),
                "unexpected decode error for {raw_code}: {error:?}"
            );
            assert!(
                error
                    .to_string()
                    .contains("unknown error code requires an operation id"),
                "unexpected decode error for {raw_code}: {error}"
            );
            assert_eq!(error.wire_code(), WireErrorCode::MalformedFrame);
            assert_eq!(
                error.wire_code().terminal_scope(),
                TerminalScope::Connection
            );
        }
    }
}

#[test]
fn wire_error_codes_matches_the_code_enum() {
    // `WIRE_ERROR_CODES` gates the codec's decode-time error-scope
    // check: a code missing from the set silently takes the
    // unknown-code exemption and its scope pairing is never enforced.
    // The set must therefore cover every `WireErrorCode` variant
    // exactly.
    //
    // Set side, fully mechanical: every entry must round-trip through
    // serde to a variant whose `as_str` equals the entry. A typo'd or
    // stale entry deserializes to `Internal` via `#[serde(other)]` and
    // fails the `as_str` comparison.
    for entry in WIRE_ERROR_CODES {
        let json = format!("\"{entry}\"");
        let parsed: WireErrorCode = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed.as_str(),
            *entry,
            "WIRE_ERROR_CODES entry {entry:?} names no variant \
                 (deserialized to {parsed:?})"
        );
    }
    let mut deduped: Vec<&str> = WIRE_ERROR_CODES.to_vec();
    deduped.sort_unstable();
    deduped.dedup();
    assert_eq!(
        deduped.len(),
        WIRE_ERROR_CODES.len(),
        "WIRE_ERROR_CODES contains duplicates"
    );

    // Variant side: the list below mirrors the enum; the wildcard-free
    // match forces whoever adds a variant to visit this site (the
    // compiler cannot force the array itself). Each variant must be in
    // the set, and equal counts close the loop.
    use WireErrorCode::*;
    let variants = [
        UnsupportedVersion,
        IdentityRejected,
        MalformedFrame,
        FrameTooLarge,
        SubscriberOverflow,
        SubscriptionRevoked,
        ContextRejected,
        PeerClassDenied,
        SubscriptionDenied,
        AlreadySubscribed,
        CursorExpired,
        InFlightLimitExceeded,
        DeadlineExceeded,
        Cancelled,
        ShuttingDown,
        Internal,
    ];
    for code in variants {
        match code {
            UnsupportedVersion
            | IdentityRejected
            | MalformedFrame
            | FrameTooLarge
            | SubscriberOverflow
            | SubscriptionRevoked
            | ContextRejected
            | PeerClassDenied
            | SubscriptionDenied
            | AlreadySubscribed
            | CursorExpired
            | InFlightLimitExceeded
            | DeadlineExceeded
            | Cancelled
            | ShuttingDown
            | Internal => {}
        }
        assert!(
            WIRE_ERROR_CODES.contains(&code.as_str()),
            "WIRE_ERROR_CODES is missing {code:?} ({}); its scope pairing \
                 would silently go unenforced at decode",
            code.as_str()
        );
    }
    assert_eq!(
        WIRE_ERROR_CODES.len(),
        variants.len(),
        "WIRE_ERROR_CODES and the variant list disagree on count"
    );
}
