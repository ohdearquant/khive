//! 256-bit content hash for checkpoint integrity verification.
//!
//! # Formal proof reference
//!
//! `proofs/Retrieval/Distance.lean` — hash identity used in checkpoint
//! compatibility checks.

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
