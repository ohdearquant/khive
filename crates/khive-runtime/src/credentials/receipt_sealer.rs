//! Runtime-owned ADR-144 receipt capability. This prerequisite has no issuance
//! call site yet; the v1 writer/reader and cutover policy remain unchanged.

use std::sync::Arc;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chacha20poly1305::{AeadInPlace, KeyInit, Tag, XChaCha20Poly1305, XNonce};
use rand::{rngs::OsRng, RngCore};
use thiserror::Error;
use zeroize::{Zeroize, Zeroizing};

use super::{CredentialKind, CredentialRegistry, VisibilityReceiptConfig};

const VERSION: u8 = 2;
const NONCE_BYTES: usize = 24;
const TAG_BYTES: usize = 16;
const MIN_PLAINTEXT_BYTES: usize = 12;
const MAX_ENVELOPE_BYTES: usize = 65_536;
const MAX_ENCODED_BYTES: usize = 87_382;
const PURPOSE: &[u8] = b"khive.memory.visibility";

/// Errors intentionally contain no provider error, key bytes or receipt fields.
#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum ReceiptSealError {
    #[error("visibility receipt key unavailable")]
    KeyUnavailable,
    #[error("invalid visibility receipt")]
    InvalidReceipt,
    #[error("visibility receipt nonce unavailable")]
    NonceUnavailable,
}

/// Authenticated fields are only exposed inside the runtime. The later recall
/// integration must apply effective namespace, model and age policy before use.
/// Deliberately lacks Debug, Display and Serialize.
pub(crate) struct ReceiptFields {
    pub(crate) namespace: String,
    pub(crate) issued_at: i64,
    pub(crate) fences: Vec<(String, u64)>,
}

impl Drop for ReceiptFields {
    fn drop(&mut self) {
        self.namespace.zeroize();
        self.issued_at.zeroize();
        for (model, sequence) in &mut self.fences {
            model.zeroize();
            sequence.zeroize();
        }
    }
}

/// Only the runtime can construct or use this capability. The key ring stores
/// references, never key material; providers are resolved at each operation.
/// Configuration must preserve immutable ID-to-key mappings across replicas.
pub(crate) struct ReceiptSealer {
    ring: VisibilityReceiptConfig,
    credentials: Arc<CredentialRegistry>,
}

impl ReceiptSealer {
    pub(crate) fn new(
        ring: VisibilityReceiptConfig,
        credentials: Arc<CredentialRegistry>,
    ) -> Result<Self, ReceiptSealError> {
        // Revalidate programmatic callers, independently of TOML startup checks.
        let mut ids = std::collections::BTreeSet::new();
        let mut encrypting = 0;
        for entry in &ring.keys {
            if !valid_key_id(entry.id.as_bytes())
                || !ids.insert(entry.id.as_str())
                || credentials.kind(&entry.credential).ok() != Some(CredentialKind::SigningKey)
            {
                return Err(ReceiptSealError::KeyUnavailable);
            }
            encrypting += usize::from(entry.encrypt);
        }
        if encrypting != 1 {
            return Err(ReceiptSealError::KeyUnavailable);
        }
        Ok(Self { ring, credentials })
    }

    pub(crate) fn seal(
        &self,
        namespace: &str,
        fences: &[(String, u64)],
    ) -> Result<String, ReceiptSealError> {
        self.seal_with_nonce_source(namespace, fences, |nonce| {
            OsRng
                .try_fill_bytes(nonce)
                .map_err(|_| ReceiptSealError::NonceUnavailable)
        })
    }

    fn seal_with_nonce_source(
        &self,
        namespace: &str,
        fences: &[(String, u64)],
        fill_nonce: impl FnOnce(&mut [u8; NONCE_BYTES]) -> Result<(), ReceiptSealError>,
    ) -> Result<String, ReceiptSealError> {
        let entry = self
            .ring
            .keys
            .iter()
            .find(|entry| entry.encrypt)
            .ok_or(ReceiptSealError::KeyUnavailable)?;
        let cipher = self.cipher(&entry.id)?;
        let mut nonce = [0; NONCE_BYTES];
        fill_nonce(&mut nonce)?;
        let mut plaintext = encode_plaintext(
            namespace,
            chrono::Utc::now().timestamp_millis(),
            fences,
            entry.id.len(),
        )?;
        let tag = cipher
            .encrypt_in_place_detached(
                XNonce::from_slice(&nonce),
                &associated_data(&entry.id),
                plaintext.as_mut_slice(),
            )
            .map_err(|_| ReceiptSealError::InvalidReceipt)?;
        let mut envelope =
            Vec::with_capacity(2 + entry.id.len() + NONCE_BYTES + plaintext.len() + TAG_BYTES);
        envelope.extend_from_slice(&[VERSION, entry.id.len() as u8]);
        envelope.extend_from_slice(entry.id.as_bytes());
        envelope.extend_from_slice(&nonce);
        envelope.extend_from_slice(&plaintext);
        envelope.extend_from_slice(&tag);
        Ok(URL_SAFE_NO_PAD.encode(envelope))
    }

    pub(crate) fn open(&self, token: &str) -> Result<ReceiptFields, ReceiptSealError> {
        // Both encoded and decoded bounds are checked before any key lookup.
        if token.len() > MAX_ENCODED_BYTES {
            return Err(ReceiptSealError::InvalidReceipt);
        }
        let envelope = URL_SAFE_NO_PAD
            .decode(token)
            .map_err(|_| ReceiptSealError::InvalidReceipt)?;
        if envelope.len() > MAX_ENVELOPE_BYTES
            || URL_SAFE_NO_PAD.encode(&envelope) != token
            || envelope.first() != Some(&VERSION)
        {
            return Err(ReceiptSealError::InvalidReceipt);
        }
        let key_len = usize::from(*envelope.get(1).ok_or(ReceiptSealError::InvalidReceipt)?);
        let header_end = 2 + key_len;
        let key_bytes = envelope
            .get(2..header_end)
            .ok_or(ReceiptSealError::InvalidReceipt)?;
        // The smallest valid record carries a one-byte namespace.
        if !valid_key_id(key_bytes)
            || envelope.len() < header_end + NONCE_BYTES + TAG_BYTES + MIN_PLAINTEXT_BYTES + 1
        {
            return Err(ReceiptSealError::InvalidReceipt);
        }
        let id = std::str::from_utf8(key_bytes).map_err(|_| ReceiptSealError::InvalidReceipt)?;
        let cipher = self.cipher(id)?;
        let nonce_end = header_end + NONCE_BYTES;
        let tag_start = envelope.len() - TAG_BYTES;
        let mut plaintext = Zeroizing::new(envelope[nonce_end..tag_start].to_vec());
        cipher
            .decrypt_in_place_detached(
                XNonce::from_slice(&envelope[header_end..nonce_end]),
                &associated_data(id),
                plaintext.as_mut_slice(),
                Tag::from_slice(&envelope[tag_start..]),
            )
            .map_err(|_| ReceiptSealError::InvalidReceipt)?;
        decode_plaintext(&plaintext)
    }

    fn cipher(&self, id: &str) -> Result<XChaCha20Poly1305, ReceiptSealError> {
        let entry = self
            .ring
            .keys
            .iter()
            .find(|entry| entry.id == id)
            .ok_or(ReceiptSealError::KeyUnavailable)?;
        let material = self
            .credentials
            .resolve(&entry.credential)
            .map_err(|_| ReceiptSealError::KeyUnavailable)?;
        // Canonical unpadded base64url of 32 bytes is exactly 43 bytes. Decoding
        // directly into a zeroizing fixed buffer avoids an unbounded key copy.
        if material.bytes.len() != 43 {
            return Err(ReceiptSealError::KeyUnavailable);
        }
        let mut key = Zeroizing::new([0_u8; 32]);
        let length = URL_SAFE_NO_PAD
            .decode_slice(&material.bytes, key.as_mut())
            .map_err(|_| ReceiptSealError::KeyUnavailable)?;
        if length != 32 {
            return Err(ReceiptSealError::KeyUnavailable);
        }
        XChaCha20Poly1305::new_from_slice(key.as_ref())
            .map_err(|_| ReceiptSealError::KeyUnavailable)
    }
}

fn valid_key_id(id: &[u8]) -> bool {
    (1..=64).contains(&id.len())
        && id
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(byte))
}

fn associated_data(id: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(PURPOSE.len() + 2 + id.len());
    bytes.extend_from_slice(PURPOSE);
    bytes.extend_from_slice(&[VERSION, id.len() as u8]);
    bytes.extend_from_slice(id.as_bytes());
    bytes
}

fn encode_plaintext(
    namespace: &str,
    issued_at: i64,
    fences: &[(String, u64)],
    key_id_len: usize,
) -> Result<Zeroizing<Vec<u8>>, ReceiptSealError> {
    let namespace_len =
        u16::try_from(namespace.len()).map_err(|_| ReceiptSealError::InvalidReceipt)?;
    let count = u16::try_from(fences.len()).map_err(|_| ReceiptSealError::InvalidReceipt)?;
    let mut size = MIN_PLAINTEXT_BYTES + namespace.len();
    for (model, sequence) in fences {
        if model.is_empty() || model.len() > usize::from(u16::MAX) || *sequence == 0 {
            return Err(ReceiptSealError::InvalidReceipt);
        }
        size = size
            .checked_add(10 + model.len())
            .ok_or(ReceiptSealError::InvalidReceipt)?;
    }
    if namespace.is_empty() || size > MAX_ENVELOPE_BYTES - 2 - key_id_len - NONCE_BYTES - TAG_BYTES
    {
        return Err(ReceiptSealError::InvalidReceipt);
    }
    let mut sorted: Vec<_> = fences.iter().collect();
    sorted.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    if sorted.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(ReceiptSealError::InvalidReceipt);
    }
    // Allocate once at the fully checked size so growth cannot leave plaintext
    // behind in a freed, non-zeroized allocation.
    let mut bytes = Zeroizing::new(Vec::with_capacity(size));
    bytes.extend_from_slice(&namespace_len.to_be_bytes());
    bytes.extend_from_slice(namespace.as_bytes());
    bytes.extend_from_slice(&issued_at.to_be_bytes());
    bytes.extend_from_slice(&count.to_be_bytes());
    for (model, sequence) in sorted {
        bytes.extend_from_slice(&(model.len() as u16).to_be_bytes());
        bytes.extend_from_slice(model.as_bytes());
        bytes.extend_from_slice(&sequence.to_be_bytes());
    }
    Ok(bytes)
}

fn decode_plaintext(bytes: &[u8]) -> Result<ReceiptFields, ReceiptSealError> {
    let mut cursor = PlaintextCursor { bytes, offset: 0 };
    let mut receipt = ReceiptFields {
        namespace: String::new(),
        issued_at: 0,
        fences: Vec::new(),
    };
    receipt.namespace = cursor.string()?;
    receipt.issued_at = i64::from_be_bytes(
        cursor
            .take(8)?
            .try_into()
            .map_err(|_| ReceiptSealError::InvalidReceipt)?,
    );
    let count = usize::from(cursor.u16()?);
    // Even empty model names need ten framing bytes. Refuse impossible counts
    // before reserving space; the decoder additionally refuses empty names.
    if count > (bytes.len() - cursor.offset) / 10 {
        return Err(ReceiptSealError::InvalidReceipt);
    }
    receipt.fences.reserve_exact(count);
    for _ in 0..count {
        let mut model = cursor.string()?;
        let sequence = match cursor.take(8) {
            Ok(bytes) => u64::from_be_bytes(
                bytes
                    .try_into()
                    .map_err(|_| ReceiptSealError::InvalidReceipt)?,
            ),
            Err(error) => {
                model.zeroize();
                return Err(error);
            }
        };
        if sequence == 0
            || receipt
                .fences
                .last()
                .is_some_and(|(previous, _)| previous >= &model)
        {
            model.zeroize();
            return Err(ReceiptSealError::InvalidReceipt);
        }
        receipt.fences.push((model, sequence));
    }
    if cursor.offset != bytes.len() {
        return Err(ReceiptSealError::InvalidReceipt);
    }
    Ok(receipt)
}

struct PlaintextCursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> PlaintextCursor<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8], ReceiptSealError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or(ReceiptSealError::InvalidReceipt)?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or(ReceiptSealError::InvalidReceipt)?;
        self.offset = end;
        Ok(value)
    }

    fn u16(&mut self) -> Result<u16, ReceiptSealError> {
        Ok(u16::from_be_bytes(
            self.take(2)?
                .try_into()
                .map_err(|_| ReceiptSealError::InvalidReceipt)?,
        ))
    }

    fn string(&mut self) -> Result<String, ReceiptSealError> {
        let length = usize::from(self.u16()?);
        let value = std::str::from_utf8(self.take(length)?)
            .map_err(|_| ReceiptSealError::InvalidReceipt)?;
        if value.is_empty() {
            return Err(ReceiptSealError::InvalidReceipt);
        }
        Ok(value.to_owned())
    }
}

#[cfg(test)]
#[path = "receipt_sealer_tests.rs"]
mod tests;
