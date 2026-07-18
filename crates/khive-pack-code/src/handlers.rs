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

use crate::db_target::{resolve_target_db, verify_opened_target};
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
        // `main_backend_db_path` resolves the real configured database file
        // in BOTH the single-backend (`KhiveRuntime::new`) and multi-backend
        // (`from_backend`, where `config().db_path` is always `None`) boot
        // paths, so a caller cannot dodge the fence by naming the
        // multi-backend deployment's actual main backend file (#1087 item 4).
        let runtime_db_path = self.runtime.main_backend_db_path();
        let db_path = resolve_target_db(db_param, &path, runtime_db_path.as_deref())
            .map_err(RuntimeError::InvalidInput)?;

        // Defaults to L1+L1.5 only (`["l1", "l1.5"]`) so a caller that never
        // passes `tiers` sees byte-identical behavior to before L2 existed.
        // Each named tier runs ONLY its own entities/edges (ADR-085 Amendment
        // 2 B3: tiers are independently switchable) — `tiers=["l1"]` runs
        // manifest parsing alone, `["l1.5"]` runs the import scan alone,
        // `["l2"]` runs the symbol tier alone, and any combination composes.
        const VALID_TIERS: &[&str] = &["l1", "l1.5", "l2"];
        let (enable_l1, enable_l1_5, enable_l2) = match params.get("tiers") {
            None | Some(Value::Null) => (true, true, false),
            Some(v) => {
                let arr = v.as_array().ok_or_else(|| {
                    RuntimeError::InvalidInput("tiers must be an array of strings".into())
                })?;
                let mut selected: BTreeSet<&str> = BTreeSet::new();
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
                    selected.insert(s);
                }
                (
                    selected.contains("l1"),
                    selected.contains("l1.5"),
                    selected.contains("l2"),
                )
            }
        };

        let config = RuntimeConfig {
            db_path: Some(db_path.clone()),
            packs: vec!["kg".to_string(), "code".to_string()],
            ..RuntimeConfig::no_embeddings()
        };
        // `new_guarded` runs `verify_opened_target` against the just-opened
        // backend's file BEFORE migrations (or any other write) touch it --
        // `new` runs migrations immediately on open, which would otherwise
        // let a swapped-in production database get migrated before this
        // fence ever sees it (#1087 item 2).
        let target_rt = KhiveRuntime::new_guarded(config, |opened_path| {
            verify_opened_target(opened_path, runtime_db_path.as_deref())
        })
        .map_err(|e| RuntimeError::InvalidInput(format!("opening target db {db_path:?}: {e}")))?;
        // Re-verify immediately before the write-heavy ingest phase begins --
        // narrows the window between the open-time check above and the
        // first actual write, the most exploitable point in the TOCTOU: an
        // attacker who swaps the file at `db_path` after the open-time check
        // but before ingest starts is still caught here, closed. This is a
        // narrowing, not a full close -- a true race-resistant fence requires
        // a VFS-level check (ADR-085 Amendment 4, in review).
        verify_opened_target(&db_path, runtime_db_path.as_deref())
            .map_err(RuntimeError::InvalidInput)?;
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
                enable_l1,
                enable_l1_5,
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
