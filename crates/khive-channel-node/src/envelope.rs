//! Auth-mode HPKE envelope inputs (ADR-105 A.5).
use crate::encoding::{
    context, length_prefix, Base64Bytes, CanonicalUuid, Epoch, ProtocolVersion, Realm,
};
use crate::ProtocolError;
use serde::{de::Error as _, Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

pub const MAX_PLAINTEXT_BYTES: usize = 65_520;
pub const MAX_CIPHERTEXT_BYTES: usize = 65_536;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvelopeHeader {
    pub protocol_version: ProtocolVersion,
    pub realm: Realm,
    pub sender_agent_id: CanonicalUuid,
    pub sender_device_id: CanonicalUuid,
    pub sender_key_epoch: Epoch,
    pub recipient_agent_id: CanonicalUuid,
    pub recipient_device_id: CanonicalUuid,
    pub recipient_key_epoch: Epoch,
}
impl EnvelopeHeader {
    pub fn header(&self) -> Result<Vec<u8>, ProtocolError> {
        let mut bytes = context("envelope-header")?;
        bytes.extend_from_slice(&(self.protocol_version.get() as u32).to_be_bytes());
        bytes.extend_from_slice(&length_prefix(self.realm.as_str().as_bytes())?);
        bytes.extend_from_slice(self.sender_agent_id.as_bytes());
        bytes.extend_from_slice(self.sender_device_id.as_bytes());
        bytes.extend_from_slice(&self.sender_key_epoch.get().to_be_bytes());
        bytes.extend_from_slice(self.recipient_agent_id.as_bytes());
        bytes.extend_from_slice(self.recipient_device_id.as_bytes());
        bytes.extend_from_slice(&self.recipient_key_epoch.get().to_be_bytes());
        Ok(bytes)
    }
    pub fn info(&self) -> Result<Vec<u8>, ProtocolError> {
        let mut bytes = context("envelope")?;
        bytes.extend_from_slice(&Sha256::digest(self.header()?));
        Ok(bytes)
    }
}
pub fn aad(logical_message_id: CanonicalUuid) -> Result<Vec<u8>, ProtocolError> {
    let mut bytes = context("aad")?;
    bytes.extend_from_slice(logical_message_id.as_bytes());
    Ok(bytes)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ciphertext(Vec<u8>);
impl Ciphertext {
    pub fn new(bytes: Vec<u8>) -> Result<Self, ProtocolError> {
        if bytes.len() > MAX_CIPHERTEXT_BYTES {
            return Err(ProtocolError::EnvelopeTooLarge);
        }
        Ok(Self(bytes))
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}
impl Serialize for Ciphertext {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&crate::encoding::encode_base64url(&self.0))
    }
}
impl<'de> Deserialize<'de> for Ciphertext {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        if value.len() > MAX_CIPHERTEXT_BYTES.div_ceil(3) * 4 {
            return Err(D::Error::custom(ProtocolError::EnvelopeTooLarge));
        }
        Self::new(crate::encoding::decode_base64url(&value).map_err(D::Error::custom)?)
            .map_err(D::Error::custom)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SealedEnvelope {
    pub enc: Base64Bytes<32>,
    pub ciphertext: Ciphertext,
}
impl SealedEnvelope {
    pub fn digest(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(self.enc.as_bytes());
        hash.update(self.ciphertext.as_bytes());
        hash.finalize().into()
    }
}
