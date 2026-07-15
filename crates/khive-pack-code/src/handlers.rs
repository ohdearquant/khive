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
        let db_path = resolve_target_db(db_param, &path).map_err(RuntimeError::InvalidInput)?;

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
