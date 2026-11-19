#[test]
fn fnv1a_64_matches_published_vectors() {
    // draft-eastlake-fnv-19, Appendix C: strings with and without NUL.
    let cases: &[(&[u8], u64)] = &[
        (b"", 0xcbf2_9ce4_8422_2325),
        (b"a", 0xaf63_dc4c_8601_ec8c),
        (b"foobar", 0x8594_4171_f739_67e8),
        (b"\0", 0xaf63_bd4c_8601_b7df),
        (b"a\0", 0x089b_e207_b544_f1e4),
        (b"foobar\0", 0x3453_1ca7_168b_8f38),
    ];
    for &(bytes, expected) in cases {
        assert_eq!(crate::fnv1a_64(bytes), expected);
    }
}
