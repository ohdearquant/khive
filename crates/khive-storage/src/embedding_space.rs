//! Immutable embedding-space fences defined by ADR-160 D6.
//!
//! Protocol owners supply a fingerprint of their complete governed descriptor,
//! including the protocol identifier. This module validates the shared fields
//! and derives the physical key; it cannot verify the descriptor's preimage.

use std::fmt;
use std::num::NonZeroU32;

use khive_types::Hash32;

/// An opaque physical vector-space key, derived only by [`EmbeddingSpaceIdentity`].
///
/// No raw-string constructor or deserializer is provided: a key cannot be
/// supplied independently of the fingerprint and dimensions that determine it.
///
/// ```compile_fail
/// use khive_storage::EmbeddingSpaceKey;
/// let key = EmbeddingSpaceKey("caller_chosen_table".to_owned());
/// ```
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct EmbeddingSpaceKey(String);

impl EmbeddingSpaceKey {
    /// Borrow the canonical ASCII physical key.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for EmbeddingSpaceKey {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for EmbeddingSpaceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A validated immutable identity for one physical embedding space.
///
/// The key is exactly `{prefix}_{lowercase_hex(fingerprint)}_{dimensions}`.
/// Namespace and the display model label do not select the space. The protocol
/// identifier must already be governed by the fingerprint preimage; changing
/// that identifier while reusing a fingerprint leaves the key unchanged. No
/// extra domain string is hashed here, preserving existing descriptor goldens.
///
/// This is a value object, not a provider attestation or a runtime registration.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct EmbeddingSpaceIdentity {
    space_key: EmbeddingSpaceKey,
    protocol: String,
    fingerprint: [u8; 32],
    model_name: String,
    dimensions: NonZeroU32,
}

/// A shared-field validation failure while constructing an embedding identity.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum EmbeddingSpaceIdentityError {
    /// The prefix is empty or contains a byte outside ASCII alphanumeric/underscore.
    #[error("embedding space key prefix must be non-empty ASCII alphanumeric/underscore")]
    InvalidKeyPrefix,
    /// The protocol has an invalid byte or is outside its byte-length bound.
    #[error("embedding protocol must be 1..=128 bytes from [A-Za-z0-9._-]")]
    InvalidProtocol,
    /// The label is empty, too long, or has surrounding whitespace.
    #[error("embedding model name must be 1..=512 bytes with no surrounding whitespace")]
    InvalidModelName,
    /// The dimension is outside the portable bound.
    #[error("embedding dimensions must be in 1..=8192, got {dimensions}")]
    InvalidDimensions {
        /// The rejected dimension.
        dimensions: u32,
    },
    /// All component fields are valid but their derived physical key is too long.
    #[error("embedding space key must be at most 128 bytes, got {bytes}")]
    SpaceKeyTooLong {
        /// The derived key length in bytes.
        bytes: usize,
    },
}

impl EmbeddingSpaceIdentity {
    /// Validate the identity fields and derive its physical key.
    ///
    /// `fingerprint` is exactly 32 bytes by construction. `protocol` names the
    /// owner and canonicalization revision; only its shared lexical contract is
    /// checked here. Descriptor construction and hashing remain with that owner.
    ///
    /// # Errors
    ///
    /// Rejects invalid prefix, protocol, model label, dimensions, or derived key
    /// length, in that order. Model labels retain their exact UTF-8 bytes.
    pub fn new(
        key_prefix: &str,
        protocol: &str,
        fingerprint: [u8; 32],
        model_name: &str,
        dimensions: u32,
    ) -> Result<Self, EmbeddingSpaceIdentityError> {
        if key_prefix.is_empty()
            || !key_prefix
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(EmbeddingSpaceIdentityError::InvalidKeyPrefix);
        }
        if protocol.is_empty()
            || protocol.len() > 128
            || !protocol
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        {
            return Err(EmbeddingSpaceIdentityError::InvalidProtocol);
        }
        if model_name.trim().is_empty() || model_name.trim() != model_name || model_name.len() > 512
        {
            return Err(EmbeddingSpaceIdentityError::InvalidModelName);
        }
        if !(1..=8192).contains(&dimensions) {
            return Err(EmbeddingSpaceIdentityError::InvalidDimensions { dimensions });
        }
        let dimension_text = dimensions.to_string();
        let key_bytes = key_prefix.len() + 1 + 64 + 1 + dimension_text.len();
        if key_bytes > 128 {
            return Err(EmbeddingSpaceIdentityError::SpaceKeyTooLong { bytes: key_bytes });
        }
        let space_key = EmbeddingSpaceKey(format!(
            "{key_prefix}_{}_{dimension_text}",
            Hash32::from_bytes(fingerprint)
        ));
        Ok(Self {
            space_key,
            protocol: protocol.to_owned(),
            fingerprint,
            model_name: model_name.to_owned(),
            dimensions: NonZeroU32::new(dimensions).expect("validated nonzero dimensions"),
        })
    }

    /// The derived physical storage key.
    pub fn space_key(&self) -> &EmbeddingSpaceKey {
        &self.space_key
    }

    /// The exact protocol identifier supplied by its owner.
    pub fn protocol(&self) -> &str {
        &self.protocol
    }

    /// The complete owner-supplied descriptor fingerprint.
    pub fn fingerprint(&self) -> &[u8; 32] {
        &self.fingerprint
    }

    /// The exact display model label, which does not determine the physical key.
    pub fn model_name(&self) -> &str {
        &self.model_name
    }

    /// The validated nonzero vector dimension.
    pub fn dimensions(&self) -> NonZeroU32 {
        self.dimensions
    }
}
