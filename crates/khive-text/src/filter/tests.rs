use super::*;

#[test]
fn lowercase_ascii() {
    assert_eq!(LowercaseFilter.apply("HELLO".into()), Some("hello".into()));
}

#[test]
fn lowercase_unicode() {
    assert_eq!(
        LowercaseFilter.apply("Straße".into()),
        Some("straße".into())
    );
}

#[test]
fn min_length_boundary() {
    let f = MinLengthFilter(3);
    assert_eq!(f.apply("ab".into()), None);
    assert_eq!(f.apply("abc".into()), Some("abc".into()));
}

#[test]
fn max_length_boundary() {
    let f = MaxLengthFilter(5);
    assert_eq!(f.apply("hello".into()), Some("hello".into()));
    assert_eq!(f.apply("helloo".into()), None);
}

#[test]
fn max_length_default_is_40() {
    let f = MaxLengthFilter::default();
    assert_eq!(f.0, 40);
}

#[test]
fn stop_word_drops() {
    assert_eq!(StopWordFilter.apply("the".into()), None);
    assert_eq!(StopWordFilter.apply("is".into()), None);
    assert_eq!(
        StopWordFilter.apply("transformer".into()),
        Some("transformer".into())
    );
}

#[test]
fn stop_word_case_sensitive() {
    // StopWordFilter expects lowercased input
    assert_eq!(StopWordFilter.apply("The".into()), Some("The".into()));
}

#[test]
fn lowercase_ascii_reuses_owned_buffer() {
    for (input, expected) in [
        ("HELLO_123", "hello_123"),
        ("already_lower", "already_lower"),
    ] {
        let mut token = String::with_capacity(128);
        token.push_str(input);
        let pointer = token.as_ptr();
        let capacity = token.capacity();

        let output = LowercaseFilter
            .apply(token)
            .expect("lowercase retains ASCII tokens");

        assert_eq!(output, expected);
        assert_eq!(
            output.as_ptr(),
            pointer,
            "ASCII lowercase replaced the owned buffer"
        );
        assert_eq!(
            output.capacity(),
            capacity,
            "ASCII lowercase changed the owned capacity"
        );
    }
}

#[test]
fn lowercase_unicode_preserves_expansion_and_context() {
    for (input, expected) in [("İ", "i\u{307}"), ("ΟΣ", "ος"), ("ΟΣΑ", "οσα")] {
        assert_eq!(LowercaseFilter.apply(input.into()), Some(expected.into()));
    }
}

#[test]
fn lowercase_empty_token_is_retained() {
    assert_eq!(LowercaseFilter.apply(String::new()), Some(String::new()));
}
