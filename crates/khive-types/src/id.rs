//! 128-bit identifier — universal ID primitive.
//!
//! Wire format: canonical hyphenated UUID (8-4-4-4-12).
//! Parses both hyphenated (`xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`)
//! and simple (`xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx`) forms.
//! Hashable, totally ordered, has a nil sentinel.

#![allow(clippy::manual_range_contains)]

use core::fmt;
use core::str::FromStr;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Id128([u8; 16]);

impl Id128 {
    pub const NIL: Self = Self([0; 16]);

    #[inline]
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    #[inline]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    #[inline]
    pub const fn is_nil(&self) -> bool {
        let b = &self.0;
        let mut i = 0;
        while i < 16 {
            if b[i] != 0 {
                return false;
            }
            i += 1;
        }
        true
    }

    #[inline]
    pub const fn from_u128(v: u128) -> Self {
        Self(v.to_be_bytes())
    }

    #[inline]
    pub const fn to_u128(&self) -> u128 {
        u128::from_be_bytes(self.0)
    }
}

const HEX_CHARS: &[u8; 16] = b"0123456789abcdef";

impl fmt::Display for Id128 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = &self.0;
        let mut buf = [0u8; 36];
        let mut pos = 0;

        // Groups: 4 bytes, 2 bytes, 2 bytes, 2 bytes, 6 bytes
        let groups: &[(usize, usize)] = &[(0, 4), (4, 6), (6, 8), (8, 10), (10, 16)];
        for (gi, &(start, end)) in groups.iter().enumerate() {
            if gi > 0 {
                buf[pos] = b'-';
                pos += 1;
            }
            for i in start..end {
                buf[pos] = HEX_CHARS[(b[i] >> 4) as usize];
                buf[pos + 1] = HEX_CHARS[(b[i] & 0x0f) as usize];
                pos += 2;
            }
        }
        f.write_str(core::str::from_utf8(&buf[..pos]).expect("hex chars are valid utf8"))
    }
}

impl fmt::Debug for Id128 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Id128({self})")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseIdError {
    InvalidLength,
    InvalidHex,
}

impl fmt::Display for ParseIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLength => f.write_str("expected UUID: 32 hex chars or 36 with hyphens"),
            Self::InvalidHex => f.write_str("invalid hex character in UUID"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for ParseIdError {}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn parse_hex_bytes(hex: &[u8]) -> Result<[u8; 16], ParseIdError> {
    if hex.len() != 32 {
        return Err(ParseIdError::InvalidLength);
    }
    let mut bytes = [0u8; 16];
    for i in 0..16 {
        let hi = hex_val(hex[i * 2]).ok_or(ParseIdError::InvalidHex)?;
        let lo = hex_val(hex[i * 2 + 1]).ok_or(ParseIdError::InvalidHex)?;
        bytes[i] = (hi << 4) | lo;
    }
    Ok(bytes)
}

impl FromStr for Id128 {
    type Err = ParseIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let b = s.as_bytes();
        match b.len() {
            32 => Ok(Self(parse_hex_bytes(b)?)),
            36 => {
                // Strip hyphens at positions 8, 13, 18, 23
                if b[8] != b'-' || b[13] != b'-' || b[18] != b'-' || b[23] != b'-' {
                    return Err(ParseIdError::InvalidHex);
                }
                let mut hex = [0u8; 32];
                hex[..8].copy_from_slice(&b[..8]);
                hex[8..12].copy_from_slice(&b[9..13]);
                hex[12..16].copy_from_slice(&b[14..18]);
                hex[16..20].copy_from_slice(&b[19..23]);
                hex[20..32].copy_from_slice(&b[24..36]);
                Ok(Self(parse_hex_bytes(&hex)?))
            }
            _ => Err(ParseIdError::InvalidLength),
        }
    }
}

impl Default for Id128 {
    #[inline]
    fn default() -> Self {
        Self::NIL
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for Id128 {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use alloc::string::ToString;
        serializer.serialize_str(&self.to_string())
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Id128 {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = <&str>::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;

    #[test]
    fn nil() {
        assert!(Id128::NIL.is_nil());
        assert!(!Id128::from_u128(1).is_nil());
    }

    #[test]
    fn roundtrip_u128() {
        let v: u128 = 0xdeadbeef_12345678_9abcdef0_11223344;
        let id = Id128::from_u128(v);
        assert_eq!(id.to_u128(), v);
    }

    #[test]
    fn display_is_hyphenated_uuid() {
        let id = Id128::from_u128(0xabcdef0123456789abcdef0123456789);
        let s = format!("{id}");
        assert_eq!(s.len(), 36);
        assert_eq!(s, "abcdef01-2345-6789-abcd-ef0123456789");
    }

    #[test]
    fn parse_hyphenated() {
        let id: Id128 = "abcdef01-2345-6789-abcd-ef0123456789".parse().unwrap();
        assert_eq!(id.to_u128(), 0xabcdef0123456789abcdef0123456789);
    }

    #[test]
    fn parse_simple() {
        let id: Id128 = "abcdef0123456789abcdef0123456789".parse().unwrap();
        assert_eq!(id.to_u128(), 0xabcdef0123456789abcdef0123456789);
    }

    #[test]
    fn display_parse_roundtrip() {
        let id = Id128::from_u128(0xabcdef0123456789abcdef0123456789);
        let s = format!("{id}");
        let parsed: Id128 = s.parse().unwrap();
        assert_eq!(parsed, id);
    }

    #[test]
    fn parse_errors() {
        assert_eq!("abc".parse::<Id128>(), Err(ParseIdError::InvalidLength));
        assert_eq!(
            "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz".parse::<Id128>(),
            Err(ParseIdError::InvalidHex)
        );
        // Wrong hyphen positions
        assert_eq!(
            "abcdef01-2345-6789-abcd-ef012345678".parse::<Id128>(),
            Err(ParseIdError::InvalidLength)
        );
    }

    #[test]
    fn ordering() {
        let a = Id128::from_u128(1);
        let b = Id128::from_u128(2);
        assert!(a < b);
    }
}
