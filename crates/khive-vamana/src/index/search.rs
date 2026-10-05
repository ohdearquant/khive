use super::{require_finite, Result, VamanaError, VamanaIndex, VisitedSet};
use crate::graph::{greedy_search_inner, greedy_search_inner_sq8};

impl VamanaIndex {
    /// Search for `k` nearest neighbors. Errors if dimension mismatch or non-finite query values.
    ///
    /// Uses `GsSq8Codec` for acquisition-tier traversal; returned distances are exact f32 L2²
    /// (ADR-052 §1 two-tier: SQ8 for candidate selection, exact f32 for final results).
    pub fn search(&self, query: &[f32], k: usize) -> Result<Vec<(u32, f32)>> {
        if !self.validate_search(query, k)? {
            return Ok(Vec::new());
        }
        let mut visited = self.search_visited.checkout(self.num_vectors);
        self.search_with_visited(query, k, &mut visited)
    }

    /// Search with a fresh visited tracker, without checking out or retaining pooled scratch.
    ///
    /// This provides an allocating baseline for comparison with [`Self::search`]. Each
    /// valid, nonzero-`k` call allocates one `u64` mark per vector and releases it on return.
    /// Validation, traversal and ordered results are the same as [`Self::search`].
    pub fn search_allocating(&self, query: &[f32], k: usize) -> Result<Vec<(u32, f32)>> {
        if !self.validate_search(query, k)? {
            return Ok(Vec::new());
        }
        let mut visited = VisitedSet::new(self.num_vectors);
        self.search_with_visited(query, k, &mut visited)
    }

    fn validate_search(&self, query: &[f32], k: usize) -> Result<bool> {
        if query.len() != self.dimensions {
            return Err(VamanaError::DimensionMismatch {
                expected: self.dimensions,
                actual: query.len(),
            });
        }
        if k == 0 {
            return Ok(false);
        }
        require_finite(query, "search query")?;
        Ok(true)
    }

    fn search_with_visited(
        &self,
        query: &[f32],
        k: usize,
        visited: &mut VisitedSet,
    ) -> Result<Vec<(u32, f32)>> {
        let tombstones = if self.tombstone_count > 0 {
            Some(self.tombstones.as_slice())
        } else {
            None
        };
        // OOD fallback (ADR-052 §2): if any query component lies outside the codec's
        // trained range [min_d, min_d + 255·gs], encoding clamps that dimension and
        // SQ8 distances cannot correctly order the frontier. Fall back to exact f32
        // greedy search for this query; in-distribution queries keep the SQ8 path.
        let result = if self.gs_codec.is_in_distribution(query) {
            let query_enc = self.gs_codec.encode(query);
            greedy_search_inner_sq8(
                self.vectors()?,
                self.dimensions,
                self.gs_codes.view(),
                &self.gs_codec,
                self.graph.adjacency(),
                query,
                &query_enc.codes,
                self.graph.medoid(),
                k,
                self.config.search_list_size,
                visited,
                tombstones,
            )
        } else {
            greedy_search_inner(
                self.vectors()?,
                self.dimensions,
                self.graph.adjacency(),
                query,
                self.graph.medoid(),
                k,
                self.config.search_list_size,
                visited,
                tombstones,
            )
        };

        let mut output = result.results;
        output.sort_unstable_by(|(a_id, a_d), (b_id, b_d)| {
            a_d.total_cmp(b_d).then_with(|| a_id.cmp(b_id))
        });
        Ok(output)
    }
}
