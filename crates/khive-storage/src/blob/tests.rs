use super::*;

#[test]
fn upload_id_roundtrips_and_rejects_path_or_noncanonical_input() {
    let id = UploadId::from_bytes(&[0xab; 16]);
    assert_eq!(id.as_str(), "ab".repeat(16));
    assert_eq!(
        serde_json::from_str::<UploadId>(&serde_json::to_string(&id).unwrap()).unwrap(),
        id
    );
    for invalid in [
        String::new(),
        "a".repeat(31),
        "a".repeat(33),
        "A".repeat(32),
        "g".repeat(32),
        "../outside".into(),
        "a/b".repeat(11),
    ] {
        assert!(UploadId::from_hex(&invalid).is_err());
        assert!(serde_json::from_value::<UploadId>(serde_json::json!(invalid)).is_err());
    }
}

#[test]
fn from_hex_accepts_valid_lowercase_digest() {
    let hex = "a".repeat(64);
    let cref = ContentRef::from_hex(hex.clone()).unwrap();
    assert_eq!(cref.as_str(), hex);
    assert_eq!(cref.to_string(), hex);
}

#[test]
fn from_hex_rejects_short_string() {
    let err = ContentRef::from_hex("abc").unwrap_err();
    assert!(
        err.contains("64"),
        "error must mention expected length: {err}"
    );
}

#[test]
fn from_hex_rejects_long_string() {
    let err = ContentRef::from_hex("a".repeat(65)).unwrap_err();
    assert!(
        err.contains("64"),
        "error must mention expected length: {err}"
    );
}

#[test]
fn from_hex_rejects_uppercase() {
    let err = ContentRef::from_hex("A".repeat(64)).unwrap_err();
    assert!(
        err.contains("lowercase"),
        "error must mention lowercase requirement: {err}"
    );
}

#[test]
fn from_hex_rejects_non_hex_characters() {
    let mut hex = "a".repeat(63);
    hex.push('z');
    let err = ContentRef::from_hex(hex).unwrap_err();
    assert!(
        err.contains("lowercase hex"),
        "error must mention hex requirement: {err}"
    );
}

#[test]
fn from_digest_bytes_matches_known_blake3_hash() {
    // BLAKE3("") -> af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262
    let hash = blake3_hash_of_empty();
    let cref = ContentRef::from_digest_bytes(&hash);
    assert_eq!(
        cref.as_str(),
        "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
    );
}

// hand-rolled BLAKE3("") vector (see docs/api/blob-store.md)
fn blake3_hash_of_empty() -> [u8; 32] {
    let hex = "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";
    let mut out = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let s = std::str::from_utf8(chunk).unwrap();
        out[i] = u8::from_str_radix(s, 16).unwrap();
    }
    out
}

#[test]
fn deserialize_accepts_a_valid_lowercase_digest() {
    let hex = "d".repeat(64);
    let json = serde_json::to_string(&hex).unwrap();
    let cref: ContentRef = serde_json::from_str(&json).unwrap();
    assert_eq!(cref.as_str(), hex);
}

#[test]
fn deserialize_rejects_short_string() {
    let err = serde_json::from_str::<ContentRef>("\"x\"").unwrap_err();
    assert!(
        err.to_string().contains("64"),
        "deserialize error must mention the expected length: {err}"
    );
}

#[test]
fn deserialize_rejects_uppercase() {
    let hex = "A".repeat(64);
    let json = serde_json::to_string(&hex).unwrap();
    let err = serde_json::from_str::<ContentRef>(&json).unwrap_err();
    assert!(
        err.to_string().contains("lowercase"),
        "deserialize error must mention the lowercase requirement: {err}"
    );
}

#[test]
fn deserialize_rejects_non_hex_characters() {
    let mut hex = "a".repeat(63);
    hex.push('z');
    let json = serde_json::to_string(&hex).unwrap();
    let err = serde_json::from_str::<ContentRef>(&json).unwrap_err();
    assert!(
        err.to_string().contains("lowercase hex"),
        "deserialize error must mention the hex requirement: {err}"
    );
}

#[test]
fn content_ref_equality_and_hash_are_string_based() {
    let a = ContentRef::from_hex("b".repeat(64)).unwrap();
    let b = ContentRef::from_hex("b".repeat(64)).unwrap();
    let c = ContentRef::from_hex("c".repeat(64)).unwrap();
    assert_eq!(a, b);
    assert_ne!(a, c);

    use std::collections::HashSet;
    let mut set = HashSet::new();
    set.insert(a.clone());
    assert!(set.contains(&b));
    assert!(!set.contains(&c));
}
