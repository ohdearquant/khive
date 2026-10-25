use super::*;
use crate::{
    decode_frame, decode_frame_with_consumed, encode_frame, Frame, OperationId,
    DEFAULT_MAX_FRAME_BYTES,
};

fn wire(raw: &str) -> Vec<u8> {
    let mut bytes = u32::try_from(raw.len()).unwrap().to_be_bytes().to_vec();
    bytes.extend_from_slice(raw.as_bytes());
    bytes
}

fn opaque(frame: &Frame) -> &OpaqueValue {
    match frame {
        Frame::Response { result, .. } => result,
        Frame::Event { payload, .. } => payload,
        other => panic!("expected an opaque-bearing frame, got {other:?}"),
    }
}

fn assert_token_round_trip(frame: &Frame, token: &str) {
    assert_eq!(opaque(frame).received_json(), Some(token));
    let direct = serde_json::to_vec(frame).unwrap();
    let encoded = encode_frame(frame).unwrap();
    assert_eq!(&encoded[4..], direct.as_slice());
    let (decoded, consumed) =
        decode_frame_with_consumed(&encoded, DEFAULT_MAX_FRAME_BYTES).unwrap();
    assert_eq!(consumed, encoded.len());
    assert_eq!(
        opaque(&decoded).received_json(),
        Some(token),
        "received opaque token changed on encode"
    );
}

#[test]
fn received_response_tokens_survive_raw_serde_and_codec_round_trips() {
    // Decision 9 applies to the token, not to envelope member order or the
    // whitespace outside the token. Nested duplicates are opaque data.
    for token in [
        r#"{"z":1e+02, "a":"\u0061", "same":1,"same":2,"huge":1e10000}"#,
        r#"[ null, -0.00, 1E-02, "slash\/and\u0061" ]"#,
        r#""slash\/and\u0061""#,
        "1e10000",
        "null",
        "true",
    ] {
        let raw = format!("{{\"result\": \n {token} \n ,\"id\":\"op-1\",\"kind\":\"response\"}}");
        let direct: Frame = serde_json::from_str(&raw).unwrap();
        let sliced: Frame = serde_json::from_slice(raw.as_bytes()).unwrap();
        let decoded = decode_frame(&wire(&raw), DEFAULT_MAX_FRAME_BYTES).unwrap();
        for frame in [&direct, &sliced, &decoded] {
            assert_token_round_trip(frame, token);
        }
    }
}

#[test]
fn received_event_objects_keep_members_and_unprojectable_numbers() {
    let token = r#"{ "z":1e10000, "a": [null,1e+02], "dup":0, "dup":1 }"#;
    let raw = format!(
        "{{\"payload\": \n {token} ,\"occurred_at\":\"2026-08-04T11:00:00Z\",\"cursor\":7,\"topic\":\"a.b\",\"kind\":\"event\"}}"
    );
    let direct: Frame = serde_json::from_str(&raw).unwrap();
    let decoded = decode_frame(&wire(&raw), DEFAULT_MAX_FRAME_BYTES).unwrap();
    for frame in [&direct, &decoded] {
        assert!(opaque(frame).is_object());
        assert!(
            opaque(frame).to_value().is_err(),
            "fixture must exceed the parsed Value number range"
        );
        assert_token_round_trip(frame, token);
    }
}

#[test]
fn received_values_own_bytes_after_input_drop_and_clone() {
    let token = r#"{"z":"\u0061", "a":1e+02}"#;
    let mut input = wire(&format!(
        r#"{{"kind":"response","id":"op-1","result":{token}}}"#
    ));
    let frame = decode_frame(&input, DEFAULT_MAX_FRAME_BYTES).unwrap();
    let cloned = frame.clone();
    input.fill(b'?');
    drop(input);
    assert_token_round_trip(&frame, token);
    assert_token_round_trip(&cloned, token);
    assert_eq!(
        encode_frame(&frame).unwrap(),
        encode_frame(&cloned).unwrap()
    );

    let event = {
        let input = format!(
            r#"{{"kind":"event","topic":"a.b","cursor":1,"occurred_at":"t","payload":{token}}}"#
        );
        serde_json::from_str::<Frame>(&input).unwrap()
    };
    assert_token_round_trip(&event.clone(), token);
}

#[test]
fn parsed_projection_is_independent_and_local_serialization_stays_normal() {
    let token = r#"{"z":1e+02, "a":"\u0061"}"#;
    let raw = format!(r#"{{"kind":"response","id":"op-1","result":{token}}}"#);
    let frame: Frame = serde_json::from_str(&raw).unwrap();
    let mut projection = opaque(&frame).to_value().unwrap();
    projection["a"] = Value::String("changed".to_string());
    assert_token_round_trip(&frame, token);
    assert_eq!(
        opaque(&frame).to_value().unwrap()["a"],
        Value::String("a".to_string())
    );

    let local = Frame::Response {
        id: OperationId::from("op-1"),
        result: projection.clone().into(),
    };
    assert_eq!(opaque(&local).received_json(), None);
    assert_eq!(opaque(&local).to_value().unwrap(), projection);
    let direct = serde_json::to_string(&local).unwrap();
    assert_eq!(
        direct,
        format!(
            r#"{{"kind":"response","id":"op-1","result":{}}}"#,
            serde_json::to_string(&projection).unwrap()
        )
    );
    let wire = encode_frame(&local).unwrap();
    let decoded = decode_frame(&wire, DEFAULT_MAX_FRAME_BYTES).unwrap();
    assert_eq!(opaque(&decoded).to_value().unwrap(), projection);
}

#[test]
fn opaque_equality_uses_value_semantics_and_exact_unprojectable_fallback() {
    let received = |raw: &str| serde_json::from_str::<OpaqueValue>(raw).unwrap();
    let local = OpaqueValue::from(serde_json::json!({"a": [true, null], "b": 2}));
    let reordered = received(r#"{ "b":2, "a":[true,null] }"#);
    assert_eq!(local, reordered);
    assert_eq!(reordered, local);
    assert_eq!(received("1.0"), received("1e0"));
    assert_ne!(received("1"), received("1.0"));
    assert_ne!(received("true"), received("1"));
    assert_eq!(received("-0.0"), received("0.0"));

    let huge = received("1e10000");
    let same = received("1e10000");
    let different = received("2e10000");
    assert!(huge.to_value().is_err());
    assert!(same.to_value().is_err());
    assert!(different.to_value().is_err());
    assert_eq!(huge, huge.clone());
    assert_eq!(huge, same);
    assert_ne!(huge, different);
    assert_ne!(huge, OpaqueValue::from(Value::Null));
    assert_eq!(serde_json::to_string(&huge).unwrap(), "1e10000");
}

#[test]
fn from_value_frames_keep_normalized_json_compatibility() {
    for input in [
        serde_json::json!({"kind":"response","id":"op-1","result":{"z":1,"a":null}}),
        serde_json::json!({"kind":"event","topic":"a.b","cursor":1,"occurred_at":"t","payload":{"nested":[true,null]}}),
    ] {
        let expected = input
            .get(if input["kind"] == "response" {
                "result"
            } else {
                "payload"
            })
            .unwrap()
            .clone();
        // A caller-provided Value already normalized its lexemes. Preserve
        // compatibility without claiming recovery of the original text.
        let frame: Frame = serde_json::from_value(input).unwrap();
        assert_eq!(opaque(&frame).to_value().unwrap(), expected);
        let encoded = encode_frame(&frame).unwrap();
        let decoded = decode_frame(&encoded, DEFAULT_MAX_FRAME_BYTES).unwrap();
        assert_eq!(opaque(&decoded).to_value().unwrap(), expected);
    }
}

#[test]
fn raw_capture_keeps_envelope_guards_and_opaque_duplicates_separate() {
    let token = r#"{"same":1,"same":2,"number":1e10000}"#;
    let healthy = format!(r#"{{"kind":"response","id":"op-1","result":{token}}}"#);
    let frame: Frame = serde_json::from_str(&healthy).unwrap();
    assert_token_round_trip(&frame, token);

    let repeated = format!(r#"{{"kind":"unknown","id":"op-1","result":{token},"id":"op-2"}}"#);
    let direct = serde_json::from_str::<Frame>(&repeated).unwrap_err();
    assert!(
        direct.to_string().contains("duplicate envelope field `id`"),
        "D4 must precede kind validation: {direct}"
    );
    // The codec's diagnostic Value fallback has a representable, known-kind
    // fixture here; do not confuse its number/kind diagnostic with admission.
    let codec_repeated =
        r#"{"kind":"response","id":"op-1","result":{"same":1,"same":2},"id":"op-2"}"#;
    let codec = decode_frame(&wire(codec_repeated), DEFAULT_MAX_FRAME_BYTES).unwrap_err();
    assert!(
        codec.to_string().contains("duplicate envelope field `id`"),
        "D4 codec must retain raw-root validation: {codec}"
    );

    let missing = r#"{"kind":"response","id":"op-1"}"#;
    let missing_error = serde_json::from_str::<Frame>(missing).unwrap_err();
    assert!(missing_error.to_string().contains("result"));
    let present: Frame =
        serde_json::from_str(r#"{"kind":"response","id":"op-1","result":null}"#).unwrap();
    assert_token_round_trip(&present, "null");
    assert_eq!(opaque(&present).to_value().unwrap(), Value::Null);
}
