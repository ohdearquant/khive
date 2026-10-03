//! Canonical boundary encodings (ADR-105 A.2 and A.10).
use crate::ProtocolError;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{de::Error as _, Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use uuid::Uuid;

pub const MAX_JSON_INTEGER: u64 = (1 << 53) - 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CanonicalUuid(Uuid);
impl CanonicalUuid {
    pub fn parse(value: &str) -> Result<Self, ProtocolError> {
        let id = Uuid::parse_str(value).map_err(|_| ProtocolError::InvalidEncoding)?;
        if id.to_string() != value {
            return Err(ProtocolError::InvalidEncoding);
        }
        Ok(Self(id))
    }
    pub fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }
    pub fn as_bytes(&self) -> &[u8; 16] {
        self.0.as_bytes()
    }
    pub fn into_uuid(self) -> Uuid {
        self.0
    }
}
impl fmt::Display for CanonicalUuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}
impl Serialize for CanonicalUuid {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}
impl<'de> Deserialize<'de> for CanonicalUuid {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(&String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Realm(String);
impl Realm {
    pub fn parse(value: &str) -> Result<Self, ProtocolError> {
        if value.is_empty()
            || value.len() > 64
            || !value
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
        {
            return Err(ProtocolError::InvalidEncoding);
        }
        Ok(Self(value.into()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl Serialize for Realm {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}
impl<'de> Deserialize<'de> for Realm {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(&String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeAddress {
    realm: Realm,
    agent: CanonicalUuid,
}
impl NodeAddress {
    pub fn new(realm: Realm, agent: CanonicalUuid) -> Self {
        Self { realm, agent }
    }
    pub fn parse(value: &str) -> Result<Self, ProtocolError> {
        let (realm, agent) = value
            .strip_prefix("khive1:")
            .and_then(|v| v.split_once('/'))
            .ok_or(ProtocolError::InvalidEncoding)?;
        Ok(Self::new(
            Realm::parse(realm)?,
            CanonicalUuid::parse(agent)?,
        ))
    }
    pub fn realm(&self) -> &Realm {
        &self.realm
    }
    pub fn agent(&self) -> CanonicalUuid {
        self.agent
    }
    pub fn require_realm(&self, realm: &Realm) -> Result<(), ProtocolError> {
        if &self.realm == realm {
            Ok(())
        } else {
            Err(ProtocolError::InvalidEncoding)
        }
    }
}
impl fmt::Display for NodeAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "khive1:{}/{}", self.realm.as_str(), self.agent)
    }
}
impl Serialize for NodeAddress {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}
impl<'de> Deserialize<'de> for NodeAddress {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(&String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

macro_rules! bounded_integer {
    ($name:ident, $min:expr, $max:expr) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub struct $name(u64);
        impl $name {
            pub fn new(value: u64) -> Result<Self, ProtocolError> {
                if !($min..=$max).contains(&value) {
                    return Err(ProtocolError::InvalidInteger);
                }
                Ok(Self(value))
            }
            pub fn get(self) -> u64 {
                self.0
            }
        }
        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_u64(self.0)
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                Self::new(u64::deserialize(deserializer)?).map_err(D::Error::custom)
            }
        }
    };
}
bounded_integer!(Epoch, 1, u32::MAX as u64);
bounded_integer!(JsonInteger, 0, MAX_JSON_INTEGER);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProtocolVersion;
impl ProtocolVersion {
    pub fn new(value: u64) -> Result<Self, ProtocolError> {
        if value != 1 {
            return Err(ProtocolError::UnsupportedVersion);
        }
        Ok(Self)
    }
    pub fn get(self) -> u64 {
        1
    }
}
impl Serialize for ProtocolVersion {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u32(1)
    }
}
impl<'de> Deserialize<'de> for ProtocolVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(u64::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}
bounded_integer!(PollWait, 0, 25);

pub fn decode_hex(value: &str) -> Result<Vec<u8>, ProtocolError> {
    if !value.len().is_multiple_of(2)
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(ProtocolError::InvalidEncoding);
    }
    hex::decode(value).map_err(|_| ProtocolError::InvalidEncoding)
}
pub fn decode_base64url(value: &str) -> Result<Vec<u8>, ProtocolError> {
    if !value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(ProtocolError::InvalidEncoding);
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| ProtocolError::InvalidEncoding)?;
    if URL_SAFE_NO_PAD.encode(&bytes) != value {
        return Err(ProtocolError::InvalidEncoding);
    }
    Ok(bytes)
}
pub fn encode_base64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HexBytes<const N: usize>([u8; N]);
impl<const N: usize> HexBytes<N> {
    pub fn new(bytes: [u8; N]) -> Self {
        Self(bytes)
    }
    pub fn parse(value: &str) -> Result<Self, ProtocolError> {
        Ok(Self(
            decode_hex(value)?
                .try_into()
                .map_err(|_| ProtocolError::InvalidEncoding)?,
        ))
    }
    pub fn as_bytes(&self) -> &[u8; N] {
        &self.0
    }
}
impl<const N: usize> Serialize for HexBytes<N> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&hex::encode(self.0))
    }
}
impl<'de, const N: usize> Deserialize<'de> for HexBytes<N> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(&String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Base64Bytes<const N: usize>([u8; N]);
impl<const N: usize> Base64Bytes<N> {
    pub fn new(bytes: [u8; N]) -> Self {
        Self(bytes)
    }
    pub fn parse(value: &str) -> Result<Self, ProtocolError> {
        Ok(Self(
            decode_base64url(value)?
                .try_into()
                .map_err(|_| ProtocolError::InvalidEncoding)?,
        ))
    }
    pub fn as_bytes(&self) -> &[u8; N] {
        &self.0
    }
}
impl<const N: usize> Serialize for Base64Bytes<N> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&encode_base64url(&self.0))
    }
}
impl<'de, const N: usize> Deserialize<'de> for Base64Bytes<N> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(&String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

pub fn context(label: &str) -> Result<Vec<u8>, ProtocolError> {
    if !label.is_ascii() || label.as_bytes().contains(&0) {
        return Err(ProtocolError::InvalidEncoding);
    }
    let mut result = b"khive-node-v1/".to_vec();
    result.extend_from_slice(label.as_bytes());
    result.push(0);
    Ok(result)
}
pub fn length_prefix(bytes: &[u8]) -> Result<Vec<u8>, ProtocolError> {
    let length = u16::try_from(bytes.len()).map_err(|_| ProtocolError::InvalidEncoding)?;
    let mut result = length.to_be_bytes().to_vec();
    result.extend_from_slice(bytes);
    Ok(result)
}
