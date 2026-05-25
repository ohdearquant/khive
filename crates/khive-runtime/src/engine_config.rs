//! TOML-based embedding engine configuration for khive.
//!
//! Loads `./.khive/config.toml` (or a path from `--config` / `KHIVE_CONFIG`)
//! and exposes an `[[engines]]` array that drives arbitrary-N embedding engine
//! registration per ADR-031 §D3.
//!
//! # Config file format
//!
//! ```toml
//! [[engines]]
//! name = "default"
//! model = "all-minilm-l6-v2"
//! default = true
//! fusion_weight = 0.5
//!
//! [[engines]]
//! name = "paraphrase"
//! model = "paraphrase-multilingual-minilm-l12-v2"
//! fusion_weight = 0.5
//! ```
//!
//! # Resolution order
//!
//! 1. Config file (from `--config` / `KHIVE_CONFIG` / `./.khive/config.toml`)
//! 2. Env-var fallback (`KHIVE_EMBEDDING_MODEL` + `KHIVE_ADDITIONAL_EMBEDDING_MODELS`)
//!    when no config file is present
//!
//! If both file and env vars are present, the file wins and a warning is emitted.

use std::path::{Path, PathBuf};

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

/// Top-level khive configuration loaded from `khive.toml` or `config.toml`.
///
/// Only the `[[engines]]` array is consumed today. Future sections (packs,
/// gate, namespace) can be added as named struct fields; unknown keys are
/// silently ignored by serde.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct KhiveConfig {
    /// Embedding engine declarations (ADR-031 §D3).
    #[serde(default)]
    pub engines: Vec<EngineConfig>,
}

impl KhiveConfig {
    /// Load and validate a `KhiveConfig`.
    ///
    /// Search order:
    /// 1. `path` argument (explicit override — e.g. from `--config` / `KHIVE_CONFIG`)
    /// 2. `./.khive/config.toml` (project-local config, relative to the MCP server cwd)
    ///
    /// The project-local default collocates config with the `khive-test.db` that already
    /// lives under `.khive/` in each project directory. `~/.khive/config.toml` is
    /// reserved for personal/global settings and is NOT searched automatically.
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

        // Positive fusion_weight when present
        for engine in &self.engines {
            if let Some(w) = engine.fusion_weight {
                if w <= 0.0 {
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

    KhiveConfig { engines }
}

// ---- Tests ----

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
        let cfg = KhiveConfig { engines };
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
}
