use std::path::PathBuf;

use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig, RuntimeError};
use serde::Deserialize;
use serde_json::Value;

use crate::db_target::resolve_target_db;
use crate::extract::extract;
use crate::manifest::read_manifest;
use crate::persistence::persist;
use crate::WebPack;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IngestParams {
    source: String,
    db: Option<String>,
    #[serde(default = "include_views_default")]
    include_views: bool,
}

fn include_views_default() -> bool {
    true
}

impl WebPack {
    pub(crate) async fn handle_ingest(&self, params: Value) -> Result<Value, RuntimeError> {
        let params: IngestParams = serde_json::from_value(params).map_err(|error| {
            RuntimeError::InvalidInput(format!("invalid web.ingest arguments: {error}"))
        })?;
        let source = PathBuf::from(&params.source);
        if params.source.trim().is_empty() || !source.is_dir() {
            return Err(RuntimeError::InvalidInput(
                "web.ingest source must be an existing local directory".to_string(),
            ));
        }
        let source = source.canonicalize().map_err(|error| {
            RuntimeError::InvalidInput(format!("cannot resolve source: {error}"))
        })?;
        let db_path = resolve_target_db(
            params.db.as_deref(),
            &source,
            self.runtime.config().db_path.as_deref(),
        )
        .map_err(RuntimeError::InvalidInput)?;
        let manifest = read_manifest(&source).map_err(RuntimeError::InvalidInput)?;
        let extracted =
            extract(&source, manifest, params.include_views).map_err(RuntimeError::InvalidInput)?;
        let target = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(db_path.clone()),
            packs: vec!["kg".to_string(), "web".to_string()],
            ..RuntimeConfig::no_embeddings()
        })
        .map_err(|error| {
            RuntimeError::InvalidInput(format!("cannot open web map database: {error}"))
        })?;
        let token = target.authorize(Namespace::local())?;
        persist(&target, &token, extracted.entities, extracted.edges).await?;
        let mut report = serde_json::to_value(extracted.report).map_err(|error| {
            RuntimeError::Internal(format!("cannot serialize web.ingest report: {error}"))
        })?;
        report["db_path"] = Value::String(db_path.display().to_string());
        Ok(report)
    }
}
