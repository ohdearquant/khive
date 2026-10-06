use std::sync::Arc;

use async_trait::async_trait;
use khive_runtime::EmbedderProvider;
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};

/// Trivial constant-vector embedding service for testing without real model weights.
/// The `_model` parameter is ignored; returns a synthetic `dims × seed` vector.
struct ConstVecService {
    dims: usize,
    seed: f32,
}

#[async_trait]
impl EmbeddingService for ConstVecService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| vec![self.seed; self.dims]).collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "const-vec"
    }
}

pub struct ConstVecProvider {
    provider_name: String,
    dims: usize,
    seed: f32,
}

impl ConstVecProvider {
    pub fn new(name: &str, dims: usize, seed: f32) -> Self {
        Self {
            provider_name: name.to_owned(),
            dims,
            seed,
        }
    }
}

#[async_trait]
impl EmbedderProvider for ConstVecProvider {
    fn name(&self) -> &str {
        &self.provider_name
    }

    fn dimensions(&self) -> usize {
        self.dims
    }

    async fn build(&self) -> Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
        Ok(Arc::new(ConstVecService {
            dims: self.dims,
            seed: self.seed,
        }))
    }
}
