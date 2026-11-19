//! Standalone raw-value codec; mirror read and write paths do not use it yet.
//!
//! Legacy TEXT is unconditional plaintext. BLOB prefix 0x01 carries a level-3
//! zstd frame without a dictionary; other prefixes remain unsupported.

use std::borrow::Cow;
use std::fmt;

use khive_storage::SqlValue;

/// A format, compression or UTF-8 refusal without the stored raw content.
#[derive(Debug)]
pub enum RawCodecError {
    UnsupportedValue(&'static str),
    EmptyBlob,
    UnsupportedPrefix(u8),
    Compression(std::io::Error),
    Decompression(std::io::Error),
    InvalidUtf8(std::str::Utf8Error),
}

impl fmt::Display for RawCodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedValue(kind) => {
                write!(f, "session raw codec requires TEXT or BLOB, got {kind}")
            }
            Self::EmptyBlob => f.write_str("session raw BLOB has no format prefix"),
            Self::UnsupportedPrefix(prefix) => {
                write!(f, "unsupported session raw BLOB prefix 0x{prefix:02x}")
            }
            Self::Compression(error) => write!(f, "session raw compression failed: {error}"),
            Self::Decompression(error) => write!(f, "session raw decompression failed: {error}"),
            Self::InvalidUtf8(error) => write!(f, "decoded session raw is not UTF-8: {error}"),
        }
    }
}

impl std::error::Error for RawCodecError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Compression(error) | Self::Decompression(error) => Some(error),
            Self::InvalidUtf8(error) => Some(error),
            _ => None,
        }
    }
}

/// Encode plaintext as a version-prefixed BLOB, including an empty plaintext.
///
/// Compression failures are returned; no TEXT or uncompressed fallback is used.
pub fn encode_raw(raw: &str) -> Result<SqlValue, RawCodecError> {
    let frame = zstd::bulk::compress(raw.as_bytes(), 3).map_err(RawCodecError::Compression)?;
    let mut blob = Vec::with_capacity(frame.len() + 1);
    blob.push(0x01);
    blob.extend_from_slice(&frame);
    Ok(SqlValue::Blob(blob))
}

/// Decode a stored raw value with a caller-selected BLOB output-size bound.
///
/// TEXT is borrowed byte-for-byte, even when the BLOB bound is zero. The bound
/// limits decompressed bytes only; it does not set a storage or operator policy.
/// Invalid prefixes, frames, excessive output and invalid UTF-8 return errors.
pub fn decode_raw(
    value: &SqlValue,
    max_decoded_blob_bytes: usize,
) -> Result<Cow<'_, str>, RawCodecError> {
    match value {
        SqlValue::Text(text) => Ok(Cow::Borrowed(text)),
        SqlValue::Blob(blob) => {
            let (&prefix, frame) = blob.split_first().ok_or(RawCodecError::EmptyBlob)?;
            if prefix != 0x01 {
                return Err(RawCodecError::UnsupportedPrefix(prefix));
            }
            // zstd accepts an empty byte slice as zero output, but this format
            // requires a real frame even when the original plaintext is empty.
            if frame.is_empty() {
                return Err(RawCodecError::Decompression(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "session raw zstd frame is empty",
                )));
            }
            let bytes = zstd::bulk::decompress(frame, max_decoded_blob_bytes)
                .map_err(RawCodecError::Decompression)?;
            let text = String::from_utf8(bytes)
                .map_err(|error| RawCodecError::InvalidUtf8(error.utf8_error()))?;
            Ok(Cow::Owned(text))
        }
        SqlValue::Null => Err(RawCodecError::UnsupportedValue("Null")),
        SqlValue::Bool(_) => Err(RawCodecError::UnsupportedValue("Bool")),
        SqlValue::Integer(_) => Err(RawCodecError::UnsupportedValue("Integer")),
        SqlValue::Float(_) => Err(RawCodecError::UnsupportedValue("Float")),
        SqlValue::Json(_) => Err(RawCodecError::UnsupportedValue("Json")),
        SqlValue::Uuid(_) => Err(RawCodecError::UnsupportedValue("Uuid")),
        SqlValue::Timestamp(_) => Err(RawCodecError::UnsupportedValue("Timestamp")),
    }
}
