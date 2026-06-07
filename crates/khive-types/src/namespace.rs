//! Namespace — validated string token for scoping substrate records.

extern crate alloc;
use alloc::string::String;
use core::fmt;

/// Validation error returned when a namespace string is rejected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NamespaceError {
    /// The input was empty.
    Empty,
    /// The input exceeded the maximum allowed length.
    TooLong {
        /// The maximum allowed number of bytes.
        max: usize,
    },
    /// The input contained a character outside `[a-zA-Z0-9\-_.]`.
    InvalidCharacter {
        /// The offending character.
        ch: char,
    },
    /// A `::`-separated segment was empty.
    EmptySegment,
    /// The input ended with a `:` separator.
    TrailingSeparator,
}

impl fmt::Display for NamespaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("namespace must not be empty"),
            Self::TooLong { max } => write!(f, "namespace exceeds {max} characters"),
            Self::InvalidCharacter { ch } => {
                write!(f, "namespace contains invalid character {ch:?}")
            }
            Self::EmptySegment => f.write_str("namespace must not contain empty path segments"),
            Self::TrailingSeparator => f.write_str("namespace must not end with ':'"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for NamespaceError {}

fn validate_namespace(value: &str) -> Result<(), NamespaceError> {
    const MAX_LEN: usize = 256;
    if value.is_empty() {
        return Err(NamespaceError::Empty);
    }
    if value.len() > MAX_LEN {
        return Err(NamespaceError::TooLong { max: MAX_LEN });
    }
    if value.ends_with(':') {
        return Err(NamespaceError::TrailingSeparator);
    }
    for segment in value.split(':') {
        if segment.is_empty() {
            return Err(NamespaceError::EmptySegment);
        }
        for ch in segment.chars() {
            if !ch.is_ascii_alphanumeric() && ch != '-' && ch != '_' && ch != '.' {
                return Err(NamespaceError::InvalidCharacter { ch });
            }
        }
    }
    Ok(())
}

/// A validated, opaque namespace identifier.
///
/// Construct via [`Namespace::parse`] or [`Namespace::local`]. The absence of
/// `From<String>` / `From<&str>` impls is intentional — callers must validate.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Namespace(String);

impl Namespace {
    /// The name of the default local namespace.
    pub const LOCAL: &'static str = "local";

    /// Parse and validate a namespace string.
    ///
    /// Returns `Err(NamespaceError)` if the string is empty, too long, contains
    /// invalid characters, has empty segments, or ends with `:`.
    pub fn parse(value: &str) -> Result<Self, NamespaceError> {
        validate_namespace(value)?;
        Ok(Self(String::from(value)))
    }

    /// Construct the default `"local"` namespace (always valid; no allocation).
    pub fn local() -> Self {
        Self(String::from(Self::LOCAL))
    }

    /// Return the namespace as a string slice.
    #[inline]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume the `Namespace` and return the underlying owned string.
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl core::convert::TryFrom<String> for Namespace {
    type Error = NamespaceError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl core::convert::TryFrom<&str> for Namespace {
    type Error = NamespaceError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl fmt::Display for Namespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for Namespace {
    #[inline]
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Returns `true` if `child` is a hierarchical prefix-descendant of `parent`.
///
/// Example: `"research:lattice"` is a prefix-child of `"research"`.
pub fn has_segment_prefix(child: &Namespace, parent: &Namespace) -> bool {
    let c = child.as_str();
    let p = parent.as_str();
    c.len() > p.len() && c.starts_with(p) && c.as_bytes().get(p.len()) == Some(&b':')
}

#[cfg(feature = "serde")]
mod serde_impl {
    use super::*;
    use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

    impl Serialize for Namespace {
        fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            s.serialize_str(&self.0)
        }
    }

    impl<'de> Deserialize<'de> for Namespace {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            let s = String::deserialize(d)?;
            Namespace::parse(&s).map_err(de::Error::custom)
        }
    }
}

#[cfg(test)]
mod tests {
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
}
