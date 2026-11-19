use super::*;

#[test]
fn parse_valid_namespace() {
    let ns = Namespace::parse("research").unwrap();
    assert_eq!(ns.as_str(), "research");
}

#[test]
fn local_is_local() {
    assert_eq!(Namespace::local().as_str(), "local");
}

#[test]
fn parse_hierarchical_namespace() {
    let ns = Namespace::parse("research:lattice").unwrap();
    assert_eq!(ns.as_str(), "research:lattice");
}

#[test]
fn parse_empty_returns_error() {
    assert_eq!(Namespace::parse(""), Err(NamespaceError::Empty));
}

#[test]
fn parse_trailing_separator_returns_error() {
    assert_eq!(
        Namespace::parse("research:"),
        Err(NamespaceError::TrailingSeparator)
    );
}

#[test]
fn parse_double_colon_returns_empty_segment() {
    assert_eq!(Namespace::parse("a::b"), Err(NamespaceError::EmptySegment));
}

#[test]
fn parse_invalid_char_returns_error() {
    assert!(matches!(
        Namespace::parse("bad namespace"),
        Err(NamespaceError::InvalidCharacter { ch: ' ' })
    ));
}

#[test]
fn try_from_string() {
    use core::convert::TryFrom;
    let ns = Namespace::try_from(String::from("my-ns")).unwrap();
    assert_eq!(ns.as_str(), "my-ns");
}

#[test]
fn has_segment_prefix_detects_child() {
    let parent = Namespace::parse("research").unwrap();
    let child = Namespace::parse("research:lattice").unwrap();
    let sibling = Namespace::parse("other").unwrap();

    assert!(has_segment_prefix(&child, &parent));
    assert!(!has_segment_prefix(&sibling, &parent));
    assert!(!has_segment_prefix(&parent, &parent));
}

#[cfg(feature = "serde")]
#[test]
fn serde_roundtrip() {
    let ns = Namespace::parse("proj-123").unwrap();
    let json = serde_json::to_string(&ns).unwrap();
    let back: Namespace = serde_json::from_str(&json).unwrap();
    assert_eq!(ns, back);
}

#[cfg(feature = "serde")]
#[test]
fn serde_deserialize_rejects_invalid() {
    let result: Result<Namespace, _> = serde_json::from_str("\"\"");
    assert!(result.is_err());
}

#[test]
fn parse_slash_is_rejected() {
    // Forward slashes are not in the allowed charset (alphanumeric, `-`, `_`, `.`).
    assert!(matches!(
        Namespace::parse("tenant/sub"),
        Err(NamespaceError::InvalidCharacter { ch: '/' })
    ));
}

#[test]
fn parse_unicode_is_rejected() {
    // Only ASCII characters are allowed; non-ASCII (e.g. accented letters) must fail.
    assert!(matches!(
        Namespace::parse("café"),
        Err(NamespaceError::InvalidCharacter { .. })
    ));
}

#[test]
fn parse_dot_is_valid() {
    // Dots are explicitly allowed to support version-style namespaces like "v1.5".
    let ns = Namespace::parse("v1.5").unwrap();
    assert_eq!(ns.as_str(), "v1.5");
}

#[test]
fn parse_too_long_is_rejected() {
    let long = "a".repeat(257);
    assert!(matches!(
        Namespace::parse(&long),
        Err(NamespaceError::TooLong { .. })
    ));
}

#[test]
fn parse_exactly_256_chars_is_valid() {
    let max = "a".repeat(256);
    assert!(Namespace::parse(&max).is_ok());
}
