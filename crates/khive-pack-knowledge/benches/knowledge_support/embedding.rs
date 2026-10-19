use std::sync::Arc;

use async_trait::async_trait;
use khive_runtime::EmbedderProvider;
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};

pub const MODEL: EmbeddingModel = EmbeddingModel::AllMiniLmL6V2;
pub struct Synthetic;

fn passage(text: &str) -> Vec<f32> {
    let digest = blake3::hash(text.as_bytes());
    let mut state = u64::from_le_bytes(digest.as_bytes()[..8].try_into().unwrap()) | 1;
    let mut vector: Vec<f32> = (0..MODEL.native_dimensions())
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state as u32 as f32 / u32::MAX as f32 - 0.5) * 0.01
        })
        .collect();
    // Drafts are closest to the fixed query axis. Small deterministic noise
    // keeps different subjects distinct; this is not a semantic-quality model.
    vector[usize::from(!text.contains("draftmarker"))] += 1.0;
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    for value in &mut vector {
        *value /= norm;
    }
    vector
}

#[async_trait]
impl EmbeddingService for Synthetic {
    async fn embed(
        &self,
        texts: &[String],
        model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        assert_eq!(model, MODEL);
        Ok(texts.iter().map(|text| passage(text)).collect())
    }

    async fn embed_query(
        &self,
        texts: &[String],
        model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        assert_eq!(model, MODEL);
        let mut query = vec![0.0; MODEL.native_dimensions()];
        query[0] = 1.0;
        Ok(texts.iter().map(|_| query.clone()).collect())
    }

    fn supports_model(&self, model: EmbeddingModel) -> bool {
        model == MODEL
    }
    fn name(&self) -> &'static str {
        "knowledge-benchmark-synthetic-384"
    }
}

#[async_trait]
impl EmbedderProvider for Synthetic {
    fn name(&self) -> &str {
        "all-minilm-l6-v2"
    }
    fn dimensions(&self) -> usize {
        MODEL.native_dimensions()
    }
    async fn build(&self) -> Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
        Ok(Arc::new(Synthetic))
    }
}
