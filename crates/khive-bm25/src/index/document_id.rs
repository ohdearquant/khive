//! Typed document identifier for BM25 index operations.

/// Typed document identifier serialized as a plain JSON string.
///
/// See `crates/khive-bm25/docs/api/index-lifecycle.md` for wire compatibility.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct DocumentId(String);

impl DocumentId {
    /// Create a new `DocumentId` from any `Into<String>`.
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Return the inner string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume `self` and return the inner `String`.
    pub fn into_inner(self) -> String {
        self.0
    }

    /// Return the length of the underlying string in bytes.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Return `true` if the underlying string is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Display for DocumentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::ops::Deref for DocumentId {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl std::borrow::Borrow<str> for DocumentId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for DocumentId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<String> for DocumentId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for DocumentId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl PartialEq<str> for DocumentId {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for DocumentId {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<String> for DocumentId {
    fn eq(&self, other: &String) -> bool {
        &self.0 == other
    }
}

#[cfg(test)]
mod document_id_wire_format {
    use super::DocumentId;

    /// DocumentId must serialize as a bare JSON string (enforced by serde transparent).
    #[test]
    fn document_id_serializes_as_plain_string() {
        let id = DocumentId::new("some-document-identifier");
        let json = serde_json::to_string(&id).expect("DocumentId serialize");
        assert_eq!(
            json, r#""some-document-identifier""#,
            "wire format drift detected in DocumentId — must be plain JSON string",
        );
    }

    #[test]
    fn document_id_roundtrip() {
        let id = DocumentId::new("doc_abc_123");
        let json = serde_json::to_string(&id).expect("serialize");
        let back: DocumentId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, id, "serde roundtrip must produce identical value");
    }

    #[test]
    fn document_id_empty_string_roundtrip() {
        let id = DocumentId::new("");
        let json = serde_json::to_string(&id).expect("serialize");
        assert_eq!(
            json, r#""""#,
            "empty DocumentId must serialize as empty JSON string"
        );
        let back: DocumentId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, id);
    }

    #[test]
    fn document_id_unicode_roundtrip() {
        let id = DocumentId::new("doc_\u{4e2d}\u{6587}");
        let json = serde_json::to_string(&id).expect("serialize");
        let back: DocumentId = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, id);
    }
}
