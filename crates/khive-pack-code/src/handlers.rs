//! `code.ingest` verb handler (ADR-085 Amendment 2 B1, B7).
//!
//! Opens a fresh `KhiveRuntime` bound to the caller-selected (or default
//! workspace-local) target database — never the shared production
//! runtime/backend the pack itself was constructed with — and drives the L1
//! + L1.5 pipeline in `source_ingest`.

use std::collections::BTreeSet;
use std::path::PathBuf;

use chrono::Utc;
use serde_json::Value;

use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig, RuntimeError};

use crate::db_target::resolve_target_db;
use crate::manifest::LANGUAGES;
use crate::source_ingest::{run_code_ingest, CodeSourceIngestOptions};
use crate::CodePack;

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

        let db_param = params.get("db").and_then(Value::as_str);
        let runtime_db_path = self.runtime.config().db_path.clone();
        let db_path = resolve_target_db(db_param, &path, runtime_db_path.as_deref())
            .map_err(RuntimeError::InvalidInput)?;

        // Defaults to L1+L1.5 only (`["l1", "l1.5"]`) so a caller that never
        // passes `tiers` sees byte-identical behavior to before L2 existed
        // (ADR-085 Amendment 2 B3: tiers are independently usable). `l2`
        // opts into the Rust Scanner/Extractor symbol pass.
        const VALID_TIERS: &[&str] = &["l1", "l1.5", "l2"];
        let enable_l2 = match params.get("tiers") {
            None | Some(Value::Null) => false,
            Some(v) => {
                let arr = v.as_array().ok_or_else(|| {
                    RuntimeError::InvalidInput("tiers must be an array of strings".into())
                })?;
                let mut enable_l2 = false;
                for entry in arr {
                    let s = entry.as_str().ok_or_else(|| {
                        RuntimeError::InvalidInput("tiers entries must be strings".into())
                    })?;
                    if !VALID_TIERS.contains(&s) {
                        return Err(RuntimeError::InvalidInput(format!(
                            "unknown tier {s:?}; valid: {}",
                            VALID_TIERS.join(", ")
                        )));
                    }
                    if s == "l2" {
                        enable_l2 = true;
                    }
                }
                enable_l2
            }
        };

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
                enable_l2,
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
