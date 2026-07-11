//! TOML-based embedding engine configuration for khive.
//!
//! Loads `.khive/config.toml` (or `--config` / `KHIVE_CONFIG`) and exposes an
//! `[[engines]]` array for arbitrary-N embedding engine registration. Falls back
//! to `KHIVE_EMBEDDING_MODEL` env vars when no config file is present.

use std::path::{Path, PathBuf};

use khive_types::namespace::Namespace;
use serde::Deserialize;
use thiserror::Error;

use crate::presentation::OutputFormat;

// ---- Error type ----

/// Errors produced while loading or validating a `KhiveConfig`.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("config file I/O: {0}")]
    Io(#[from] std::io::Error),

    #[error("config TOML parse error in {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("exactly one engine must be marked `default = true`; found {found}")]
    DefaultCount { found: usize },

    #[error("duplicate engine name: {name:?}")]
    DuplicateName { name: String },

    #[error(
        "engine {name:?}: model {model:?} is not a recognized lattice_embed::EmbeddingModel name"
    )]
    UnknownModel { name: String, model: String },

    #[error("engine {name:?}: fusion_weight must be > 0, got {value}")]
    InvalidFusionWeight { name: String, value: f64 },

    #[error("actor.id {id:?} is not a valid namespace: {reason}")]
    InvalidActorId { id: String, reason: String },

    #[error("duplicate backend name: {name:?}")]
    DuplicateBackendName { name: String },

    #[error(
        "[packs.{pack}].backend = {backend:?} references an unknown backend; \
         defined backends: {defined}"
    )]
    UnknownPackBackend {
        pack: String,
        backend: String,
        defined: String,
    },

    #[error(
        "[[backends]] entry {name:?}: field `{field}` is not yet supported; \
         remove it from the config or wait for a future release that implements it"
    )]
    UnsupportedBackendField { name: String, field: &'static str },

    #[error(
        "top-level `db = {value:?}` is not a supported config-file key; \
         use `--db` / `KHIVE_DB` to select a single-file database, or \
         `[[backends]].path` to declare storage backend topology"
    )]
    UnsupportedTopLevelDb { value: String },
}

// ---- Config structs ----

/// Configuration for a single embedding engine.
#[derive(Debug, Clone, Deserialize)]
pub struct EngineConfig {
    /// Logical name used to reference this engine in logs and fusion.
    pub name: String,

    /// Lattice-embed model name (e.g. `"all-minilm-l6-v2"`).
    ///
    /// Must be parseable via `lattice_embed::EmbeddingModel::from_str` (or a
    /// recognised short alias handled by `parse_embedding_model_alias`).
    pub model: String,

    /// When `true`, this engine's model becomes the primary (`RuntimeConfig::embedding_model`).
    /// Exactly one engine in the list must set this. If absent, defaults to `false`.
    #[serde(default)]
    pub default: bool,

    /// RRF fusion weight for weighted multi-engine fusion.
    ///
    /// Only meaningful when multiple engines are loaded. Must be `> 0` when
    /// present. `None` means the engine participates in fusion with equal weight
    /// to other engines that also lack a `fusion_weight`.
    ///
    /// For RRF: `fusion_weight` provides per-engine relative importance during
    /// weighted RRF; it does NOT apply to rank-based unweighted RRF (the weights
    /// are injected into `FusionStrategy::Weighted` only).
    pub fusion_weight: Option<f64>,

    /// Expected output dimensionality (optional sanity check).
    ///
    /// Not used at runtime — dimensions are authoritative from
    /// `EmbeddingModel::dimensions()`. Present so operators can document the
    /// expected shape alongside the model name.
    pub dims: Option<u32>,
}

/// Actor configuration — the default namespace / identity for this khive instance.
///
/// Corresponds to the `[actor]` TOML section. `id` is used as the
/// `default_namespace` for gate/attribution policy input. OSS dispatch pins
/// writes to the shared `local` namespace regardless of this value (ADR-007
/// Rev 4 Rule 0); cloud deployments derive the namespace from an authenticated
/// `NamespaceToken` instead.
///
/// ```toml
/// [actor]
/// id = "lambda:leo"                          # attribution identity (required)
/// display_name = "example actor"   # human label (optional)
/// visible_namespaces = ["lambda:khive", "local"]  # widens default read scope (ADR-007 Rev 4 Rule 3b)
/// ```
///
/// `visible_namespaces` is consumed by OSS dispatch to widen the DEFAULT
/// multi-record read scope to `['local'] ∪ visible_namespaces` (ADR-007 Rev 4
/// Rule 3b). Writes remain pinned to `'local'`. An explicit `namespace=` request
/// param is a precise single-namespace escape and is not widened. A cloud gate
/// may also consult this list as policy input at its own layer.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ActorConfig {
    /// Namespace identifier used as the default actor for all operations.
    ///
    /// Must be a valid `Namespace` string (e.g. `"local"`, `"lambda:khive"`).
    /// Defaults to `"local"` when absent — backward-compatible with pre-actor
    /// deployments.
    #[serde(default)]
    pub id: Option<String>,

    /// Optional human-readable label for this actor. Not used by the runtime;
    /// surfaced in introspection and log output only.
    #[serde(default)]
    pub display_name: Option<String>,

    /// Additional namespaces that widen the DEFAULT multi-record read scope
    /// to `['local'] ∪ visible_namespaces` (ADR-007 Rev 4 Rule 3b). Each string
    /// must be a valid `Namespace`. Writes remain pinned to `'local'`. An
    /// explicit `namespace=` request param is a precise escape and is not widened
    /// by this list. A cloud gate may also consult it as policy input.
    #[serde(default)]
    pub visible_namespaces: Option<Vec<String>>,

    /// Namespaces this actor's comm.send/reply may deliver messages INTO
    /// (outbound, sender-side). Empty by default — cross-namespace delivery
    /// denied unless explicitly declared. The comm handler uses an ordinary
    /// `NamespaceToken` (minted via `with_namespace`) in an append-only manner;
    /// the token itself is NOT type-enforced write-only. The recipient-side
    /// `allowed_inbound_namespaces` (bilateral mutual opt-in) is reserved for
    /// a future cloud-path authorization ADR (not yet written).
    ///
    /// Each entry must be a valid `Namespace` string; validated at
    /// config-load time. An empty list preserves the prior deny-all behavior
    /// for any actor that does not add this field.
    #[serde(default)]
    pub allowed_outbound_namespaces: Vec<String>,
}

// ---- Per-pack backend config (ADR-028) ----

/// Storage backend kind.
#[derive(Debug, Clone, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    /// SQLite file-backed database (default).
    #[default]
    Sqlite,
    /// In-memory database — for testing only; state is lost on restart.
    Memory,
}

/// Configuration for a named storage backend.
///
/// Corresponds to a `[[backends]]` entry in `khive.toml`.
/// When no `[[backends]]` section is present, a single implicit `main` backend
/// is synthesised from the existing `--db` / `KHIVE_DB` / default-path resolution.
/// All packs fall back to `main` when their name is absent from `[packs]`.
///
/// ```toml
/// [[backends]]
/// name = "knowledge"
/// kind = "sqlite"
/// path = "~/.khive/knowledge.db"
/// cache_mb = 128
/// journal_mode = "wal"
/// read_only = false
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct BackendConfig {
    /// Unique backend name. Referenced by `[packs.<name>].backend`.
    pub name: String,
    /// Storage backend kind. Defaults to `sqlite`.
    #[serde(default)]
    pub kind: BackendKind,
    /// Filesystem path for `sqlite` kind. Tilde is expanded to `$HOME`.
    /// `None` for `memory` kind (path is ignored when present).
    pub path: Option<std::path::PathBuf>,
    /// SQLite page-cache size in MiB.
    pub cache_mb: Option<u32>,
    /// SQLite journal mode (e.g. `"wal"`).
    pub journal_mode: Option<String>,
    /// Open the backend read-only. Defaults to `false`.
    #[serde(default)]
    pub read_only: bool,
}

/// Per-pack backend assignment.
///
/// Corresponds to a `[packs.<pack-name>]` entry in `khive.toml`.
/// Packs whose name is absent from `[packs]` fall back to the `main` backend.
///
/// ```toml
/// [packs.knowledge]
/// backend = "knowledge"
/// ```
#[derive(Debug, Clone, Deserialize)]
pub struct PackConfig {
    /// Backend name this pack is assigned to. Must match a `[[backends]].name`.
    pub backend: String,
}

/// Top-level khive configuration loaded from `khive.toml` or `config.toml`.
///
/// Sections consumed today:
/// - `[[engines]]`: embedding engine declarations
/// - `[actor]`: default namespace / identity (OSS actor model)
/// - `[runtime]`: runtime knobs (namespace, brain_profile)
/// - `[[backends]]`: storage backend declarations (ADR-028)
/// - `[packs.<name>]`: per-pack backend assignments (ADR-028)
///
/// Unknown keys are silently ignored by serde — forward-compatible.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct KhiveConfig {
    /// Typed only so a top-level `db` key can be rejected loudly by
    /// [`KhiveConfig::validate`] instead of being silently ignored as an
    /// unknown key. Not a supported config-file storage selector: single-file
    /// database selection is `--db`/`KHIVE_DB`, and storage topology is
    /// `[[backends]].path`.
    #[serde(default)]
    pub db: Option<String>,

    /// Embedding engine declarations.
    #[serde(default)]
    pub engines: Vec<EngineConfig>,

    /// Default actor identity for this khive instance.
    ///
    /// When present, `actor.id` feeds configuration identity and gate/attribution
    /// policy input.  A non-`'local'` `actor.id` is folded into the default READ
    /// visible-set at config load (ADR-007 Rev 4 Rule 3b) — it widens what default
    /// multi-record reads return, but never routes writes or sets `default_namespace`.
    /// Cloud model derives actor identity from an authenticated token.
    #[serde(default)]
    pub actor: ActorConfig,

    /// Runtime knobs: namespace overrides, brain profile, etc.
    #[serde(default)]
    pub runtime: RuntimeSectionConfig,

    /// Named storage backends (ADR-028).
    ///
    /// When absent or empty, a single implicit `main` backend is used and all
    /// packs share it — identical to pre-ADR-028 behavior.
    #[serde(default)]
    pub backends: Vec<BackendConfig>,

    /// Per-pack backend assignments (ADR-028).
    ///
    /// Maps pack name to backend name. Packs absent from this map fall back to
    /// the `main` backend. Validated at load time: every referenced backend name
    /// must appear in `backends`.
    #[serde(default)]
    pub packs: std::collections::HashMap<String, PackConfig>,
}

/// `[runtime]` section in `khive.toml`.
///
/// Carries runtime knobs that mirror the CLI flag / env var tier.
/// All fields are optional; absent keys fall through to env vars or built-in
/// defaults.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct RuntimeSectionConfig {
    /// Brain profile ID to use for `memory.feedback` / `knowledge.feedback`
    /// and recall-time score boosting (ADR-035 §Brain profile configuration).
    ///
    /// Mirrors `--brain-profile` / `KHIVE_BRAIN_PROFILE`. When absent, the
    /// namespace-bound profile (via `brain.resolve`) is tried, then the
    /// global tuning prior is used as the final fallback.
    #[serde(default)]
    pub brain_profile: Option<String>,

    /// Default output serialization format (ADR-078).
    ///
    /// Mirrors `--output-format` / `KHIVE_OUTPUT_FORMAT`. Precedence (highest to lowest):
    /// per-request `format` field → `KHIVE_OUTPUT_FORMAT` → this field → builtin `json`.
    ///
    /// Accepted values: `"json"` (default), `"auto"`, `"table"`.
    #[serde(default)]
    pub default_output_format: Option<OutputFormat>,
}

impl KhiveConfig {
    /// Load and validate a `KhiveConfig` from an explicit path.
    ///
    /// Search order:
    /// 1. `path` argument (explicit override — e.g. from `--config` / `KHIVE_CONFIG`)
    /// 2. `./.khive/config.toml` (project-local config, relative to the MCP server cwd)
    ///
    /// The project-local default collocates config with the `khive-test.db` that already
    /// lives under `.khive/` in each project directory. `~/.khive/config.toml` is searched
    /// by [`KhiveConfig::load_with_home_fallback`] when the project-local file is absent.
    ///
    /// If the resolved file does **not exist**, returns `Ok(None)`.
    /// A missing config is not an error — callers fall back to the env-var path.
    ///
    /// If the file exists but cannot be parsed, returns a `ConfigError`.
    /// After parsing, `validate()` runs and any logical errors are returned.
    pub fn load(path: Option<&Path>) -> Result<Option<Self>, ConfigError> {
        let resolved = match path {
            Some(p) => p.to_path_buf(),
            None => PathBuf::from(".khive/config.toml"),
        };

        if !resolved.exists() {
            return Ok(None);
        }

        let raw = std::fs::read_to_string(&resolved)?;
        let cfg: KhiveConfig = toml::from_str(&raw).map_err(|source| ConfigError::Parse {
            path: resolved,
            source,
        })?;
        cfg.validate()?;
        Ok(Some(cfg))
    }

    /// Load config with the full resolution order:
    ///
    /// 1. Explicit `path` (from `--config` / `KHIVE_CONFIG`)
    /// 2. `./khive.toml` (project-local, project root)
    /// 3. `<db-dir>/config.toml` (project-local, anchored to the resolved database's
    ///    own directory — see `project_config_anchor_dir`)
    /// 4. `~/.khive/config.toml` (user-global)
    ///
    /// Returns the first file found, or `Ok(None)` when none exist.
    /// Parse errors are propagated immediately — a malformed config is always
    /// an error regardless of which tier it came from.
    ///
    /// `db_path` should be the same database path the caller is about to open
    /// (or has already resolved). Passing it makes tier 3 resolve identically
    /// for any two processes that target the same database, regardless of
    /// their process working directory — this is what lets a thin client and
    /// a warm daemon serving the same database agree on one config file. Pass
    /// `None` when no database path is known yet; tier 3 then falls back to
    /// the process cwd, matching the pre-existing behavior.
    pub fn load_with_home_fallback(
        path: Option<&Path>,
        db_path: Option<&Path>,
    ) -> Result<Option<Self>, ConfigError> {
        // Tier 1: explicit path (highest priority).
        if let Some(p) = path {
            return Self::load(Some(p));
        }

        // Tiers 2-4: search project root, db-anchored hidden dir, user-global.
        let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let home_root = std::env::var_os("HOME").map(PathBuf::from);
        Self::load_with_roots(&project_root, home_root.as_deref(), db_path)
    }

    /// Testable inner search: tiers 2-4, given explicit roots instead of
    /// reading `cwd` and `HOME` from process state.
    ///
    /// - Tier 2: `<project_root>/khive.toml` (still cwd-anchored — unchanged)
    /// - Tier 3: `<db_dir>/config.toml`, anchored to `db_path` rather than
    ///   `project_root` (see `project_config_anchor_dir`); falls back to
    ///   `<project_root>/.khive/config.toml` when `db_path` is `None`
    /// - Tier 4: `<home_root>/.khive/config.toml` (skipped when `None`)
    pub(crate) fn load_with_roots(
        project_root: &Path,
        home_root: Option<&Path>,
        db_path: Option<&Path>,
    ) -> Result<Option<Self>, ConfigError> {
        // Tier 2: project root khive.toml.
        let tier2 = project_root.join("khive.toml");
        if tier2.exists() {
            return Self::load(Some(&tier2));
        }

        // Tier 3: project-local hidden dir, anchored to the resolved database's
        // own directory instead of the process cwd.
        let tier3 = Self::project_config_anchor_dir(db_path, project_root).join("config.toml");
        if tier3.exists() {
            return Self::load(Some(&tier3));
        }

        // Tier 4: user-global ~/.khive/config.toml.
        if let Some(home) = home_root {
            let tier4 = home.join(".khive/config.toml");
            if tier4.exists() {
                return Self::load(Some(&tier4));
            }
        }

        Ok(None)
    }

    /// Resolve the directory searched for the tier-3 project-local config file.
    ///
    /// Anchored to the directory containing the resolved database file, not the
    /// process cwd: two processes at different working directories that open the
    /// same database agree on this directory, which is what keeps their
    /// `config_id` fingerprints in sync (a client and a warm daemon serving the
    /// same database must resolve identical config so the daemon accepts the
    /// client's forwarded requests instead of rejecting them on a config
    /// mismatch).
    ///
    /// `db_path` is canonicalized first so symlinks/relative components collapse
    /// to the same absolute directory regardless of caller cwd. The database file
    /// may not exist yet (first run before anything has been written) — in that
    /// case canonicalization fails and the path is absolutized against
    /// `project_root` instead (or used as-is if already absolute); this must
    /// never panic, it is the expected cold-start case.
    ///
    /// If `db_dir` (the resolved database's parent directory) is itself named
    /// `.khive`, the config lives directly inside it (`<db_dir>/config.toml`) —
    /// this is the common case where the database is `<root>/.khive/khive.db`.
    /// Otherwise the config lives in a `.khive` subdirectory of `db_dir`.
    ///
    /// `db_path == None` (e.g. an in-memory database, or no database path known
    /// yet) falls back to `<project_root>/.khive`, preserving the pre-existing
    /// cwd-anchored behavior for callers with no database to anchor on.
    fn project_config_anchor_dir(db_path: Option<&Path>, project_root: &Path) -> PathBuf {
        let Some(db_path) = db_path else {
            return project_root.join(".khive");
        };

        let absolute = std::fs::canonicalize(db_path).unwrap_or_else(|_| {
            if db_path.is_absolute() {
                db_path.to_path_buf()
            } else {
                project_root.join(db_path)
            }
        });

        let db_dir = absolute.parent().map(Path::to_path_buf).unwrap_or(absolute);

        if db_dir.file_name().is_some_and(|name| name == ".khive") {
            db_dir
        } else {
            db_dir.join(".khive")
        }
    }

    /// Validate the parsed config for logical consistency.
    ///
    /// Checks:
    /// - Exactly one engine has `default = true` (when the list is non-empty).
    /// - Engine names are unique.
    /// - `fusion_weight`, when present, is `> 0`.
    ///
    /// Model name validity is checked lazily at runtime (the config loader does
    /// not import `lattice_embed` directly to keep the dep surface minimal).
    pub fn validate(&self) -> Result<(), ConfigError> {
        // Reject a top-level `db` key loudly instead of letting serde's
        // forward-compatible unknown-key tolerance silently swallow it — a
        // config author expecting `db=` to select the database would
        // otherwise get silent divergence from `--db`/`KHIVE_DB`.
        if let Some(value) = self.db.as_deref() {
            if !value.is_empty() {
                return Err(ConfigError::UnsupportedTopLevelDb {
                    value: value.to_string(),
                });
            }
        }

        // Validate actor.id when present — an invalid namespace is a startup error,
        // not a silent fallback.
        if let Some(id) = self.actor.id.as_deref() {
            if id.is_empty() {
                return Err(ConfigError::InvalidActorId {
                    id: id.to_string(),
                    reason: "actor.id must not be empty; remove the key or provide a value"
                        .to_string(),
                });
            }
            Namespace::parse(id).map_err(|e| ConfigError::InvalidActorId {
                id: id.to_string(),
                reason: e.to_string(),
            })?;
        }

        if let Some(ref vis) = self.actor.visible_namespaces {
            for ns_str in vis {
                if ns_str.is_empty() {
                    return Err(ConfigError::InvalidActorId {
                        id: ns_str.clone(),
                        reason: "visible_namespaces entries must not be empty".to_string(),
                    });
                }
                Namespace::parse(ns_str).map_err(|e| ConfigError::InvalidActorId {
                    id: ns_str.clone(),
                    reason: format!("invalid visible namespace: {e}"),
                })?;
            }
        }

        // Validate actor.allowed_outbound_namespaces (fail-closed at startup on malformed entry).
        for ns_str in &self.actor.allowed_outbound_namespaces {
            if ns_str.is_empty() {
                return Err(ConfigError::InvalidActorId {
                    id: ns_str.clone(),
                    reason: "allowed_outbound_namespaces entries must not be empty".to_string(),
                });
            }
            Namespace::parse(ns_str).map_err(|e| ConfigError::InvalidActorId {
                id: ns_str.clone(),
                reason: format!("invalid allowed_outbound_namespaces entry: {e}"),
            })?;
        }

        // Backend names must be unique.
        if !self.backends.is_empty() {
            let mut seen_backends = std::collections::HashSet::new();
            for backend in &self.backends {
                if !seen_backends.insert(backend.name.clone()) {
                    return Err(ConfigError::DuplicateBackendName {
                        name: backend.name.clone(),
                    });
                }

                // Reject fields that are parsed but not yet implemented — silently
                // accepting them would let misconfiguration slip past startup.
                if backend.cache_mb.is_some() {
                    return Err(ConfigError::UnsupportedBackendField {
                        name: backend.name.clone(),
                        field: "cache_mb",
                    });
                }
                if backend.journal_mode.is_some() {
                    return Err(ConfigError::UnsupportedBackendField {
                        name: backend.name.clone(),
                        field: "journal_mode",
                    });
                }
            }

            // Every pack-referenced backend name must be declared in `backends`.
            let defined: Vec<&str> = self.backends.iter().map(|b| b.name.as_str()).collect();
            for (pack_name, pack_cfg) in &self.packs {
                if !defined.contains(&pack_cfg.backend.as_str()) {
                    return Err(ConfigError::UnknownPackBackend {
                        pack: pack_name.clone(),
                        backend: pack_cfg.backend.clone(),
                        defined: defined.join(", "),
                    });
                }
            }
        }

        if self.engines.is_empty() {
            return Ok(());
        }

        let mut seen_names = std::collections::HashSet::new();
        for engine in &self.engines {
            if !seen_names.insert(engine.name.clone()) {
                return Err(ConfigError::DuplicateName {
                    name: engine.name.clone(),
                });
            }
        }

        let default_count = self.engines.iter().filter(|e| e.default).count();
        if default_count != 1 {
            return Err(ConfigError::DefaultCount {
                found: default_count,
            });
        }

        // Reject non-finite fusion_weight explicitly: NaN doesn't satisfy `w <= 0.0`
        // and +inf is unbounded, so neither is caught by the range check alone.
        for engine in &self.engines {
            if let Some(w) = engine.fusion_weight {
                if !w.is_finite() || w <= 0.0 {
                    return Err(ConfigError::InvalidFusionWeight {
                        name: engine.name.clone(),
                        value: w,
                    });
                }
            }
        }

        Ok(())
    }

    /// Return the engine flagged `default = true`, or `None` if the list is empty.
    pub fn default_engine(&self) -> Option<&EngineConfig> {
        self.engines.iter().find(|e| e.default)
    }
}

// ---- Env-var fallback ----

/// Build an in-memory `KhiveConfig` from the legacy env-var path.
///
/// Used when no config file is present. Emits `tracing::info!` directing
/// operators to migrate to `~/.khive/config.toml`.
///
/// The primary model (`KHIVE_EMBEDDING_MODEL`) becomes the `default = true`
/// engine; additional models become non-default secondary engines.
pub fn config_from_env() -> KhiveConfig {
    let primary_model = std::env::var("KHIVE_EMBEDDING_MODEL")
        .ok()
        .filter(|s| !s.trim().is_empty());
    let additional_raw = std::env::var("KHIVE_ADDITIONAL_EMBEDDING_MODELS")
        .ok()
        .unwrap_or_default();
    let additional: Vec<String> = crate::runtime::parse_pack_list(&additional_raw)
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();

    if primary_model.is_none() && additional.is_empty() {
        return KhiveConfig::default();
    }

    tracing::info!(
        "using env-var embedding config; consider migrating to .khive/config.toml in your project root"
    );

    let mut engines = Vec::new();

    if let Some(model) = primary_model {
        engines.push(EngineConfig {
            name: "default".to_string(),
            model,
            default: true,
            fusion_weight: None,
            dims: None,
        });
    }

    for (i, model) in additional.into_iter().enumerate() {
        engines.push(EngineConfig {
            name: format!("engine-{}", i + 1),
            model,
            default: false,
            fusion_weight: None,
            dims: None,
        });
    }

    // If no primary was specified but there are additional models, promote the
    // first additional model as the default so the list stays valid.
    if !engines.is_empty() && !engines.iter().any(|e| e.default) {
        engines[0].default = true;
    }

    KhiveConfig {
        engines,
        ..KhiveConfig::default()
    }
}

// ---- Tests ----

// Kept inline (not tests/): exercises private ConfigError variants not part
// of the public API.
#[cfg(test)]
mod tests {
    use super::*;

    fn write_toml(dir: &tempfile::TempDir, content: &str) -> PathBuf {
        let path = dir.path().join("config.toml");
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn test_load_minimal_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(
            &dir,
            r#"
[[engines]]
name = "x"
model = "all-minilm-l6-v2"
default = true
"#,
        );
        let cfg = KhiveConfig::load(Some(&path))
            .expect("load should succeed")
            .expect("file should be found");
        assert_eq!(cfg.engines.len(), 1);
        assert_eq!(cfg.engines[0].name, "x");
        assert_eq!(cfg.engines[0].model, "all-minilm-l6-v2");
        assert!(cfg.engines[0].default);
    }

    #[test]
    fn test_default_engine_required_when_engines_present() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(
            &dir,
            r#"
[[engines]]
name = "a"
model = "all-minilm-l6-v2"
"#,
        );
        let err = KhiveConfig::load(Some(&path)).expect_err("should fail with no default flagged");
        assert!(
            matches!(err, ConfigError::DefaultCount { found: 0 }),
            "expected DefaultCount {{ found: 0 }}, got {err:?}"
        );
    }

    #[test]
    fn test_multiple_default_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(
            &dir,
            r#"
[[engines]]
name = "a"
model = "all-minilm-l6-v2"
default = true

[[engines]]
name = "b"
model = "paraphrase-multilingual-minilm-l12-v2"
default = true
"#,
        );
        let err = KhiveConfig::load(Some(&path)).expect_err("should fail with two defaults");
        assert!(
            matches!(err, ConfigError::DefaultCount { found: 2 }),
            "expected DefaultCount {{ found: 2 }}, got {err:?}"
        );
    }

    #[test]
    fn test_fusion_weight_validation() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(
            &dir,
            r#"
[[engines]]
name = "a"
model = "all-minilm-l6-v2"
default = true
fusion_weight = -0.5
"#,
        );
        let err =
            KhiveConfig::load(Some(&path)).expect_err("should fail with negative fusion_weight");
        assert!(
            matches!(err, ConfigError::InvalidFusionWeight { .. }),
            "expected InvalidFusionWeight, got {err:?}"
        );

        let path2 = write_toml(
            &dir,
            r#"
[[engines]]
name = "a"
model = "all-minilm-l6-v2"
default = true
fusion_weight = 0.0
"#,
        );
        let err2 =
            KhiveConfig::load(Some(&path2)).expect_err("should fail with zero fusion_weight");
        assert!(
            matches!(err2, ConfigError::InvalidFusionWeight { .. }),
            "expected InvalidFusionWeight, got {err2:?}"
        );
    }

    #[test]
    fn test_env_var_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("missing.toml");

        let loaded = KhiveConfig::load(Some(&absent)).unwrap();
        assert!(loaded.is_none());

        // Can't safely set env vars in a parallel test suite, so exercise the
        // direct construction path instead.
        let primary = "all-minilm-l6-v2".to_string();
        let additional = vec!["paraphrase-multilingual-minilm-l12-v2".to_string()];

        let mut engines = vec![EngineConfig {
            name: "default".to_string(),
            model: primary,
            default: true,
            fusion_weight: None,
            dims: None,
        }];
        for (i, model) in additional.into_iter().enumerate() {
            engines.push(EngineConfig {
                name: format!("engine-{}", i + 1),
                model,
                default: false,
                fusion_weight: None,
                dims: None,
            });
        }
        let cfg = KhiveConfig {
            engines,
            ..KhiveConfig::default()
        };
        cfg.validate().expect("env-derived config should be valid");
        assert_eq!(cfg.engines.len(), 2);
        assert!(cfg.default_engine().is_some());
        assert_eq!(cfg.default_engine().unwrap().name, "default");
    }

    #[test]
    fn test_file_overrides_env() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(
            &dir,
            r#"
[[engines]]
name = "file-engine"
model = "all-minilm-l6-v2"
default = true
"#,
        );

        // KhiveConfig::load returns the file config regardless of env vars;
        // warning-on-conflict is the caller's responsibility.
        let cfg = KhiveConfig::load(Some(&path))
            .expect("load should succeed")
            .expect("file should be present");
        assert_eq!(cfg.engines[0].name, "file-engine");
    }

    #[test]
    fn test_duplicate_engine_names_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(
            &dir,
            r#"
[[engines]]
name = "shared"
model = "all-minilm-l6-v2"
default = true

[[engines]]
name = "shared"
model = "paraphrase-multilingual-minilm-l12-v2"
"#,
        );
        let err = KhiveConfig::load(Some(&path)).expect_err("should fail with duplicate name");
        assert!(
            matches!(err, ConfigError::DuplicateName { .. }),
            "expected DuplicateName, got {err:?}"
        );
    }

    #[test]
    fn test_empty_config_is_valid() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(&dir, "# no engines\n");
        let cfg = KhiveConfig::load(Some(&path))
            .expect("load should succeed")
            .expect("file should be found");
        assert!(cfg.engines.is_empty());
        cfg.validate().expect("empty config should be valid");
    }

    #[test]
    fn test_multi_engine_positive_fusion_weight() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(
            &dir,
            r#"
[[engines]]
name = "primary"
model = "all-minilm-l6-v2"
default = true
fusion_weight = 0.7

[[engines]]
name = "secondary"
model = "paraphrase-multilingual-minilm-l12-v2"
fusion_weight = 0.3
"#,
        );
        let cfg = KhiveConfig::load(Some(&path))
            .expect("load should succeed")
            .expect("file should be found");
        assert_eq!(cfg.engines.len(), 2);
        assert_eq!(cfg.engines[0].fusion_weight, Some(0.7));
        assert_eq!(cfg.engines[1].fusion_weight, Some(0.3));
    }

    #[test]
    fn test_actor_id_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(
            &dir,
            r#"
[actor]
id = "lambda:khive"
display_name = "example actor"
"#,
        );
        let cfg = KhiveConfig::load(Some(&path))
            .expect("load should succeed")
            .expect("file should be found");
        assert_eq!(cfg.actor.id.as_deref(), Some("lambda:khive"));
        assert_eq!(cfg.actor.display_name.as_deref(), Some("example actor"));
        assert!(cfg.engines.is_empty());
    }

    #[test]
    fn test_actor_and_engines_together() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(
            &dir,
            r#"
[actor]
id = "lambda:test"

[[engines]]
name = "default"
model = "all-minilm-l6-v2"
default = true
"#,
        );
        let cfg = KhiveConfig::load(Some(&path))
            .expect("load should succeed")
            .expect("file should be found");
        assert_eq!(cfg.actor.id.as_deref(), Some("lambda:test"));
        assert_eq!(cfg.engines.len(), 1);
    }

    #[test]
    fn test_actor_absent_defaults_to_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(
            &dir,
            r#"
[[engines]]
name = "x"
model = "all-minilm-l6-v2"
default = true
"#,
        );
        let cfg = KhiveConfig::load(Some(&path))
            .expect("load should succeed")
            .expect("file should be found");
        assert!(
            cfg.actor.id.is_none(),
            "actor.id must be None when [actor] section is absent"
        );
    }

    #[test]
    fn test_load_with_home_fallback_no_files() {
        let project_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        let result = KhiveConfig::load_with_roots(project_dir.path(), Some(home_dir.path()), None);
        assert!(
            result.expect("no error expected").is_none(),
            "should return None when no config files exist in the given roots"
        );
    }

    #[test]
    fn test_load_with_home_fallback_explicit_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(
            &dir,
            r#"
[actor]
id = "lambda:explicit"
"#,
        );
        let cfg = KhiveConfig::load_with_home_fallback(Some(&path), None)
            .expect("no error expected")
            .expect("file found");
        assert_eq!(cfg.actor.id.as_deref(), Some("lambda:explicit"));
    }

    #[test]
    fn test_invalid_actor_id_rejected_at_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(
            &dir,
            r#"
[actor]
id = "bad namespace"
"#,
        );
        let err = KhiveConfig::load(Some(&path)).expect_err("should fail with invalid actor.id");
        assert!(
            matches!(err, ConfigError::InvalidActorId { .. }),
            "expected InvalidActorId, got {err:?}"
        );
    }

    #[test]
    fn test_empty_actor_id_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(
            &dir,
            r#"
[actor]
id = ""
"#,
        );
        let err = KhiveConfig::load(Some(&path)).expect_err("empty actor.id should be rejected");
        assert!(
            matches!(err, ConfigError::InvalidActorId { .. }),
            "expected InvalidActorId for empty string, got {err:?}"
        );
    }

    #[test]
    fn test_malformed_actor_id_lambda_colon_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(
            &dir,
            r#"
[actor]
id = "lambda:"
"#,
        );
        let err =
            KhiveConfig::load(Some(&path)).expect_err("lambda: with no slug should be rejected");
        assert!(
            matches!(err, ConfigError::InvalidActorId { .. }),
            "expected InvalidActorId for 'lambda:', got {err:?}"
        );
    }

    // actor.id must not become default_namespace — writes stay pinned to `local`
    // even though a non-local actor.id widens the default read visible-set.
    #[test]
    fn test_runtime_config_actor_id_does_not_override_namespace() {
        use crate::runtime::runtime_config_from_khive_config;
        use crate::RuntimeConfig;
        use khive_types::namespace::Namespace;

        let cfg = KhiveConfig {
            engines: vec![],
            actor: ActorConfig {
                id: Some("lambda:test-actor".to_string()),
                display_name: None,
                ..Default::default()
            },
            ..KhiveConfig::default()
        };
        cfg.validate().expect("valid config");

        let base = RuntimeConfig::default();
        let result = runtime_config_from_khive_config(&cfg, base);
        assert_eq!(
            result.default_namespace,
            Namespace::local(),
            "actor.id must NOT become default_namespace (ADR-007 Rev 4 Rule 0); \
             writes stay pinned to local"
        );
        // actor.id must also appear in visible_namespaces — the load-bearing
        // side effect that widens default reads to {local} ∪ {actor namespace}.
        assert!(
            result
                .visible_namespaces
                .contains(&Namespace::parse("lambda:test-actor").unwrap()),
            "actor.id must be folded into visible_namespaces (ADR-007 Rev 4 Rule 3b fold-in); \
             got: {:?}",
            result.visible_namespaces
        );
    }

    #[test]
    fn test_runtime_config_no_actor_preserves_base() {
        use crate::runtime::runtime_config_from_khive_config;
        use crate::RuntimeConfig;
        use khive_types::namespace::Namespace;

        let cfg = KhiveConfig {
            engines: vec![],
            actor: ActorConfig {
                id: None,
                display_name: None,
                ..Default::default()
            },
            ..KhiveConfig::default()
        };
        cfg.validate().expect("valid config");

        let base_ns = Namespace::parse("lambda:base").unwrap();
        let base = RuntimeConfig {
            default_namespace: base_ns.clone(),
            ..RuntimeConfig::default()
        };
        let result = runtime_config_from_khive_config(&cfg, base);
        assert_eq!(
            result.default_namespace, base_ns,
            "no actor.id must leave base namespace unchanged"
        );
    }

    #[test]
    fn test_load_with_home_fallback_project_root_over_hidden() {
        let dir = tempfile::tempdir().unwrap();

        // Write .khive/config.toml (tier 3).
        std::fs::create_dir_all(dir.path().join(".khive")).unwrap();
        std::fs::write(
            dir.path().join(".khive/config.toml"),
            "[actor]\nid = \"lambda:hidden\"\n",
        )
        .unwrap();

        // Write khive.toml (tier 2) — should win.
        std::fs::write(
            dir.path().join("khive.toml"),
            "[actor]\nid = \"lambda:project-root\"\n",
        )
        .unwrap();

        let cfg = KhiveConfig::load_with_roots(dir.path(), None, None)
            .expect("no error expected")
            .expect("file should be found");
        assert_eq!(
            cfg.actor.id.as_deref(),
            Some("lambda:project-root"),
            "khive.toml (tier 2) must win over .khive/config.toml (tier 3)"
        );
    }

    #[test]
    fn test_load_with_home_fallback_hidden_over_absent_root() {
        let dir = tempfile::tempdir().unwrap();

        std::fs::create_dir_all(dir.path().join(".khive")).unwrap();
        std::fs::write(
            dir.path().join(".khive/config.toml"),
            "[actor]\nid = \"lambda:hidden-config\"\n",
        )
        .unwrap();
        // No khive.toml.

        let cfg = KhiveConfig::load_with_roots(dir.path(), None, None)
            .expect("no error expected")
            .expect("file should be found");
        assert_eq!(
            cfg.actor.id.as_deref(),
            Some("lambda:hidden-config"),
            ".khive/config.toml (tier 3) must be found when khive.toml is absent"
        );
    }

    #[test]
    fn test_load_with_roots_home_tier_found() {
        let project_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();

        std::fs::create_dir_all(home_dir.path().join(".khive")).unwrap();
        std::fs::write(
            home_dir.path().join(".khive/config.toml"),
            "[actor]\nid = \"lambda:user-global\"\n",
        )
        .unwrap();
        // No project-level files.

        let cfg = KhiveConfig::load_with_roots(project_dir.path(), Some(home_dir.path()), None)
            .expect("no error expected")
            .expect("file should be found");
        assert_eq!(
            cfg.actor.id.as_deref(),
            Some("lambda:user-global"),
            "~/.khive/config.toml (tier 4) must be found when project files absent"
        );
    }

    #[test]
    fn test_load_with_roots_project_wins_over_home() {
        let project_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();

        // Home has a config.
        std::fs::create_dir_all(home_dir.path().join(".khive")).unwrap();
        std::fs::write(
            home_dir.path().join(".khive/config.toml"),
            "[actor]\nid = \"lambda:user-global\"\n",
        )
        .unwrap();

        // Project also has a config — should win.
        std::fs::create_dir_all(project_dir.path().join(".khive")).unwrap();
        std::fs::write(
            project_dir.path().join(".khive/config.toml"),
            "[actor]\nid = \"lambda:project-wins\"\n",
        )
        .unwrap();

        let cfg = KhiveConfig::load_with_roots(project_dir.path(), Some(home_dir.path()), None)
            .expect("no error expected")
            .expect("file should be found");
        assert_eq!(
            cfg.actor.id.as_deref(),
            Some("lambda:project-wins"),
            "project .khive/config.toml (tier 3) must win over ~/.khive/config.toml (tier 4)"
        );
    }

    // ── tier-3 db-dir anchor tests (config discovery canonicalization) ─────

    // Two different process working directories, targeting the same database,
    // must resolve the identical tier-3 config file. Each cwd also carries its
    // own decoy `.khive/config.toml` so the test fails loudly (mismatched
    // actor ids) if the resolver ever falls back to the old cwd anchor instead
    // of the db-dir anchor.
    #[test]
    fn test_load_with_roots_same_db_different_cwd_resolves_identical_config() {
        let cwd_a = tempfile::tempdir().unwrap();
        let cwd_b = tempfile::tempdir().unwrap();

        // Decoy cwd-anchored configs — must NOT be picked up once anchoring
        // moves to the db directory.
        std::fs::create_dir_all(cwd_a.path().join(".khive")).unwrap();
        std::fs::write(
            cwd_a.path().join(".khive/config.toml"),
            "[actor]\nid = \"lambda:wrong-cwd-a\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(cwd_b.path().join(".khive")).unwrap();
        std::fs::write(
            cwd_b.path().join(".khive/config.toml"),
            "[actor]\nid = \"lambda:wrong-cwd-b\"\n",
        )
        .unwrap();

        // The database and its co-located config live under a THIRD root,
        // distinct from either simulated cwd.
        let db_root = tempfile::tempdir().unwrap();
        let khive_dir = db_root.path().join(".khive");
        std::fs::create_dir_all(&khive_dir).unwrap();
        let db_path = khive_dir.join("khive.db");
        std::fs::write(&db_path, b"").unwrap(); // must exist for canonicalize to succeed
        std::fs::write(
            khive_dir.join("config.toml"),
            "[actor]\nid = \"lambda:db-anchored\"\n",
        )
        .unwrap();

        let cfg_a = KhiveConfig::load_with_roots(cwd_a.path(), None, Some(&db_path))
            .expect("no error expected")
            .expect("db-anchored config must be found from cwd A");
        let cfg_b = KhiveConfig::load_with_roots(cwd_b.path(), None, Some(&db_path))
            .expect("no error expected")
            .expect("db-anchored config must be found from cwd B");

        assert_eq!(
            cfg_a.actor.id.as_deref(),
            Some("lambda:db-anchored"),
            "cwd A must resolve the db-anchored config, not its own decoy"
        );
        assert_eq!(
            cfg_b.actor.id.as_deref(),
            Some("lambda:db-anchored"),
            "cwd B must resolve the db-anchored config, not its own decoy"
        );
        assert_eq!(
            cfg_a.actor.id, cfg_b.actor.id,
            "two processes at different cwds targeting the same db must resolve \
             identical config, killing config_id drift between client and daemon"
        );
    }

    // Explicit `--config`/`KHIVE_CONFIG` (tier 1) must still win over the new
    // db-dir anchor (tier 3) — precedence is preserved, only the tier-3 anchor
    // moved.
    #[test]
    fn test_load_with_home_fallback_explicit_config_wins_over_db_anchor() {
        let explicit_dir = tempfile::tempdir().unwrap();
        let explicit_path = write_toml(&explicit_dir, "[actor]\nid = \"lambda:explicit-wins\"\n");

        let db_root = tempfile::tempdir().unwrap();
        let khive_dir = db_root.path().join(".khive");
        std::fs::create_dir_all(&khive_dir).unwrap();
        let db_path = khive_dir.join("khive.db");
        std::fs::write(&db_path, b"").unwrap();
        std::fs::write(
            khive_dir.join("config.toml"),
            "[actor]\nid = \"lambda:db-anchor-loses\"\n",
        )
        .unwrap();

        let cfg = KhiveConfig::load_with_home_fallback(Some(&explicit_path), Some(&db_path))
            .expect("no error expected")
            .expect("explicit path must be found");
        assert_eq!(
            cfg.actor.id.as_deref(),
            Some("lambda:explicit-wins"),
            "explicit --config/KHIVE_CONFIG must win over the db-dir anchor"
        );
    }

    // Tier 4 (`~/.khive/config.toml`) must still be reached when the db-anchored
    // tier-3 directory has no `config.toml` alongside it.
    #[test]
    fn test_load_with_roots_home_fallback_reached_when_db_anchor_has_no_config() {
        let cwd = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home_dir.path().join(".khive")).unwrap();
        std::fs::write(
            home_dir.path().join(".khive/config.toml"),
            "[actor]\nid = \"lambda:home-fallback\"\n",
        )
        .unwrap();

        // A real db directory that exists but has no co-located config.toml.
        let db_root = tempfile::tempdir().unwrap();
        let khive_dir = db_root.path().join(".khive");
        std::fs::create_dir_all(&khive_dir).unwrap();
        let db_path = khive_dir.join("khive.db");
        std::fs::write(&db_path, b"").unwrap();

        let cfg = KhiveConfig::load_with_roots(cwd.path(), Some(home_dir.path()), Some(&db_path))
            .expect("no error expected")
            .expect("home-tier config must be found");
        assert_eq!(
            cfg.actor.id.as_deref(),
            Some("lambda:home-fallback"),
            "tier 4 (~/.khive/config.toml) must still be reached when the db-anchored \
             tier-3 directory has no config.toml"
        );
    }

    // Cold start: the database file does not exist yet (first run). Anchor
    // resolution must not panic and must fall through the remaining tiers.
    #[test]
    fn test_load_with_roots_nonexistent_db_path_does_not_panic_and_falls_through() {
        let cwd = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home_dir.path().join(".khive")).unwrap();
        std::fs::write(
            home_dir.path().join(".khive/config.toml"),
            "[actor]\nid = \"lambda:home-cold-start\"\n",
        )
        .unwrap();

        // Absolute path under a directory tree that was never created.
        let nonexistent_db = cwd.path().join("never-created/.khive/khive.db");

        let cfg =
            KhiveConfig::load_with_roots(cwd.path(), Some(home_dir.path()), Some(&nonexistent_db))
                .expect("cold-start db path must not error or panic")
                .expect("home-tier config must still be found");
        assert_eq!(
            cfg.actor.id.as_deref(),
            Some("lambda:home-cold-start"),
            "a nonexistent db path (cold start) must fall through to tier 4, not panic"
        );
    }

    // Cold start with a *relative* nonexistent db path exercises the
    // cwd-join fallback branch specifically (as opposed to the
    // already-absolute fallback branch above). Must not panic; no config
    // exists anywhere so the result is `Ok(None)`.
    #[test]
    fn test_load_with_roots_relative_nonexistent_db_path_does_not_panic() {
        let cwd = tempfile::tempdir().unwrap();
        let relative_db = PathBuf::from("never-created/.khive/khive.db");

        let result = KhiveConfig::load_with_roots(cwd.path(), None, Some(&relative_db));
        assert!(
            result.is_ok(),
            "relative cold-start db path must not error or panic: {result:?}"
        );
        assert!(
            result.unwrap().is_none(),
            "no config exists anywhere in this test; result must be None"
        );
    }

    // ── ADR-028 backend / pack config tests ─────────────────────────────────

    #[test]
    fn test_no_backends_section_is_valid() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(
            &dir,
            r#"
[[engines]]
name = "default"
model = "all-minilm-l6-v2"
default = true
"#,
        );
        let cfg = KhiveConfig::load(Some(&path))
            .expect("no error")
            .expect("file found");
        assert!(cfg.backends.is_empty());
        assert!(cfg.packs.is_empty());
    }

    #[test]
    fn test_single_sqlite_backend_parses() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(
            &dir,
            r#"
[[backends]]
name = "knowledge"
kind = "sqlite"
path = "/tmp/knowledge.db"
"#,
        );
        let cfg = KhiveConfig::load(Some(&path))
            .expect("no error")
            .expect("file found");
        assert_eq!(cfg.backends.len(), 1);
        let b = &cfg.backends[0];
        assert_eq!(b.name, "knowledge");
        assert!(matches!(b.kind, BackendKind::Sqlite));
        assert_eq!(
            b.path.as_ref().and_then(|p| p.to_str()),
            Some("/tmp/knowledge.db")
        );
    }

    #[test]
    fn test_memory_backend_parses() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(
            &dir,
            r#"
[[backends]]
name = "ephemeral"
kind = "memory"
"#,
        );
        let cfg = KhiveConfig::load(Some(&path))
            .expect("no error")
            .expect("file found");
        assert_eq!(cfg.backends.len(), 1);
        assert!(matches!(cfg.backends[0].kind, BackendKind::Memory));
    }

    #[test]
    fn test_pack_backend_assignment_parses() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(
            &dir,
            r#"
[[backends]]
name = "knowledge"
kind = "memory"

[packs.knowledge]
backend = "knowledge"
"#,
        );
        let cfg = KhiveConfig::load(Some(&path))
            .expect("no error")
            .expect("file found");
        assert_eq!(cfg.packs.len(), 1);
        let pc = cfg.packs.get("knowledge").expect("knowledge pack present");
        assert_eq!(pc.backend, "knowledge");
    }

    #[test]
    fn test_duplicate_backend_name_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(
            &dir,
            r#"
[[backends]]
name = "dup"
kind = "memory"

[[backends]]
name = "dup"
kind = "memory"
"#,
        );
        let err = KhiveConfig::load(Some(&path)).expect_err("should fail with duplicate name");
        assert!(
            matches!(err, ConfigError::DuplicateBackendName { ref name } if name == "dup"),
            "expected DuplicateBackendName {{ name: \"dup\" }}, got {err:?}"
        );
    }

    #[test]
    fn test_pack_referencing_undefined_backend_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(
            &dir,
            r#"
[[backends]]
name = "knowledge"
kind = "memory"

[packs.kg]
backend = "nonexistent"
"#,
        );
        let err =
            KhiveConfig::load(Some(&path)).expect_err("should fail with unknown backend reference");
        assert!(
            matches!(err, ConfigError::UnknownPackBackend { ref pack, ref backend, .. }
                if pack == "kg" && backend == "nonexistent"),
            "expected UnknownPackBackend for kg→nonexistent, got {err:?}"
        );
    }

    #[test]
    fn test_pack_config_without_backends_section_is_allowed() {
        let dir = tempfile::tempdir().unwrap();
        // When [[backends]] is absent/empty, packs are not validated — all
        // packs fall through to the implicit main backend.
        let path = write_toml(
            &dir,
            r#"
[packs.kg]
backend = "main"
"#,
        );
        let cfg = KhiveConfig::load(Some(&path))
            .expect("no error expected")
            .expect("file found");
        assert_eq!(cfg.backends.len(), 0);
        assert_eq!(cfg.packs.len(), 1);
    }

    #[test]
    fn test_backend_cache_mb_rejected_at_validate() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(
            &dir,
            r#"
[[backends]]
name = "main"
kind = "memory"
cache_mb = 128
"#,
        );
        let err = KhiveConfig::load(Some(&path)).expect_err("cache_mb must be rejected");
        assert!(
            matches!(err, ConfigError::UnsupportedBackendField { ref name, field: "cache_mb" } if name == "main"),
            "expected UnsupportedBackendField {{ name: \"main\", field: \"cache_mb\" }}, got {err:?}"
        );
    }

    #[test]
    fn test_backend_journal_mode_rejected_at_validate() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(
            &dir,
            r#"
[[backends]]
name = "main"
kind = "memory"
journal_mode = "wal"
"#,
        );
        let err = KhiveConfig::load(Some(&path)).expect_err("journal_mode must be rejected");
        assert!(
            matches!(err, ConfigError::UnsupportedBackendField { ref name, field: "journal_mode" } if name == "main"),
            "expected UnsupportedBackendField {{ name: \"main\", field: \"journal_mode\" }}, got {err:?}"
        );
    }

    // A top-level `db` key must be rejected loudly instead of silently
    // ignored as an unknown key by serde's forward-compatible default.
    #[test]
    fn test_top_level_db_rejected_at_validate() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(
            &dir,
            r#"
db = "/tmp/scratch/demo.db"
"#,
        );
        let err = KhiveConfig::load(Some(&path)).expect_err("top-level db must be rejected");
        assert!(
            matches!(err, ConfigError::UnsupportedTopLevelDb { ref value } if value == "/tmp/scratch/demo.db"),
            "expected UnsupportedTopLevelDb {{ value: \"/tmp/scratch/demo.db\" }}, got {err:?}"
        );
    }
}
