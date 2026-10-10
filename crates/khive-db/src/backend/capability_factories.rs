use std::sync::Arc;

use khive_storage::{
    SparseStore, SparseStoreFactory, StorageCapability, StorageResult, TextSearch,
    TextSearchFactory, VectorStore, VectorStoreFactory,
};

use super::core_stores::map_open_error;
use super::StorageBackend;

impl VectorStoreFactory for StorageBackend {
    fn open(
        &self,
        model_key: &str,
        embedding_model: &str,
        dimensions: usize,
        namespace: &str,
    ) -> StorageResult<Arc<dyn VectorStore>> {
        self.vectors_for_namespace(model_key, embedding_model, dimensions, namespace)
            .map_err(|error| {
                map_open_error(error, StorageCapability::Vectors, "vectors_for_namespace")
            })
    }
}

impl SparseStoreFactory for StorageBackend {
    fn open(&self, model_key: &str, namespace: &str) -> StorageResult<Arc<dyn SparseStore>> {
        self.sparse_for_namespace(model_key, namespace)
            .map_err(|error| {
                map_open_error(error, StorageCapability::Sparse, "sparse_for_namespace")
            })
    }
}

impl TextSearchFactory for StorageBackend {
    fn open(&self, table_key: &str, tokenizer: &str) -> StorageResult<Arc<dyn TextSearch>> {
        self.text_with_tokenizer(table_key, tokenizer)
            .map_err(|error| map_open_error(error, StorageCapability::Text, "text_with_tokenizer"))
    }
}

#[cfg(test)]
mod tests;
