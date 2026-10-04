//! Shared core of the pack-side ANN bridges: a Vamana index, the position-to-UUID map that
//! names the subject stored at each ordinal, and the digest of the commit record the bridge
//! was loaded from. Packs wrap this core with their own lifecycle state.

use std::path::Path;

use uuid::Uuid;

use crate::config::VamanaConfig;
use crate::distance::l2_normalize;
use crate::error::VamanaError;
use crate::external_ids::{
    read_external_ids_sidecar, segment_commit_digest, write_external_ids_sidecar,
};
use crate::index::{read_commit_fingerprint, VamanaIndex};

/// A Vamana index paired with its ordinal-to-UUID map and commit identity.
#[doc(hidden)]
pub struct AnnBridgeCore {
    pub index: VamanaIndex,
    /// Entry `i` names the subject stored at ordinal `i`.
    pub id_map: Vec<Uuid>,
    /// Digest of the v2 commit record this mmap bridge loaded. Every
    /// file-backed publication carries a fresh nonce, so equality means the
    /// mapped file generation is still current (#2081). Owned builds have no
    /// publication identity until they are persisted and reopened.
    pub commit_digest: Option<[u8; 32]>,
}

impl AnnBridgeCore {
    pub fn build(mut vectors: Vec<f32>, dim: usize, id_map: Vec<Uuid>) -> Result<Self, String> {
        if dim == 0 {
            return Err("dimension must be > 0".into());
        }
        if vectors.is_empty() || id_map.is_empty() {
            return Err("no vectors to build ANN index from".into());
        }
        let n = vectors.len() / dim;
        if n != id_map.len() {
            return Err(format!(
                "id_map length {} != vector count {}",
                id_map.len(),
                n
            ));
        }
        // L2→cosine conversion requires unit vectors; normalize before building.
        for row in vectors.chunks_exact_mut(dim) {
            l2_normalize(row);
        }
        let cfg = VamanaConfig::with_dimensions(dim);
        let index = VamanaIndex::build_owned(vectors, cfg).map_err(|e| format!("{e}"))?;
        Ok(Self {
            index,
            id_map,
            commit_digest: None,
        })
    }

    /// Search for the `k` nearest neighbors of `query` (normalized here) and pair each hit with
    /// its subject. The distance is the squared L2 distance between unit vectors; ordinals
    /// without an id-map entry are dropped.
    pub fn search_hits(&self, query: &[f32], k: usize) -> Result<Vec<(Uuid, f32)>, VamanaError> {
        let mut q = query.to_vec();
        l2_normalize(&mut q);
        let raw = self.index.search(&q, k)?;
        let mut hits = Vec::with_capacity(raw.len());
        for (idx, dist) in raw {
            if let Some(uuid) = self.id_map.get(idx as usize) {
                hits.push((*uuid, dist));
            }
        }
        Ok(hits)
    }

    /// Save this bridge to `dir` atomically: v2 Vamana segments (commit
    /// record is the gate), then the id-map sidecar bound to the blake3
    /// digest of that record. A crash between the two writes leaves a
    /// digest mismatch that `load` detects as a torn pair. Returns the digest
    /// of the committed record.
    pub fn save_atomic(&self, dir: &Path) -> Result<[u8; 32], String> {
        let count = self.id_map.len();
        if count != self.index.num_vectors() {
            return Err(format!(
                "id_map length {count} != index.num_vectors() {}",
                self.index.num_vectors()
            ));
        }
        self.index
            .save_atomic(dir)
            .map_err(|e| format!("VamanaIndex::save_atomic: {e}"))?;
        let digest = segment_commit_digest(dir)
            .map_err(|e| format!("segment_commit_digest after save: {e}"))?
            .ok_or_else(|| {
                "save_atomic succeeded but metadata.bin is absent (torn commit)".to_string()
            })?;
        write_external_ids_sidecar(dir, &digest, &self.id_map).map_err(|e| e.to_string())?;
        Ok(digest)
    }

    /// Load a bridge core from a segment directory written by `save_atomic`,
    /// together with the commit digest it is bound to (also stored in
    /// `commit_digest`). Any missing, torn, or cross-check-failing state
    /// returns `Err`; the caller treats that as a Cold signal.
    pub fn load(dir: &Path) -> Result<(Self, [u8; 32]), String> {
        read_commit_fingerprint(dir)
            .map_err(|e| format!("read_commit_fingerprint: {e}"))?
            .ok_or_else(|| {
                "no v2 commit fingerprint: segment dir is absent, v1, or has a torn commit"
                    .to_string()
            })?;
        let index = VamanaIndex::load(dir).map_err(|e| format!("VamanaIndex::load: {e}"))?;
        let (sidecar_digest, id_map) = read_external_ids_sidecar(dir)?;
        let commit_digest = segment_commit_digest(dir)
            .map_err(|e| format!("segment_commit_digest: {e}"))?
            .ok_or_else(|| "metadata.bin vanished between fingerprint and digest".to_string())?;
        if sidecar_digest != commit_digest {
            return Err(
                "external_ids.bin commit-digest mismatch: torn segment/sidecar pair".to_string(),
            );
        }
        if id_map.len() != index.num_vectors() {
            return Err(format!(
                "external_ids.bin count {} != index.num_vectors() {}",
                id_map.len(),
                index.num_vectors()
            ));
        }
        Ok((
            Self {
                index,
                id_map,
                commit_digest: Some(commit_digest),
            },
            commit_digest,
        ))
    }
}
