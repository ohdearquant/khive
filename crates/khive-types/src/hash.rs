//! 256-bit content hash for checkpoint integrity verification.

use core::fmt;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// 256-bit (32-byte) content hash.
///
/// Used as a content-addressed identifier for HNSW checkpoints and other
/// snapshot artifacts. The underlying algorithm is caller-defined; the type
/// carries the raw bytes without encoding assumptions.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct Hash32([u8; 32]);

impl Hash32 {
    /// Zero hash (nil value).
    pub const ZERO: Self = Self([0u8; 32]);

    /// Construct from raw bytes.
    #[inline]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Return the raw byte representation.
    #[inline]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Compute a BLAKE3 hash over the given byte slice.
    ///
    /// Requires the `blake3` feature.
    #[cfg(feature = "blake3")]
    #[inline]
    pub fn from_blake3(data: &[u8]) -> Self {
        let hash = blake3::hash(data);
        Self(*hash.as_bytes())
    }

    /// Constant-time equality check.
    ///
    /// Accumulates XOR over all 32 bytes without early exit so the comparison
    /// takes the same number of iterations regardless of where bytes differ.
    /// Suitable for integrity comparisons where timing side-channels are a
    /// concern.  The `#[inline(never)]` attribute discourages the compiler from
    /// inlining and optimising away the full-loop traversal.
    #[inline(never)]
    pub fn eq_ct(&self, other: &Self) -> bool {
        let diff = self
            .0
            .iter()
            .zip(other.0.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b));
        diff == 0
    }
}

impl From<[u8; 32]> for Hash32 {
    #[inline]
    fn from(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl fmt::Debug for Hash32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash32(")?;
        for b in &self.0 {
            write!(f, "{b:02x}")?;
        }
        write!(f, ")")
    }
}

impl fmt::Display for Hash32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in &self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}
