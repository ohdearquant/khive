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
use crate::sql::SqlAccess;
use crate::types::StorageResult;

/// Number of hex characters in a BLAKE3-256 digest (32 bytes -> 64 hex chars).
const CONTENT_REF_HEX_LEN: usize = 64;

/// An opaque, content-addressed reference to a stored blob.
///
/// Backed by a lowercase-hex BLAKE3 digest of the blob's bytes: identical
/// content always produces the same `ContentRef`, so storing the same bytes
/// twice is a no-op after the first write. Callers must treat the value as
/// opaque — the backend, not the caller, decides how a `ContentRef` maps to
/// physical storage.
///
/// `Deserialize` is hand-written (below) to reject any string that is not 64
/// lowercase hex characters — a naive derive would let an unvalidated value
/// panic later in `shard_path`'s slicing.
/// See `crates/khive-storage/docs/api/blob-store.md` for the full rationale.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ContentRef(String);

impl<'de> Deserialize<'de> for ContentRef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        ContentRef::from_hex(raw).map_err(serde::de::Error::custom)
    }
}

impl ContentRef {
    /// Parse a `ContentRef` from a caller-supplied hex string.
    ///
    /// Rejects anything that is not exactly 64 lowercase hex characters.
    /// Uppercase is rejected (not normalized) to keep one canonical string
    /// form per digest — see `docs/api/blob-store.md`.
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
/// `live_refs` is a point-in-time snapshot the caller assembles (this trait
/// has no visibility into SQL substrates — ADR-005 constraint 4), not a live
/// query. See [`BlobStore::orphan_sweep`] for the concurrency hazard this
/// implies, and `crates/khive-storage/docs/api/blob-store.md` for the full
/// rationale.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BlobOrphanSweepConfig {
    /// Content refs currently referenced by at least one live row somewhere
    /// in the system, as of when the caller assembled this set. Anything
    /// this backend stores that is NOT in this set is treated as orphaned
    /// and deleted (or reported, in `dry_run` mode) — including a
    /// `content_ref` that becomes live after this snapshot was taken.
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
// `Debug` is a supertrait so boot-path tests can distinguish which concrete
// backend was installed behind `Arc<dyn BlobStore>` via `format!("{:?}", ..)`
// without adding a downcast/type-name method to the production surface.
#[async_trait]
pub trait BlobStore: Send + Sync + std::fmt::Debug + 'static {
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

    /// The size in bytes of the object stored under `content_ref`, without
    /// hydrating its bytes.
    ///
    /// Returns `Ok(None)` when no object exists for this reference — this is
    /// the existence check and the size read in one call, so a caller never
    /// pays for a full read just to answer "does this exist and how big is
    /// it". On the filesystem backend this maps to a file metadata stat; on
    /// an object-storage backend it maps to a `HEAD Object` request.
    async fn size(&self, content_ref: &ContentRef) -> StorageResult<Option<u64>>;

    /// Remove the object stored under `content_ref`.
    ///
    /// Returns `true` when an object was actually removed, `false` when
    /// none existed — deleting an absent object is not an error.
    ///
    /// # Safety / concurrency hazard (ADR-111 §8, amended)
    ///
    /// Unconditional physical removal with **no coordination against any
    /// entity that might reference `content_ref`**. Safe to call only when
    /// the caller has independently quiesced whatever writer could attach a
    /// new `content_ref` to an entity for the duration of the call — this
    /// trait does not detect or prevent a race. Offline-maintenance-only.
    /// See `crates/khive-storage/docs/api/blob-store.md`.
    async fn delete(&self, content_ref: &ContentRef) -> StorageResult<bool>;

    /// Enumerate every object this backend holds and delete (or, in
    /// `dry_run` mode, report) those absent from `config.live_refs`.
    /// Operator-side GC path (khive#292 deliverable 5) — admin-only, not an
    /// MCP verb. Default returns `StorageError::Unsupported`; the filesystem
    /// backend overrides it with a real directory walk.
    ///
    /// # Safety / concurrency hazard (ADR-111 §8, amended)
    ///
    /// `config.live_refs` is a **snapshot**; a `content_ref` that becomes
    /// newly live between the snapshot and the sweep is deleted anyway.
    /// **Callers MUST quiesce entity writes** for the duration of
    /// snapshot-plus-sweep. See `crates/khive-storage/docs/api/blob-store.md`
    /// for the race repro. Concurrent callers must use
    /// [`Self::transactional_orphan_sweep`] instead.
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

    /// Select live entity references and sweep orphaned blobs in one database
    /// writer transaction.
    ///
    /// Unlike [`Self::orphan_sweep`], this operation obtains liveness itself
    /// from `sql`; callers do not assemble a stale snapshot. `sql` must be the
    /// same database capability used for the entity writes that own these
    /// references. Implementations must also ensure an object published after
    /// the sweep's candidate set is captured cannot be mistaken for an orphan,
    /// including when it is published between selecting live references and
    /// physical deletion.
    /// Coordination may be advisory, so callers must publish through the
    /// backend rather than mutate its physical storage directly.
    /// Backends that cannot provide both guarantees return
    /// `StorageError::Unsupported`.
    async fn transactional_orphan_sweep(
        &self,
        sql: &dyn SqlAccess,
        dry_run: bool,
    ) -> StorageResult<BlobOrphanSweepResult> {
        let _ = (sql, dry_run);
        Err(StorageError::Unsupported {
            capability: StorageCapability::Blob,
            operation: "transactional_orphan_sweep".into(),
            message: "this backend does not support a database-coordinated orphan sweep".into(),
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

    // hand-rolled BLAKE3("") vector (see docs/api/blob-store.md)
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
    fn deserialize_accepts_a_valid_lowercase_digest() {
        let hex = "d".repeat(64);
        let json = serde_json::to_string(&hex).unwrap();
        let cref: ContentRef = serde_json::from_str(&json).unwrap();
        assert_eq!(cref.as_str(), hex);
    }

    #[test]
    fn deserialize_rejects_short_string() {
        let err = serde_json::from_str::<ContentRef>("\"x\"").unwrap_err();
        assert!(
            err.to_string().contains("64"),
            "deserialize error must mention the expected length: {err}"
        );
    }

    #[test]
    fn deserialize_rejects_uppercase() {
        let hex = "A".repeat(64);
        let json = serde_json::to_string(&hex).unwrap();
        let err = serde_json::from_str::<ContentRef>(&json).unwrap_err();
        assert!(
            err.to_string().contains("lowercase"),
            "deserialize error must mention the lowercase requirement: {err}"
        );
    }

    #[test]
    fn deserialize_rejects_non_hex_characters() {
        let mut hex = "a".repeat(63);
        hex.push('z');
        let json = serde_json::to_string(&hex).unwrap();
        let err = serde_json::from_str::<ContentRef>(&json).unwrap_err();
        assert!(
            err.to_string().contains("lowercase hex"),
            "deserialize error must mention the hex requirement: {err}"
        );
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
