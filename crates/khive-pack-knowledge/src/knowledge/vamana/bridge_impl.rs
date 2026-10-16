use super::*;

impl AnnBridge {
    pub(super) fn from_core(core: AnnBridgeCore) -> Self {
        Self {
            core,
            generation: 0,
            #[cfg(test)]
            drop_probe: None,
            #[cfg(test)]
            search_pause: None,
        }
    }

    pub fn build(vectors: Vec<f32>, dim: usize, id_map: Vec<Uuid>) -> Result<Self, String> {
        let core = AnnBridgeCore::build(vectors, dim, id_map)?;
        Ok(Self::from_core(core))
    }

    /// Stamp the ann_write_log watermark this bridge's corpus state reflects
    /// (ADR-079 Amendment 1). Persisted by `save_atomic` into the extended
    /// commit record.
    pub(crate) fn set_applied_seq(&mut self, seq: u64) {
        self.index.set_last_applied_seq(Some(seq));
    }

    /// Ordinal lookup for only the subjects in a coalesced tail. Scan the
    /// id-map once, but allocate and hash at most one entry per tail subject
    /// instead of rebuilding a corpus-sized reverse map for a short replay.
    /// Highest ordinal wins for a repeated uuid: inserts append, so the
    /// latest slot is the live one; earlier slots are tombstoned.
    ///
    /// A tombstoned ordinal has no owner (ADR-079 Amendment 1 id-map
    /// ownership rule) — `id_map` entries for already-tombstoned slots are
    /// stale (tombstoning never clears them) and are excluded here, or a
    /// reused slot's new owner can be tombstoned by a replay op for the
    /// old, already-deleted subject (#1150).
    pub(crate) fn reverse_map_for(
        &self,
        subjects: impl IntoIterator<Item = Uuid>,
    ) -> HashMap<Uuid, u32> {
        let wanted: Vec<Uuid> = subjects.into_iter().collect();
        // One/few-row tails are common: comparing UUID bytes is cheaper
        // than hashing every id-map entry while scanning the corpus.
        let wanted_set = (wanted.len() > 4).then(|| wanted.iter().copied().collect::<HashSet<_>>());
        let mut reverse: HashMap<Uuid, u32> =
            HashMap::with_capacity(wanted.len().min(self.index.live_count()));
        for (ordinal, uuid) in self.id_map.iter().enumerate() {
            let in_tail = match &wanted_set {
                Some(set) => set.contains(uuid),
                None => wanted.contains(uuid),
            };
            if !in_tail || self.index.is_tombstoned(ordinal as u32) {
                continue;
            }
            reverse.insert(*uuid, ordinal as u32);
        }
        reverse
    }

    /// Apply one subject's coalesced final state (ADR-079 Amendment 1):
    /// `Some(embedding)` replays a final upsert (tombstone the mapped old
    /// ordinal, then exactly one insert); `None` replays a final delete
    /// (tombstone if mapped, no-op otherwise). `reverse` is the map from
    /// [`reverse_map_for`](Self::reverse_map_for), kept current across calls.
    ///
    /// A delete whose mapped ordinal has been reassigned by an earlier
    /// upsert in this replay is skipped with a warning, not an error: the
    /// old subject's vector was already tombstoned when the slot was
    /// reused, so there is nothing left to delete. Any other id-map
    /// contradiction returns `Err` — the caller escalates to Cold.
    pub(crate) fn apply_final_op(
        &mut self,
        reverse: &mut HashMap<Uuid, u32>,
        uuid: Uuid,
        op: Option<Vec<f32>>,
    ) -> Result<(), String> {
        match op {
            None => {
                if let Some(&ordinal) = reverse.get(&uuid) {
                    // Fail closed on ownership contradictions: if the
                    // slot's current id-map owner is no longer this
                    // subject (an earlier op in this replay already reused
                    // the slot), skip the tombstone rather than delete
                    // someone else's live vector.
                    if self.id_map.get(ordinal as usize) != Some(&uuid) {
                        tracing::warn!(
                            subject = %uuid,
                            ordinal,
                            "replay delete: ordinal reassigned within batch, skipping tombstone"
                        );
                        reverse.remove(&uuid);
                        return Ok(());
                    }
                    self.index
                        .tombstone(ordinal)
                        .map_err(|e| format!("replay tombstone({ordinal}): {e}"))?;
                    reverse.remove(&uuid);
                }
            }
            Some(mut embedding) => {
                l2_normalize(&mut embedding);
                if let Some(&old) = reverse.get(&uuid) {
                    self.index
                        .tombstone(old)
                        .map_err(|e| format!("replay tombstone({old}): {e}"))?;
                }
                let ordinal = self
                    .index
                    .insert(&embedding)
                    .map_err(|e| format!("replay insert: {e}"))?;
                let slot = ordinal as usize;
                match slot.cmp(&self.id_map.len()) {
                    std::cmp::Ordering::Less => self.id_map[slot] = uuid,
                    std::cmp::Ordering::Equal => self.id_map.push(uuid),
                    std::cmp::Ordering::Greater => {
                        return Err(format!(
                            "replay insert returned ordinal {ordinal} beyond id_map len {}",
                            self.id_map.len()
                        ));
                    }
                }
                reverse.insert(uuid, ordinal);
            }
        }
        Ok(())
    }

    pub fn search(&self, query: &[f32], k: usize) -> Vec<(Uuid, f32)> {
        #[cfg(test)]
        if let Some(pause) = &self.search_pause {
            pause.wait();
        }
        match self.core.search_hits(query, k) {
            Ok(hits) => hits
                .into_iter()
                .map(|(uuid, dist)| {
                    // L2² → cosine: cos(a,b) = 1 - L2²(a,b)/2 for unit vectors
                    let cosine = 1.0 - dist / 2.0;
                    (uuid, cosine.max(0.0))
                })
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "vamana ANN search failed");
                Vec::new()
            }
        }
    }

    pub fn num_vectors(&self) -> usize {
        self.index.num_vectors()
    }

    pub fn from_vamana_snapshot(snapshot: VamanaSnapshot) -> Result<Self, String> {
        let id_map: Vec<Uuid> = snapshot
            .external_ids
            .iter()
            .map(|s| Uuid::parse_str(s).map_err(|e| format!("bad UUID {s}: {e}")))
            .collect::<Result<_, _>>()?;
        let index =
            VamanaIndex::from_snapshot(&snapshot).map_err(|e| format!("snapshot restore: {e}"))?;
        let core = AnnBridgeCore {
            index,
            id_map,
            commit_digest: None,
        };
        Ok(Self::from_core(core))
    }

    /// Save this bridge to `dir` atomically: writes v2 Vamana segments (commits
    /// `metadata.bin`), then the id-map sidecar (`external_ids.bin`,
    /// tmp-then-rename) bound to the blake3 digest of the just-committed record.
    /// Crash-safety invariant: a crash between the two writes leaves the
    /// sidecar's stored digest mismatched against the on-disk commit record, so
    /// the load-time cross-check detects the torn pair and the caller
    /// rebuilds -- ordering alone is not the guarantee, the digest cross-check
    /// is. See crates/khive-pack-knowledge/docs/api/vamana.md#save_atomic.
    #[allow(dead_code)]
    pub fn save_atomic(&self, dir: &std::path::Path) -> Result<(), String> {
        let _publication_lock =
            khive_retrieval::ann::acquire_checkpoint_lock(dir, BRIDGE_LOCK_MESSAGE_PREFIX)?;
        self.save_atomic_locked(dir)
    }

    /// Save while the caller holds this directory's bridge-level publication
    /// lock.  Keeping the lock above both the Vamana commit and UUID sidecar
    /// prevents two writers from pairing one commit digest with another
    /// writer's id map.
    pub(super) fn save_atomic_locked(&self, dir: &std::path::Path) -> Result<(), String> {
        // The core writes the v2 segments (metadata.bin is the commit gate), digests the
        // just-committed record, then writes the id-map sidecar bound to that digest so any
        // segment/sidecar pairing from different saves is self-detecting at load time.
        self.core.save_atomic(dir)?;
        Ok(())
    }

    /// Load a bridge from a segment directory previously written by
    /// [`AnnBridge::save_atomic`].
    ///
    /// Both the Vamana v2 commit record and the id-map sidecar must be present and
    /// self-consistent (sidecar bound to the exact commit-record digest, matching
    /// vector count). Any mismatch returns `Err`; the caller should treat that as a
    /// Cold signal and rebuild from the corpus.
    #[allow(dead_code)]
    pub fn load(dir: &std::path::Path) -> Result<Self, String> {
        // The core requires a v2 commit fingerprint (absent/v1/torn → Cold), raw-loads the
        // committed v2 index (VamanaIndex::load is v2-aware, ADR-079), then cross-checks the
        // external_ids sidecar against the exact commit record and the vector count.
        let (core, _commit_digest) = AnnBridgeCore::load(dir)?;
        Ok(Self::from_core(core))
    }
}

pub(super) const BRIDGE_LOCK_MESSAGE_PREFIX: &str = "ANN bridge";
