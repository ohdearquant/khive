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

    /// Construct an explicit `[min, max]` inclusive supported range, for a
    /// server that intentionally narrows or widens the default policy.
    pub const fn new(min: ProtocolVersion, max: ProtocolVersion) -> Self {
        Self { min, max }
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
