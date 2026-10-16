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
    let payload = br#"{"kind":"error","code":"bogus","message":"future code"}"#;
    let frame = crate::codec::decode_payload(payload).unwrap();
    match frame {
        crate::frame::Frame::Error {
            id,
            code,
            unrecognized_code,
            ..
        } => {
            assert_eq!(code, WireErrorCode::Internal);
            assert!(id.is_none());
            // The raw string survives the fallback; it is not
            // silently discarded.
            assert_eq!(unrecognized_code.as_deref(), Some("bogus"));
        }
        other => panic!("expected an error frame, got {other:?}"),
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
