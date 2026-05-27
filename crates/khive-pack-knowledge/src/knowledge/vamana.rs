//! Vamana ANN bridge — parallel semantic signal for `knowledge.search`.
//!
//! Wraps `khive_vamana::VamanaIndex` with an ID map (u32 → UUID) so search
//! results can be fused with FTS5 candidates via RRF.

use std::sync::Arc;

use khive_vamana::{VamanaConfig, VamanaIndex};
use tokio::sync::RwLock;
use uuid::Uuid;

pub(crate) struct AnnBridge {
    index: VamanaIndex,
    id_map: Vec<Uuid>,
}

pub(crate) type SharedAnn = Arc<RwLock<Option<AnnBridge>>>;

pub(crate) fn new_shared() -> SharedAnn {
    Arc::new(RwLock::new(None))
}

impl AnnBridge {
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
        let index = VamanaIndex::build(&vectors, cfg).map_err(|e| format!("{e}"))?;
        Ok(Self { index, id_map })
    }

    pub fn search(&self, query: &[f32], k: usize) -> Vec<(Uuid, f32)> {
        let mut q = query.to_vec();
        l2_normalize(&mut q);
        match self.index.search(&q, k) {
            Ok(results) => results
                .into_iter()
                .filter_map(|(idx, dist)| {
                    self.id_map.get(idx as usize).map(|uuid| {
                        // L2² → cosine: cos(a,b) = 1 - L2²(a,b)/2 for unit vectors
                        let cosine = 1.0 - dist / 2.0;
                        (*uuid, cosine.max(0.0))
                    })
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
}

fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-8 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}
