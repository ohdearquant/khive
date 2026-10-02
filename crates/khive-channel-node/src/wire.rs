//! Strict A.6 JSON models. Canonical wrappers also constrain serialization.
use crate::encoding::{
    Base64Bytes, CanonicalUuid, Epoch, HexBytes, JsonInteger, NodeAddress, ProtocolVersion, Realm,
};
use crate::envelope::Ciphertext;
use crate::keys::{DevicePublicKeys, KemPublicKey, SigningPublicKey};
use crate::receipt::WireReceipt;
use crate::ProtocolError;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{de::Error as _, Deserialize, Deserializer, Serialize, Serializer};

pub(crate) fn required_option<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UtcTimestamp(DateTime<Utc>);
impl UtcTimestamp {
    pub fn parse(value: &str) -> Result<Self, ProtocolError> {
        let value =
            DateTime::parse_from_rfc3339(value).map_err(|_| ProtocolError::InvalidEncoding)?;
        if value.offset().local_minus_utc() != 0 {
            return Err(ProtocolError::InvalidEncoding);
        }
        Ok(Self(value.with_timezone(&Utc)))
    }
    pub fn from_utc(value: DateTime<Utc>) -> Self {
        Self(value)
    }
    pub fn as_utc(&self) -> DateTime<Utc> {
        self.0
    }
}
impl Serialize for UtcTimestamp {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0.to_rfc3339_opts(SecondsFormat::AutoSi, true))
    }
}
impl<'de> Deserialize<'de> for UtcTimestamp {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::parse(&String::deserialize(d)?).map_err(D::Error::custom)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundedList<T, const N: usize>(Vec<T>);
impl<T, const N: usize> BoundedList<T, N> {
    pub fn new(values: Vec<T>) -> Result<Self, ProtocolError> {
        if values.len() > N {
            return Err(ProtocolError::InvalidEncoding);
        }
        Ok(Self(values))
    }
    pub fn as_slice(&self) -> &[T] {
        &self.0
    }
    pub fn into_vec(self) -> Vec<T> {
        self.0
    }
}
impl<T: Serialize, const N: usize> Serialize for BoundedList<T, N> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}
impl<'de, T: Deserialize<'de>, const N: usize> Deserialize<'de> for BoundedList<T, N> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(Vec::<T>::deserialize(d)?).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContactResponse {
    pub agent_id: CanonicalUuid,
    pub address: NodeAddress,
    pub device_id: CanonicalUuid,
    pub key_epoch: Epoch,
    pub kem_public_key: KemPublicKey,
    pub signing_public_key: SigningPublicKey,
    pub fingerprint: HexBytes<32>,
    pub contact_generation: Epoch,
}
impl ContactResponse {
    /// Validate directory consistency. Pin confirmation remains the caller's operation.
    pub fn validated_keys(&self, realm: &Realm) -> Result<DevicePublicKeys, ProtocolError> {
        self.address.require_realm(realm)?;
        let keys = DevicePublicKeys {
            kem: self.kem_public_key.clone(),
            signing: self.signing_public_key.clone(),
        };
        if self.address.agent() != self.agent_id || keys.fingerprint() != self.fingerprint {
            return Err(ProtocolError::InvalidKey);
        }
        Ok(keys)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitRequest {
    pub protocol_version: ProtocolVersion,
    pub logical_message_id: CanonicalUuid,
    pub recipient: NodeAddress,
    pub recipient_device_id: CanonicalUuid,
    pub recipient_key_epoch: Epoch,
    pub sender_key_epoch: Epoch,
    pub contact_generation: Epoch,
    pub enc: Base64Bytes<32>,
    pub ciphertext: Ciphertext,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingState {
    Pending,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionResponse {
    pub state: PendingState,
    pub delivery_attempt_id: CanonicalUuid,
    pub admitted_at: UtcTimestamp,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Delivery {
    pub delivery_attempt_id: CanonicalUuid,
    pub logical_message_id: CanonicalUuid,
    pub protocol_version: ProtocolVersion,
    pub sender_agent_id: CanonicalUuid,
    pub sender_device_id: CanonicalUuid,
    pub sender_key_epoch: Epoch,
    pub recipient_agent_id: CanonicalUuid,
    pub recipient_device_id: CanonicalUuid,
    pub recipient_key_epoch: Epoch,
    pub contact_generation: Epoch,
    pub enc: Base64Bytes<32>,
    pub ciphertext: Ciphertext,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptItem {
    pub seq: JsonInteger,
    pub receipt: WireReceipt,
    pub recorded_at: UtcTimestamp,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PollResponse {
    pub deliveries: BoundedList<Delivery, 16>,
    pub receipts: BoundedList<ReceiptItem, 64>,
    pub receipts_cursor: JsonInteger,
    pub server_time: UtcTimestamp,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageState {
    Pending,
    RecipientStored,
    RecipientQuarantined,
    Unknown,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusResponse {
    pub logical_message_id: CanonicalUuid,
    pub state: MessageState,
    #[serde(deserialize_with = "required_option")]
    pub delivery_attempt_id: Option<CanonicalUuid>,
    #[serde(deserialize_with = "required_option")]
    pub receipt: Option<WireReceipt>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Recorded;
impl Serialize for Recorded {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bool(true)
    }
}
impl<'de> Deserialize<'de> for Recorded {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        if bool::deserialize(d)? {
            Ok(Self)
        } else {
            Err(D::Error::custom("receipt was not recorded"))
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcknowledgeResponse {
    pub recorded: Recorded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalCode {
    UnsupportedVersion,
    Unauthenticated,
    InsufficientCredit,
    ContactNotActive,
    NotFound,
    RecipientOffline,
    RecipientKeyChanged,
    EnvelopeConflict,
    ReceiptConflict,
    PollInProgress,
    PayloadTooLarge,
    ReceiptInvalid,
    RateLimited,
    CapacityExhausted,
    #[serde(other)]
    InvalidRequest,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefusalResponse {
    pub error: RefusalCode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<JsonInteger>,
}
