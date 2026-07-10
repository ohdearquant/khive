//! RuntimeConfig, BackendId, NamespaceToken, and embedding model helpers.

use std::sync::Arc;

use khive_db::StorageBackend;
use khive_gate::{ActorRef, AllowAllGate, GateRef};
use khive_types::Namespace;
use lattice_embed::EmbeddingModel;

use crate::error::RuntimeResult;

// ---- BackendId ----

/// Identifies a named backend in a multi-backend deployment.
///
/// The `main` backend is the default single-backend name. Multi-backend deployments
/// assign each `[[backends]]` entry a distinct `BackendId`. The
/// `SubstrateCoordinator` in `kkernel`
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
/// Created by [`crate::VerbRegistry::dispatch`] after the gate approves the request.
/// The sealed inner field prevents external code from constructing a token
/// without going through the authorization path.
///
/// The `namespace` field is the **write namespace**: all records created via
/// this token land in that namespace. `visible` is the **read visibility set**:
/// list/search/get operations will return records from any namespace in this
/// set. The write namespace is always a member of the visible set.
///
/// Single-namespace behaviour (backward-compatible default): `visible` contains
/// exactly `[namespace]` — identical to the old strict-equality checks.
#[derive(Clone, Debug)]
pub struct NamespaceToken {
    namespace: Namespace,
    visible: Vec<Namespace>,
    actor: ActorRef,
    _sealed: private::Sealed,
}

impl NamespaceToken {
    /// Mint an authorized token with an extended visibility set.
    ///
    /// `extra_visible` lists namespaces beyond the primary that the token may
    /// read. The primary namespace is always included in the visible set
    /// regardless of what `extra_visible` contains. Duplicates are removed.
    pub(crate) fn mint_with_visibility(
        namespace: Namespace,
        extra_visible: Vec<Namespace>,
        actor: ActorRef,
    ) -> Self {
        let mut visible = vec![namespace.clone()];
        for ns in extra_visible {
            if !visible.contains(&ns) {
                visible.push(ns);
            }
        }
        debug_assert!(!visible.is_empty(), "visible set must be non-empty");
        Self {
            namespace,
            visible,
            actor,
            _sealed: private::Sealed,
        }
    }

    /// Mint an authorized token. Only callable from within `khive-runtime`.
    ///
    /// The visible set defaults to `[namespace]` — backward-compatible with
    /// single-namespace enforcement.
    pub(crate) fn mint_authorized(namespace: Namespace, actor: ActorRef) -> Self {
        Self::mint_with_visibility(namespace, vec![], actor)
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

    /// Return the write namespace this token authorises.
    ///
    /// All records created via this token land in this namespace.
    pub fn namespace(&self) -> &Namespace {
        &self.namespace
    }

    /// Return the read-visibility set.
    ///
    /// List, search, and get operations must accept records whose namespace is
    /// a member of this set. The write namespace is always included.
    pub fn visible_namespaces(&self) -> &[Namespace] {
        &self.visible
    }

    /// Return a deduplicated list of visible namespace strings (borrowed).
    ///
    /// Convenience for passing directly to storage layer filters.
    pub fn visible_namespace_strs(&self) -> Vec<&str> {
        self.visible.iter().map(|ns| ns.as_str()).collect()
    }

    /// Return the actor reference embedded in this token.
    pub fn actor(&self) -> &ActorRef {
        &self.actor
    }

    /// Return a new token with the same actor but a different namespace.
    ///
    /// The visible set is replaced with `[ns]` — the minted token has
    /// `namespace = ns` and `visible = [ns]`. This is a full read+write token
    /// for `ns`: public runtime APIs (`list_notes`, `update_note`, `delete_note`,
    /// etc.) accept it and will operate on `ns`. It is NOT a type-enforced
    /// write-only or append-only capability. This is a capability-transfer
    /// primitive, not a policy gate: the caller is responsible for enforcing any
    /// ACL check before calling this method and for using the minted token only
    /// in the intended narrow scope (e.g. a single `create_note` call).
    ///
    /// Callers today:
    /// - `khive-pack-memory` FTS fanout: iterates token's own visible set; no escalation.
    /// - `khive-pack-comm` inbound delivery: mints a token for the recipient ns,
    ///   gated by `actor.allowed_outbound_namespaces` allowlist check immediately
    ///   before, and uses it for exactly one `create_note` call (append-only by
    ///   convention, not by type enforcement).
    ///
    /// Under a security model (cloud, mutual auth), replace this call pattern with a
    /// type-enforced append-only capability or a `comm.ingest` Subhandler dispatch
    /// (see ADR-056/ADR-053) that goes through the Gate.
    pub fn with_namespace(&self, ns: Namespace) -> Self {
        Self::mint_authorized(ns, self.actor.clone())
    }
}

// ---- RuntimeConfig ----

/// Runtime configuration.
///
/// The `db_path` and `embedding_model` fields are deprecated in favour of
/// constructing the backend externally and calling [`crate::KhiveRuntime::from_backend`].
/// They remain for backward compatibility with tests and single-binary deployments.
#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    /// Path to the SQLite database file. `None` = in-memory (tests).
    ///
    /// Deprecated: use [`crate::KhiveRuntime::from_backend`] instead. The boot path
    /// constructs backends from `khive.toml` (`AppConfig`) and passes them to
    /// `from_backend`. Direct `db_path` usage persists only in tests.
    pub db_path: Option<std::path::PathBuf>,
    /// Namespace used when no explicit namespace is provided.
    pub default_namespace: Namespace,
    /// Local embedding model. `None` disables embedding and hybrid vector search;
    /// `hybrid_search` then falls back to text-only.
    ///
    /// Deprecated: embedding engines move to a per-pack `EmbedderRegistry`.
    /// This field persists for backward compatibility until the embedder registry
    /// is fully plumbed.
    pub embedding_model: Option<EmbeddingModel>,
    /// Additional embedding models to make available by request name.
    ///
    /// `embedding_model` remains the default used by existing `embed()` and
    /// `embed_batch()` callers. This list adds non-default models that can be
    /// selected with `embedder(name)`, `embed_with_model(...)`, memory
    /// `remember.embedding_model`, and memory `recall.embedding_model`.
    pub additional_embedding_models: Vec<EmbeddingModel>,
    /// Authorization gate consulted before each verb dispatch.
    /// Default: `AllowAllGate` (permissive). For production policy enforcement,
    /// plug in a Rego- or capability-witness-backed impl.
    pub gate: GateRef,
    /// Names of packs the transport layer should register into the VerbRegistry.
    /// The transport layer (e.g. `khive-mcp`) reads this list and instantiates
    /// the matching concrete pack types. Unknown names are reported as errors
    /// by the transport, not silently ignored.
    /// Default: `["kg"]`.
    pub packs: Vec<String>,
    /// Identifies this runtime's backend in a multi-backend deployment.
    ///
    /// Set by the boot path when constructing per-pack runtimes from `khive.toml`.
    /// Single-backend deployments use the default `BackendId::MAIN`.
    pub backend_id: BackendId,
    /// Brain profile to use for `memory.feedback` / `knowledge.feedback` and
    /// recall-time score boosting (ADR-035 §Brain profile configuration).
    ///
    /// Resolution order (highest to lowest, ADR-035): CLI flag, then
    /// `runtime.brain_profile` in project/global `khive.toml`, then the
    /// `KHIVE_BRAIN_PROFILE` env var as fallback default. Callers must keep
    /// env OUT of the base config they pass in (see `khive-mcp` serve.rs).
    /// 1. `--brain-profile` CLI flag (explicit only)
    /// 2. Namespace-bound profile resolved via `brain.resolve` at feedback time
    /// 3. Pack-local global tuning prior (default fallback)
    pub brain_profile: Option<String>,
    /// Operator-configured read-visibility set (ADR-007 Rev 4 Rule 3b).
    ///
    /// OSS dispatch widens the DEFAULT multi-record read scope to
    /// `['local'] ∪ visible_namespaces`. Writes remain pinned to `'local'`.
    /// An explicit `namespace=` request param is a precise single-namespace
    /// escape and is not widened. Populated from `actor.visible_namespaces`
    /// in `khive.toml`.
    pub visible_namespaces: Vec<Namespace>,
    /// Namespaces this actor's comm.send/reply may deliver messages INTO
    /// (outbound, sender-side). Populated from `actor.allowed_outbound_namespaces`
    /// in `khive.toml`. Empty by default — cross-namespace delivery denied
    /// unless explicitly declared. The comm handler uses an ordinary
    /// `NamespaceToken` (minted via `with_namespace`) in an append-only manner;
    /// the token itself is NOT type-enforced write-only. The recipient-side
    /// `allowed_inbound_namespaces` (bilateral mutual opt-in) is reserved for
    /// a future cloud-path authorization ADR (not yet written).
    pub allowed_outbound_namespaces: Vec<Namespace>,
    /// Configured actor identity label (ADR-057). Populated from `[actor] id` in
    /// `khive.toml`. When `Some`, `authorize()` mints tokens carrying this actor
    /// label so that `comm.inbox` filters by `to_actor` instead of falling back to
    /// the party-line "local" behavior. When `None` (default), tokens carry
    /// `ActorRef::anonymous()` and inbox is scoped to party-line messages —
    /// those addressed to `"local"` or carrying no `to_actor` stamp.
    pub actor_id: Option<String>,
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
            .map(|h| std::path::PathBuf::from(h).join(".khive/khive.db"));
        let embedding_model = std::env::var("KHIVE_EMBEDDING_MODEL")
            .ok()
            .and_then(|s| s.parse().ok())
            .or(Some(EmbeddingModel::AllMiniLmL6V2));
        let additional_embedding_models = std::env::var("KHIVE_ADDITIONAL_EMBEDDING_MODELS")
            .ok()
            .map(|s| parse_embedding_model_list(&s))
            .unwrap_or_else(|| vec![EmbeddingModel::ParaphraseMultilingualMiniLmL12V2]);
        let packs = std::env::var("KHIVE_PACKS")
            .ok()
            .map(|s| parse_pack_list(&s))
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| {
                vec![
                    "kg",
                    "gtd",
                    "memory",
                    "brain",
                    "comm",
                    "schedule",
                    "knowledge",
                    "session",
                    "git",
                ]
                .into_iter()
                .map(String::from)
                .collect()
            });
        let brain_profile = std::env::var("KHIVE_BRAIN_PROFILE")
            .ok()
            .filter(|s| !s.trim().is_empty());
        let actor_id = std::env::var("KHIVE_ACTOR")
            .ok()
            .filter(|s| !s.trim().is_empty());
        Self {
            db_path,
            default_namespace: Namespace::local(),
            embedding_model,
            additional_embedding_models,
            gate: Arc::new(AllowAllGate),
            packs,
            backend_id: BackendId::main(),
            brain_profile,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id,
        }
    }
}

impl RuntimeConfig {
    /// Build a `RuntimeConfig` with embedding disabled entirely (issue #396).
    ///
    /// `embedding_model` and `additional_embedding_models` are computed
    /// *independently* inside [`Default::default`] (see above): the former from
    /// `KHIVE_EMBEDDING_MODEL`, the latter from `KHIVE_ADDITIONAL_EMBEDDING_MODELS`
    /// (falling back to seeding `ParaphraseMultilingualMiniLmL12V2` when that var
    /// is unset). Writing `RuntimeConfig { embedding_model: None,
    /// ..RuntimeConfig::default() }` therefore does *not* produce a model-less
    /// runtime: `additional_embedding_models` still carries the env-driven
    /// fallback seed, `configured_embedding_models` still reports it, and the
    /// note-write path fans out embedding to every *registered* model regardless
    /// of `embedding_model` — so the first `memory.remember` on a machine without
    /// local model files hard-fails instead of degrading to FTS-only.
    ///
    /// This constructor clears both fields together and is the one ergonomic,
    /// correct spelling for "no embed runtime". Model-less machines (CI runners,
    /// fresh installs without local model files) should use it instead of the
    /// two-field struct-update form above.
    ///
    /// Deliberately ignores `KHIVE_ADDITIONAL_EMBEDDING_MODELS`: a caller
    /// reaching for `no_embeddings()` wants zero embedders unconditionally, not
    /// "zero unless the environment happens to disagree".
    pub fn no_embeddings() -> Self {
        Self {
            embedding_model: None,
            additional_embedding_models: Vec::new(),
            ..Self::default()
        }
    }
}

/// Resolve the `--db`/`KHIVE_DB` value into the db path used to ANCHOR tier-3
/// project-local `.khive/config.toml` discovery (`KhiveConfig::load_with_home_fallback`'s
/// `db_path` parameter), mirroring the precedence `kkernel mcp`'s and `kkernel exec`'s
/// two call sites use to open the database itself: `:memory:` has no file to anchor on
/// (`None`); an explicit path anchors on that path; an unset value falls back to
/// `$HOME/.khive/khive.db`, or the relative path `./.khive/khive.db` when `HOME` is unset —
/// matching those two call sites' own pre-existing `unwrap_or_else(|_| ".".into())` handling.
///
/// This is distinct from a plain override resolver that answers "does the caller want to
/// override `RuntimeConfig::default().db_path`?" (2-arm, `None` meaning "keep the default").
/// `resolve_db_anchor` always resolves to a concrete anchor value (3-arm) — it materializes
/// an anchor path unconditionally rather than asking whether to override one.
///
/// Note one deliberate divergence from `RuntimeConfig::default()`: when `HOME` is unset,
/// `RuntimeConfig::default()` maps its own `db_path` to `None` (`.ok()` on the failed env
/// lookup short-circuits the following `Option::map`), whereas this function falls back to
/// `./.khive/khive.db` instead. A caller anchoring config-file discovery wants a concrete
/// directory to search even without `HOME`; this function is not a stand-in for
/// `RuntimeConfig::default()`'s own db-path default, only for the two call sites' shared
/// anchor-derivation logic.
pub fn resolve_db_anchor(db: Option<&str>) -> Option<std::path::PathBuf> {
    match db {
        Some(":memory:") => None,
        Some(path) => Some(std::path::PathBuf::from(path)),
        None => {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            Some(std::path::PathBuf::from(format!("{home}/.khive/khive.db")))
        }
    }
}

/// Assert that a resolved `db_path` — the path a runtime is about to open, and
/// the same value `compute_config_id` folds into a process's `config_id` — agrees
/// with what [`resolve_db_anchor`] derives from the same explicit `--db` /
/// `KHIVE_DB` input. Call this at each construction boundary right after the
/// `db_path` that boundary is about to use has been resolved.
///
/// A construction path that recomputes `db_path` independently of
/// `resolve_db_anchor` (instead of routing through it, directly or via
/// `resolve_runtime_config`) would otherwise diverge only silently: its
/// `config_id` would carry the wrong db path, so it would stop matching a
/// daemon or peer process anchored on the same database, and would silently
/// fall back to a disconnected, out-of-sync runtime instead of failing loud.
/// This turns that divergence into a hard error at the point it is introduced.
///
/// When `resolve_db_anchor(args_db)` itself resolves to `None` (the
/// `:memory:` sentinel), there is no canonical anchor path to compare
/// against, so the guard is inert and returns `Ok(())` unconditionally.
pub fn assert_db_anchor_consistent(
    resolved_db_path: Option<&std::path::Path>,
    args_db: Option<&str>,
) -> anyhow::Result<()> {
    let Some(anchor) = resolve_db_anchor(args_db) else {
        return Ok(());
    };
    if resolved_db_path != Some(anchor.as_path()) {
        anyhow::bail!(
            "db-path resolution drift at server construction: resolved db_path {:?} \
             does not match the canonical anchor {:?} computed by resolve_db_anchor \
             from the same --db input; this construction path likely recomputed the \
             db path independently instead of routing through the shared resolver, \
             which would desynchronize config_id from other processes sharing the \
             same database",
            resolved_db_path,
            anchor
        );
    }
    Ok(())
}

/// Resolve the per-connection attribution actor from the project/cwd-anchored
/// config tier, independently of the database-anchored config load that governs
/// `config_id` (ADR-096 Fork 2).
///
/// Anchoring tier-3 `.khive/config.toml` discovery to the resolved database's own
/// directory (commit 10d9c92c, #651) keeps `config_id` coherent between a
/// short-lived client and a long-running daemon that share one database — that is
/// correct for the fields that make up `config_id` (packs/db/embed/backend/
/// outbound policy). It also relocated discovery of the project-local `[actor]`
/// block, though. When many per-project connections share one database under a
/// single `HOME` (the daemon-multiplexed fleet case), the shared database-anchored
/// config carries no `[actor]`, so every connection's write-stamp attribution
/// collapses to the default identity.
///
/// This performs a SEPARATE, cwd-anchored lookup (`db_path: None`, matching the
/// pre-#651 tier-3 search) and reads only `[actor].id`. It intentionally does not
/// read or return anything else from the resolved `KhiveConfig` — `config_id`,
/// `default_namespace` remain governed exclusively by the existing
/// database-anchored load and must not be perturbed by this tier: `actor_id`
/// and identity-derived `visible_namespaces` are not part of
/// `compute_config_id` (`khive-mcp` `server.rs`), and ADR-007 Rev 4 Rule 0
/// already keeps `[actor].id` out of `default_namespace`.
///
/// `config_path` is the same explicit `--config` / `KHIVE_CONFIG` override the
/// caller's database-anchored load receives — tier 1 short-circuits identically
/// in both loads, so an explicit override still wins here too.
///
/// Returns `Ok(None)` when no project-anchored config exists, or it exists but
/// carries no non-empty `[actor].id` — callers fall through to their own env /
/// anonymous tiers in that case.
pub fn resolve_project_actor_id(
    config_path: Option<&std::path::Path>,
) -> Result<Option<String>, crate::engine_config::ConfigError> {
    let khive_cfg = crate::engine_config::KhiveConfig::load_with_home_fallback(config_path, None)?;
    Ok(khive_cfg
        .and_then(|cfg| cfg.actor.id)
        .filter(|s| !s.trim().is_empty()))
}

// ---- Embedding model helpers ----

/// Sanitize an embedding model name into a valid SQL table suffix.
/// e.g. `bge-small-en-v1.5` -> `bge_small_en_v1_5`
pub(crate) fn vec_model_key(model: EmbeddingModel) -> String {
    sanitize_key(&model.to_string())
}

pub(crate) fn sanitize_key(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

pub(crate) fn build_embedder_registry(
    config: &RuntimeConfig,
) -> (crate::embedder_registry::EmbedderRegistry, Arc<str>) {
    use crate::embedder_registry::{EmbedderRegistry, LatticeEmbedderProvider};
    let mut registry = EmbedderRegistry::new();
    for model in configured_embedding_models(config) {
        registry.register(LatticeEmbedderProvider::new(model));
    }
    let default_embedder_name = config
        .embedding_model
        .map(|model| Arc::<str>::from(model.to_string()))
        .unwrap_or_else(|| Arc::<str>::from(""));
    (registry, default_embedder_name)
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

pub(crate) fn register_configured_embedding_models(
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

/// Build a `RuntimeConfig` from a parsed `KhiveConfig`.
///
/// For each `[[engines]]` entry:
/// - The engine flagged `default = true` becomes `RuntimeConfig::embedding_model`.
/// - All other engines become `RuntimeConfig::additional_embedding_models`.
///
/// Model name validity is checked here: any engine whose `model` field cannot
/// be parsed via `parse_embedding_model_alias` is skipped with a warning.
///
/// If `khive_cfg.engines` is empty, the returned `RuntimeConfig` uses the
/// env-var-derived defaults from `RuntimeConfig::default()`.
///
/// When both a config file and `KHIVE_EMBEDDING_MODEL` env var are present,
/// the caller is responsible for emitting a warning that env vars are overridden.
/// This function purely converts `KhiveConfig` to `RuntimeConfig` fields.
pub fn runtime_config_from_khive_config(
    khive_cfg: &crate::engine_config::KhiveConfig,
    base: RuntimeConfig,
) -> RuntimeConfig {
    // ADR-007 Rev 4 Rule 0: `[actor] id` does NOT become the storage namespace
    // (writes always pin to `local`). `default_namespace` is whatever the caller
    // resolved into `base` (explicit `--namespace` / `KHIVE_NAMESPACE`, else `local`).
    // `actor.id` contributes to the read visible-set only (see fold-in below).
    let default_namespace = base.default_namespace.clone();

    // base.brain_profile must carry ONLY the explicit CLI tier — never an env
    // value (env sits BELOW toml per ADR-035; the MCP resolver applies it after).
    let brain_profile = base.brain_profile.clone().or_else(|| {
        khive_cfg
            .runtime
            .brain_profile
            .clone()
            .filter(|s| !s.trim().is_empty())
    });

    let visible_namespaces: Vec<Namespace> = khive_cfg
        .actor
        .visible_namespaces
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter_map(|s| match Namespace::parse(s) {
            Ok(ns) => Some(ns),
            Err(e) => {
                tracing::warn!(ns = %s, error = %e, "actor.visible_namespaces: invalid namespace; skipped");
                None
            }
        })
        .collect();

    // ADR-007 Rev 4: fold actor.id's namespace into visible_namespaces so that
    // default reads widen to {local} ∪ {actor namespace}. Skipped when actor.id
    // parses to `local` (mint already includes primary=local on the default path,
    // adding it here would create a duplicate). Also skipped when it is already
    // present from actor.visible_namespaces above.
    let visible_namespaces = if let Some(id) = khive_cfg.actor.id.as_deref() {
        match Namespace::parse(id) {
            Ok(actor_ns) if actor_ns != Namespace::local() => {
                let mut v = visible_namespaces;
                if !v.contains(&actor_ns) {
                    v.push(actor_ns);
                }
                v
            }
            _ => visible_namespaces,
        }
    } else {
        visible_namespaces
    };

    // KhiveConfig::validate() guarantees every entry in allowed_outbound_namespaces is a
    // structurally valid Namespace string, so Namespace::parse failures here are unreachable
    // for validated configs. We still filter_map with a warn so a runtime bug doesn't panic.
    let allowed_outbound_namespaces: Vec<Namespace> = khive_cfg
        .actor
        .allowed_outbound_namespaces
        .iter()
        .filter_map(|s| match Namespace::parse(s) {
            Ok(ns) => Some(ns),
            Err(e) => {
                tracing::warn!(ns = %s, error = %e, "actor.allowed_outbound_namespaces: invalid namespace; skipped");
                None
            }
        })
        .collect();

    // ADR-057: store actor.id as actor_id for token minting. Precedence is
    // TOML `[actor] id` > `base.actor_id` (the env/CLI-resolved value the
    // caller already put in `base`) > anonymous. When `[actor] id` is absent
    // or empty, fall back to `base.actor_id` instead of clobbering it with
    // `None` — otherwise an env-resolved actor (e.g. `KHIVE_ACTOR`) is
    // silently dropped whenever a project config is found without an
    // `[actor]` block, in both the engines-empty and engines-present arms
    // below.
    let actor_id = khive_cfg
        .actor
        .id
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| base.actor_id.clone());

    if khive_cfg.engines.is_empty() {
        return RuntimeConfig {
            default_namespace,
            brain_profile,
            visible_namespaces,
            allowed_outbound_namespaces,
            actor_id,
            ..base
        };
    }

    let mut embedding_model: Option<EmbeddingModel> = None;
    let mut additional: Vec<EmbeddingModel> = Vec::new();

    for engine in &khive_cfg.engines {
        match parse_embedding_model_alias(&engine.model) {
            Some(model) => {
                if engine.default {
                    embedding_model = Some(model);
                } else {
                    additional.push(model);
                }
            }
            None => {
                tracing::warn!(
                    engine = %engine.name,
                    model = %engine.model,
                    "engine config: unknown model name; engine will be skipped"
                );
            }
        }
    }

    RuntimeConfig {
        embedding_model,
        additional_embedding_models: additional,
        default_namespace,
        brain_profile,
        visible_namespaces,
        allowed_outbound_namespaces,
        actor_id,
        ..base
    }
}

/// Parse a comma- or whitespace-separated list of embedding model names.
fn parse_embedding_model_list(s: &str) -> Vec<EmbeddingModel> {
    parse_pack_list(s)
        .into_iter()
        .filter_map(|raw| {
            let parsed = parse_embedding_model_alias(&raw);
            if parsed.is_none() && !raw.trim().is_empty() {
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

pub(crate) fn parse_embedding_model_alias(name: &str) -> Option<EmbeddingModel> {
    let normalized = name.trim().to_ascii_lowercase().replace('_', "-");
    match normalized.as_str() {
        "paraphrase" => Some(EmbeddingModel::ParaphraseMultilingualMiniLmL12V2),
        _ => normalized.parse().ok(),
    }
}

#[cfg(test)]
mod resolve_db_anchor_tests {
    use super::resolve_db_anchor;

    #[test]
    fn memory_sentinel_maps_to_none() {
        assert_eq!(resolve_db_anchor(Some(":memory:")), None);
    }

    #[test]
    fn explicit_path_maps_to_some() {
        assert_eq!(
            resolve_db_anchor(Some("/tmp/khive-anchor-test.db")),
            Some(std::path::PathBuf::from("/tmp/khive-anchor-test.db"))
        );
    }

    #[test]
    fn absent_maps_to_home_default() {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        let expected = std::path::PathBuf::from(format!("{home}/.khive/khive.db"));
        assert_eq!(resolve_db_anchor(None), Some(expected));
    }
}

#[cfg(test)]
mod assert_db_anchor_consistent_tests {
    use super::{assert_db_anchor_consistent, resolve_db_anchor};

    #[test]
    fn diverging_db_path_is_rejected_naming_both_paths() {
        let args_db = "/tmp/khive-anchor-guard-real.db";
        let anchor = resolve_db_anchor(Some(args_db)).expect("explicit path always anchors");
        let wrong = std::path::PathBuf::from("/tmp/khive-anchor-guard-wrong.db");

        let err = assert_db_anchor_consistent(Some(wrong.as_path()), Some(args_db))
            .expect_err("a resolved db_path diverging from the anchor must be rejected");

        let msg = err.to_string();
        assert!(
            msg.contains(&wrong.display().to_string()),
            "error must name the resolved (wrong) path: {msg}"
        );
        assert!(
            msg.contains(&anchor.display().to_string()),
            "error must name the canonical anchor path: {msg}"
        );
    }

    #[test]
    fn matching_explicit_db_path_passes() {
        let args_db = "/tmp/khive-anchor-guard-consistent.db";
        let anchor = resolve_db_anchor(Some(args_db)).expect("explicit path always anchors");
        assert!(assert_db_anchor_consistent(Some(anchor.as_path()), Some(args_db)).is_ok());
    }

    #[test]
    fn memory_sentinel_anchor_is_inert() {
        // `resolve_db_anchor(":memory:")` yields `None` — there is no canonical
        // path to assert against, so the guard passes regardless of what
        // `resolved_db_path` happens to carry.
        let bogus = std::path::PathBuf::from("/tmp/should-not-matter.db");
        assert!(assert_db_anchor_consistent(Some(bogus.as_path()), Some(":memory:")).is_ok());
        assert!(assert_db_anchor_consistent(None, Some(":memory:")).is_ok());
    }

    #[test]
    fn normal_boot_with_db_unset_passes_silently() {
        // Mirrors a normal boot: `--db` unset. `resolve_db_anchor(None)` always
        // resolves to `Some(..)` (HOME-set or HOME-unset both produce a
        // concrete anchor — see `absent_maps_to_home_default` above and the
        // `resolve_db_anchor` doc comment on the HOME-unset arm), so a runtime
        // whose resolved `db_path` equals that anchor passes silently.
        let anchor = resolve_db_anchor(None);
        assert!(assert_db_anchor_consistent(anchor.as_deref(), None).is_ok());
    }
}

#[cfg(test)]
mod resolve_project_actor_id_tests {
    use super::resolve_project_actor_id;

    fn write_toml(dir: &tempfile::TempDir, body: &str) -> std::path::PathBuf {
        let path = dir.path().join("config.toml");
        std::fs::write(&path, body).expect("write config.toml");
        path
    }

    #[test]
    fn extracts_non_empty_actor_id_from_explicit_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_toml(&dir, "[actor]\nid = \"lambda:explicit-actor\"\n");

        assert_eq!(
            resolve_project_actor_id(Some(&path)).expect("no error"),
            Some("lambda:explicit-actor".to_string())
        );
    }

    #[test]
    fn returns_none_for_missing_explicit_path() {
        let missing = std::path::PathBuf::from("/nonexistent/khive-project-actor-test/config.toml");
        assert_eq!(
            resolve_project_actor_id(Some(&missing)).expect("no error"),
            None,
            "a nonexistent explicit path must resolve to None, not an error"
        );
    }

    #[test]
    fn propagates_load_error_for_invalid_actor_id() {
        // `KhiveConfig::load`'s `validate()` rejects an empty `[actor] id` at load
        // time (`ConfigError::InvalidActorId`) before the emptiness filter in
        // `resolve_project_actor_id` would ever see it — this asserts the error
        // surfaces rather than being silently swallowed into `Ok(None)`.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_toml(&dir, "[actor]\nid = \"\"\n");

        let err = resolve_project_actor_id(Some(&path)).expect_err("invalid actor.id must error");
        assert!(
            matches!(
                err,
                crate::engine_config::ConfigError::InvalidActorId { .. }
            ),
            "expected InvalidActorId, got {err:?}"
        );
    }

    #[test]
    fn returns_none_when_config_has_no_actor_section() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_toml(
            &dir,
            "[[engines]]\nname = \"primary\"\nmodel = \"bge-small-en-v1.5\"\ndefault = true\n",
        );

        assert_eq!(
            resolve_project_actor_id(Some(&path)).expect("no error"),
            None,
            "a config file with no [actor] section must resolve to None"
        );
    }
}

#[cfg(test)]
mod no_embeddings_tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn no_embeddings_clears_both_fields() {
        let config = RuntimeConfig::no_embeddings();
        assert_eq!(config.embedding_model, None);
        assert!(config.additional_embedding_models.is_empty());
        assert!(
            configured_embedding_models(&config).is_empty(),
            "no_embeddings() must yield zero configured embedders"
        );
    }

    #[test]
    #[serial]
    fn no_embeddings_ignores_additional_env_override() {
        // no_embeddings() is an unconditional opt-out: even if the caller's
        // environment sets KHIVE_ADDITIONAL_EMBEDDING_MODELS, the resulting
        // config must still report zero embedders.
        std::env::set_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS", "paraphrase");
        let config = RuntimeConfig::no_embeddings();
        std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");

        assert!(config.additional_embedding_models.is_empty());
        assert!(configured_embedding_models(&config).is_empty());
    }

    #[test]
    #[serial]
    fn default_still_seeds_additional_models_when_env_unset() {
        // Regression guard for issue #396's root cause: `Default` must keep
        // computing `embedding_model` and `additional_embedding_models`
        // independently. `no_embeddings()` is a new opt-out constructor, not a
        // change to `Default`'s existing (env-driven) seeding behavior.
        std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
        let config = RuntimeConfig::default();

        assert_eq!(
            config.additional_embedding_models,
            vec![EmbeddingModel::ParaphraseMultilingualMiniLmL12V2]
        );

        // The bug shape from #396: overriding only `embedding_model` via
        // struct-update syntax does not clear `additional_embedding_models`.
        let buggy_form = RuntimeConfig {
            embedding_model: None,
            ..RuntimeConfig::default()
        };
        assert!(
            !buggy_form.additional_embedding_models.is_empty(),
            "Default's independent-field seeding must remain unchanged; \
             no_embeddings() is the fix, not a change to Default"
        );
    }
}
