use khive_quant::GsEncodedVector;

use crate::{
    config::VamanaConfig,
    error::{Result, VamanaError},
    graph::{is_tombstoned_bit, robust_prune_inner, sort_dedup_u32, VamanaGraph, VisitedSet},
};

use super::{
    elect_medoid, require_finite, set_tombstone_bit, tombstone_words_for, wolverine_repair,
    CodeStore, SearchVisitedPool, VamanaIndex, VectorStorage,
};

impl VamanaIndex {
    /// Return a reference to the underlying Vamana graph.
    pub fn graph(&self) -> &VamanaGraph {
        &self.graph
    }

    /// Return a reference to the build configuration.
    pub fn config(&self) -> &VamanaConfig {
        &self.config
    }

    /// Return the number of indexed vectors.
    pub fn num_vectors(&self) -> usize {
        self.num_vectors
    }

    /// Return the vector dimensionality.
    pub fn dimensions(&self) -> usize {
        self.dimensions
    }

    /// Return the flat row-major vector data as a slice.
    pub fn vectors(&self) -> Result<&[f32]> {
        self.vectors.as_slice()
    }

    /// Write-log watermark carried by the v2 commit record. `None` on indexes
    /// built in-memory or loaded from segments predating the field.
    pub fn last_applied_seq(&self) -> Option<u64> {
        self.last_applied_seq
    }

    /// Set the write-log watermark to persist with the next [`Self::save_atomic`].
    /// The caller owns the log and must pass the highest sequence whose write is
    /// reflected in this index's current state.
    pub fn set_last_applied_seq(&mut self, seq: Option<u64>) {
        self.last_applied_seq = seq;
    }

    // ---- PR2: lifecycle API (ADR-052 §2) ----

    /// True if `node_id` has been soft-deleted.
    pub fn is_tombstoned(&self, node_id: u32) -> bool {
        is_tombstoned_bit(&self.tombstones, node_id as usize)
    }

    /// Count of currently tombstoned (soft-deleted) nodes.
    pub fn tombstone_count(&self) -> usize {
        self.tombstone_count
    }

    /// Count of live (non-tombstoned) nodes.
    pub fn live_count(&self) -> usize {
        self.num_vectors - self.tombstone_count
    }

    /// Cumulative delete+insert churn since the last consolidation.
    pub fn ops_since_consolidation(&self) -> usize {
        self.ops_since_consolidation
    }

    /// Fork the complete mutable state for maintenance while retaining any
    /// read-only vector and SQ8 mappings. Owned buffers are copied; mutation
    /// promotes mappings through the same path as an ordinary insert.
    pub fn fork_for_maintenance(&self) -> Self {
        Self {
            vectors: self.vectors.clone(),
            graph: self.graph.clone(),
            config: self.config.clone(),
            num_vectors: self.num_vectors,
            dimensions: self.dimensions,
            search_visited: SearchVisitedPool::default(),
            tombstones: self.tombstones.clone(),
            tombstone_count: self.tombstone_count,
            ops_since_consolidation: self.ops_since_consolidation,
            free_slots: self.free_slots.clone(),
            consolidation_tau: self.consolidation_tau,
            gs_codec: self.gs_codec.clone(),
            gs_codes: self.gs_codes.clone(),
            last_applied_seq: self.last_applied_seq,
        }
    }

    /// True when `ops_since_consolidation >= consolidation_tau`.
    pub fn needs_consolidation(&self) -> bool {
        self.ops_since_consolidation >= self.consolidation_tau
    }

    // ---- PR3: Mmap-to-Owned promotion helper ----

    /// Promote `VectorStorage::Mmap` to `Owned` (no-op if already `Owned`); called first
    /// by both `insert` and `consolidate` so subsequent writes hit a mutable buffer.
    pub(super) fn ensure_owned(&mut self) -> Result<()> {
        #[cfg(feature = "mmap")]
        if let VectorStorage::Mmap { .. } = &self.vectors {
            let owned: Vec<f32> = self.vectors.as_slice()?.to_vec();
            self.vectors = VectorStorage::Owned(owned);
        }
        self.gs_codes.ensure_owned();
        Ok(())
    }

    // ---- PR3: insert and consolidate (ADR-052 §2) ----

    /// Insert a new vector into the index. Returns the ordinal assigned to the new node.
    ///
    /// If `free_slots` is non-empty a recycled ordinal is reused; otherwise a new slot
    /// is appended. Either way, the call runs greedy search from the current medoid,
    /// selects out-edges via RobustPrune, wires back-edges with reverse_adj update, and
    /// increments `ops_since_consolidation`.
    ///
    /// Mmap-backed indexes are promoted to Owned on the first insert call. Callers that
    /// held ordinals across a previous consolidate must treat those ordinals as invalid;
    /// ordinals are NOT stable across consolidate().
    ///
    /// Returns `Err` without mutating state if the vector is non-finite, wrong dimension,
    /// or would push `num_vectors` past `u32::MAX`.
    pub fn insert(&mut self, vector: &[f32]) -> Result<u32> {
        // Preflight — validate before ANY state change (including Mmap promotion).
        if vector.len() != self.dimensions {
            return Err(VamanaError::DimensionMismatch {
                expected: self.dimensions,
                actual: vector.len(),
            });
        }
        require_finite(vector, "insert vector")?;
        if self.num_vectors >= u32::MAX as usize {
            return Err(VamanaError::TooManyVectors {
                count: self.num_vectors,
            });
        }

        // GAP-1 resolution: promote Mmap to Owned before any vector mutation.
        // Only reached when preflight passed — a rejected insert never touches Mmap.
        self.ensure_owned()?;

        // Slot assignment: recycle or append.
        let ordinal: u32;
        if !self.free_slots.is_empty() {
            // Recycle path: LIFO pop. Guard against corrupted free_slots entry.
            let candidate = *self.free_slots.last().unwrap();
            if !is_tombstoned_bit(&self.tombstones, candidate as usize) {
                return Err(VamanaError::invalid_format(format!(
                    "insert: free slot {candidate} is not tombstoned"
                )));
            }
            self.free_slots.pop();
            ordinal = candidate;

            // Clear tombstone bit and decrement count before graph wiring.
            let word = ordinal as usize / 64;
            self.tombstones[word] &= !(1u64 << (ordinal as usize % 64));
            self.tombstone_count -= 1;

            // Write vector into recycled slot in-place.
            let start = ordinal as usize * self.dimensions;
            let end = start + self.dimensions;
            match &mut self.vectors {
                VectorStorage::Owned(v) => v[start..end].copy_from_slice(vector),
                #[cfg(feature = "mmap")]
                VectorStorage::Mmap { .. } => {
                    return Err(VamanaError::invalid_format(
                        "insert: unexpected Mmap after ensure_owned".into(),
                    ));
                }
            }
            // Update SQ8 code for the recycled slot.
            let code = self.gs_codec.encode(vector);
            self.gs_codes.owned_mut()?[ordinal as usize] = code;
        } else {
            // Append path: assign next ordinal and extend storage.
            ordinal = self.num_vectors as u32;
            self.num_vectors += 1;

            // Extend graph: adjacency and reverse_adj grow atomically.
            self.graph.add_node()?;

            // Extend tombstone bitvec if the new ordinal falls in a new word.
            let word = ordinal as usize / 64;
            if word >= self.tombstones.len() {
                self.tombstones.resize(word + 1, 0);
            }

            // Append vector to Owned storage.
            match &mut self.vectors {
                VectorStorage::Owned(v) => v.extend_from_slice(vector),
                #[cfg(feature = "mmap")]
                VectorStorage::Mmap { .. } => {
                    return Err(VamanaError::invalid_format(
                        "insert: unexpected Mmap after ensure_owned".into(),
                    ));
                }
            }
            // Append SQ8 code for the new slot.
            let code = self.gs_codec.encode(vector);
            self.gs_codes.owned_mut()?.push(code);
        }

        // Graph wiring.
        let live_before = self.num_vectors - self.tombstone_count - 1; // before this insert contributed
        if live_before == 0 {
            // Only one live node (the one just inserted); skip greedy search and set medoid.
            self.graph.set_medoid(ordinal);
        } else {
            let vecs = self.vectors.as_slice()?;

            let tombstones_opt = if self.tombstone_count > 0 {
                Some(self.tombstones.as_slice())
            } else {
                None
            };

            // Insert uses exact f32 distances for graph wiring. The SQ8 codec is
            // trained on the build corpus; inserted vectors may be out of that range,
            // causing u8 clamping and wrong orderings. Exact f32 is correct here —
            // insert is not a hot path. The gs_codes entry for ordinal is already
            // written above (recycle or push) so search() uses SQ8 correctly.
            let mut visited = VisitedSet::new(self.num_vectors);
            let search_result = self.graph.greedy_search(
                vecs,
                self.dimensions,
                vector,
                self.config.search_list_size,
                self.config.search_list_size,
                &mut visited,
                tombstones_opt,
            )?;

            // Candidate pool: expanded ∪ results, excluding ordinal itself, deduped.
            let mut candidates: Vec<u32> = search_result
                .expanded
                .iter()
                .map(|(id, _)| *id)
                .chain(search_result.results.iter().map(|(id, _)| *id))
                .filter(|&id| id != ordinal)
                .collect();
            sort_dedup_u32(&mut candidates);

            let vecs = self.vectors.as_slice()?;
            let new_neighbors = robust_prune_inner(
                vecs,
                self.dimensions,
                ordinal,
                candidates,
                self.config.alpha,
                self.config.max_degree,
            );

            // Wire new node's forward adjacency and reverse_adj in lockstep.
            self.graph
                .replace_adjacency_and_update_reverse(ordinal, new_neighbors.clone());

            // INVARIANT (never-drop insert): insert() never removes an existing node's
            // inbound edge, so nothing reachable before this call becomes unreachable.
            // Back-edge rule (Option E): add j→ordinal only if j has a free slot; a full
            // j keeps all its existing edges instead. See
            // crates/khive-vamana/docs/index.md#insert-back-edge-and-medoid-pin-rules.
            for &j in &new_neighbors {
                if self.graph.adjacency()[j as usize].len() < self.config.max_degree {
                    // j has a free slot: add the back-edge without dropping anything.
                    let mut j_adj: Vec<u32> = self.graph.adjacency()[j as usize]
                        .iter()
                        .copied()
                        .chain(std::iter::once(ordinal))
                        .filter(|&x| x != j)
                        .collect();
                    sort_dedup_u32(&mut j_adj);
                    self.graph.replace_adjacency_and_update_reverse(j, j_adj);
                }
                // j is full: skip the back-edge to preserve all of j's existing edges.
            }

            // Medoid-pin eager repair: if no out-neighbor had a free slot, pin
            // medoid→ordinal so the new node stays reachable (medoid is always
            // reachable). See crates/khive-vamana/docs/index.md#insert-back-edge-and-medoid-pin-rules.
            debug_assert!(
                !new_neighbors.is_empty(),
                "insert: new_neighbors must be non-empty when live_before > 0"
            );
            if self.graph.reverse_adjacency()[ordinal as usize].is_empty() {
                let medoid = self.graph.medoid();
                debug_assert_ne!(
                    medoid, ordinal,
                    "insert: medoid == ordinal in live_before>0 branch — impossible"
                );
                let mut medoid_adj: Vec<u32> = self.graph.adjacency()[medoid as usize]
                    .iter()
                    .copied()
                    .chain(std::iter::once(ordinal))
                    .filter(|&x| x != medoid)
                    .collect();
                sort_dedup_u32(&mut medoid_adj);
                // Medoid may now exceed max_degree — resolved at serialization time.
                self.graph
                    .replace_adjacency_and_update_reverse(medoid, medoid_adj);
            }
        }

        self.ops_since_consolidation += 1;
        Ok(ordinal)
    }

    /// Compact tombstoned slots: renumber live nodes to contiguous ordinals `0..M`,
    /// rebuild adjacency and `reverse_adj` over the new ordinals, and reset
    /// tombstone/free-slot state. Does NOT re-run graph construction.
    ///
    /// Returns `new_to_old` where `new_to_old[new_ordinal] == old_ordinal`, allowing
    /// callers that hold external-id maps (e.g., `AnnBridge`'s ordinal→UUID table) to
    /// remap their data. An empty `Vec` is returned on the no-op fast path (zero
    /// tombstones), signaling that ordinals are unchanged and no remap is needed.
    ///
    /// **Ordinals are NOT stable across consolidate().** Any external holder of a `u32`
    /// ordinal must rebuild its mapping using the returned `new_to_old` vector after
    /// each non-no-op consolidation. This is an invariant break visible to callers.
    ///
    /// After return: `tombstone_count == 0`, `free_slots` is empty,
    /// `ops_since_consolidation == 0`, `num_vectors == prior live_count`.
    pub fn consolidate(&mut self) -> Result<Vec<u32>> {
        // No-op fast path: no tombstones — skip Mmap promotion entirely.
        // A clean index on a Mmap-backed store stays Mmap after a no-op consolidate.
        if self.tombstone_count == 0 {
            self.ops_since_consolidation = 0;
            return Ok(Vec::new());
        }

        // GAP-1 resolution: promote Mmap to Owned before the compaction rebuild.
        // Only reached when there are tombstones to compact.
        self.ensure_owned()?;

        let m = self.num_vectors - self.tombstone_count;

        // Build old→new and new→old remap tables.
        let mut old_to_new: Vec<u32> = vec![u32::MAX; self.num_vectors];
        let mut new_to_old: Vec<u32> = Vec::with_capacity(m);
        let mut new_ord: u32 = 0;
        for (old, slot) in old_to_new.iter_mut().enumerate() {
            if !is_tombstoned_bit(&self.tombstones, old) {
                *slot = new_ord;
                new_to_old.push(old as u32);
                new_ord += 1;
            }
        }
        debug_assert_eq!(new_ord as usize, m);

        // Build compacted vector store (always Owned after consolidation).
        let old_vecs = self.vectors.as_slice()?;
        let mut new_vecs: Vec<f32> = Vec::with_capacity(m * self.dimensions);
        for &old in &new_to_old {
            let src =
                &old_vecs[old as usize * self.dimensions..(old as usize + 1) * self.dimensions];
            new_vecs.extend_from_slice(src);
        }

        // Build compacted adjacency lists with remapped ordinals.
        let mut new_adj: Vec<Vec<u32>> = vec![Vec::new(); m];
        for new_u in 0..m {
            let old_u = new_to_old[new_u] as usize;
            let remapped: Vec<u32> = self.graph.adjacency()[old_u]
                .iter()
                .filter_map(|&old_v| {
                    let nv = old_to_new[old_v as usize];
                    if nv == u32::MAX {
                        None // tombstoned target — drop
                    } else {
                        Some(nv)
                    }
                })
                .collect();
            new_adj[new_u] = remapped;
        }

        // Remap medoid.
        let old_medoid = self.graph.medoid() as usize;
        let new_medoid = old_to_new[old_medoid];
        debug_assert!(
            new_medoid != u32::MAX,
            "consolidate: medoid {old_medoid} is tombstoned — invariant violated"
        );

        // Swap in new graph state.
        let mut new_graph = VamanaGraph::new(m, new_medoid)?;
        for (i, neighbors) in new_adj.into_iter().enumerate() {
            new_graph.adjacency_mut_for_load()[i] = neighbors;
        }
        new_graph.rebuild_reverse_adj_from_adjacency();

        // Compact the SQ8 code table to match the new ordinal space.
        let codes_view = self.gs_codes.view();
        let new_gs_codes: Vec<GsEncodedVector> = new_to_old
            .iter()
            .map(|&old| GsEncodedVector {
                codes: codes_view.code(old as usize).to_vec(),
            })
            .collect();

        self.graph = new_graph;
        self.vectors = VectorStorage::Owned(new_vecs);
        self.num_vectors = m;
        self.tombstones = tombstone_words_for(m);
        self.tombstone_count = 0;
        self.free_slots.clear();
        self.ops_since_consolidation = 0;
        self.gs_codes = CodeStore::Owned(new_gs_codes);
        self.search_visited.clear();

        Ok(new_to_old)
    }

    /// Soft-delete the node at `node_id` with eager Wolverine 2-hop repair (ADR-052 §2;
    /// see crates/khive-vamana/docs/api/algorithm.md#wolverine-2-hop-repair for the rewire
    /// mechanism). If `node_id` was the medoid, a new medoid is elected (centroid-nearest
    /// live node). Returns an error without mutating any state if the op would leave zero
    /// live nodes.
    pub fn tombstone(&mut self, node_id: u32) -> Result<()> {
        let idx = node_id as usize;
        if idx >= self.num_vectors {
            return Err(VamanaError::invalid_format(format!(
                "tombstone: node_id {node_id} out of range ({} nodes)",
                self.num_vectors
            )));
        }
        if is_tombstoned_bit(&self.tombstones, idx) {
            return Err(VamanaError::invalid_format(format!(
                "tombstone: node_id {node_id} is already tombstoned"
            )));
        }
        // Preflight: reject if this would leave zero live nodes (elect_medoid would fail
        // with EmptyInput; guard here so no state is mutated on the error path).
        if self.tombstone_count + 1 >= self.num_vectors {
            return Err(VamanaError::invalid_format(format!(
                "tombstone: deleting node {node_id} would leave zero live nodes"
            )));
        }

        // Step 1: mark tombstoned, update counters.
        set_tombstone_bit(&mut self.tombstones, idx);
        self.tombstone_count += 1;
        self.ops_since_consolidation += 1;

        let vecs = self.vectors.as_slice()?;
        wolverine_repair(
            vecs,
            self.dimensions,
            &mut self.graph,
            node_id,
            &self.tombstones,
            self.config.alpha,
            self.config.max_degree,
        );

        // Step 9: if the deleted node was the medoid, re-elect.
        if self.graph.medoid() == node_id {
            let new_medoid =
                elect_medoid(vecs, self.dimensions, self.num_vectors, &self.tombstones)?;
            self.graph.set_medoid(new_medoid);
        }

        // Step 10: push to free_slots for future insert recycling (PR3).
        self.free_slots.push(node_id);

        // OQ4: after repair, the deleted node's in-neighbor set must be empty.
        debug_assert!(
            self.graph.reverse_adjacency()[idx].is_empty(),
            "tombstone: node {node_id} still has live in-neighbors post-repair"
        );

        Ok(())
    }

    /// Tombstone a batch of node ordinals, deferring medoid re-election to once per batch.
    ///
    /// Performs all structural rewires (Wolverine 2-hop repair, reverse_adj updates) for
    /// every node in `ordinals` first. Re-elects the medoid exactly once at the end, only
    /// if the current medoid is in the batch. Single-delete callers should use `tombstone()`.
    ///
    /// Returns an error without mutating any state if the batch would leave zero live nodes.
    pub fn tombstone_batch(&mut self, ordinals: &[u32]) -> Result<()> {
        if ordinals.is_empty() {
            return Ok(());
        }

        // Preflight: validate all ordinals and check the all-tombstoned case before any
        // mutation. This keeps the error path clean — no partial state on Err.
        let mut unique_live: std::collections::HashSet<u32> = std::collections::HashSet::new();
        for &node_id in ordinals {
            let idx = node_id as usize;
            if idx >= self.num_vectors {
                return Err(VamanaError::invalid_format(format!(
                    "tombstone_batch: node_id {node_id} out of range ({} nodes)",
                    self.num_vectors
                )));
            }
            if is_tombstoned_bit(&self.tombstones, idx) {
                return Err(VamanaError::invalid_format(format!(
                    "tombstone_batch: node_id {node_id} is already tombstoned"
                )));
            }
            if !unique_live.insert(node_id) {
                return Err(VamanaError::invalid_format(format!(
                    "tombstone_batch: duplicate ordinal {node_id} in batch"
                )));
            }
        }
        let new_live = self.num_vectors - self.tombstone_count - unique_live.len();
        if new_live == 0 {
            return Err(VamanaError::invalid_format(
                "tombstone_batch: batch would leave zero live nodes".into(),
            ));
        }

        // Obtain a read-only slice over the vector store without cloning.
        // Inline the VectorStorage match rather than calling self.vectors.as_slice()
        // (a method call) so the borrow checker sees self.vectors and self.graph /
        // self.tombstones as separate fields and allows the simultaneous &mut borrows
        // inside the loop.
        let vecs: &[f32] = match &self.vectors {
            VectorStorage::Owned(v) => v.as_slice(),
            #[cfg(feature = "mmap")]
            VectorStorage::Mmap { mmap, len_f32 } => {
                let floats: &[f32] = bytemuck::try_cast_slice(mmap.as_ref().as_ref())
                    .map_err(|_| VamanaError::invalid_format("vector mmap cast failed".into()))?;
                if floats.len() != *len_f32 {
                    return Err(VamanaError::invalid_format(format!(
                        "mmap f32 length {} != expected {}",
                        floats.len(),
                        len_f32
                    )));
                }
                floats
            }
        };

        let mut medoid_affected = false;

        for &node_id in ordinals {
            let idx = node_id as usize;

            // Track whether the current medoid is in this batch.
            if self.graph.medoid() == node_id {
                medoid_affected = true;
            }

            set_tombstone_bit(&mut self.tombstones, idx);
            self.tombstone_count += 1;
            self.ops_since_consolidation += 1;

            wolverine_repair(
                vecs,
                self.dimensions,
                &mut self.graph,
                node_id,
                &self.tombstones,
                self.config.alpha,
                self.config.max_degree,
            );

            self.free_slots.push(node_id);
        }

        // Single medoid re-election after all rewires — O(N*dims) once, not K times.
        if medoid_affected {
            let new_medoid =
                elect_medoid(vecs, self.dimensions, self.num_vectors, &self.tombstones)?;
            self.graph.set_medoid(new_medoid);
        }

        Ok(())
    }

    /// Tombstone a batch without Wolverine rewiring — test support only, builds the OQ1
    /// no-repair control. See crates/khive-vamana/docs/testing.md#oq1-no-repair-control.
    #[doc(hidden)]
    pub fn tombstone_batch_no_repair(&mut self, ordinals: &[u32]) -> Result<()> {
        if ordinals.is_empty() {
            return Ok(());
        }

        // Same preflight as tombstone_batch.
        let mut unique_live: std::collections::HashSet<u32> = std::collections::HashSet::new();
        for &node_id in ordinals {
            let idx = node_id as usize;
            if idx >= self.num_vectors {
                return Err(VamanaError::invalid_format(format!(
                    "tombstone_batch_no_repair: node_id {node_id} out of range ({} nodes)",
                    self.num_vectors
                )));
            }
            if is_tombstoned_bit(&self.tombstones, idx) {
                return Err(VamanaError::invalid_format(format!(
                    "tombstone_batch_no_repair: node_id {node_id} is already tombstoned"
                )));
            }
            if !unique_live.insert(node_id) {
                return Err(VamanaError::invalid_format(format!(
                    "tombstone_batch_no_repair: duplicate ordinal {node_id} in batch"
                )));
            }
        }
        let new_live = self.num_vectors - self.tombstone_count - unique_live.len();
        if new_live == 0 {
            return Err(VamanaError::invalid_format(
                "tombstone_batch_no_repair: batch would leave zero live nodes".into(),
            ));
        }

        let mut medoid_affected = false;

        for &node_id in ordinals {
            let idx = node_id as usize;

            if self.graph.medoid() == node_id {
                medoid_affected = true;
            }

            set_tombstone_bit(&mut self.tombstones, idx);
            self.tombstone_count += 1;
            self.ops_since_consolidation += 1;

            // No Wolverine rewire. Just clear the deleted node's own forward adjacency so
            // there are no outgoing edges from a dead node (reverse_adj updated in lockstep).
            self.graph
                .replace_adjacency_and_update_reverse(node_id, Vec::new());

            self.free_slots.push(node_id);
        }

        if medoid_affected {
            let vecs = self.vectors.as_slice()?.to_vec();
            let new_medoid =
                elect_medoid(&vecs, self.dimensions, self.num_vectors, &self.tombstones)?;
            self.graph.set_medoid(new_medoid);
        }

        Ok(())
    }
}
