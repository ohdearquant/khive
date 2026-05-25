//! KhiveRuntime — composable handle to all storage capabilities.

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use khive_db::StorageBackend;
use khive_gate::{ActorRef, AllowAllGate, GateRef};
use khive_storage::{EntityStore, EventStore, GraphStore, NoteStore, SqlAccess};
use khive_types::{EdgeEndpointRule, Namespace};
use lattice_embed::{
    CachedEmbeddingService, EmbeddingModel, EmbeddingService, NativeEmbeddingService,
};
use tokio::sync::OnceCell;

use crate::error::RuntimeResult;

// ---- BackendId ----

/// Identifies a named backend in a multi-backend deployment (ADR-009, ADR-028).
///
/// The `main` backend is the default single-backend name. Multi-backend deployments
/// assign each `[[backends]]` entry a distinct `BackendId`. The
/// [`SubstrateCoordinator`](kkernel::coordinator::SubstrateCoordinator) in `kkernel`
/// uses `BackendId` for node-to-backend resolution and cross-backend edge routing.
///
/// A single-backend `KhiveRuntime` always has `BackendId("main")` by default.
/// The boot path in `kkernel` or `khive-mcp` sets the id via `RuntimeConfig::backend_id`
/// when constructing per-pack runtimes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BackendId(pub String);

impl BackendId {
    /// The default single-backend name.
    pub const MAIN: &'static str = "main";

    /// Construct from a string name.
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The default `main` backend id.
    pub fn main() -> Self {
        Self(Self::MAIN.to_string())
    }

    /// Return the backend name as a `&str`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for BackendId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

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
    /// Only callable from within `khive-runtime`. External callers must use
    /// [`KhiveRuntime::authorize`] to mint tokens.
    // Used only in #[cfg(test)] blocks within this crate's src/ files.
    #[allow(dead_code)]
    pub(crate) fn local() -> Self {
        Self::mint_authorized(Namespace::local(), ActorRef::anonymous())
    }

    /// Convenience constructor for a specific namespace with an anonymous actor.
    ///
    /// Only callable from within `khive-runtime`. External callers must use
    /// [`KhiveRuntime::authorize`] to mint tokens.
    // Used only in #[cfg(test)] blocks within this crate's src/ files.
    #[allow(dead_code)]
    pub(crate) fn for_namespace(ns: Namespace) -> Self {
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
///
/// Per ADR-028, the `db_path` and `embedding_model` fields are deprecated in favour of
/// constructing the backend externally and calling [`KhiveRuntime::from_backend`].
/// They remain for backward compatibility with tests and single-binary deployments.
#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    /// Path to the SQLite database file. `None` = in-memory (tests).
    ///
    /// Deprecated: use [`KhiveRuntime::from_backend`] instead. The boot path
    /// constructs backends from `khive.toml` (`AppConfig`) and passes them to
    /// `from_backend`. Direct `db_path` usage persists only in tests.
    pub db_path: Option<std::path::PathBuf>,
    /// Namespace used when no explicit namespace is provided.
    pub default_namespace: Namespace,
    /// Local embedding model. `None` disables embedding and hybrid vector search;
    /// `hybrid_search` then falls back to text-only.
    ///
    /// Deprecated: per ADR-028/ADR-031, embedding engines move to a per-pack
    /// `EmbedderRegistry`. This field persists for backward compatibility until
    /// the embedder registry is fully plumbed.
    pub embedding_model: Option<EmbeddingModel>,
    /// Additional embedding models to make available by request name.
    ///
    /// `embedding_model` remains the default used by existing `embed()` and
    /// `embed_batch()` callers. This list adds non-default models that can be
    /// selected with `embedder(name)`, `embed_with_model(...)`, memory
    /// `remember.embedding_model`, and memory `recall.embedding_model`.
    pub additional_embedding_models: Vec<EmbeddingModel>,
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
    /// Identifies this runtime's backend in a multi-backend deployment (ADR-009, ADR-028).
    ///
    /// Set by the boot path when constructing per-pack runtimes from `khive.toml`.
    /// Single-backend deployments use the default `BackendId::MAIN`.
    pub backend_id: BackendId,
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
        let additional_embedding_models = std::env::var("KHIVE_ADDITIONAL_EMBEDDING_MODELS")
            .ok()
            .map(|s| parse_embedding_model_list(&s))
            .unwrap_or_default();
        let packs = std::env::var("KHIVE_PACKS")
            .ok()
            .map(|s| parse_pack_list(&s))
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| vec!["kg".to_string()]);
        Self {
            db_path,
            default_namespace: Namespace::local(),
            embedding_model,
            additional_embedding_models,
            gate: Arc::new(AllowAllGate),
            packs,
            backend_id: BackendId::main(),
        }
    }
}

// ---- KhiveRuntime ----

#[derive(Clone)]
struct EmbedderEntry {
    model: EmbeddingModel,
    cell: Arc<OnceCell<Arc<dyn EmbeddingService>>>,
}

/// Composable runtime handle used by the MCP server.
///
/// Wraps a `StorageBackend` and provides namespace-scoped accessor methods
/// for each storage capability, plus a lazily-loaded embedder.
#[derive(Clone)]
pub struct KhiveRuntime {
    backend: Arc<StorageBackend>,
    config: RuntimeConfig,
    embedders: Arc<HashMap<String, EmbedderEntry>>,
    default_embedder_name: Arc<str>,
    /// Pack-extensible edge endpoint rules (ADR-031). Shared across clones
    /// via `Arc<RwLock<_>>`; installed once by the transport after the
    /// `VerbRegistry` is built. Empty until installed — base rules
    /// (ADR-002) still apply on their own.
    edge_rules: Arc<RwLock<Vec<EdgeEndpointRule>>>,
}

impl KhiveRuntime {
    /// Create a new runtime with the given config.
    ///
    /// The config's `db_path` is used to open or create the SQLite backend.
    /// For the preferred boot path in multi-backend deployments, use
    /// [`from_backend`](Self::from_backend) instead.
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
        register_configured_embedding_models(&backend, &config)?;
        let (embedders, default_embedder_name) = build_embedder_registry(&config);
        Ok(Self {
            backend: Arc::new(backend),
            config,
            embedders: Arc::new(embedders),
            default_embedder_name,
            edge_rules: Arc::new(RwLock::new(Vec::new())),
        })
    }

    /// Construct a runtime from an already-opened backend (ADR-028 boot path).
    ///
    /// This is the preferred constructor for multi-backend deployments. The caller
    /// (boot path in `kkernel` or `khive-mcp`) opens each backend from `khive.toml`,
    /// then constructs a `KhiveRuntime` per pack using this method.
    ///
    /// The returned runtime has `db_path = None` and `embedding_model = None`; all
    /// storage access is through the provided `backend`. Set `backend_id` and
    /// `default_namespace` via the config builder pattern if non-defaults are needed.
    pub fn from_backend(backend: Arc<StorageBackend>, config: RuntimeConfig) -> Self {
        if let Err(err) = register_configured_embedding_models(&backend, &config) {
            tracing::warn!(error = %err, "failed to register configured embedding models");
        }
        let (embedders, default_embedder_name) = build_embedder_registry(&config);
        Self {
            backend,
            config,
            embedders: Arc::new(embedders),
            default_embedder_name,
            edge_rules: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Create an in-memory runtime (for tests and ephemeral use).
    pub fn memory() -> RuntimeResult<Self> {
        Self::new(RuntimeConfig {
            db_path: None,
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string()],
            backend_id: BackendId::main(),
        })
    }

    /// Return the [`BackendId`] for this runtime's backend.
    ///
    /// Used by the [`SubstrateCoordinator`](kkernel::coordinator::SubstrateCoordinator)
    /// to identify which backend owns a given node, and to detect cross-backend merges.
    pub fn backend_id(&self) -> &BackendId {
        &self.config.backend_id
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
        let model = self.resolve_embedding_model(None)?;
        self.vectors_for_embedding_model(token, model)
    }

    /// Get a VectorStore for a specific named embedding model, scoped to the token's namespace.
    pub fn vectors_for_model(
        &self,
        token: &NamespaceToken,
        model_name: &str,
    ) -> RuntimeResult<Arc<dyn khive_storage::VectorStore>> {
        let model = self.resolve_embedding_model(Some(model_name))?;
        self.vectors_for_embedding_model(token, model)
    }

    fn vectors_for_embedding_model(
        &self,
        token: &NamespaceToken,
        model: EmbeddingModel,
    ) -> RuntimeResult<Arc<dyn khive_storage::VectorStore>> {
        Ok(self.backend.vectors_for_namespace(
            &vec_model_key(model),
            &model.to_string(),
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

    /// Mint an authorization token for the given namespace.
    ///
    /// This is the official OSS API for obtaining a [`NamespaceToken`]. In
    /// local / single-user mode (the default) this always succeeds — there is
    /// no multi-tenant gate to consult. Multi-tenant deployments replace the
    /// gate with a policy-backed impl; this method would then enforce it.
    pub fn authorize(&self, ns: Namespace) -> NamespaceToken {
        NamespaceToken::mint_authorized(ns, ActorRef::anonymous())
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

    /// Return the name of the default embedding model (empty string if none configured).
    pub fn default_embedder_name(&self) -> &str {
        self.default_embedder_name.as_ref()
    }

    /// Resolve a model name (or `None` for the default) to an `EmbeddingModel`.
    ///
    /// Returns `UnknownModel` if the name is not in the registry, or
    /// `Unconfigured` if `None` is passed and no default model is set.
    pub fn resolve_embedding_model(&self, name: Option<&str>) -> RuntimeResult<EmbeddingModel> {
        let model = match name {
            Some(raw) => parse_embedding_model_alias(raw)
                .ok_or_else(|| crate::RuntimeError::UnknownModel(raw.to_string()))?,
            None => self
                .config
                .embedding_model
                .ok_or_else(|| crate::RuntimeError::Unconfigured("embedding_model".into()))?,
        };
        let key = model.to_string();
        if self.embedders.contains_key(&key) {
            Ok(model)
        } else {
            Err(crate::RuntimeError::UnknownModel(
                name.unwrap_or_else(|| self.default_embedder_name())
                    .to_string(),
            ))
        }
    }

    /// Names of all registered embedding models in this runtime.
    ///
    /// Useful for operations that must touch every model's storage (e.g.,
    /// scoped vector deletion on note delete — codex High 2 (PR #407)).
    /// The default model is included.
    pub fn registered_embedding_model_names(&self) -> Vec<String> {
        self.embedders.keys().cloned().collect()
    }

    /// Get the lazily-initialized embedding service for the named model.
    ///
    /// Returns a `CachedEmbeddingService` wrapping a `NativeEmbeddingService`.
    /// First call loads the model (cold start cost); subsequent calls are cheap and
    /// benefit from LRU caching of repeated inputs.
    pub async fn embedder(&self, name: &str) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        let model = self.resolve_embedding_model(Some(name))?;
        let key = model.to_string();
        let entry = self
            .embedders
            .get(&key)
            .ok_or_else(|| crate::RuntimeError::UnknownModel(name.to_string()))?
            .clone();
        Ok(entry
            .cell
            .get_or_init(|| async move {
                let native = Arc::new(NativeEmbeddingService::with_model(entry.model));
                let cached = CachedEmbeddingService::with_default_cache(native);
                Arc::new(cached) as Arc<dyn EmbeddingService>
            })
            .await
            .clone())
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

fn build_embedder_registry(config: &RuntimeConfig) -> (HashMap<String, EmbedderEntry>, Arc<str>) {
    let mut embedders = HashMap::new();
    for model in configured_embedding_models(config) {
        embedders.insert(
            model.to_string(),
            EmbedderEntry {
                model,
                cell: Arc::new(OnceCell::new()),
            },
        );
    }
    let default_embedder_name = config
        .embedding_model
        .map(|model| Arc::<str>::from(model.to_string()))
        .unwrap_or_else(|| Arc::<str>::from(""));
    (embedders, default_embedder_name)
}

fn configured_embedding_models(config: &RuntimeConfig) -> Vec<EmbeddingModel> {
    let mut models = Vec::new();
    if let Some(model) = config.embedding_model {
        models.push(model);
    }
    models.extend(config.additional_embedding_models.iter().copied());
    models.sort_by_key(|model| model.to_string());
    models.dedup();
    models
}

fn register_configured_embedding_models(
    backend: &StorageBackend,
    config: &RuntimeConfig,
) -> RuntimeResult<()> {
    for model in configured_embedding_models(config) {
        backend.register_embedding_model(
            &model.to_string(),
            model.model_id(),
            model.key_version(),
            model.dimensions() as u32,
        )?;
    }
    Ok(())
}

/// Parse a comma- or whitespace-separated list of embedding model names.
fn parse_embedding_model_list(s: &str) -> Vec<EmbeddingModel> {
    parse_pack_list(s)
        .into_iter()
        .filter_map(|raw| {
            let parsed = parse_embedding_model_alias(&raw);
            if parsed.is_none() && !raw.trim().is_empty() {
                // Codex Medium (PR #407): silent filter_map masks operator typos. Warn loudly
                // so misconfiguration surfaces at startup rather than as an UnknownModel error
                // at request time. We do not fail startup — a partially valid list still
                // produces a functional runtime — but the warning is unambiguous.
                tracing::warn!(
                    model = %raw,
                    "KHIVE_ADDITIONAL_EMBEDDING_MODELS contains unknown model name; ignored. \
                     Valid forms: short alias like 'paraphrase' or a fully-qualified key \
                     from lattice_embed::EmbeddingModel::from_str."
                );
            }
            parsed
        })
        .collect()
}

fn parse_embedding_model_alias(name: &str) -> Option<EmbeddingModel> {
    let normalized = name.trim().to_ascii_lowercase().replace('_', "-");
    match normalized.as_str() {
        "paraphrase" => Some(EmbeddingModel::ParaphraseMultilingualMiniLmL12V2),
        _ => normalized.parse().ok(),
    }
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
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string()],
            backend_id: BackendId::main(),
        };
        let rt = KhiveRuntime::new(config).expect("file runtime should create");
        assert!(path.exists());
        assert_eq!(rt.config().default_namespace.as_str(), "test");
    }

    #[test]
    fn from_backend_uses_provided_backend() {
        let backend = Arc::new(StorageBackend::memory().expect("memory backend"));
        let config = RuntimeConfig {
            db_path: None,
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string()],
            backend_id: BackendId::new("lore"),
        };
        let rt = KhiveRuntime::from_backend(backend, config);
        assert_eq!(rt.backend_id().as_str(), "lore");
        assert!(rt.config().db_path.is_none());
    }

    #[test]
    fn backend_id_defaults_to_main() {
        let rt = KhiveRuntime::memory().unwrap();
        assert_eq!(rt.backend_id().as_str(), BackendId::MAIN);
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
