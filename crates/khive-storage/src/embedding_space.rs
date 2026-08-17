//! Backend-neutral identity for one immutable physical embedding space.

use std::fmt;
use std::num::NonZeroU32;

const MAX_SPACE_KEY_BYTES: usize = 128;
const MAX_PROTOCOL_BYTES: usize = 128;
const MAX_MODEL_NAME_BYTES: usize = 512;
const MAX_DIMENSIONS: u32 = 8192;

/// Validation failure while constructing an [`EmbeddingSpaceIdentity`].
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum EmbeddingSpaceIdentityError {
    /// The caller-supplied prefix is empty or contains a non-key character.
    #[error("embedding space key prefix must be non-empty ASCII alphanumeric/underscore")]
    InvalidKeyPrefix,
    /// The owner protocol is empty, overlong, or contains a disallowed byte.
    #[error("embedding protocol must be 1..=128 bytes from [A-Za-z0-9._-]")]
    InvalidProtocol,
    /// The display label is empty, overlong, or has surrounding whitespace.
    #[error("embedding model name must be 1..=512 bytes with no surrounding whitespace")]
    InvalidModelName,
    /// The vector geometry is outside the portable supported range.
    #[error("embedding dimensions must be in 1..=8192, got {dimensions}")]
    InvalidDimensions { dimensions: u32 },
    /// The derived physical key exceeds the portable key limit.
    #[error("derived embedding space key must be at most {max_bytes} bytes, got {actual_bytes}")]
    DerivedKeyTooLong {
        actual_bytes: usize,
        max_bytes: usize,
    },
}

/// Validated physical key derived from an embedding fingerprint and geometry.
///
/// There is intentionally no public constructor: callers construct a complete
/// [`EmbeddingSpaceIdentity`], which derives this key from the fingerprint and
/// dimensions instead of accepting an independently supplied table name.
///
/// ```compile_fail
/// use khive_storage::EmbeddingSpaceKey;
///
/// let _unchecked = EmbeddingSpaceKey("caller_selected_table".to_string());
/// ```
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EmbeddingSpaceKey(String);

impl EmbeddingSpaceKey {
    /// Borrow the canonical ASCII key.
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
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Validated owner and canonicalization revision for an embedding identity.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EmbeddingProtocol(String);

impl EmbeddingProtocol {
    /// Borrow the governed protocol identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for EmbeddingProtocol {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for EmbeddingProtocol {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Immutable, complete fence for one physical vector space.
///
/// The protocol owner decides which vector-affecting fields form the supplied
/// fingerprint and golden-tests that preimage. This shared type validates the
/// closed envelope and derives the physical key; it does not infer or
/// canonicalize model-specific identity fields.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct EmbeddingSpaceIdentity {
    space_key: EmbeddingSpaceKey,
    protocol: EmbeddingProtocol,
    fingerprint: [u8; 32],
    model_name: String,
    dimensions: NonZeroU32,
}

impl EmbeddingSpaceIdentity {
    /// Validate a complete identity and derive its physical space key.
    ///
    /// `key_prefix` must be non-empty ASCII alphanumeric/underscore. The
    /// derived key is `{key_prefix}_{lowercase_hex(fingerprint)}_{dimensions}`
    /// and may contain at most 128 bytes. `protocol` identifies the owner and
    /// canonicalization revision and is restricted to 1..=128 bytes from
    /// `[A-Za-z0-9._-]`. `model_name` is a display label, not a storage key.
    pub fn new(
        key_prefix: &str,
        protocol: &str,
        fingerprint: [u8; 32],
        model_name: &str,
        dimensions: u32,
    ) -> Result<Self, EmbeddingSpaceIdentityError> {
        if key_prefix.is_empty() {
            return Err(EmbeddingSpaceIdentityError::InvalidKeyPrefix);
        }

        let derived_key_bytes = key_prefix
            .len()
            .saturating_add(1 + 64 + 1 + decimal_digits(dimensions));
        if derived_key_bytes > MAX_SPACE_KEY_BYTES {
            return Err(EmbeddingSpaceIdentityError::DerivedKeyTooLong {
                actual_bytes: derived_key_bytes,
                max_bytes: MAX_SPACE_KEY_BYTES,
            });
        }
        if !key_prefix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return Err(EmbeddingSpaceIdentityError::InvalidKeyPrefix);
        }

        if protocol.is_empty()
            || protocol.len() > MAX_PROTOCOL_BYTES
            || !protocol
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(EmbeddingSpaceIdentityError::InvalidProtocol);
        }

        if model_name.is_empty()
            || model_name.len() > MAX_MODEL_NAME_BYTES
            || model_name.trim() != model_name
        {
            return Err(EmbeddingSpaceIdentityError::InvalidModelName);
        }

        let dimensions = NonZeroU32::new(dimensions)
            .filter(|value| value.get() <= MAX_DIMENSIONS)
            .ok_or(EmbeddingSpaceIdentityError::InvalidDimensions { dimensions })?;

        let space_key = format!(
            "{key_prefix}_{}_{}",
            lowercase_hex(&fingerprint),
            dimensions.get()
        );
        debug_assert_eq!(space_key.len(), derived_key_bytes);

        Ok(Self {
            space_key: EmbeddingSpaceKey(space_key),
            protocol: EmbeddingProtocol(protocol.to_string()),
            fingerprint,
            model_name: model_name.to_string(),
            dimensions,
        })
    }

    /// Return the complete derived physical key.
    pub fn space_key(&self) -> &EmbeddingSpaceKey {
        &self.space_key
    }

    /// Return the governed identity protocol.
    pub fn protocol(&self) -> &EmbeddingProtocol {
        &self.protocol
    }

    /// Return the owner's canonical 32-byte fingerprint.
    pub fn fingerprint(&self) -> &[u8; 32] {
        &self.fingerprint
    }

    /// Return the display model label.
    pub fn model_name(&self) -> &str {
        &self.model_name
    }

    /// Return the validated vector dimensions.
    pub fn dimensions(&self) -> NonZeroU32 {
        self.dimensions
    }
}

fn decimal_digits(value: u32) -> usize {
    match value {
        0..=9 => 1,
        10..=99 => 2,
        100..=999 => 3,
        1_000..=9_999 => 4,
        10_000..=99_999 => 5,
        100_000..=999_999 => 6,
        1_000_000..=9_999_999 => 7,
        10_000_000..=99_999_999 => 8,
        100_000_000..=999_999_999 => 9,
        _ => 10,
    }
}

fn lowercase_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}
