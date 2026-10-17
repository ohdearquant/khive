use super::*;

// ── AnnBridge ─────────────────────────────────────────────────────────────────

impl AnnBridge {
    pub(crate) fn build(
        vectors: Vec<f32>,
        dim: usize,
        id_map: Vec<Uuid>,
        namespace_set: HashSet<String>,
    ) -> Result<Self, RuntimeError> {
        let core = AnnBridgeCore::build(vectors, dim, id_map).map_err(RuntimeError::Internal)?;
        // The core accepted the build only when the vector count equals the id-map length.
        let n = core.id_map.len();
        Ok(Self {
            core,
            incarnation: Arc::new(()),
            #[cfg(test)]
            reverse_map_scan_hook: None,
            reverse_map: None,
            #[cfg(test)]
            reverse_map_builds: 0,
            dirty_ops: 0,
            published_seq: 0,
            last_checkpoint: std::time::Instant::now(),
            base_commit_digest: None,
            base_applied_seq: 0,
            base_ops: n,
            delta_batches: Vec::new(),
            delta_raw_ops: 0,
            delta_chunks: 0,
            last_delta_nonce: None,
            namespace_set,
            generation: 0,
            epoch_baseline: 0,
            #[cfg(test)]
            drop_probe: None,
        })
    }

    /// Stamps the build with the independently durable corpus epoch it observed.
    pub(crate) fn with_epoch_baseline(mut self, epoch: u64) -> Self {
        self.epoch_baseline = epoch;
        self
    }

    pub(crate) fn search_with_route(
        &self,
        query: &[f32],
        k: usize,
        route: AnnScoreRoute,
    ) -> Result<Vec<(Uuid, f64)>, RuntimeError> {
        let raw = self
            .core
            .search_hits(query, k)
            .map_err(|e| RuntimeError::Internal(format!("memory ANN search: {e}")))?;
        let mut hits = Vec::with_capacity(raw.len());
        for (uuid, dist) in raw {
            // The graph emits normalized-vector L2². For note search,
            // L2²/2 is cosine distance and uses the same converter as the
            // exact sqlite-vec route; memory recall retains its prior floor.
            hits.push((uuid, route.graph_score(dist)?));
        }
        Ok(hits)
    }

    #[cfg(test)]
    pub(super) fn search(&self, query: &[f32], k: usize) -> Result<Vec<(Uuid, f64)>, RuntimeError> {
        self.search_with_route(query, k, AnnScoreRoute::Memory)
    }

    /// Stamp the ann_write_log watermark this bridge's corpus state reflects
    /// (ADR-079 Amendment 1). Persisted by `save_atomic` into the extended
    /// commit record.
    pub(crate) fn set_applied_seq(&mut self, seq: u64) {
        self.index.set_last_applied_seq(Some(seq));
        self.published_seq = seq;
    }

    pub(crate) fn mark_checkpointed(&mut self) {
        self.dirty_ops = 0;
        self.published_seq = self.index.last_applied_seq().unwrap_or(0);
        self.last_checkpoint = std::time::Instant::now();
    }

    /// A full save can succeed even when the immediate mmap re-adoption fails.
    /// The retained owned bridge represents that exact durable base and must
    /// carry its identity before another incremental delta can be published.
    pub(super) fn mark_full_checkpoint_base(&mut self, digest: [u8; 32]) {
        self.commit_digest = Some(digest);
        self.base_commit_digest = Some(digest);
        self.base_applied_seq = self.index.last_applied_seq().unwrap_or(0);
        self.base_ops = self.index.num_vectors();
        self.delta_batches.clear();
        self.delta_raw_ops = 0;
        self.delta_chunks = 0;
        self.last_delta_nonce = None;
    }

    pub(super) fn mark_delta_checkpoint(&mut self, publication: &delta::DeltaPublication) {
        self.commit_digest = Some(publication.identity);
        self.last_delta_nonce = Some(publication.last_nonce);
        self.delta_chunks = publication.chunk_count;
        self.delta_batches.clear();
    }

    pub(super) fn record_delta_batch(
        &mut self,
        ops: Vec<(Uuid, Option<Vec<f32>>)>,
        applied_seq: u64,
        raw_count: u64,
    ) {
        if raw_count == 0 {
            return;
        }
        self.delta_batches.push(delta::DeltaBatch {
            applied_seq,
            raw_count,
            ops,
        });
        self.delta_raw_ops = self.delta_raw_ops.saturating_add(raw_count);
    }

    pub(super) fn needs_full_compaction(&self) -> bool {
        self.needs_full_compaction_with_chain_limit(delta::MAX_RETIRED_CHAIN)
    }

    pub(super) fn needs_full_compaction_with_chain_limit(&self, max_chunks: usize) -> bool {
        self.delta_batches.is_empty()
            || self.delta_raw_ops >= delta::compaction_limit(self.base_ops)
            || self.delta_chunks >= max_chunks
    }

    pub(super) fn fork_for_maintenance(&self) -> Self {
        Self {
            core: AnnBridgeCore {
                index: self.index.fork_for_maintenance(),
                id_map: self.id_map.clone(),
                commit_digest: self.commit_digest,
            },
            incarnation: Arc::new(()),
            #[cfg(test)]
            reverse_map_scan_hook: self.reverse_map_scan_hook.clone(),
            reverse_map: self.reverse_map.clone(),
            #[cfg(test)]
            reverse_map_builds: self.reverse_map_builds,
            dirty_ops: self.dirty_ops,
            published_seq: self.published_seq,
            last_checkpoint: self.last_checkpoint,
            base_commit_digest: self.base_commit_digest,
            base_applied_seq: self.base_applied_seq,
            base_ops: self.base_ops,
            delta_batches: self.delta_batches.clone(),
            delta_raw_ops: self.delta_raw_ops,
            delta_chunks: self.delta_chunks,
            last_delta_nonce: self.last_delta_nonce,
            namespace_set: self.namespace_set.clone(),
            generation: self.generation,
            epoch_baseline: self.epoch_baseline,
            #[cfg(test)]
            drop_probe: None,
        }
    }

    pub(super) fn rebuild_reverse_map(&mut self) {
        // A tombstoned ordinal can retain its previous UUID until slot reuse.
        let mut reverse = HashMap::with_capacity(self.index.live_count());
        for (ordinal, uuid) in self.id_map.iter().enumerate() {
            #[cfg(test)]
            if ordinal == self.id_map.len() / 2 {
                if let Some(hook) = &self.reverse_map_scan_hook {
                    hook();
                }
            }
            if !self.index.is_tombstoned(ordinal as u32) {
                reverse.insert(*uuid, ordinal as u32);
            }
        }
        self.reverse_map = Some(reverse);
        #[cfg(test)]
        {
            self.reverse_map_builds += 1;
        }
    }

    /// Apply a coalesced final-state tail (ADR-079 Amendment 1) to this
    /// bridge: `Some(embedding)` replays a final upsert; `None` replays a
    /// final delete. `new_s` is stamped as the new applied watermark. A
    /// delete whose ordinal was reassigned by an earlier same-batch upsert
    /// is skipped with a warning, not an error; any other id-map
    /// contradiction returns `Err` (caller escalates to Cold). See
    /// `docs/ann.md` for the ownership-rule rationale (#1150).
    pub(crate) fn apply_final_ops(
        &mut self,
        ops: Vec<(Uuid, Option<Vec<f32>>)>,
        new_s: u64,
    ) -> Result<(), String> {
        if self.reverse_map.is_none() {
            self.rebuild_reverse_map();
        }
        // Disjoint field borrows: `Deref` would borrow the whole bridge while `reverse` is live.
        let core = &mut self.core;
        let reverse = self.reverse_map.as_mut().expect("initialized reverse map");

        for (uuid, op) in ops {
            match op {
                None => {
                    if let Some(&ordinal) = reverse.get(&uuid) {
                        // Fail closed: if a same-batch upsert already reused
                        // this slot, skip the tombstone instead of deleting
                        // someone else's live vector.
                        if core.id_map.get(ordinal as usize) != Some(&uuid) {
                            tracing::warn!(
                                subject = %uuid,
                                ordinal,
                                "replay delete: ordinal reassigned within batch, skipping tombstone"
                            );
                            reverse.remove(&uuid);
                            continue;
                        }
                        core.index
                            .tombstone(ordinal)
                            .map_err(|e| format!("replay tombstone({ordinal}): {e}"))?;
                        reverse.remove(&uuid);
                    }
                }
                Some(mut embedding) => {
                    l2_normalize(&mut embedding);
                    if let Some(&old) = reverse.get(&uuid) {
                        if core.id_map.get(old as usize) != Some(&uuid) {
                            return Err(format!(
                                "replay upsert: ordinal {old} is no longer owned by {uuid}"
                            ));
                        }
                        core.index
                            .tombstone(old)
                            .map_err(|e| format!("replay tombstone({old}): {e}"))?;
                        reverse.remove(&uuid);
                    }
                    let ordinal = core
                        .index
                        .insert(&embedding)
                        .map_err(|e| format!("replay insert: {e}"))?;
                    let slot = ordinal as usize;
                    match slot.cmp(&core.id_map.len()) {
                        std::cmp::Ordering::Less => {
                            let previous_owner = core.id_map[slot];
                            if reverse.get(&previous_owner) == Some(&ordinal) {
                                reverse.remove(&previous_owner);
                            }
                            core.id_map[slot] = uuid;
                        }
                        std::cmp::Ordering::Equal => core.id_map.push(uuid),
                        std::cmp::Ordering::Greater => {
                            return Err(format!(
                                "replay insert returned ordinal {ordinal} beyond id_map len {}",
                                core.id_map.len()
                            ));
                        }
                    }
                    reverse.insert(uuid, ordinal);
                }
            }
        }
        core.index.set_last_applied_seq(Some(new_s));
        Ok(())
    }

    pub(crate) fn consolidate_if_needed(&mut self, tau: usize) -> Result<bool, String> {
        let core = &mut self.core;
        if !core.index.needs_consolidation() && core.index.ops_since_consolidation() < tau {
            return Ok(false);
        }
        if core.id_map.len() != core.index.num_vectors() {
            return Err("consolidation: id_map length differs from vector count".to_string());
        }
        let new_to_old = core
            .index
            .consolidate()
            .map_err(|e| format!("memory ANN consolidation: {e}"))?;
        if !new_to_old.is_empty() {
            core.id_map = new_to_old
                .into_iter()
                .map(|old| {
                    core.id_map.get(old as usize).copied().ok_or_else(|| {
                        format!("consolidation: old ordinal {old} is outside id_map")
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
        }
        if self.reverse_map.is_some() {
            self.rebuild_reverse_map();
        }
        Ok(true)
    }

    /// Save this bridge to `dir` atomically: v2 Vamana segments (commit
    /// record is the gate), then the id-map sidecar bound to the blake3
    /// digest of that record. A crash between the two writes leaves a
    /// digest mismatch that `load` detects as a torn pair. Once both are
    /// committed, the retired delta HEAD and its chunks are removed; a failure
    /// there is logged rather than returned, because readers already ignore a
    /// HEAD whose watermark the new base covers.
    pub(crate) fn save_atomic(&self, dir: &std::path::Path) -> Result<[u8; 32], String> {
        let digest = self.core.save_atomic(dir)?;
        if let Err(error) = delta::clear(dir) {
            tracing::warn!(%error, "memory delta cleanup failed after full checkpoint commit");
        }
        Ok(digest)
    }

    /// Load a bridge from a segment directory written by `save_atomic`. Any
    /// missing, torn, or cross-check-failing state returns `Err`; the caller
    /// treats that as a Cold signal. `namespace_set` starts empty
    /// (conservative — recall assumes non-visible namespaces may exist) until
    /// the caller populates it.
    pub(crate) fn load(dir: &std::path::Path) -> Result<Self, String> {
        let (core, commit_digest) = AnnBridgeCore::load(dir)?;
        let base_applied_seq = core.index.last_applied_seq().unwrap_or(0);
        let base_ops = core.index.num_vectors();
        let mut bridge = Self {
            core,
            incarnation: Arc::new(()),
            #[cfg(test)]
            reverse_map_scan_hook: None,
            reverse_map: None,
            #[cfg(test)]
            reverse_map_builds: 0,
            dirty_ops: 0,
            published_seq: base_applied_seq,
            last_checkpoint: std::time::Instant::now(),
            base_commit_digest: Some(commit_digest),
            base_applied_seq,
            base_ops,
            delta_batches: Vec::new(),
            delta_raw_ops: 0,
            delta_chunks: 0,
            last_delta_nonce: None,
            namespace_set: HashSet::new(),
            generation: 0,
            epoch_baseline: 0,
            #[cfg(test)]
            drop_probe: None,
        };
        if let Some(overlay) = delta::read(
            dir,
            &commit_digest,
            base_applied_seq,
            bridge.index.dimensions(),
            base_ops,
        )? {
            for batch in &overlay.batches {
                bridge.apply_final_ops(batch.ops.clone(), batch.applied_seq)?;
            }
            bridge.published_seq = overlay.applied_seq;
            bridge.commit_digest = Some(overlay.identity);
            bridge.delta_raw_ops = overlay.raw_count;
            bridge.delta_chunks = overlay.batches.len();
            bridge.last_delta_nonce = Some(overlay.last_nonce);
        }
        Ok(bridge)
    }

    /// Populate `namespace_set` from an already-queried set of namespace strings.
    pub(crate) fn set_namespace_set(&mut self, ns_set: HashSet<String>) {
        self.namespace_set = ns_set;
    }
}
