use super::*;
use alloc::format;
use alloc::string::ToString;

#[test]
fn nil() {
    assert!(Id128::NIL.is_nil());
    assert!(!Id128::from_u128(1).is_nil());
}

#[test]
fn roundtrip_u128() {
    let v: u128 = 0xdeadbeef_12345678_9abcdef0_11223344;
    let id = Id128::from_u128(v);
    assert_eq!(id.to_u128(), v);
}

#[test]
fn display_is_hyphenated_uuid() {
    let id = Id128::from_u128(0xabcdef0123456789abcdef0123456789);
    let s = format!("{id}");
    assert_eq!(s.len(), 36);
    assert_eq!(s, "abcdef01-2345-6789-abcd-ef0123456789");
}

#[test]
fn parse_hyphenated() {
    let id: Id128 = "abcdef01-2345-6789-abcd-ef0123456789".parse().unwrap();
    assert_eq!(id.to_u128(), 0xabcdef0123456789abcdef0123456789);
}

#[test]
fn parse_simple() {
    let id: Id128 = "abcdef0123456789abcdef0123456789".parse().unwrap();
    assert_eq!(id.to_u128(), 0xabcdef0123456789abcdef0123456789);
}

#[test]
fn display_parse_roundtrip() {
    let id = Id128::from_u128(0xabcdef0123456789abcdef0123456789);
    let s = format!("{id}");
    let parsed: Id128 = s.parse().unwrap();
    assert_eq!(parsed, id);
}

#[test]
fn parse_errors() {
    assert_eq!("abc".parse::<Id128>(), Err(ParseIdError::InvalidLength));
    assert_eq!(
        "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz".parse::<Id128>(),
        Err(ParseIdError::InvalidHex)
    );
    // Wrong hyphen positions
    assert_eq!(
        "abcdef01-2345-6789-abcd-ef012345678".parse::<Id128>(),
        Err(ParseIdError::InvalidLength)
    );
}

/// Pin the `ParseIdError` Display wording: downstream error-shape
/// classification (propose changeset identifier failures) matches on
/// these exact strings, so a wording change must break this test at the
/// source rather than silently drift a caller's heuristic.
#[test]
fn parse_error_display_is_pinned() {
    assert_eq!(
        ParseIdError::InvalidLength.to_string(),
        ParseIdError::INVALID_LENGTH_TEXT
    );
    assert_eq!(
        ParseIdError::InvalidHex.to_string(),
        ParseIdError::INVALID_HEX_TEXT
    );
}

#[test]
fn ordering() {
    let a = Id128::from_u128(1);
    let b = Id128::from_u128(2);
    assert!(a < b);
}

#[test]
fn lowercase_hex_accepts_digits_and_a_through_f_only() {
    assert!(is_lowercase_hex("0123456789abcdef"));
    assert!(is_lowercase_hex(""));
    assert!(!is_lowercase_hex("abcdeF"));
    assert!(!is_lowercase_hex("abcdeg"));
    assert!(!is_lowercase_hex("ab cd"));
    assert!(!is_lowercase_hex("0x12"));
    assert!(!is_lowercase_hex("ab\u{e9}"));
}

/// C1 regression: Id128 must deserialize from an owned serde_json::Value string,
/// not only from a borrowed &str.  Previously used `<&str>::deserialize` which
/// fails when the deserializer holds owned data (e.g. Value-backed deserializer).
#[cfg(feature = "serde")]
#[test]
fn deserialize_from_owned_value() {
    use alloc::string::ToString;
    let uuid_str = "abcdef01-2345-6789-abcd-ef0123456789";
    // serde_json::from_value takes a Value (owned), exercising the owned-string path.
    let val = serde_json::Value::String(uuid_str.to_string());
    let id: Id128 = serde_json::from_value(val).expect("Id128 must deserialize from Value");
    assert_eq!(format!("{id}"), uuid_str);
}
