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
/// dropped-pair count. [`DETAILS_TRUNCATED_KEY`] is a *reserved* key: a
/// client-supplied pair using that name is never retained as an ordinary
/// entry (PR #549) — it is stripped and folded into the
/// drop count instead, so a client can neither fake truncation on a small
/// map nor shadow the real indicator on an oversized one. The drop count
/// itself is tracked in an internal, non-serialized field
/// ([`Details::dropped_count`]) rather than parsed back out of the entry
/// list, so a same-shaped client map can't spoof it either.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Details {
    entries: alloc::vec::Vec<(Cow<'static, str>, Cow<'static, str>)>,
    /// Internal truncation flag — `Some(dropped_count)` when this instance
    /// was built by dropping pairs (8-entry overflow and/or a reserved-key
    /// collision), `None` otherwise. Not serialized directly; the wire
    /// shape communicates truncation via the [`DETAILS_TRUNCATED_KEY`]
    /// entry, this field is the trusted, non-spoofable read side.
    dropped: Option<usize>,
}

impl Details {
    /// Build `Details` from an iterable of `(&'static str, &'static str)` pairs.
    ///
    /// Up to 8 pairs are kept as-is. When more than 8 are supplied, the first
    /// 7 client pairs are kept and the 8th slot is replaced with
    /// [`DETAILS_TRUNCATED_KEY`] carrying the dropped-pair count, so the
    /// truncation is observable rather than silent. A client-supplied pair
    /// named [`DETAILS_TRUNCATED_KEY`] is always treated as reserved: it is
    /// dropped (never stored as an ordinary entry) and counted, even when
    /// the remaining pairs fit within the 8-entry bound.
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

    /// Shared bounding/truncation logic for the constructor: partition the
    /// source into ordinary pairs and reserved-key collisions, then hand
    /// off to [`Details::build`] for the bound + indicator logic.
    fn from_owned<I>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (Cow<'static, str>, Cow<'static, str>)>,
    {
        let mut ordinary: alloc::vec::Vec<(Cow<'static, str>, Cow<'static, str>)> =
            alloc::vec::Vec::new();
        let mut total_ordinary: usize = 0;
        let mut collisions: usize = 0;
        for (k, v) in pairs {
            if k.as_ref() == DETAILS_TRUNCATED_KEY {
                collisions += 1;
            } else {
                total_ordinary += 1;
                if ordinary.len() < 8 {
                    ordinary.push((k, v));
                }
            }
        }
        Self::build(ordinary, total_ordinary, collisions)
    }

    /// Bounding/truncation core. See
    /// crates/khive-types/docs/pack-error-internals.md#detailsbuild--boundingtruncation-algorithm
    fn build(
        ordinary: alloc::vec::Vec<(Cow<'static, str>, Cow<'static, str>)>,
        total_ordinary: usize,
        collisions: usize,
    ) -> Self {
        if total_ordinary <= 8 && collisions == 0 {
            return Self {
                entries: ordinary,
                dropped: None,
            };
        }
        let keep = total_ordinary.min(7);
        let dropped = (total_ordinary - keep) + collisions;
        let mut entries: alloc::vec::Vec<_> = ordinary.into_iter().take(keep).collect();
        entries.push((
            Cow::Borrowed(DETAILS_TRUNCATED_KEY),
            Cow::Owned(alloc::format!("{dropped}")),
        ));
        Self {
            entries,
            dropped: Some(dropped),
        }
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

    /// Number of pairs dropped due to the 8-entry bound and/or a
    /// reserved-key collision, if any were.
    ///
    /// Returns `None` when nothing was dropped. Returns
    /// `Some(dropped_count)` otherwise, read from the internal truncation
    /// flag set at construction/deserialization time — never re-parsed from
    /// the entry list, so a client-supplied `details_truncated` pair can't
    /// spoof this value.
    pub fn dropped_count(&self) -> Option<usize> {
        self.dropped
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
                // Drains to completion regardless of size (fixes #487: early-exit
                // at 8 entries left trailing map bytes unconsumed). Detects a
                // round-tripped self-truncated map vs. a client-supplied
                // DETAILS_TRUNCATED_KEY collision — see
                // crates/khive-types/docs/pack-error-internals.md#details-deserialization--round-trip-detection-of-self-truncated-maps
                let mut ordinary: alloc::vec::Vec<(Cow<'static, str>, Cow<'static, str>)> =
                    alloc::vec::Vec::new();
                let mut total_ordinary: usize = 0;
                let mut reserved_count: usize = 0;
                let mut reserved_is_trailing = false;
                let mut reserved_follows_seven = false;
                let mut last_reserved_value: Option<String> = None;
                while let Some((k, v)) = map.next_entry::<String, String>()? {
                    if k == DETAILS_TRUNCATED_KEY {
                        reserved_count += 1;
                        reserved_is_trailing = true;
                        reserved_follows_seven = total_ordinary == 7;
                        last_reserved_value = Some(v);
                    } else {
                        reserved_is_trailing = false;
                        total_ordinary += 1;
                        if ordinary.len() < 8 {
                            ordinary.push((Cow::Owned(k), Cow::Owned(v)));
                        }
                    }
                }
                if reserved_count == 1 && reserved_is_trailing && reserved_follows_seven {
                    if let Some(dropped) =
                        last_reserved_value.as_deref().and_then(|s| s.parse().ok())
                    {
                        let mut entries = ordinary;
                        entries.push((
                            Cow::Borrowed(DETAILS_TRUNCATED_KEY),
                            Cow::Owned(alloc::format!("{dropped}")),
                        ));
                        return Ok(Details {
                            entries,
                            dropped: Some(dropped),
                        });
                    }
                }
                Ok(Details::build(ordinary, total_ordinary, reserved_count))
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
