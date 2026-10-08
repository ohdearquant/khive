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
mod tests {
    use super::*;

    const BITS: [u32; 8] = [
        0,
        0x8000_0000,
        1,
        0x3f80_0000,
        0xc020_0000,
        0x7f80_0000,
        0xff80_0000,
        0x7fc1_2345,
    ];
    const LE: [u8; 32] = [
        0, 0, 0, 0, 0, 0, 0, 0x80, 1, 0, 0, 0, 0, 0, 0x80, 0x3f, 0, 0, 0x20, 0xc0, 0, 0, 0x80,
        0x7f, 0, 0, 0x80, 0xff, 0x45, 0x23, 0xc1, 0x7f,
    ];
    const BE: [u8; 32] = [
        0, 0, 0, 0, 0x80, 0, 0, 0, 0, 0, 0, 1, 0x3f, 0x80, 0, 0, 0xc0, 0x20, 0, 0, 0x7f, 0x80, 0,
        0, 0xff, 0x80, 0, 0, 0x7f, 0xc1, 0x23, 0x45,
    ];

    #[test]
    fn little_endian_fixed_bytes_preserve_all_float_bits() {
        let values = BITS.map(f32::from_bits);
        assert_eq!(encode_f32_le(&values), LE);
        assert_eq!(
            decode_f32_le(&LE)
                .unwrap()
                .iter()
                .map(|v| v.to_bits())
                .collect::<Vec<_>>(),
            BITS
        );
    }

    #[test]
    fn native_fixed_bytes_follow_the_target_abi() {
        let expected = if cfg!(target_endian = "little") {
            LE
        } else {
            BE
        };
        let values = BITS.map(f32::from_bits);
        assert_eq!(encode_f32_native(&values), expected);
        assert_eq!(
            decode_f32_native(&expected)
                .unwrap()
                .iter()
                .map(|v| v.to_bits())
                .collect::<Vec<_>>(),
            BITS
        );
    }

    #[test]
    fn empty_input_and_incomplete_values_are_distinct() {
        type Decoder = fn(&[u8]) -> Result<Vec<f32>, VectorCodecError>;
        assert!(encode_f32_le(&[]).is_empty());
        assert!(encode_f32_native(&[]).is_empty());
        for decode in [decode_f32_le as Decoder, decode_f32_native] {
            assert!(decode(&[]).unwrap().is_empty());
            for length in [1, 2, 3, 5, 6, 7] {
                assert_eq!(
                    decode(&[0; 7][..length]).unwrap_err(),
                    VectorCodecError { byte_len: length }
                );
            }
            assert_eq!(decode(&[0; 4]).unwrap()[0].to_bits(), 0);
        }
    }
}
