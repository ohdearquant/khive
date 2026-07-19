//! External-id sidecar codec for ANN segment directories (ADR-079).
//!
//! `external_ids.bin` maps segment ordinals back to caller UUIDs and is
//! stamped with the v2 commit's `content_hash` so a torn segment/sidecar pair
//! (crash between the segment commit and the sidecar write) is self-detecting
//! at load time. One codec shared by every pack that persists a segment.
//!
//! Binary format:
//!   magic        8 bytes   b"KHVANIDS"
//!   content_hash 32 bytes  corpus blake3 hash (from the v2 commit fingerprint)
//!   count        8 bytes   u64 little-endian — number of UUIDs
//!   ids          16 × count bytes — raw UUID bytes

use uuid::Uuid;

const SIDECAR_MAGIC: &[u8; 8] = b"KHVANIDS";

/// Write `ids` to `dir/external_ids.bin` using a tmp-then-rename pattern.
///
/// The sidecar is stamped with `content_hash` so a loader can detect a torn
/// segment/sidecar pair (segments committed with hash A, sidecar still holding
/// hash B from a prior save, or vice versa).
pub fn write_external_ids_sidecar(
    dir: &std::path::Path,
    content_hash: &[u8; 32],
    ids: &[Uuid],
) -> Result<(), String> {
    use std::io::Write as _;

    let tmp_path = dir.join("external_ids.bin.tmp");
    let final_path = dir.join("external_ids.bin");

    let count = ids.len() as u64;
    let mut buf: Vec<u8> = Vec::with_capacity(8 + 32 + 8 + ids.len() * 16);
    buf.extend_from_slice(SIDECAR_MAGIC);
    buf.extend_from_slice(content_hash);
    buf.extend_from_slice(&count.to_le_bytes());
    for id in ids {
        buf.extend_from_slice(id.as_bytes());
    }

    let mut f = std::fs::File::create(&tmp_path)
        .map_err(|e| format!("create external_ids.bin.tmp: {e}"))?;
    f.write_all(&buf)
        .map_err(|e| format!("write external_ids.bin.tmp: {e}"))?;
    f.sync_all()
        .map_err(|e| format!("sync external_ids.bin.tmp: {e}"))?;
    drop(f);
    std::fs::rename(&tmp_path, &final_path)
        .map_err(|e| format!("rename external_ids.bin.tmp -> external_ids.bin: {e}"))
}

/// Read `dir/external_ids.bin` and return `(content_hash, ids)`.
///
/// Returns `Err` on any I/O error, wrong magic, truncated header, or
/// count/size mismatch.
pub fn read_external_ids_sidecar(dir: &std::path::Path) -> Result<([u8; 32], Vec<Uuid>), String> {
    let bytes = std::fs::read(dir.join("external_ids.bin"))
        .map_err(|e| format!("read external_ids.bin: {e}"))?;

    // magic (8) + content_hash (32) + count (8) = 48 bytes minimum header
    if bytes.len() < 48 {
        return Err(format!(
            "external_ids.bin too short: {} bytes (need at least 48)",
            bytes.len()
        ));
    }

    let magic = &bytes[0..8];
    if magic != SIDECAR_MAGIC {
        return Err(format!(
            "external_ids.bin bad magic: got {:?}, expected {:?}",
            magic, SIDECAR_MAGIC
        ));
    }

    let mut content_hash = [0u8; 32];
    content_hash.copy_from_slice(&bytes[8..40]);

    let count = u64::from_le_bytes(bytes[40..48].try_into().unwrap()) as usize;
    let expected_len = 48 + count * 16;
    if bytes.len() != expected_len {
        return Err(format!(
            "external_ids.bin length mismatch: got {} bytes, expected {} for {count} UUIDs",
            bytes.len(),
            expected_len
        ));
    }

    let mut ids = Vec::with_capacity(count);
    for i in 0..count {
        let start = 48 + i * 16;
        let raw: [u8; 16] = bytes[start..start + 16].try_into().unwrap();
        ids.push(Uuid::from_bytes(raw));
    }

    Ok((content_hash, ids))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hash = [7u8; 32];
        let ids: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
        write_external_ids_sidecar(dir.path(), &hash, &ids).expect("write");
        let (read_hash, read_ids) = read_external_ids_sidecar(dir.path()).expect("read");
        assert_eq!(read_hash, hash);
        assert_eq!(read_ids, ids);
    }

    #[test]
    fn sidecar_rejects_truncation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ids: Vec<Uuid> = (0..3).map(|_| Uuid::new_v4()).collect();
        write_external_ids_sidecar(dir.path(), &[0u8; 32], &ids).expect("write");
        let path = dir.path().join("external_ids.bin");
        let bytes = std::fs::read(&path).expect("read back");
        std::fs::write(&path, &bytes[..bytes.len() - 1]).expect("truncate");
        assert!(read_external_ids_sidecar(dir.path()).is_err());
    }
}
