//! Strict A.6 JSON models. Canonical wrappers also constrain serialization.
use crate::encoding::{
    Base64Bytes, CanonicalUuid, Epoch, HexBytes, JsonInteger, NodeAddress, ProtocolVersion, Realm,
};
use crate::envelope::Ciphertext;
use crate::keys::{DevicePublicKeys, KemPublicKey, SigningPublicKey};
use crate::receipt::WireReceipt;
pub use crate::timestamp::{ServerTimestamp, UtcTimestamp};
use crate::ProtocolError;
use serde::{de::Error as _, Deserialize, Deserializer, Serialize, Serializer};

pub(crate) fn required_option<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d)
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
#[serde(try_from = "UncheckedSubmitRequest")]
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingState {
    Pending,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionResponse {
    pub state: PendingState,
    pub delivery_attempt_id: CanonicalUuid,
    pub admitted_at: ServerTimestamp,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "UncheckedDelivery")]
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
    pub recorded_at: ServerTimestamp,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PollResponse {
    pub deliveries: PollItems<Delivery, 16>,
    pub receipts: PollItems<ReceiptItem, 64>,
    pub receipts_cursor: JsonInteger,
    pub server_time: ServerTimestamp,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
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
    InvalidRequest,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefusalResponse {
    pub error: RefusalCode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<JsonInteger>,
}

/// A parsed item keeps its position even when the item's wire shape is invalid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PollItem<T> {
    pub index: usize,
    pub result: Result<T, PollItemFailure>,
}

/// An invalid item cannot yield a delivery or a receipt to act on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PollItemFailure {
    pub code: RefusalCode,
    raw: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ParsedPollItem<T> {
    Valid(T),
    Invalid(serde_json::Value),
}

/// Bounded poll items are parsed independently, without discarding their neighbours.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PollItems<T, const N: usize>(Vec<PollItem<T>>);
impl<T, const N: usize> PollItems<T, N> {
    pub fn new(values: Vec<T>) -> Result<Self, ProtocolError> {
        Ok(Self(
            BoundedList::<T, N>::new(values)?
                .into_vec()
                .into_iter()
                .enumerate()
                .map(|(index, value)| PollItem {
                    index,
                    result: Ok(value),
                })
                .collect(),
        ))
    }
    pub fn as_slice(&self) -> &[PollItem<T>] {
        &self.0
    }
    pub fn into_vec(self) -> Vec<PollItem<T>> {
        self.0
    }
}
impl<T: Serialize, const N: usize> Serialize for PollItems<T, N> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for item in &self.0 {
            match &item.result {
                Ok(value) => seq.serialize_element(value)?,
                Err(failure) => seq.serialize_element(&failure.raw)?,
            }
        }
        seq.end()
    }
}
impl<'de, T: Deserialize<'de>, const N: usize> Deserialize<'de> for PollItems<T, N> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let values = BoundedList::<ParsedPollItem<T>, N>::deserialize(deserializer)?.into_vec();
        Ok(Self(
            values
                .into_iter()
                .enumerate()
                .map(|(index, item)| PollItem {
                    index,
                    result: match item {
                        ParsedPollItem::Valid(value) => Ok(value),
                        ParsedPollItem::Invalid(raw) => Err(PollItemFailure {
                            code: RefusalCode::InvalidRequest,
                            raw,
                        }),
                    },
                })
                .collect(),
        ))
    }
}

/// Explicit JSON entry points retain typed protocol refusals instead of error text.
#[derive(Debug, thiserror::Error)]
pub enum WireDecodeError {
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error("invalid wire JSON")]
    Json(#[from] serde_json::Error),
}
impl WireDecodeError {
    pub fn refusal_code(&self) -> RefusalCode {
        match self {
            Self::Protocol(ProtocolError::UnsupportedVersion) => RefusalCode::UnsupportedVersion,
            Self::Protocol(ProtocolError::EnvelopeTooLarge) => RefusalCode::PayloadTooLarge,
            _ => RefusalCode::InvalidRequest,
        }
    }
}
fn checked_ciphertext(encoded: String) -> Result<Ciphertext, ProtocolError> {
    if encoded.len() > crate::envelope::MAX_CIPHERTEXT_BYTES.div_ceil(3) * 4 {
        return Err(ProtocolError::EnvelopeTooLarge);
    }
    Ciphertext::new(crate::encoding::decode_base64url(&encoded)?)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UncheckedSubmitRequest {
    protocol_version: JsonInteger,
    logical_message_id: CanonicalUuid,
    recipient: NodeAddress,
    recipient_device_id: CanonicalUuid,
    recipient_key_epoch: Epoch,
    sender_key_epoch: Epoch,
    contact_generation: Epoch,
    enc: Base64Bytes<32>,
    ciphertext: String,
}
impl TryFrom<UncheckedSubmitRequest> for SubmitRequest {
    type Error = ProtocolError;
    fn try_from(raw: UncheckedSubmitRequest) -> Result<Self, Self::Error> {
        let protocol_version = ProtocolVersion::new(raw.protocol_version.get())?;
        let ciphertext = checked_ciphertext(raw.ciphertext)?;
        Ok(Self {
            protocol_version,
            ciphertext,
            logical_message_id: raw.logical_message_id,
            recipient: raw.recipient,
            recipient_device_id: raw.recipient_device_id,
            recipient_key_epoch: raw.recipient_key_epoch,
            sender_key_epoch: raw.sender_key_epoch,
            contact_generation: raw.contact_generation,
            enc: raw.enc,
        })
    }
}
impl SubmitRequest {
    pub fn parse(bytes: &[u8]) -> Result<Self, WireDecodeError> {
        let raw: UncheckedSubmitRequest = serde_json::from_slice(bytes)?;
        Ok(Self::try_from(raw)?)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UncheckedDelivery {
    protocol_version: JsonInteger,
    delivery_attempt_id: CanonicalUuid,
    logical_message_id: CanonicalUuid,
    sender_agent_id: CanonicalUuid,
    sender_device_id: CanonicalUuid,
    sender_key_epoch: Epoch,
    recipient_agent_id: CanonicalUuid,
    recipient_device_id: CanonicalUuid,
    recipient_key_epoch: Epoch,
    contact_generation: Epoch,
    enc: Base64Bytes<32>,
    ciphertext: String,
}
impl TryFrom<UncheckedDelivery> for Delivery {
    type Error = ProtocolError;
    fn try_from(raw: UncheckedDelivery) -> Result<Self, Self::Error> {
        let protocol_version = ProtocolVersion::new(raw.protocol_version.get())?;
        let ciphertext = checked_ciphertext(raw.ciphertext)?;
        Ok(Self {
            protocol_version,
            ciphertext,
            delivery_attempt_id: raw.delivery_attempt_id,
            logical_message_id: raw.logical_message_id,
            sender_agent_id: raw.sender_agent_id,
            sender_device_id: raw.sender_device_id,
            sender_key_epoch: raw.sender_key_epoch,
            recipient_agent_id: raw.recipient_agent_id,
            recipient_device_id: raw.recipient_device_id,
            recipient_key_epoch: raw.recipient_key_epoch,
            contact_generation: raw.contact_generation,
            enc: raw.enc,
        })
    }
}
impl Delivery {
    pub fn parse(bytes: &[u8]) -> Result<Self, WireDecodeError> {
        let raw: UncheckedDelivery = serde_json::from_slice(bytes)?;
        Ok(Self::try_from(raw)?)
    }
}

impl<'de> Deserialize<'de> for PendingState {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match String::deserialize(d)?.as_str() {
            "pending" => Ok(Self::Pending),
            _ => Err(D::Error::custom("invalid pending state")),
        }
    }
}
impl<'de> Deserialize<'de> for MessageState {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match String::deserialize(d)?.as_str() {
            "pending" => Ok(Self::Pending),
            "recipient_stored" => Ok(Self::RecipientStored),
            "recipient_quarantined" => Ok(Self::RecipientQuarantined),
            "unknown" => Ok(Self::Unknown),
            _ => Err(D::Error::custom("invalid message state")),
        }
    }
}
impl<'de> Deserialize<'de> for RefusalCode {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(match String::deserialize(d)?.as_str() {
            "unsupported_version" => Self::UnsupportedVersion,
            "unauthenticated" => Self::Unauthenticated,
            "insufficient_credit" => Self::InsufficientCredit,
            "contact_not_active" => Self::ContactNotActive,
            "not_found" => Self::NotFound,
            "recipient_offline" => Self::RecipientOffline,
            "recipient_key_changed" => Self::RecipientKeyChanged,
            "envelope_conflict" => Self::EnvelopeConflict,
            "receipt_conflict" => Self::ReceiptConflict,
            "poll_in_progress" => Self::PollInProgress,
            "payload_too_large" => Self::PayloadTooLarge,
            "receipt_invalid" => Self::ReceiptInvalid,
            "rate_limited" => Self::RateLimited,
            "capacity_exhausted" => Self::CapacityExhausted,
            _ => Self::InvalidRequest,
        })
    }
}
