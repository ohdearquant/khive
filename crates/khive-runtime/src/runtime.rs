//! KhiveRuntime — composable handle to all storage capabilities.

use std::sync::{Arc, RwLock};

use khive_db::StorageBackend;
use khive_gate::{ActorRef, AllowAllGate, GateRef};
use khive_storage::{EntityStore, EventStore, GraphStore, NoteStore, SqlAccess};
use khive_types::{EdgeEndpointRule, Namespace};
use lattice_embed::{
    CachedEmbeddingService, EmbeddingModel, EmbeddingService, NativeEmbeddingService,
};
use tokio::sync::OnceCell;

use crate::error::RuntimeResult;

// ---- Sealed token ----

mod private {
    #[derive(Clone, Debug)]
    pub(crate) struct Sealed;
}

/// Authorization proof that a caller is permitted to access a specific namespace.
///
/// Created by [`VerbRegistry::dispatch`] after the gate approves the request.
/// The sealed inner field prevents external code from constructing a token
/// without going through the authorization path.
#[derive(Clone, Debug)]
pub struct NamespaceToken {
    namespace: Namespace,
    actor: ActorRef,
    _sealed: private::Sealed,
}

impl NamespaceToken {
    /// Mint an authorized token. Only callable from within `khive-runtime`.
    pub(crate) fn mint_authorized(namespace: Namespace, actor: ActorRef) -> Self {
        Self {
            namespace,
            actor,
            _sealed: private::Sealed,
        }
    }

    /// Convenience constructor for the local namespace with an anonymous actor.
    ///
    /// Suitable for OSS / local-dev use and in-crate tests. In multi-tenant
    /// deployments the gate mints the token via dispatch; callers should not
    /// use this in authenticated contexts.
    pub fn local() -> Self {
        Self::mint_authorized(Namespace::local(), ActorRef::anonymous())
    }

    /// Convenience constructor for a specific namespace with an anonymous actor.
    ///
    /// Intended for tests and OSS use that need to operate in a named namespace.
    pub fn for_namespace(ns: Namespace) -> Self {
        Self::mint_authorized(ns, ActorRef::anonymous())
    }

    pub fn namespace(&self) -> &Namespace {
        &self.namespace
    }

    pub fn actor(&self) -> &ActorRef {
        &self.actor
    }
}

// ---- RuntimeConfig ----

/// Runtime configuration.
#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    /// Path to the SQLite database file. `None` = in-memory (tests).
    pub db_path: Option<std::path::PathBuf>,
    /// Namespace used when no explicit namespace is provided.
    pub default_namespace: Namespace,
    /// Local embedding model. `None` disables embedding and hybrid vector search;
    /// `hybrid_search` then falls back to text-only.
    pub embedding_model: Option<EmbeddingModel>,
    /// Authorization gate consulted before each verb dispatch (ADR-029).
    /// Default: `AllowAllGate` (permissive). For production policy enforcement,
    /// plug in a Rego- or capability-witness-backed impl.
    pub gate: GateRef,
    /// Names of packs the transport layer should register into the VerbRegistry.
    /// The transport layer (e.g. `khive-mcp`) reads this list and instantiates
    /// the matching concrete pack types. Unknown names are reported as errors
    /// by the transport, not silently ignored.
    /// Default: `["kg"]`.
    pub packs: Vec<String>,
}

/// Parse a comma- or whitespace-separated pack list from a single string.
///
/// Empty entries are dropped, surrounding whitespace is trimmed.
pub fn parse_pack_list(s: &str) -> Vec<String> {
    s.split(|c: char| c == ',' || c.is_whitespace())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
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
        let packs = std::env::var("KHIVE_PACKS")
            .ok()
            .map(|s| parse_pack_list(&s))
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| vec!["kg".to_string()]);
        Self {
            db_path,
            default_namespace: Namespace::local(),
            embedding_model,
            gate: Arc::new(AllowAllGate),
            packs,
        }
    }
}

// ---- KhiveRuntime ----

/// Composable runtime handle used by the MCP server.
///
/// Wraps a `StorageBackend` and provides namespace-scoped accessor methods
/// for each storage capability, plus a lazily-loaded embedder.
#[derive(Clone)]
pub struct KhiveRuntime {
    backend: Arc<StorageBackend>,
    config: RuntimeConfig,
    embedder: Arc<OnceCell<Arc<dyn EmbeddingService>>>,
    /// Pack-extensible edge endpoint rules (ADR-031). Shared across clones
    /// via `Arc<RwLock<_>>`; installed once by the transport after the
    /// `VerbRegistry` is built. Empty until installed — base rules
    /// (ADR-002) still apply on their own.
    edge_rules: Arc<RwLock<Vec<EdgeEndpointRule>>>,
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
            edge_rules: Arc::new(RwLock::new(Vec::new())),
        })
    }

    /// Create an in-memory runtime (for tests and ephemeral use).
    pub fn memory() -> RuntimeResult<Self> {
        Self::new(RuntimeConfig {
            db_path: None,
            default_namespace: Namespace::local(),
            embedding_model: None,
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string()],
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

    // ---- Store accessors (token-scoped) ----

    /// Get an EntityStore scoped to the token's namespace.
    pub fn entities(&self, token: &NamespaceToken) -> RuntimeResult<Arc<dyn EntityStore>> {
        Ok(self
            .backend
            .entities_for_namespace(token.namespace().as_str())?)
    }

    /// Get a GraphStore scoped to the token's namespace.
    pub fn graph(&self, token: &NamespaceToken) -> RuntimeResult<Arc<dyn GraphStore>> {
        Ok(self
            .backend
            .graph_for_namespace(token.namespace().as_str())?)
    }

    /// Get a NoteStore scoped to the token's namespace.
    pub fn notes(&self, token: &NamespaceToken) -> RuntimeResult<Arc<dyn NoteStore>> {
        Ok(self
            .backend
            .notes_for_namespace(token.namespace().as_str())?)
    }

    /// Get an EventStore scoped to the token's namespace.
    pub fn events(&self, token: &NamespaceToken) -> RuntimeResult<Arc<dyn EventStore>> {
        Ok(self
            .backend
            .events_for_namespace(token.namespace().as_str())?)
    }

    /// Get the raw SQL access capability (for ad-hoc queries).
    pub fn sql(&self) -> Arc<dyn SqlAccess> {
        self.backend.sql()
    }

    /// Get a VectorStore for the configured embedding model, scoped to the token's namespace.
    ///
    /// Returns `Unconfigured("embedding_model")` if no model is set.
    pub fn vectors(
        &self,
        token: &NamespaceToken,
    ) -> RuntimeResult<Arc<dyn khive_storage::VectorStore>> {
        let model = self
            .config
            .embedding_model
            .ok_or_else(|| crate::RuntimeError::Unconfigured("embedding_model".into()))?;
        Ok(self.backend.vectors_for_namespace(
            &vec_model_key(model),
            model.dimensions(),
            token.namespace().as_str(),
        )?)
    }

    /// Get a TextSearch index for the token's namespace entity corpus.
    pub fn text(
        &self,
        token: &NamespaceToken,
    ) -> RuntimeResult<Arc<dyn khive_storage::TextSearch>> {
        let key = format!("entities_{}", sanitize_key(token.namespace().as_str()));
        Ok(self.backend.text(&key)?)
    }

    /// Get a TextSearch index for the token's namespace notes corpus.
    pub fn text_for_notes(
        &self,
        token: &NamespaceToken,
    ) -> RuntimeResult<Arc<dyn khive_storage::TextSearch>> {
        let key = format!("notes_{}", sanitize_key(token.namespace().as_str()));
        Ok(self.backend.text(&key)?)
    }

    /// Install the pack-aggregated edge endpoint rules (ADR-031).
    ///
    /// Called by the transport layer after the `VerbRegistry` is built so
    /// that runtime-layer edge validation (in `validate_edge_relation_endpoints`)
    /// can consult pack rules in addition to the ADR-002 base contract. Idempotent:
    /// later calls overwrite the previous rule set.
    pub fn install_edge_rules(&self, rules: Vec<EdgeEndpointRule>) {
        if let Ok(mut guard) = self.edge_rules.write() {
            *guard = rules;
        }
    }

    /// Snapshot of currently-installed pack edge rules.
    pub(crate) fn pack_edge_rules(&self) -> Vec<EdgeEndpointRule> {
        self.edge_rules
            .read()
            .map(|g| g.clone())
            .unwrap_or_default()
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
            default_namespace: Namespace::parse("test").unwrap(),
            embedding_model: None,
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string()],
        };
        let rt = KhiveRuntime::new(config).expect("file runtime should create");
        assert!(path.exists());
        assert_eq!(rt.config().default_namespace.as_str(), "test");
    }

    #[test]
    fn store_accessors_return_ok() {
        let rt = KhiveRuntime::memory().unwrap();
        let tok = NamespaceToken::local();
        assert!(rt.entities(&tok).is_ok());
        assert!(rt.graph(&tok).is_ok());
        assert!(rt.notes(&tok).is_ok());
        assert!(rt.events(&tok).is_ok());
    }

    #[test]
    fn vectors_returns_unconfigured_without_model() {
        let rt = KhiveRuntime::memory().unwrap();
        let tok = NamespaceToken::local();
        match rt.vectors(&tok) {
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
    fn default_config_uses_allow_all_gate() {
        let cfg = RuntimeConfig::default();
        assert_eq!(cfg.default_namespace.as_str(), "local");
        let _: GateRef = cfg.gate.clone();
    }

    #[test]
    fn parse_pack_list_handles_comma_and_whitespace() {
        assert_eq!(parse_pack_list("kg"), vec!["kg".to_string()]);
        assert_eq!(
            parse_pack_list("kg,gtd"),
            vec!["kg".to_string(), "gtd".to_string()]
        );
        assert_eq!(
            parse_pack_list("  kg ,  gtd  "),
            vec!["kg".to_string(), "gtd".to_string()]
        );
        assert_eq!(
            parse_pack_list("kg gtd"),
            vec!["kg".to_string(), "gtd".to_string()]
        );
        assert_eq!(parse_pack_list(",,"), Vec::<String>::new());
        assert_eq!(parse_pack_list(""), Vec::<String>::new());
    }

    #[test]
    fn default_config_packs_falls_back_to_kg() {
        let prior = std::env::var("KHIVE_PACKS").ok();
        // SAFETY: test function runs single-threaded; no other threads read or write KHIVE_PACKS.
        unsafe {
            std::env::remove_var("KHIVE_PACKS");
        }
        let cfg = RuntimeConfig::default();
        assert_eq!(cfg.packs, vec!["kg".to_string()]);
        if let Some(v) = prior {
            // SAFETY: single-threaded test cleanup; restores KHIVE_PACKS to its prior value.
            unsafe {
                std::env::set_var("KHIVE_PACKS", v);
            }
        }
    }

    #[test]
    fn default_config_uses_minilm_when_env_unset() {
        let prior = std::env::var("KHIVE_EMBEDDING_MODEL").ok();
        // SAFETY: tests are serial by default for env mutation here; if other tests
        // mutate this var, mark them with the same scope.
        unsafe {
            std::env::remove_var("KHIVE_EMBEDDING_MODEL");
        }
        let cfg = RuntimeConfig::default();
        assert_eq!(cfg.embedding_model, Some(EmbeddingModel::AllMiniLmL6V2));
        if let Some(v) = prior {
            // SAFETY: single-threaded test cleanup; restores KHIVE_EMBEDDING_MODEL to its prior value.
            unsafe {
                std::env::set_var("KHIVE_EMBEDDING_MODEL", v);
            }
        }
    }
}
