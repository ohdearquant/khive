// Bench-only deterministic embedder — never compiled into release/publish builds.
// Registered under the `all-minilm-l6-v2` name so it overrides the real lattice
// provider for that slot while leaving `compute_config_id` unchanged (the
// configured model NAME stays `all-minilm-l6-v2`; only the impl changes).

use std::sync::Arc;

use async_trait::async_trait;
use khive_runtime::{EmbedderProvider, RuntimeResult};
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};

const DIM: usize = 384;
pub(crate) const MODEL_NAME: &str = "all-minilm-l6-v2";

fn fnv1a_64(data: &[u8]) -> u64 {
    const BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = BASIS;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(PRIME);
    }
    h
}

fn hash_embed(text: &str) -> Vec<f32> {
    let bytes = text.as_bytes();
    let mut v = vec![0.0f32; DIM];
    for (i, slot) in v.iter_mut().enumerate() {
        let seed = (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        let salted: Vec<u8> = seed
            .to_le_bytes()
            .iter()
            .chain(bytes.iter())
            .copied()
            .collect();
        let h = fnv1a_64(&salted);
        let h2 = fnv1a_64(&h.to_le_bytes());
        let raw = (h ^ h2.rotate_right(17)) as i64;
        *slot = raw as f32;
    }
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
    v.iter_mut().for_each(|x| *x /= norm);
    v
}

struct FeatureHashService;

#[async_trait]
impl EmbeddingService for FeatureHashService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|t| hash_embed(t)).collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        MODEL_NAME
    }
}

pub(crate) struct FeatureHashProvider;

#[async_trait]
impl EmbedderProvider for FeatureHashProvider {
    fn name(&self) -> &str {
        MODEL_NAME
    }

    fn dimensions(&self) -> usize {
        DIM
    }

    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(Arc::new(FeatureHashService))
    }
}
