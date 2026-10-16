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
