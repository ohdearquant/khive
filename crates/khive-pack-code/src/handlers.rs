//! `code.ingest` verb handler (ADR-085 Amendment 2 B1/B7 and Amendment 7).
//!
//! Opens a fresh `KhiveRuntime` bound to the target database — never the
//! shared production runtime/backend the pack itself was constructed with —
//! and drives the caller-selected L1, L1.5, and L2 tiers in `source_ingest`.
//! An explicit `db` must already exist at the current schema version; omitting
//! it intentionally creates/migrates the workspace-local default.

use std::collections::BTreeSet;
use std::path::PathBuf;

use chrono::Utc;
use serde_json::Value;

use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig, RuntimeError};

use crate::db_target::resolve_target_db;
use crate::manifest::LANGUAGES;
use crate::source_ingest::{run_code_ingest, CodeSourceIngestOptions};
use crate::CodePack;

const VALID_TIERS: &[&str] = &["l1", "l1.5", "l2"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TierSelection {
    pub enable_l1: bool,
    pub enable_l1_5: bool,
    pub enable_l2: bool,
}

pub(crate) fn parse_tiers(value: Option<&Value>) -> Result<TierSelection, RuntimeError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(TierSelection {
            enable_l1: true,
            enable_l1_5: true,
            enable_l2: false,
        });
    };
    let entries = value
        .as_array()
        .ok_or_else(|| RuntimeError::InvalidInput("tiers must be an array of strings".into()))?;
    let mut selection = TierSelection {
        enable_l1: false,
        enable_l1_5: false,
        enable_l2: false,
    };
    for entry in entries {
        let tier = entry
            .as_str()
            .ok_or_else(|| RuntimeError::InvalidInput("tiers entries must be strings".into()))?;
        match tier {
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
        let path_raw = params
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| RuntimeError::InvalidInput("code.ingest requires path".into()))?;
        let path = PathBuf::from(path_raw);
        if !path.is_dir() {
            return Err(RuntimeError::InvalidInput(format!(
                "path {path_raw:?} does not exist or is not a directory"
            )));
        }

        let languages: BTreeSet<&'static str> = match params.get("languages") {
            None | Some(Value::Null) => LANGUAGES.iter().copied().collect(),
            Some(v) => {
                let arr = v.as_array().ok_or_else(|| {
                    RuntimeError::InvalidInput("languages must be an array of strings".into())
                })?;
                let mut set = BTreeSet::new();
                for entry in arr {
                    let s = entry.as_str().ok_or_else(|| {
                        RuntimeError::InvalidInput("languages entries must be strings".into())
                    })?;
                    let canonical =
                        LANGUAGES
                            .iter()
                            .find(|l| **l == s)
                            .copied()
                            .ok_or_else(|| {
                                RuntimeError::InvalidInput(format!(
                                    "unknown language {s:?}; valid: {}",
                                    LANGUAGES.join(", ")
                                ))
                            })?;
                    set.insert(canonical);
                }
                set
            }
        };
        let tiers = parse_tiers(params.get("tiers"))?;

        let db_param = params.get("db").and_then(Value::as_str);
        let runtime_db_path = self.runtime.config().db_path.clone();
        let db_path = resolve_target_db(db_param, &path, runtime_db_path.as_deref())
            .map_err(RuntimeError::InvalidInput)?;

        let config = RuntimeConfig {
            db_path: Some(db_path.clone()),
            packs: vec!["kg".to_string(), "code".to_string()],
            ..RuntimeConfig::no_embeddings()
        };
        let target_rt = if db_param.is_some() {
            KhiveRuntime::new_existing_current(config)
        } else {
            KhiveRuntime::new(config)
        }
        .map_err(|e| {
            RuntimeError::InvalidInput(format!("opening target map database {db_path:?}: {e}"))
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

    use super::{parse_tiers, TierSelection};

    #[test]
    fn tiers_default_to_l1_and_l1_5() {
        let expected = TierSelection {
            enable_l1: true,
            enable_l1_5: true,
            enable_l2: false,
        };

        assert_eq!(parse_tiers(None).unwrap(), expected);
        assert_eq!(
            parse_tiers(Some(&serde_json::Value::Null)).unwrap(),
            expected
        );
    }

    #[test]
    fn tiers_accept_an_empty_selection() {
        assert_eq!(
            parse_tiers(Some(&json!([]))).unwrap(),
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
            assert_eq!(parse_tiers(Some(&json!([tier]))).unwrap(), expected);
        }
    }

    #[test]
    fn tiers_deduplicate_entries_and_ignore_input_order() {
        assert_eq!(
            parse_tiers(Some(&json!(["l2", "l1.5", "l1", "l2"]))).unwrap(),
            TierSelection {
                enable_l1: true,
                enable_l1_5: true,
                enable_l2: true,
            }
        );
    }

    #[test]
    fn tiers_reject_a_scalar_value() {
        let scalar = parse_tiers(Some(&json!("l2"))).unwrap_err();
        assert_eq!(
            scalar.to_string(),
            "invalid input: tiers must be an array of strings"
        );
    }

    #[test]
    fn tiers_reject_non_string_entries() {
        let non_string = parse_tiers(Some(&json!(["l1", 2]))).unwrap_err();
        assert_eq!(
            non_string.to_string(),
            "invalid input: tiers entries must be strings"
        );
    }

    #[test]
    fn tiers_reject_unknown_values() {
        let unknown = parse_tiers(Some(&json!(["L2"]))).unwrap_err();
        assert_eq!(
            unknown.to_string(),
            "invalid input: unknown tier \"L2\"; valid: l1, l1.5, l2"
        );
    }
}
