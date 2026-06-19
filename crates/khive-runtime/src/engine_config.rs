//! TOML-based embedding engine configuration for khive.
//!
//! Loads `.khive/config.toml` (or `--config` / `KHIVE_CONFIG`) and exposes an
//! `[[engines]]` array for arbitrary-N embedding engine registration. Falls back
//! to `KHIVE_EMBEDDING_MODEL` env vars when no config file is present.

use std::path::{Path, PathBuf};

use khive_types::namespace::Namespace;
use serde::Deserialize;
use thiserror::Error;

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
/// display_name = "Leo global orchestrator"   # human label (optional)
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

/// Top-level khive configuration loaded from `khive.toml` or `config.toml`.
///
/// Sections consumed today:
/// - `[[engines]]`: embedding engine declarations
/// - `[actor]`: default namespace / identity (OSS actor model)
/// - `[runtime]`: runtime knobs (namespace, brain_profile)
///
/// Unknown keys are silently ignored by serde — forward-compatible.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct KhiveConfig {
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
    /// 3. `./.khive/config.toml` (project-local, hidden dir)
    /// 4. `~/.khive/config.toml` (user-global)
    ///
    /// Returns the first file found, or `Ok(None)` when none exist.
    /// Parse errors are propagated immediately — a malformed config is always
    /// an error regardless of which tier it came from.
    pub fn load_with_home_fallback(path: Option<&Path>) -> Result<Option<Self>, ConfigError> {
        // Tier 1: explicit path (highest priority).
        if let Some(p) = path {
            return Self::load(Some(p));
        }

        // Tiers 2-4: search project root, hidden dir, user-global.
        let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let home_root = std::env::var_os("HOME").map(PathBuf::from);
        Self::load_with_roots(&project_root, home_root.as_deref())
    }

    /// Testable inner search: tiers 2-4, given explicit roots instead of
    /// reading `cwd` and `HOME` from process state.
    ///
    /// - Tier 2: `<project_root>/khive.toml`
    /// - Tier 3: `<project_root>/.khive/config.toml`
    /// - Tier 4: `<home_root>/.khive/config.toml` (skipped when `None`)
    pub(crate) fn load_with_roots(
        project_root: &Path,
        home_root: Option<&Path>,
    ) -> Result<Option<Self>, ConfigError> {
        // Tier 2: project root khive.toml.
        let tier2 = project_root.join("khive.toml");
        if tier2.exists() {
            return Self::load(Some(&tier2));
        }

        // Tier 3: project-local hidden dir.
        let tier3 = project_root.join(".khive/config.toml");
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

        // Validate actor.visible_namespaces when present.
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

        if self.engines.is_empty() {
            return Ok(());
        }

        // Unique names
        let mut seen_names = std::collections::HashSet::new();
        for engine in &self.engines {
            if !seen_names.insert(engine.name.clone()) {
                return Err(ConfigError::DuplicateName {
                    name: engine.name.clone(),
                });
            }
        }

        // Exactly one default
        let default_count = self.engines.iter().filter(|e| e.default).count();
        if default_count != 1 {
            return Err(ConfigError::DefaultCount {
                found: default_count,
            });
        }

        // Positive, finite fusion_weight when present.
        // NaN does not satisfy `w <= 0.0`, and positive infinity is unbounded,
        // so reject all non-finite values explicitly before the range check.
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
        actor: ActorConfig::default(),
        runtime: RuntimeSectionConfig::default(),
    }
}

// ---- Tests ----

// INLINE TEST JUSTIFICATION: tests here cover config validation error paths that
// rely on private ConfigError variants and temp-file helpers shared with the
// config loader. Moving them to tests/ would require pub-exporting ConfigError
// internals that are not part of the stable public API.
#[cfg(test)]
mod tests {
    use super::*;

    // Helper: write a temp file and return the path.
    fn write_toml(dir: &tempfile::TempDir, content: &str) -> PathBuf {
        let path = dir.path().join("config.toml");
        std::fs::write(&path, content).unwrap();
        path
    }

    // 1. Minimal config parses successfully.
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

    // 2. Zero default-flagged engines -> error.
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

    // 3. Two engines both flagged default -> error.
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

    // 4. Negative or zero fusion_weight -> error.
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

    // 5. File absent + env vars set -> constructs equivalent KhiveConfig.
    #[test]
    fn test_env_var_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("missing.toml");

        // File does not exist -> KhiveConfig::load returns None.
        let loaded = KhiveConfig::load(Some(&absent)).unwrap();
        assert!(loaded.is_none());

        // With env vars set, config_from_env builds a synthetic config.
        // We can't set env vars safely in a parallel test suite, so test via
        // the direct construction path instead.
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
            actor: ActorConfig::default(),
            runtime: RuntimeSectionConfig::default(),
        };
        cfg.validate().expect("env-derived config should be valid");
        assert_eq!(cfg.engines.len(), 2);
        assert!(cfg.default_engine().is_some());
        assert_eq!(cfg.default_engine().unwrap().name, "default");
    }

    // 6. File present + env vars set -> file wins; test via RuntimeConfig.
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

        // File load succeeds even if env vars would provide a different model.
        // The caller (RuntimeConfig::from_khive_config) is responsible for
        // checking whether env vars are also present and emitting the warning.
        // Here we verify that KhiveConfig::load returns the file config.
        let cfg = KhiveConfig::load(Some(&path))
            .expect("load should succeed")
            .expect("file should be present");
        assert_eq!(cfg.engines[0].name, "file-engine");
    }

    // 7. Duplicate engine names -> error.
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

    // 8. Empty config file -> no engines; validate succeeds.
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

    // 9. Multi-engine config with valid positive fusion_weight -> succeeds.
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

    // 10. [actor] section with id -> parsed correctly.
    #[test]
    fn test_actor_id_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_toml(
            &dir,
            r#"
[actor]
id = "lambda:khive"
display_name = "Ocean's khive lambda"
"#,
        );
        let cfg = KhiveConfig::load(Some(&path))
            .expect("load should succeed")
            .expect("file should be found");
        assert_eq!(cfg.actor.id.as_deref(), Some("lambda:khive"));
        assert_eq!(
            cfg.actor.display_name.as_deref(),
            Some("Ocean's khive lambda")
        );
        assert!(cfg.engines.is_empty());
    }

    // 11. [actor] section with engines -> both parsed.
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

    // 12. Missing [actor] section -> defaults to None id (backward compat).
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

    // 13. load_with_roots returns None when no files exist in the given roots.
    #[test]
    fn test_load_with_home_fallback_no_files() {
        let project_dir = tempfile::tempdir().unwrap();
        let home_dir = tempfile::tempdir().unwrap();
        let result = KhiveConfig::load_with_roots(project_dir.path(), Some(home_dir.path()));
        assert!(
            result.expect("no error expected").is_none(),
            "should return None when no config files exist in the given roots"
        );
    }

    // 14. load_with_home_fallback explicit path overrides search.
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
        let cfg = KhiveConfig::load_with_home_fallback(Some(&path))
            .expect("no error expected")
            .expect("file found");
        assert_eq!(cfg.actor.id.as_deref(), Some("lambda:explicit"));
    }

    // 15. actor.id with an invalid namespace string -> ConfigError::InvalidActorId at load time.
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

    // 16. actor.id = "" (empty string) -> ConfigError::InvalidActorId.
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

    // 17. actor.id = "lambda:" (structurally invalid — no slug) -> ConfigError::InvalidActorId.
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

    // 18. ADR-007 Rev 4 Rule 0: actor.id must NOT become default_namespace — writes
    //     stay pinned to `local`. A non-`'local'` actor.id IS folded into the
    //     default READ visible-set (ADR-007 Rev 4 Rule 3b), but that does not affect
    //     default_namespace. This test asserts the write-routing invariant only.
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
            runtime: RuntimeSectionConfig::default(),
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
        // Also assert the fold-in: actor.id MUST appear in visible_namespaces so that
        // default reads widen to {local} ∪ {actor namespace} (ADR-007 Rev 4 Rule 3b,
        // config.rs:~444). This is the load-bearing side-effect of the actor id config.
        assert!(
            result
                .visible_namespaces
                .contains(&Namespace::parse("lambda:test-actor").unwrap()),
            "actor.id must be folded into visible_namespaces (ADR-007 Rev 4 Rule 3b fold-in); \
             got: {:?}",
            result.visible_namespaces
        );
    }

    // 19. runtime_config_from_khive_config with no actor preserves base namespace.
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
            runtime: RuntimeSectionConfig::default(),
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

    // 20. load_with_roots: khive.toml (tier 2) wins over .khive/config.toml (tier 3).
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

        let cfg = KhiveConfig::load_with_roots(dir.path(), None)
            .expect("no error expected")
            .expect("file should be found");
        assert_eq!(
            cfg.actor.id.as_deref(),
            Some("lambda:project-root"),
            "khive.toml (tier 2) must win over .khive/config.toml (tier 3)"
        );
    }

    // 21. load_with_roots: .khive/config.toml (tier 3) wins when khive.toml absent.
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

        let cfg = KhiveConfig::load_with_roots(dir.path(), None)
            .expect("no error expected")
            .expect("file should be found");
        assert_eq!(
            cfg.actor.id.as_deref(),
            Some("lambda:hidden-config"),
            ".khive/config.toml (tier 3) must be found when khive.toml is absent"
        );
    }

    // 22. load_with_roots: ~/.khive/config.toml (tier 4) found when project files absent.
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

        let cfg = KhiveConfig::load_with_roots(project_dir.path(), Some(home_dir.path()))
            .expect("no error expected")
            .expect("file should be found");
        assert_eq!(
            cfg.actor.id.as_deref(),
            Some("lambda:user-global"),
            "~/.khive/config.toml (tier 4) must be found when project files absent"
        );
    }

    // 23. load_with_roots: project tier wins over home tier.
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

        let cfg = KhiveConfig::load_with_roots(project_dir.path(), Some(home_dir.path()))
            .expect("no error expected")
            .expect("file should be found");
        assert_eq!(
            cfg.actor.id.as_deref(),
            Some("lambda:project-wins"),
            "project .khive/config.toml (tier 3) must win over ~/.khive/config.toml (tier 4)"
        );
    }
}
