//! Content hashes and non-cryptographic change-detection fingerprints.

use core::fmt;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Compute the 64-bit FNV-1a fingerprint of every byte in `data`.
///
/// Uses the standard offset basis and wrapping multiplication. This is a
/// non-cryptographic fingerprint; it is not an integrity or security boundary.
pub fn fnv1a_64(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in data {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

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

#[cfg(test)]
mod tests {
    #[test]
    fn fnv1a_64_matches_published_vectors() {
        // draft-eastlake-fnv-19, Appendix C: strings with and without NUL.
        let cases: &[(&[u8], u64)] = &[
            (b"", 0xcbf2_9ce4_8422_2325),
            (b"a", 0xaf63_dc4c_8601_ec8c),
            (b"foobar", 0x8594_4171_f739_67e8),
            (b"\0", 0xaf63_bd4c_8601_b7df),
            (b"a\0", 0x089b_e207_b544_f1e4),
            (b"foobar\0", 0x3453_1ca7_168b_8f38),
        ];
        for &(bytes, expected) in cases {
            assert_eq!(crate::fnv1a_64(bytes), expected);
        }
    }
}
