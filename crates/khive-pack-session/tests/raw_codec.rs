use std::borrow::Cow;
use std::error::Error;

use khive_pack_session::mirror::raw_codec::{decode_raw, encode_raw, RawCodecError};
use khive_storage::SqlValue;

fn blob(value: SqlValue) -> Vec<u8> {
    match value {
        SqlValue::Blob(bytes) => bytes,
        other => panic!("encoder must return a BLOB: {other:?}"),
    }
}

#[test]
fn legacy_text_is_borrowed_unchanged_independent_of_blob_budget() {
    for raw in [
        "{\"masked\":true}",
        "",
        "海洋\n🌊\0",
        "\u{1}{\"legacy\":true}",
    ] {
        let value = SqlValue::Text(raw.to_owned());
        let decoded = decode_raw(&value, 0).unwrap();
        assert_eq!(decoded.as_bytes(), raw.as_bytes());
        assert!(matches!(decoded, Cow::Borrowed(_)));
        let SqlValue::Text(original) = &value else {
            unreachable!()
        };
        assert_eq!(decoded.as_ptr(), original.as_ptr());
    }
}

#[test]
fn encoder_uses_a_real_version_one_frame_for_all_plaintexts() {
    for raw in ["", "{\"masked\":\"[REDACTED]\"}\n", "Unicode 海洋🌊\0"] {
        let encoded = encode_raw(raw).unwrap();
        let SqlValue::Blob(bytes) = &encoded else {
            panic!("encoder must return a BLOB")
        };
        assert_eq!(bytes[0], 0x01);
        assert!(bytes.len() > 1, "empty input still needs a zstd frame");
        assert_eq!(
            zstd::bulk::decompress(&bytes[1..], raw.len()).unwrap(),
            raw.as_bytes()
        );
        assert_eq!(decode_raw(&encoded, raw.len()).unwrap(), raw);
        assert!(matches!(
            decode_raw(&encoded, raw.len()).unwrap(),
            Cow::Owned(_)
        ));
    }
}

#[test]
fn decoder_reads_a_frame_created_independently_of_the_encoder() {
    let raw = "{\"source\":\"external frame\",\"text\":\"海洋\"}";
    let frame = zstd::bulk::compress(raw.as_bytes(), 1).unwrap();
    let mut bytes = vec![0x01];
    bytes.extend_from_slice(&frame);
    assert_eq!(decode_raw(&SqlValue::Blob(bytes), raw.len()).unwrap(), raw);
}

#[test]
fn empty_and_unsupported_prefixes_refuse_without_text_fallback() {
    assert!(matches!(
        decode_raw(&SqlValue::Blob(vec![]), 100),
        Err(RawCodecError::EmptyBlob)
    ));
    for prefix in [0x00, 0x02, 0xff, b'{'] {
        let value = SqlValue::Blob(vec![prefix, b'{', b'}']);
        let error = decode_raw(&value, 100).unwrap_err();
        assert!(matches!(error, RawCodecError::UnsupportedPrefix(got) if got == prefix));
        assert!(error.source().is_none());
    }
}

#[test]
fn every_non_text_blob_sql_variant_is_refused_without_value_diagnostics() {
    let secret = "raw-content-must-not-appear";
    for value in [
        SqlValue::Null,
        SqlValue::Bool(true),
        SqlValue::Integer(123),
        SqlValue::Float(f64::NAN),
        SqlValue::Json(serde_json::json!({"secret": secret})),
        SqlValue::Uuid(uuid::Uuid::nil()),
        SqlValue::Timestamp(chrono::DateTime::<chrono::Utc>::UNIX_EPOCH),
    ] {
        let error = decode_raw(&value, 100).unwrap_err();
        assert!(matches!(error, RawCodecError::UnsupportedValue(_)));
        assert!(!error.to_string().contains(secret));
        assert!(!format!("{error:?}").contains(secret));
    }
}

#[test]
fn malformed_and_every_truncated_frame_refuse_with_a_source_error() {
    let encoded = blob(encode_raw("{\"message\":\"masked content\"}").unwrap());
    for end in 1..encoded.len() {
        let value = SqlValue::Blob(encoded[..end].to_vec());
        let error = decode_raw(&value, 100).unwrap_err();
        assert!(
            matches!(error, RawCodecError::Decompression(_)),
            "cut {end}"
        );
        assert!(error.source().is_some());
    }
    let value = SqlValue::Blob(vec![0x01, 0xff, 0xfe, 0x00]);
    let error = decode_raw(&value, 100).unwrap_err();
    assert!(matches!(error, RawCodecError::Decompression(_)));
    assert!(error.source().is_some());
}

#[test]
fn valid_frame_with_invalid_utf8_refuses_without_retaining_raw_bytes() {
    let raw = b"raw-content-must-not-appear\xff";
    let mut bytes = vec![0x01];
    bytes.extend_from_slice(&zstd::bulk::compress(raw, 3).unwrap());
    let error = decode_raw(&SqlValue::Blob(bytes), raw.len()).unwrap_err();
    assert!(matches!(error, RawCodecError::InvalidUtf8(_)));
    assert!(error.source().is_some());
    assert!(!error.to_string().contains("raw-content-must-not-appear"));
    assert!(!format!("{error:?}").contains("raw-content-must-not-appear"));
}

#[test]
fn blob_budget_measures_decoded_bytes_and_has_an_exact_boundary() {
    let raw = "海".repeat(4096);
    let encoded = encode_raw(&raw).unwrap();
    let SqlValue::Blob(bytes) = &encoded else {
        panic!("encoder must return a BLOB")
    };
    assert!(bytes.len() < raw.len() / 10, "amplification fixture");
    assert_eq!(decode_raw(&encoded, raw.len()).unwrap(), raw);
    for budget in [0, bytes.len(), raw.len() - 1] {
        assert!(matches!(
            decode_raw(&encoded, budget),
            Err(RawCodecError::Decompression(_))
        ));
    }
    let legacy = SqlValue::Text(raw.clone());
    assert_eq!(decode_raw(&legacy, 0).unwrap(), raw);
}
