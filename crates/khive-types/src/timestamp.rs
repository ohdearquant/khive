//! Timestamp — microseconds since Unix epoch.
//!
//! No clock access — generation happens in host crates.

use core::fmt;

/// Microseconds since the Unix epoch, stored as a `u64`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[repr(transparent)]
pub struct Timestamp(u64);

impl Timestamp {
    /// The Unix epoch (zero microseconds).
    pub const EPOCH: Self = Self(0);
    /// The maximum representable timestamp.
    pub const MAX: Self = Self(u64::MAX);

    /// Construct from a microsecond count.
    #[inline]
    pub const fn from_micros(micros: u64) -> Self {
        Self(micros)
    }

    /// Construct from a millisecond count (converted to microseconds).
    #[inline]
    pub const fn from_millis(millis: u64) -> Self {
        Self(millis.saturating_mul(1000))
    }

    /// Construct from a whole-second count (converted to microseconds).
    #[inline]
    pub const fn from_secs(secs: u64) -> Self {
        Self(secs.saturating_mul(1_000_000))
    }

    /// Return the raw microsecond value.
    #[inline]
    pub const fn as_micros(&self) -> u64 {
        self.0
    }

    /// Return the value truncated to whole milliseconds.
    #[inline]
    pub const fn as_millis(&self) -> u64 {
        self.0 / 1000
    }

    /// Return the value truncated to whole seconds.
    #[inline]
    pub const fn as_secs(&self) -> u64 {
        self.0 / 1_000_000
    }

    /// Return `true` if this is the epoch (zero).
    #[inline]
    pub const fn is_zero(&self) -> bool {
        self.0 == 0
    }

    /// Add `micros` microseconds, saturating at `u64::MAX`.
    #[inline]
    pub const fn saturating_add(self, micros: u64) -> Self {
        Self(self.0.saturating_add(micros))
    }

    /// Subtract `micros` microseconds, saturating at zero.
    #[inline]
    pub const fn saturating_sub(self, micros: u64) -> Self {
        Self(self.0.saturating_sub(micros))
    }

    /// Return the elapsed microseconds between `earlier` and `self`, saturating at zero.
    pub const fn duration_since(self, earlier: Self) -> u64 {
        self.0.saturating_sub(earlier.0)
    }
}

impl fmt::Debug for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Timestamp({}µs)", self.0)
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}µs", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions() {
        let ts = Timestamp::from_secs(1700000000);
        assert_eq!(ts.as_secs(), 1700000000);
        assert_eq!(ts.as_millis(), 1700000000000);
        assert_eq!(ts.as_micros(), 1700000000000000);
    }

    #[test]
    fn ordering() {
        let a = Timestamp::from_secs(1);
        let b = Timestamp::from_secs(2);
        assert!(a < b);
    }

    #[test]
    fn arithmetic() {
        let ts = Timestamp::from_secs(10);
        assert_eq!(ts.saturating_add(1_000_000), Timestamp::from_secs(11));
        assert_eq!(ts.saturating_sub(5_000_000), Timestamp::from_secs(5));
        assert_eq!(Timestamp::from_secs(0).saturating_sub(1), Timestamp::EPOCH);
    }

    #[test]
    fn duration_since() {
        let a = Timestamp::from_secs(10);
        let b = Timestamp::from_secs(15);
        assert_eq!(b.duration_since(a), 5_000_000);
    }
}
