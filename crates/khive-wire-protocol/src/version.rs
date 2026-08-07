//! Protocol version and the compatibility policy from ADR-137's
//! "Compatibility policy" section.

use serde::{Deserialize, Serialize};

/// A wire protocol version number.
///
/// Version numbers are monotonic. A breaking wire change — a new frame kind,
/// a new wire error code, or a change to an existing frame's field shape that
/// isn't backward-compatible — is never introduced within a version number;
/// it requires incrementing this value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProtocolVersion(pub u32);

impl ProtocolVersion {
    pub const fn new(version: u32) -> Self {
        Self(version)
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

impl From<u32> for ProtocolVersion {
    fn from(value: u32) -> Self {
        Self(value)
    }
}

impl std::fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The current protocol version implemented by this crate.
pub const CURRENT_VERSION: ProtocolVersion = ProtocolVersion(1);

// Version 0 does not exist in this protocol: the range floor is 1
// ([`SupportedVersions::new`] rejects a zero `min`). Pin the invariant at
// compile time so a future edit setting `CURRENT_VERSION` to 0 fails the
// build instead of silently producing an all-zero supported range.
const _: () = assert!(CURRENT_VERSION.0 >= 1, "CURRENT_VERSION must be >= 1");

/// The set of protocol versions a server built against this crate accepts.
///
/// ADR-137's compatibility policy requires a server to support the current
/// version and at least the immediately prior version. This crate is
/// currently at version 1, so there is no prior version yet; the lower bound
/// saturates at 1 rather than underflowing to 0 (there is no version 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupportedVersions {
    min: ProtocolVersion,
    max: ProtocolVersion,
}

impl SupportedVersions {
    /// The default policy: the current version and the immediately prior one.
    pub const fn current() -> Self {
        let max = CURRENT_VERSION.0;
        let min = if max > 1 { max - 1 } else { 1 };
        Self {
            min: ProtocolVersion(min),
            max: ProtocolVersion(max),
        }
    }

    /// Construct an explicit `[min, max]` inclusive supported range within
    /// the protocol grammar implemented by this crate.
    ///
    /// Fails with [`SupportedVersionsError`] rather than panicking on an
    /// unusable range:
    ///
    /// - [`SupportedVersionsError::MinVersionZero`] if `min` is version 0 —
    ///   version 0 does not exist in this protocol, and a range admitting
    ///   it would accept handshakes no conforming client would send.
    /// - [`SupportedVersionsError::InvertedRange`] if `min > max` — an
    ///   inverted range would silently reject every handshake (no version
    ///   satisfies it), which is always a configuration bug.
    /// - [`SupportedVersionsError::MaxAboveCurrent`] if `max` exceeds
    ///   [`CURRENT_VERSION`] — this crate cannot admit a version whose wire
    ///   grammar it does not implement.
    pub const fn new(
        min: ProtocolVersion,
        max: ProtocolVersion,
    ) -> Result<Self, SupportedVersionsError> {
        if min.0 < 1 {
            return Err(SupportedVersionsError::MinVersionZero);
        }
        if min.0 > max.0 {
            return Err(SupportedVersionsError::InvertedRange);
        }
        if max.0 > CURRENT_VERSION.0 {
            return Err(SupportedVersionsError::MaxAboveCurrent);
        }
        Ok(Self { min, max })
    }

    pub const fn min(self) -> ProtocolVersion {
        self.min
    }

    pub const fn max(self) -> ProtocolVersion {
        self.max
    }

    pub const fn contains(self, version: ProtocolVersion) -> bool {
        version.0 >= self.min.0 && version.0 <= self.max.0
    }
}

impl Default for SupportedVersions {
    fn default() -> Self {
        Self::current()
    }
}

/// Why an explicit [`SupportedVersions::new`] range was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SupportedVersionsError {
    /// The range's lower bound was version 0; this protocol has no
    /// version 0, and the floor is 1.
    #[error("supported-version range floor must be >= 1 (version 0 does not exist)")]
    MinVersionZero,
    /// The range's lower bound exceeded its upper bound; no version
    /// satisfies it, so it would reject every handshake.
    #[error("supported-version range is inverted: min must not exceed max")]
    InvertedRange,
    /// The range's upper bound is newer than the grammar implemented by this
    /// crate.
    #[error("supported-version range max must not exceed the current protocol version")]
    MaxAboveCurrent,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_accepts_the_current_range() {
        let supported =
            SupportedVersions::new(ProtocolVersion::new(1), ProtocolVersion::new(1)).unwrap();
        assert_eq!(supported.min(), ProtocolVersion::new(1));
        assert_eq!(supported.max(), ProtocolVersion::new(1));
    }

    #[test]
    fn new_rejects_a_range_above_current_version() {
        for (min, max) in [(1, 2), (2, 2)] {
            let err = SupportedVersions::new(ProtocolVersion::new(min), ProtocolVersion::new(max))
                .unwrap_err();
            assert_eq!(err, SupportedVersionsError::MaxAboveCurrent);
        }
    }

    #[test]
    fn new_rejects_an_inverted_range() {
        let err =
            SupportedVersions::new(ProtocolVersion::new(5), ProtocolVersion::new(2)).unwrap_err();
        assert_eq!(err, SupportedVersionsError::InvertedRange);
    }

    #[test]
    fn new_rejects_a_zero_min_version() {
        // Version 0 does not exist in this protocol; the floor is 1.
        // Checked before the ordering check, so a doubly-broken range
        // reports the zero floor first.
        let err =
            SupportedVersions::new(ProtocolVersion::new(0), ProtocolVersion::new(1)).unwrap_err();
        assert_eq!(err, SupportedVersionsError::MinVersionZero);
        let err =
            SupportedVersions::new(ProtocolVersion::new(0), ProtocolVersion::new(0)).unwrap_err();
        assert_eq!(err, SupportedVersionsError::MinVersionZero);
    }

    #[test]
    fn current_version_is_at_least_one() {
        // The test mirror of the compile-time `const _: () = assert!` in
        // this module: the invariant gets an explicit, greppable failure
        // either way.
        assert!(CURRENT_VERSION.get() >= 1);
    }

    #[test]
    fn contains_is_inclusive_at_both_bounds() {
        // Boundary inclusion: min and max themselves are supported; one
        // below min and one above max are not.
        let supported =
            SupportedVersions::new(ProtocolVersion::new(1), ProtocolVersion::new(1)).unwrap();
        assert!(!supported.contains(ProtocolVersion::new(0)));
        assert!(supported.contains(ProtocolVersion::new(1)));
        assert!(!supported.contains(ProtocolVersion::new(2)));
    }

    #[test]
    fn current_saturates_at_version_one() {
        // The crate is at version 1, so there is no prior version and the
        // default range is [1, 1].
        let supported = SupportedVersions::current();
        assert_eq!(supported.min(), ProtocolVersion::new(1));
        assert_eq!(supported.max(), ProtocolVersion::new(1));
    }
}
