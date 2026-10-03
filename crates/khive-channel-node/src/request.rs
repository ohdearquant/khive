//! Request authentication over byte-exact request targets and bodies (A.4).
use crate::encoding::{context, encode_base64url, length_prefix, CanonicalUuid, HexBytes};
use crate::keys::KeyFacility;
use crate::ProtocolError;
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_REQUEST_BODY_BYTES: usize = 98_304;

pub fn request_signing_input(
    device_id: CanonicalUuid,
    timestamp: u64,
    nonce: &[u8; 16],
    method: &str,
    path_and_query: &str,
    body: &[u8],
) -> Result<Vec<u8>, ProtocolError> {
    if !method
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
        || method.is_empty()
        || !path_and_query.starts_with('/')
        || !path_and_query.is_ascii()
        || path_and_query.bytes().any(|b| b <= 0x20 || b == 0x7f)
    {
        return Err(ProtocolError::InvalidEncoding);
    }
    if body.len() > MAX_REQUEST_BODY_BYTES {
        return Err(ProtocolError::EnvelopeTooLarge);
    }
    let mut input = context("request")?;
    input.extend_from_slice(device_id.as_bytes());
    input.extend_from_slice(&timestamp.to_be_bytes());
    input.extend_from_slice(nonce);
    input.extend_from_slice(&length_prefix(method.as_bytes())?);
    input.extend_from_slice(&length_prefix(path_and_query.as_bytes())?);
    input.extend_from_slice(&Sha256::digest(body));
    Ok(input)
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestHeaders {
    pub device: CanonicalUuid,
    pub timestamp: u64,
    pub nonce: HexBytes<16>,
    pub signature: crate::encoding::Base64Bytes<64>,
}
impl RequestHeaders {
    pub fn sign(
        facility: &dyn KeyFacility,
        device: CanonicalUuid,
        timestamp: u64,
        nonce: [u8; 16],
        method: &str,
        path_and_query: &str,
        body: &[u8],
    ) -> Result<Self, ProtocolError> {
        let input = request_signing_input(device, timestamp, &nonce, method, path_and_query, body)?;
        Ok(Self {
            device,
            timestamp,
            nonce: HexBytes::new(nonce),
            signature: crate::encoding::Base64Bytes::new(facility.sign(&input)),
        })
    }
    pub fn parse(
        device: &str,
        timestamp: &str,
        nonce: &str,
        signature: &str,
    ) -> Result<Self, ProtocolError> {
        let seconds = timestamp
            .parse::<u64>()
            .map_err(|_| ProtocolError::InvalidEncoding)?;
        if seconds.to_string() != timestamp {
            return Err(ProtocolError::InvalidEncoding);
        }
        Ok(Self {
            device: CanonicalUuid::parse(device)?,
            timestamp: seconds,
            nonce: HexBytes::parse(nonce)?,
            signature: crate::encoding::Base64Bytes::parse(signature)?,
        })
    }
    pub fn verify(
        &self,
        pinned_signing_key: &crate::keys::SigningPublicKey,
        method: &str,
        path_and_query: &str,
        body: &[u8],
    ) -> Result<(), ProtocolError> {
        pinned_signing_key.verify(
            &request_signing_input(
                self.device,
                self.timestamp,
                self.nonce.as_bytes(),
                method,
                path_and_query,
                body,
            )?,
            self.signature.as_bytes(),
        )
    }
    /// Exactly the four protocol headers. Transport code applies them verbatim.
    pub fn fields(&self) -> [(&'static str, String); 4] {
        [
            ("Khive-Device", self.device.to_string()),
            ("Khive-Timestamp", self.timestamp.to_string()),
            ("Khive-Nonce", hex::encode(self.nonce.as_bytes())),
            (
                "Khive-Signature",
                encode_base64url(self.signature.as_bytes()),
            ),
        ]
    }
}
pub trait Clock {
    fn unix_seconds(&self) -> Result<u64, ProtocolError>;
}
pub struct SystemClock;
impl Clock for SystemClock {
    fn unix_seconds(&self) -> Result<u64, ProtocolError> {
        // as_secs truncates fractional seconds; it never rounds ahead.
        Ok(SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ProtocolError::Clock)?
            .as_secs())
    }
}
pub fn sign_request(
    facility: &dyn KeyFacility,
    clock: &dyn Clock,
    device: CanonicalUuid,
    method: &str,
    path_and_query: &str,
    body: &[u8],
) -> Result<RequestHeaders, ProtocolError> {
    let timestamp = clock.unix_seconds()?;
    let mut nonce = [0u8; 16];
    OsRng
        .try_fill_bytes(&mut nonce)
        .map_err(|_| ProtocolError::Randomness)?;
    RequestHeaders::sign(
        facility,
        device,
        timestamp,
        nonce,
        method,
        path_and_query,
        body,
    )
}
