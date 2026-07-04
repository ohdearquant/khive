//! Unified cross-crate error model: `KhiveError`, `ErrorKind`, `ErrorCode`, `Details`, `RetryHint`.

extern crate alloc;
use alloc::borrow::Cow;
use alloc::string::String;
use core::fmt;

#[cfg(feature = "serde")]
use alloc::string::ToString;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

// ---- ErrorKind ----

/// Semantic error category — maps to HTTP status codes.
///
/// | Variant | HTTP |
/// |---------|------|
/// | `NotFound` | 404 |
/// | `InvalidInput` | 400 |
/// | `Unauthorized` | 403 |
/// | `Conflict` | 409 |
/// | `Unavailable` | 503 |
/// | `Internal` | 500 |
///
/// Closed taxonomy. New variants are a source-breaking change and require an ADR.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum ErrorKind {
    NotFound,
    InvalidInput,
    Unauthorized,
    Conflict,
    Unavailable,
    Internal,
}

impl ErrorKind {
    /// HTTP status code for this kind.
    pub fn http_status(self) -> u16 {
        match self {
            Self::NotFound => 404,
            Self::InvalidInput => 400,
            Self::Unauthorized => 403,
            Self::Conflict => 409,
            Self::Unavailable => 503,
            Self::Internal => 500,
        }
    }

    /// Snake-case string representation (stable across versions).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::InvalidInput => "invalid_input",
            Self::Unauthorized => "unauthorized",
            Self::Conflict => "conflict",
            Self::Unavailable => "unavailable",
            Self::Internal => "internal",
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---- ErrorDomain ----

/// Domain that owns the error code namespace.
///
/// Only the OSS-relevant domains are exposed; internal-only domains
/// (auth, billing, etc.) are not included.
///
/// Closed taxonomy. New variants are a source-breaking change and require an ADR.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
pub enum ErrorDomain {
    Db,
    Query,
    Runtime,
    Types,
}

impl ErrorDomain {
    /// Return the lowercase string name for this domain.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Db => "db",
            Self::Query => "query",
            Self::Runtime => "runtime",
            Self::Types => "types",
        }
    }
}

impl fmt::Display for ErrorDomain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---- ErrorCode ----

/// Domain-scoped numeric error code.
///
/// Wire shape: `"domain:N"` (e.g., `"db:1"`, `"runtime:10"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ErrorCode {
    domain: ErrorDomain,
    code: u32,
}

impl ErrorCode {
    /// Create a new error code in the given domain.
    pub fn new(domain: ErrorDomain, code: u32) -> Self {
        Self { domain, code }
    }

    /// Return the domain that owns this error code.
    pub fn domain(self) -> ErrorDomain {
        self.domain
    }

    /// Return the numeric code within the domain.
    pub fn code(self) -> u32 {
        self.code
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.domain, self.code)
    }
}

#[cfg(feature = "serde")]
impl Serialize for ErrorCode {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

#[cfg(feature = "serde")]
impl<'de> Deserialize<'de> for ErrorCode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = alloc::string::String::deserialize(d)?;
        let (domain_str, code_str) = s
            .split_once(':')
            .ok_or_else(|| serde::de::Error::custom("expected 'domain:N'"))?;
        let domain = match domain_str {
            "db" => ErrorDomain::Db,
            "query" => ErrorDomain::Query,
            "runtime" => ErrorDomain::Runtime,
            "types" => ErrorDomain::Types,
            other => {
                return Err(serde::de::Error::custom(alloc::format!(
                    "unknown domain: {other}"
                )))
            }
        };
        let code: u32 = code_str.parse().map_err(serde::de::Error::custom)?;
        Ok(ErrorCode::new(domain, code))
    }
}

// ---- RetryHint ----

/// Guidance to callers on whether retrying the operation makes sense.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum RetryHint {
    /// Do not retry — the same request will fail again.
    NoRetry,
    /// Retry may succeed (transient failure).
    Retryable,
}

// ---- Details ----

/// Reserved key inserted in place of the 8th slot when a `Details` source
/// (constructor input or a deserialized wire map) supplies more than 8 pairs.
/// Its value is the count of dropped pairs, so truncation is observable
/// instead of silently discarding data (RUNTIME-AUD-002 / #487 follow-up).
pub const DETAILS_TRUNCATED_KEY: &str = "details_truncated";

/// Bounded key/value metadata attached to a `KhiveError` (max 8 pairs).
///
/// Stored as `Cow<'static, str>` pairs: zero-alloc for static string literals
/// (the common construction path) and owned strings on deserialization (no
/// memory leak). Both paths are `no_std` + `alloc` compatible.
///
/// When the source supplies more than 8 pairs, the wire shape stays bounded
/// at 8 entries, but the truncation is observable: the first 7 pairs are
/// retained and the 8th slot becomes [`DETAILS_TRUNCATED_KEY`] mapped to the
/// dropped-pair count. Callers that need the drop count without guessing the
/// reserved key can use [`Details::dropped_count`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Details {
    entries: alloc::vec::Vec<(Cow<'static, str>, Cow<'static, str>)>,
}

impl Details {
    /// Build `Details` from an iterable of `(&'static str, &'static str)` pairs.
    ///
    /// Up to 8 pairs are kept as-is. When more than 8 are supplied, the first
    /// 7 client pairs are kept and the 8th slot is replaced with
    /// [`DETAILS_TRUNCATED_KEY`] carrying the dropped-pair count, so the
    /// truncation is observable rather than silent.
    pub fn new<I>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (&'static str, &'static str)>,
    {
        let all: alloc::vec::Vec<(&'static str, &'static str)> = pairs.into_iter().collect();
        Self::from_owned(
            all.into_iter()
                .map(|(k, v)| (Cow::Borrowed(k), Cow::Borrowed(v))),
        )
    }

    /// Shared bounding/truncation logic for both the constructor and the
    /// deserializer: keep the first 8 pairs verbatim, or — when more than 8
    /// are supplied — keep the first 7 and append a `details_truncated`
    /// indicator carrying the dropped-pair count.
    fn from_owned<I>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (Cow<'static, str>, Cow<'static, str>)>,
    {
        let all: alloc::vec::Vec<_> = pairs.into_iter().collect();
        if all.len() <= 8 {
            return Self { entries: all };
        }
        let dropped = all.len() - 7;
        let mut entries: alloc::vec::Vec<_> = all.into_iter().take(7).collect();
        entries.push((
            Cow::Borrowed(DETAILS_TRUNCATED_KEY),
            Cow::Owned(alloc::format!("{dropped}")),
        ));
        Self { entries }
    }

    /// Look up a value by key.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(k, _)| k.as_ref() == key)
            .map(|(_, v)| v.as_ref())
    }

    /// Iterate over (key, value) pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> + '_ {
        self.entries.iter().map(|(k, v)| (k.as_ref(), v.as_ref()))
    }

    /// Number of pairs dropped due to the 8-entry bound, if any were.
    ///
    /// Returns `None` when the source supplied 8 or fewer pairs (no
    /// truncation occurred). Returns `Some(dropped_count)` when truncation
    /// occurred, parsed from the [`DETAILS_TRUNCATED_KEY`] indicator entry.
    pub fn dropped_count(&self) -> Option<usize> {
        self.get(DETAILS_TRUNCATED_KEY)?.parse().ok()
    }
}

#[cfg(feature = "serde")]
impl Serialize for Details {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = s.serialize_map(Some(self.entries.len()))?;
        for (k, v) in &self.entries {
            map.serialize_entry(k.as_ref(), v.as_ref())?;
        }
        map.end()
    }
}

#[cfg(feature = "serde")]
impl<'de> Deserialize<'de> for Details {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::{MapAccess, Visitor};

        struct DetailsVisitor;

        impl<'de> Visitor<'de> for DetailsVisitor {
            type Value = Details;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a map of string key-value pairs")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Details, A::Error> {
                // Drain to completion regardless of size (#487: a naive early-exit
                // once 8 entries are collected leaves trailing map bytes unconsumed
                // and corrupts the surrounding deserializer). Only the first 8
                // pairs are retained in memory as they arrive; entries beyond that
                // are counted, not stored, so an adversarially large map can't
                // inflate memory. Bounding + the observable-truncation indicator
                // (RUNTIME-AUD-002 / #487 follow-up) are applied afterward via the
                // same logic `Details::new` uses.
                let mut entries: alloc::vec::Vec<(Cow<'static, str>, Cow<'static, str>)> =
                    alloc::vec::Vec::new();
                let mut total: usize = 0;
                while let Some((k, v)) = map.next_entry::<String, String>()? {
                    total += 1;
                    if entries.len() < 8 {
                        entries.push((Cow::Owned(k), Cow::Owned(v)));
                    }
                }
                if total > 8 {
                    let dropped = total - 7;
                    entries.truncate(7);
                    entries.push((
                        Cow::Borrowed(DETAILS_TRUNCATED_KEY),
                        Cow::Owned(alloc::format!("{dropped}")),
                    ));
                }
                Ok(Details { entries })
            }
        }

        d.deserialize_map(DetailsVisitor)
    }
}

// ---- KhiveError ----

/// Unified error type for the khive runtime.
///
/// # Wire shape (serde)
///
/// ```json
/// {
///   "kind": "not_found",
///   "message": "entity not found: abc123",
///   "code": "runtime:10",
///   "details": { "resource": "entity", "id": "abc123" }
/// }
/// ```
///
/// `code` and `details` are `null` when absent.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct KhiveError {
    kind: ErrorKind,
    message: String,
    code: Option<ErrorCode>,
    details: Option<Details>,
}

impl KhiveError {
    // ---- constructors ----

    /// Create a `NotFound` error for a missing resource identified by `id`.
    pub fn not_found(resource: impl fmt::Display, id: impl fmt::Display) -> Self {
        Self {
            kind: ErrorKind::NotFound,
            message: alloc::format!("{resource} not found: {id}"),
            code: None,
            details: None,
        }
    }

    /// Create an `InvalidInput` error with the given message.
    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::InvalidInput,
            message: alloc::format!("invalid input: {}", message.into()),
            code: None,
            details: None,
        }
    }

    /// Create an `Unauthorized` error with the given message.
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Unauthorized,
            message: alloc::format!("unauthorized: {}", message.into()),
            code: None,
            details: None,
        }
    }

    /// Create a `Conflict` error with the given message.
    pub fn conflict(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Conflict,
            message: alloc::format!("conflict: {}", message.into()),
            code: None,
            details: None,
        }
    }

    /// Create an `Unavailable` error with the given message.
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Unavailable,
            message: alloc::format!("unavailable: {}", message.into()),
            code: None,
            details: None,
        }
    }

    /// Create an `Internal` error with the given message.
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            kind: ErrorKind::Internal,
            message: alloc::format!("internal: {}", message.into()),
            code: None,
            details: None,
        }
    }

    // ---- builder methods ----

    /// Attach a domain-scoped error code.
    pub fn with_code(mut self, code: ErrorCode) -> Self {
        self.code = Some(code);
        self
    }

    /// Attach bounded key-value metadata.
    pub fn with_details(mut self, details: Details) -> Self {
        self.details = Some(details);
        self
    }

    // ---- accessors ----

    /// Return the semantic error category.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// Return the human-readable error message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Return the domain-scoped error code, if set.
    pub fn code(&self) -> Option<ErrorCode> {
        self.code
    }

    /// Return the bounded metadata details, if set.
    pub fn details(&self) -> Option<&Details> {
        self.details.as_ref()
    }

    /// Retry guidance based on the error kind.
    pub fn retry_hint(&self) -> RetryHint {
        match self.kind {
            ErrorKind::Unavailable => RetryHint::Retryable,
            _ => RetryHint::NoRetry,
        }
    }
}

impl fmt::Display for KhiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

#[cfg(feature = "std")]
impl std::error::Error for KhiveError {}
