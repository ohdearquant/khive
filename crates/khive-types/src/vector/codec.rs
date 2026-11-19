//! Bit-preserving f32 blob codecs with explicit byte order.

use alloc::vec::Vec;
use core::fmt;

/// A byte slice cannot contain a whole number of f32 values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VectorCodecError {
    /// Length of the supplied byte slice.
    pub byte_len: usize,
}

impl fmt::Display for VectorCodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "f32 vector byte length {} is not a multiple of 4",
            self.byte_len
        )
    }
}

impl core::error::Error for VectorCodecError {}

/// Encode portable little-endian IEEE 754 values, without filtering any bit pattern.
pub fn encode_f32_le(values: &[f32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

/// Decode little-endian values, refusing an incomplete final value.
/// Empty input is valid; finite-value and dimension policies belong to callers.
pub fn decode_f32_le(bytes: &[u8]) -> Result<Vec<f32>, VectorCodecError> {
    decode(bytes, f32::from_le_bytes)
}

/// Encode the host-native f32 ABI used by sqlite-vec. This is not a portable format.
pub fn encode_f32_native(values: &[f32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_ne_bytes())
        .collect()
}

/// Decode the host-native f32 ABI, refusing an incomplete final value.
pub fn decode_f32_native(bytes: &[u8]) -> Result<Vec<f32>, VectorCodecError> {
    decode(bytes, f32::from_ne_bytes)
}

fn decode(bytes: &[u8], from_bytes: fn([u8; 4]) -> f32) -> Result<Vec<f32>, VectorCodecError> {
    let (values, remainder) = bytes.as_chunks::<4>();
    if !remainder.is_empty() {
        return Err(VectorCodecError {
            byte_len: bytes.len(),
        });
    }
    Ok(values.iter().map(|bytes| from_bytes(*bytes)).collect())
}

#[cfg(test)]
#[path = "codec_tests.rs"]
mod tests;
