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

/// Feature-hashing embedder: tokenise → per-token (dim, sign) → accumulate → L2-normalise.
///
/// Lexically similar texts share tokens and therefore accumulate signal in the
/// same dimensions with the same sign, producing correlated vectors. This lets
/// the gate exercise the vector/ANN/fusion legs rather than treating them as
/// pure noise (as the previous whole-text FNV avalanche did).
fn hash_embed(text: &str) -> Vec<f32> {
    let mut v = vec![0.0f32; DIM];
    for token in text
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
    {
        let h = fnv1a_64(token.as_bytes());
        let dim = ((h >> 1) as usize) % DIM;
        let sign = if h & 1 == 0 { 1.0_f32 } else { -1.0_f32 };
        v[dim] += sign;
    }
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
    v.iter_mut().for_each(|x| *x /= norm);
    v
}

#[cfg(test)]
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Same-cluster strings (sharing several topic words) must be more similar
    /// to each other than cross-cluster strings (disjoint vocabularies).
    /// This locks the clustering property of the feature-hashing embedder.
    #[test]
    fn feature_hash_same_cluster_more_similar_than_cross_cluster() {
        let a = hash_embed("knowledge graph entity edge relation ontology");
        let b = hash_embed("knowledge graph concept node link schema");
        let c = hash_embed("Rust cargo clippy fmt workspace crate lint");

        let sim_same = cosine(&a, &b);
        let sim_cross = cosine(&a, &c);

        assert!(
            sim_same > sim_cross,
            "same-cluster cosine ({sim_same:.4}) must be > cross-cluster cosine ({sim_cross:.4})"
        );
    }
}
