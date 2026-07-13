//! Blob storage capability — content-addressed binary object CRUD.
//!
//! `BlobStore` is the trait family added by khive#292: bytes that do not
//! belong inside the primary SQLite database (source PDFs, images, large
//! opaque payloads) are stored by a dedicated backend and referenced from
//! the graph by an opaque [`ContentRef`]. Per ADR-005's "zero
//! implementations" constraint, this module defines the contract only — the
//! first backend (filesystem, BLAKE3-addressed) lives in `khive-db`.

use std::collections::HashSet;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::capability::StorageCapability;
use crate::error::StorageError;
use crate::types::StorageResult;

/// Number of hex characters in a BLAKE3-256 digest (32 bytes -> 64 hex chars).
const CONTENT_REF_HEX_LEN: usize = 64;

/// An opaque, content-addressed reference to a stored blob.
///
/// Backed by a lowercase-hex BLAKE3 digest of the blob's bytes: identical
/// content always produces the same `ContentRef`, so storing the same bytes
/// twice is a no-op after the first write. Callers must treat the value as
/// opaque — the backend, not the caller, decides how a `ContentRef` maps to
/// physical storage (a filesystem path, an object-store key, etc.).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContentRef(String);

impl ContentRef {
    /// Parse a `ContentRef` from a caller-supplied hex string.
    ///
    /// Rejects anything that is not exactly 64 lowercase hex characters.
    /// Rejecting uppercase (rather than lowercase-normalizing) keeps a
    /// single canonical string form per digest, since the value doubles as
    /// a filesystem path component in the shipped filesystem backend —
    /// accepting both cases would let two `ContentRef` values that compare
    /// unequal as `String`s resolve to the same bytes.
    pub fn from_hex(hex: impl Into<String>) -> Result<Self, String> {
        let hex = hex.into();
        if hex.len() != CONTENT_REF_HEX_LEN {
            return Err(format!(
                "content_ref must be {CONTENT_REF_HEX_LEN} hex characters, got length {} ({hex:?})",
                hex.len()
            ));
        }
        if !hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b.is_ascii_lowercase() && b.is_ascii_hexdigit()))
        {
            return Err(format!(
                "content_ref must be lowercase hex (0-9, a-f), got {hex:?}"
            ));
        }
        Ok(Self(hex))
    }

    /// Construct a `ContentRef` directly from a BLAKE3 digest's raw bytes.
    pub fn from_digest_bytes(digest: &[u8; 32]) -> Self {
        Self(hex_encode(digest))
    }

    /// Borrow the underlying hex string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ContentRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for ContentRef {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Configuration for [`BlobStore::orphan_sweep`].
///
/// `BlobStore` has no visibility into SQL substrates (ADR-005 constraint 4:
/// a trait instance talks to exactly one backend), so it cannot itself
/// discover which content refs are still referenced by, e.g., the
/// `entities.content_ref` column. The caller assembles that set and passes
/// it in — the blob backend then owns the actual comparison and deletion,
/// per the operating rule that `BlobStore` is the *only* deletion path
/// besides an explicit [`BlobStore::delete`] (no consumer deletes blobs
/// directly).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BlobOrphanSweepConfig {
    /// Content refs currently referenced by at least one live row somewhere
    /// in the system. Anything this backend stores that is NOT in this set
    /// is orphaned.
    pub live_refs: HashSet<ContentRef>,
    /// When `true`, report what would be deleted without deleting anything.
    pub dry_run: bool,
}

/// Result of a [`BlobStore::orphan_sweep`] call.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BlobOrphanSweepResult {
    /// Total objects examined in this backend.
    pub scanned: u64,
    /// Objects actually deleted (always 0 when `dry_run = true`).
    pub deleted: u64,
    /// Objects that are orphaned (would be deleted whether or not `dry_run`
    /// is set — populated in both modes so a dry run reports the same count
    /// a real run would delete).
    pub would_delete: u64,
}

/// Content-addressed binary object CRUD.
///
/// Every method is backend-agnostic: the filesystem backend
/// (`khive-db::stores::blob::FsBlobStore`) is the first implementation, and
/// any future backend (object storage, a different CAS layout) implements
/// the same operations. Per ADR-005 constraint 4, a `BlobStore` instance
/// talks to exactly one backend.
#[async_trait]
pub trait BlobStore: Send + Sync + 'static {
    /// Store `bytes`, returning the content-addressed reference under which
    /// they are now retrievable. Storing byte-identical content more than
    /// once returns the same `ContentRef` and does not re-write the object.
    async fn put(&self, bytes: Vec<u8>) -> StorageResult<ContentRef>;

    /// Fetch the bytes stored under `content_ref`.
    ///
    /// Returns `StorageError::NotFound` (capability `Blob`) if no object
    /// exists for this reference.
    async fn get(&self, content_ref: &ContentRef) -> StorageResult<Vec<u8>>;

    /// Whether an object currently exists for `content_ref`.
    async fn exists(&self, content_ref: &ContentRef) -> StorageResult<bool>;

    /// Remove the object stored under `content_ref`.
    ///
    /// Returns `true` when an object was actually removed, `false` when
    /// none existed — deleting an absent object is not an error.
    async fn delete(&self, content_ref: &ContentRef) -> StorageResult<bool>;

    /// Enumerate every object this backend holds and delete (or, in
    /// `dry_run` mode, report) those absent from `config.live_refs`.
    ///
    /// This is the operator-side GC path (khive#292 deliverable 5) — an
    /// admin-side operation, not an MCP verb, mirroring
    /// `VectorStore::orphan_sweep`'s CLI-only precedent (ADR-044). Default
    /// returns `StorageError::Unsupported`; the filesystem backend
    /// overrides it with a real directory walk. No silent no-op.
    async fn orphan_sweep(
        &self,
        config: &BlobOrphanSweepConfig,
    ) -> StorageResult<BlobOrphanSweepResult> {
        let _ = config;
        Err(StorageError::Unsupported {
            capability: StorageCapability::Blob,
            operation: "orphan_sweep".into(),
            message: "this backend does not support orphan sweep".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_hex_accepts_valid_lowercase_digest() {
        let hex = "a".repeat(64);
        let cref = ContentRef::from_hex(hex.clone()).unwrap();
        assert_eq!(cref.as_str(), hex);
        assert_eq!(cref.to_string(), hex);
    }

    #[test]
    fn from_hex_rejects_short_string() {
        let err = ContentRef::from_hex("abc").unwrap_err();
        assert!(
            err.contains("64"),
            "error must mention expected length: {err}"
        );
    }

    #[test]
    fn from_hex_rejects_long_string() {
        let err = ContentRef::from_hex("a".repeat(65)).unwrap_err();
        assert!(
            err.contains("64"),
            "error must mention expected length: {err}"
        );
    }

    #[test]
    fn from_hex_rejects_uppercase() {
        let err = ContentRef::from_hex("A".repeat(64)).unwrap_err();
        assert!(
            err.contains("lowercase"),
            "error must mention lowercase requirement: {err}"
        );
    }

    #[test]
    fn from_hex_rejects_non_hex_characters() {
        let mut hex = "a".repeat(63);
        hex.push('z');
        let err = ContentRef::from_hex(hex).unwrap_err();
        assert!(
            err.contains("lowercase hex"),
            "error must mention hex requirement: {err}"
        );
    }

    #[test]
    fn from_digest_bytes_matches_known_blake3_hash() {
        // BLAKE3("") -> af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262
        let hash = blake3_hash_of_empty();
        let cref = ContentRef::from_digest_bytes(&hash);
        assert_eq!(
            cref.as_str(),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }

    // khive-storage has zero heavy dependencies (ADR-005) so this test hand-rolls
    // the one known BLAKE3("") vector instead of pulling in the `blake3` crate
    // just to exercise `hex_encode`.
    fn blake3_hash_of_empty() -> [u8; 32] {
        let hex = "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";
        let mut out = [0u8; 32];
        for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
            let s = std::str::from_utf8(chunk).unwrap();
            out[i] = u8::from_str_radix(s, 16).unwrap();
        }
        out
    }

    #[test]
    fn content_ref_equality_and_hash_are_string_based() {
        let a = ContentRef::from_hex("b".repeat(64)).unwrap();
        let b = ContentRef::from_hex("b".repeat(64)).unwrap();
        let c = ContentRef::from_hex("c".repeat(64)).unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);

        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(a.clone());
        assert!(set.contains(&b));
        assert!(!set.contains(&c));
    }
}
