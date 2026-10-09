#![cfg(feature = "sha2")]

use khive_types::hash::framed_text_sha256;
use khive_types::Hash32;
use sha2::{Digest, Sha256};

#[test]
fn hashes_the_persisted_byte_frames() {
    // Literal frames pin presence, byte lengths, ordering and UTF-8 encoding.
    // They deliberately do not rebuild the frame with the production algorithm.
    let cases: &[(Option<&str>, &str, &[u8])] = &[
        (None, "", b"\x00\x00\x00\x00\x00\x00\x00\x00\x00"),
        (Some(""), "", b"\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00"),
        (None, "raw", b"\x00\x00\x00\x00\x00\x00\x00\x00\x03raw"),
        (Some("a"), "bc", b"\x01\x00\x00\x00\x00\x00\x00\x00\x01a\x00\x00\x00\x00\x00\x00\x00\x02bc"),
        (Some("ab"), "c", b"\x01\x00\x00\x00\x00\x00\x00\x00\x02ab\x00\x00\x00\x00\x00\x00\x00\x01c"),
        (Some("\0"), "\n", b"\x01\x00\x00\x00\x00\x00\x00\x00\x01\x00\x00\x00\x00\x00\x00\x00\x00\x01\n"),
        (Some("中"), "é", b"\x01\x00\x00\x00\x00\x00\x00\x00\x03\xe4\xb8\xad\x00\x00\x00\x00\x00\x00\x00\x02\xc3\xa9"),
    ];
    let mut observed = Vec::new();
    for &(text, raw, frame) in cases {
        let digest = Sha256::digest(frame);
        let actual = framed_text_sha256(text, raw);
        assert_eq!(actual, Hash32::from_bytes(digest.into()));
        assert_eq!(actual.to_string(), format!("{digest:x}"));
        assert!(
            !observed.contains(&actual),
            "distinct stored frames collapsed"
        );
        observed.push(actual);
    }
}
