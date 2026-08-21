//! `code.ingest` verb handler (ADR-085 Amendment 2 B1, B7).
//!
//! Opens a fresh `KhiveRuntime` bound to the caller-selected (or default
//! workspace-local) target database — never the shared production
//! runtime/backend the pack itself was constructed with — and drives the
//! caller-selected L1, L1.5, and L2 tiers in `source_ingest`.

use std::collections::BTreeSet;
use std::path::PathBuf;

use chrono::Utc;
use serde::Deserialize;
use serde_json::Value;

use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig, RuntimeError};

use crate::db_target::resolve_target_db;
use crate::manifest::LANGUAGES;
use crate::source_ingest::{run_code_ingest, CodeSourceIngestOptions};
use crate::CodePack;

const VALID_TIERS: &[&str] = &["l1", "l1.5", "l2"];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CodeIngestParams {
    path: String,
    db: Option<String>,
    languages: Option<Vec<String>>,
    tiers: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TierSelection {
    pub enable_l1: bool,
    pub enable_l1_5: bool,
    pub enable_l2: bool,
}

fn parse_params(params: Value) -> Result<CodeIngestParams, RuntimeError> {
    serde_json::from_value(params).map_err(|error| {
        RuntimeError::InvalidInput(format!("invalid code.ingest arguments: {error}"))
    })
}

fn parse_languages(entries: Option<&[String]>) -> Result<BTreeSet<&'static str>, RuntimeError> {
    let Some(entries) = entries else {
        return Ok(LANGUAGES.iter().copied().collect());
    };
    let mut languages = BTreeSet::new();
    for language in entries {
        let canonical = LANGUAGES
            .iter()
            .find(|candidate| **candidate == language.as_str())
            .copied()
            .ok_or_else(|| {
                RuntimeError::InvalidInput(format!(
                    "unknown language {language:?}; valid: {}",
                    LANGUAGES.join(", ")
                ))
            })?;
        languages.insert(canonical);
    }
    Ok(languages)
}

pub(crate) fn parse_tiers(entries: Option<&[String]>) -> Result<TierSelection, RuntimeError> {
    let Some(entries) = entries else {
        return Ok(TierSelection {
            enable_l1: true,
            enable_l1_5: true,
            enable_l2: false,
        });
    };
    let mut selection = TierSelection {
        enable_l1: false,
        enable_l1_5: false,
        enable_l2: false,
    };
    for tier in entries {
        match tier.as_str() {
            "l1" => selection.enable_l1 = true,
            "l1.5" => selection.enable_l1_5 = true,
            "l2" => selection.enable_l2 = true,
            _ => {
                return Err(RuntimeError::InvalidInput(format!(
                    "unknown tier {tier:?}; valid: {}",
                    VALID_TIERS.join(", ")
                )));
            }
        }
    }
    Ok(selection)
}

impl CodePack {
    pub(crate) async fn handle_ingest(&self, params: Value) -> Result<Value, RuntimeError> {
        let CodeIngestParams {
            path: path_raw,
            db,
            languages,
            tiers,
        } = parse_params(params)?;
        let languages = parse_languages(languages.as_deref())?;
        let tiers = parse_tiers(tiers.as_deref())?;
        let path = PathBuf::from(&path_raw);
        if !path.is_dir() {
            return Err(RuntimeError::InvalidInput(format!(
                "path {path_raw:?} does not exist or is not a directory"
            )));
        }

        let runtime_db_path = self.runtime.config().db_path.clone();
        let db_path = resolve_target_db(db.as_deref(), &path, runtime_db_path.as_deref())
            .map_err(RuntimeError::InvalidInput)?;

        let config = RuntimeConfig {
            db_path: Some(db_path.clone()),
            packs: vec!["kg".to_string(), "code".to_string()],
            ..RuntimeConfig::no_embeddings()
        };
        let target_rt = KhiveRuntime::new(config).map_err(|e| {
            RuntimeError::InvalidInput(format!("opening target db {db_path:?}: {e}"))
        })?;
        let token = target_rt
            .authorize(Namespace::local())
            .map_err(|e| RuntimeError::InvalidInput(format!("authorizing target db: {e}")))?;

        let report = run_code_ingest(
            &target_rt,
            &token,
            CodeSourceIngestOptions {
                path: &path,
                languages,
                sweep_time: Utc::now(),
                enable_l1: tiers.enable_l1,
                enable_l1_5: tiers.enable_l1_5,
                enable_l2: tiers.enable_l2,
            },
        )
        .await
        .map_err(|e| RuntimeError::InvalidInput(e.to_string()))?;

        let mut value = serde_json::to_value(&report)
            .map_err(|e| RuntimeError::InvalidInput(format!("serializing report: {e}")))?;
        value["db_path"] = Value::String(db_path.display().to_string());
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{parse_params, parse_tiers, TierSelection};

    #[test]
    fn tiers_default_to_l1_and_l1_5() {
        let expected = TierSelection {
            enable_l1: true,
            enable_l1_5: true,
            enable_l2: false,
        };

        assert_eq!(parse_tiers(None).unwrap(), expected);
    }

    #[test]
    fn tiers_accept_an_empty_selection() {
        let tiers = Vec::new();
        assert_eq!(
            parse_tiers(Some(&tiers)).unwrap(),
            TierSelection {
                enable_l1: false,
                enable_l1_5: false,
                enable_l2: false,
            }
        );
    }

    #[test]
    fn tiers_accept_each_supported_value() {
        let cases = [
            (
                "l1",
                TierSelection {
                    enable_l1: true,
                    enable_l1_5: false,
                    enable_l2: false,
                },
            ),
            (
                "l1.5",
                TierSelection {
                    enable_l1: false,
                    enable_l1_5: true,
                    enable_l2: false,
                },
            ),
            (
                "l2",
                TierSelection {
                    enable_l1: false,
                    enable_l1_5: false,
                    enable_l2: true,
                },
            ),
        ];

        for (tier, expected) in cases {
            assert_eq!(parse_tiers(Some(&[tier.to_string()])).unwrap(), expected);
        }
    }

    #[test]
    fn tiers_deduplicate_entries_and_ignore_input_order() {
        let tiers = ["l2", "l1.5", "l1", "l2"].map(str::to_string);
        assert_eq!(
            parse_tiers(Some(&tiers)).unwrap(),
            TierSelection {
                enable_l1: true,
                enable_l1_5: true,
                enable_l2: true,
            }
        );
    }

    #[test]
    fn tiers_reject_a_scalar_value() {
        let scalar = parse_params(json!({"path": ".", "tiers": "l2"})).unwrap_err();
        assert!(scalar.to_string().contains("invalid type: string"));
    }

    #[test]
    fn tiers_reject_non_string_entries() {
        let non_string = parse_params(json!({"path": ".", "tiers": ["l1", 2]})).unwrap_err();
        assert!(non_string.to_string().contains("invalid type: integer"));
    }

    #[test]
    fn tiers_reject_unknown_values() {
        let unknown = parse_tiers(Some(&["L2".to_string()])).unwrap_err();
        assert_eq!(
            unknown.to_string(),
            "invalid input: unknown tier \"L2\"; valid: l1, l1.5, l2"
        );
    }
}
