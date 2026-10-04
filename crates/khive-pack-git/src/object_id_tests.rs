use super::*;

#[test]
fn sha1_object_ids_require_forty_ascii_hex_bytes() {
    assert!(!is_40_hex(b""));
    assert!(!is_40_hex("a".repeat(39).as_bytes()));
    assert!(is_40_hex(
        "0123456789abcdef0123456789abcdef01234567".as_bytes()
    ));
    assert!(!is_40_hex("a".repeat(41).as_bytes()));
    assert!(!is_40_hex(format!("{}g", "a".repeat(39)).as_bytes()));
    assert!(!is_40_hex(format!("{}é", "a".repeat(39)).as_bytes()));
    assert!(!is_40_hex(format!("{}é", "a".repeat(38)).as_bytes()));
    assert!(is_40_hex(
        "ABCDEF0123456789ABCDEF0123456789ABCDEF01".as_bytes()
    ));
    assert!(!is_40_hex("a".repeat(64).as_bytes()));
}
