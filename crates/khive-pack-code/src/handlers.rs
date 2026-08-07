//! `code.ingest` verb handler (ADR-085 Amendment 2 B1, B7).
//!
//! Opens a fresh `KhiveRuntime` bound to the caller-selected (or default
//! workspace-local) target database — never the shared production
//! runtime/backend the pack itself was constructed with — and drives the L1
//! + L1.5 pipeline in `source_ingest`.

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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IngestParams {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    db: Option<String>,
    #[serde(default)]
    languages: Option<Vec<String>>,
}

fn deserialize_params(params: Value) -> Result<IngestParams, RuntimeError> {
    serde_json::from_value(params)
        .map_err(|e| RuntimeError::InvalidInput(format!("code.ingest: invalid params: {e}")))
}

impl CodePack {
    pub(crate) async fn handle_ingest(&self, params: Value) -> Result<Value, RuntimeError> {
        let IngestParams {
            path: path_raw,
            db,
            languages,
        } = deserialize_params(params)?;
        let path_raw = path_raw
            .ok_or_else(|| RuntimeError::InvalidInput("code.ingest requires path".into()))?;
        let path = PathBuf::from(&path_raw);
        if !path.is_dir() {
            return Err(RuntimeError::InvalidInput(format!(
                "path {path_raw:?} does not exist or is not a directory"
            )));
        }

        let languages: BTreeSet<&'static str> = match languages {
            None => LANGUAGES.iter().copied().collect(),
            Some(values) => {
                let mut set = BTreeSet::new();
                for language in values {
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
                    set.insert(canonical);
                }
                set
            }
        };

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
