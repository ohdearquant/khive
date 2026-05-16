//! KhiveRuntime — composable handle to all storage capabilities.

use std::sync::Arc;

use khive_db::StorageBackend;
use khive_storage::{EntityStore, EventStore, GraphStore, NoteStore, SqlAccess};
use lattice_embed::{
    CachedEmbeddingService, EmbeddingModel, EmbeddingService, NativeEmbeddingService,
};
use tokio::sync::OnceCell;

use crate::error::RuntimeResult;

/// Runtime configuration.
#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    /// Path to the SQLite database file. `None` = in-memory (tests).
    pub db_path: Option<std::path::PathBuf>,
    /// Namespace used when no explicit namespace is provided.
    pub default_namespace: String,
    /// Local embedding model. `None` disables embedding and hybrid vector search;
    /// `hybrid_search` then falls back to text-only.
    pub embedding_model: Option<EmbeddingModel>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        let db_path = std::env::var("HOME")
            .ok()
            .map(|h| std::path::PathBuf::from(h).join(".khive/khive-graph.db"));
        let embedding_model = std::env::var("KHIVE_EMBEDDING_MODEL")
            .ok()
            .and_then(|s| s.parse().ok())
            .or(Some(EmbeddingModel::AllMiniLmL6V2));
        Self {
            db_path,
            default_namespace: "local".to_string(),
            embedding_model,
        }
    }
}

/// Composable runtime handle used by the MCP server.
///
/// Wraps a `StorageBackend` and provides namespace-scoped accessor methods
/// for each storage capability, plus a lazily-loaded embedder.
#[derive(Clone)]
pub struct KhiveRuntime {
    backend: Arc<StorageBackend>,
    config: RuntimeConfig,
    embedder: Arc<OnceCell<Arc<dyn EmbeddingService>>>,
}

impl KhiveRuntime {
    /// Create a new runtime with the given config.
    pub fn new(config: RuntimeConfig) -> RuntimeResult<Self> {
        let backend = match &config.db_path {
            Some(path) => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                StorageBackend::sqlite(path)?
            }
            None => StorageBackend::memory()?,
        };
        Ok(Self {
            backend: Arc::new(backend),
            config,
            embedder: Arc::new(OnceCell::new()),
        })
    }

    /// Create an in-memory runtime (for tests and ephemeral use).
    pub fn memory() -> RuntimeResult<Self> {
        Self::new(RuntimeConfig {
            db_path: None,
            default_namespace: "local".to_string(),
            embedding_model: None,
        })
    }

    /// Return a reference to the runtime config.
    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }

    /// Return a reference to the underlying storage backend.
    pub fn backend(&self) -> &StorageBackend {
        &self.backend
    }

    /// Resolve namespace: use provided value or fall back to `default_namespace`.
    pub fn ns<'a>(&'a self, namespace: Option<&'a str>) -> &'a str {
        namespace.unwrap_or(&self.config.default_namespace)
    }

    // ---- Store accessors ----

    /// Get an EntityStore scoped to the given namespace (or default).
    pub fn entities(&self, namespace: Option<&str>) -> RuntimeResult<Arc<dyn EntityStore>> {
        Ok(self.backend.entities_for_namespace(self.ns(namespace))?)
    }

    /// Get a GraphStore scoped to the given namespace (or default).
    pub fn graph(&self, namespace: Option<&str>) -> RuntimeResult<Arc<dyn GraphStore>> {
        Ok(self.backend.graph_for_namespace(self.ns(namespace))?)
    }

    /// Get a NoteStore scoped to the given namespace (or default).
    pub fn notes(&self, namespace: Option<&str>) -> RuntimeResult<Arc<dyn NoteStore>> {
        Ok(self.backend.notes_for_namespace(self.ns(namespace))?)
    }

    /// Get an EventStore scoped to the given namespace (or default).
    pub fn events(&self, namespace: Option<&str>) -> RuntimeResult<Arc<dyn EventStore>> {
        Ok(self.backend.events_for_namespace(self.ns(namespace))?)
    }

    /// Get the raw SQL access capability (for ad-hoc queries).
    pub fn sql(&self) -> Arc<dyn SqlAccess> {
        self.backend.sql()
    }

    /// Get a VectorStore for the configured embedding model, scoped to the namespace.
    ///
    /// Returns `Unconfigured("embedding_model")` if no model is set.
    pub fn vectors(
        &self,
        namespace: Option<&str>,
    ) -> RuntimeResult<Arc<dyn khive_storage::VectorStore>> {
        let model = self
            .config
            .embedding_model
            .ok_or_else(|| crate::RuntimeError::Unconfigured("embedding_model".into()))?;
        Ok(self.backend.vectors_for_namespace(
            &vec_model_key(model),
            model.dimensions(),
            self.ns(namespace),
        )?)
    }

    /// Get a TextSearch index for the namespace's entity corpus.
    pub fn text(
        &self,
        namespace: Option<&str>,
    ) -> RuntimeResult<Arc<dyn khive_storage::TextSearch>> {
        let key = format!("entities_{}", sanitize_key(self.ns(namespace)));
        Ok(self.backend.text(&key)?)
    }

    /// Get a TextSearch index for the namespace's notes corpus.
    pub fn text_for_notes(
        &self,
        namespace: Option<&str>,
    ) -> RuntimeResult<Arc<dyn khive_storage::TextSearch>> {
        let key = format!("notes_{}", sanitize_key(self.ns(namespace)));
        Ok(self.backend.text(&key)?)
    }

    /// Get the lazily-initialized embedding service.
    ///
    /// Returns a `CachedEmbeddingService` wrapping a `NativeEmbeddingService`.
    /// First call loads the model (cold start cost); subsequent calls are cheap and
    /// benefit from LRU caching of repeated inputs.
    ///
    /// Returns `Unconfigured("embedding_model")` if no model is set.
    pub async fn embedder(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        let model = self
            .config
            .embedding_model
            .ok_or_else(|| crate::RuntimeError::Unconfigured("embedding_model".into()))?;
        let service = self
            .embedder
            .get_or_init(|| async move {
                let native = Arc::new(NativeEmbeddingService::with_model(model));
                let cached = CachedEmbeddingService::with_default_cache(native);
                Arc::new(cached) as Arc<dyn EmbeddingService>
            })
            .await
            .clone();
        Ok(service)
    }
}

/// Sanitize an embedding model into a valid SQL table suffix.
/// e.g. `bge-small-en-v1.5` -> `bge_small_en_v1_5`
pub(crate) fn vec_model_key(model: EmbeddingModel) -> String {
    sanitize_key(&model.to_string())
}

fn sanitize_key(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_runtime_creates_successfully() {
        let rt = KhiveRuntime::memory().expect("memory runtime should create");
        assert!(rt.config().db_path.is_none());
    }

    #[test]
    fn file_runtime_creates_successfully() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let config = RuntimeConfig {
            db_path: Some(path.clone()),
            default_namespace: "test".to_string(),
            embedding_model: None,
        };
        let rt = KhiveRuntime::new(config).expect("file runtime should create");
        assert!(path.exists());
        assert_eq!(rt.config().default_namespace, "test");
    }

    #[test]
    fn ns_defaults_to_config_namespace() {
        let rt = KhiveRuntime::memory().unwrap();
        assert_eq!(rt.ns(None), "local");
        assert_eq!(rt.ns(Some("custom")), "custom");
    }

    #[test]
    fn store_accessors_return_ok() {
        let rt = KhiveRuntime::memory().unwrap();
        assert!(rt.entities(None).is_ok());
        assert!(rt.graph(None).is_ok());
        assert!(rt.notes(None).is_ok());
        assert!(rt.events(None).is_ok());
    }

    #[test]
    fn vectors_returns_unconfigured_without_model() {
        let rt = KhiveRuntime::memory().unwrap();
        match rt.vectors(None) {
            Err(crate::RuntimeError::Unconfigured(s)) => assert_eq!(s, "embedding_model"),
            Err(other) => panic!("expected Unconfigured, got {:?}", other),
            Ok(_) => panic!("expected Err, got Ok"),
        }
    }

    #[test]
    fn vec_model_key_sanitizes_dots_and_dashes() {
        assert_eq!(
            vec_model_key(EmbeddingModel::BgeSmallEnV15),
            "bge_small_en_v1_5"
        );
        assert_eq!(
            vec_model_key(EmbeddingModel::BgeBaseEnV15),
            "bge_base_en_v1_5"
        );
        assert_eq!(
            vec_model_key(EmbeddingModel::AllMiniLmL6V2),
            "all_minilm_l6_v2"
        );
    }

    #[test]
    fn default_config_uses_minilm_when_env_unset() {
        // Snapshot + clear the env var so this test is deterministic.
        let prior = std::env::var("KHIVE_EMBEDDING_MODEL").ok();
        // SAFETY: tests are serial by default for env mutation here; if other tests
        // mutate this var, mark them with the same scope.
        unsafe {
            std::env::remove_var("KHIVE_EMBEDDING_MODEL");
        }
        let cfg = RuntimeConfig::default();
        assert_eq!(cfg.embedding_model, Some(EmbeddingModel::AllMiniLmL6V2));
        if let Some(v) = prior {
            unsafe {
                std::env::set_var("KHIVE_EMBEDDING_MODEL", v);
            }
        }
    }
}
