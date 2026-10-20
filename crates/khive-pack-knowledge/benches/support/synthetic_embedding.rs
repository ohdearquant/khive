//! Deterministic subject/revision vectors shared by synthetic benchmark cohorts.

use async_trait::async_trait;
use khive_runtime::EmbedderProvider;
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use std::sync::Arc;

pub const MODEL: EmbeddingModel = EmbeddingModel::AllMiniLmL6V2;

pub struct SyntheticProvider;
struct SyntheticService;

#[async_trait]
impl EmbedderProvider for SyntheticProvider {
    fn name(&self) -> &str {
        "all-minilm-l6-v2"
    }
    fn dimensions(&self) -> usize {
        MODEL.native_dimensions()
    }
    async fn build(&self) -> Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
        Ok(Arc::new(SyntheticService))
    }
}

#[async_trait]
impl EmbeddingService for SyntheticService {
    async fn embed(
        &self,
        texts: &[String],
        model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        assert_eq!(model, MODEL);
        Ok(texts
            .iter()
            .map(|text| {
                let mut subject: usize = 0;
                let mut version: usize = 0;
                for word in text.split_whitespace() {
                    let word = word
                        .trim_matches(|c: char| !c.is_alphanumeric())
                        .to_ascii_lowercase();
                    if let Some(value) = word.strip_prefix("subject") {
                        subject = value.parse().unwrap_or(subject);
                    }
                    if let Some(value) = word.strip_prefix("visiblemarker") {
                        version = value.parse().unwrap_or(version);
                    }
                }
                let digest = blake3::hash(format!("subject{subject}:revision{version}").as_bytes());
                let mut state = u64::from_le_bytes(digest.as_bytes()[..8].try_into().unwrap()) | 1;
                let mut vector: Vec<f32> = (0..MODEL.native_dimensions())
                    .map(|_| {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        (state as u32 as f32 / u32::MAX as f32) * 2.0 - 1.0
                    })
                    .collect();
                let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
                for value in &mut vector {
                    *value /= norm;
                }
                vector
            })
            .collect())
    }
    fn supports_model(&self, model: EmbeddingModel) -> bool {
        model == MODEL
    }
    fn name(&self) -> &'static str {
        "tail-replay-synthetic-384"
    }
}
