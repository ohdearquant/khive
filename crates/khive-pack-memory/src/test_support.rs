//! Shared test-only fixtures for the memory pack's unit tests.

use std::sync::Arc;

use async_trait::async_trait;
use khive_runtime::EmbedderProvider;
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};

/// Deterministic embedding service: a distinct vector per unique text via an FNV
/// hash. Not semantically meaningful, but reproducible -- identical input text
/// always yields an identical vector, which is all a cosine-similarity vector leg
/// needs to find a seeded note by exact content match.
struct HashVecService {
    dims: usize,
}

fn fnv_to_vec(text: &str, dims: usize) -> Vec<f32> {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0001_0000_01b3);
    }
    let mut v = Vec::with_capacity(dims);
    let mut s = h;
    for _ in 0..dims {
        s = s
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        v.push(((s >> 33) as f32) / (0x7fff_ffff_u32 as f32) - 1.0);
    }
    v
}

#[async_trait]
impl EmbeddingService for HashVecService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|t| fnv_to_vec(t, self.dims)).collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "hash-vec"
    }
}

/// Registers a deterministic [`HashVecService`] under a caller-named model with a
/// chosen dimension count, for tests that need a reproducible embedder without
/// lattice weights.
pub(crate) struct HashVecProvider {
    pub(crate) model_name: String,
    pub(crate) dims: usize,
}

#[async_trait]
impl EmbedderProvider for HashVecProvider {
    fn name(&self) -> &str {
        &self.model_name
    }

    fn dimensions(&self) -> usize {
        self.dims
    }

    async fn build(&self) -> Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
        Ok(Arc::new(HashVecService { dims: self.dims }))
    }
}
