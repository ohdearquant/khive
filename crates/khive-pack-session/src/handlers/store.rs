//! `session.store` - store a session record as a `kind=session` note.

use serde_json::{json, Value};

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};

use super::{deser, require_non_empty_if_present, to_session_record, StoreParams, StoreResult};
use crate::vocab::SESSION_KIND;

const VERB: &str = "session.store";

pub(crate) async fn handle_store(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: StoreParams = deser(params)?;

    if p.content.trim().is_empty() {
        return Err(RuntimeError::InvalidInput(format!(
            "{VERB}: content must not be empty"
        )));
    }
    require_non_empty_if_present(&p.title, "title", VERB)?;
    require_non_empty_if_present(&p.provider, "provider", VERB)?;
    require_non_empty_if_present(&p.provider_session_id, "provider_session_id", VERB)?;
    if let Some(tags) = &p.tags {
        for tag in tags {
            if tag.trim().is_empty() {
                return Err(RuntimeError::InvalidInput(format!(
                    "{VERB}: tags entries must be non-empty strings"
                )));
            }
        }
    }

    let mut properties = serde_json::Map::new();
    if let Some(provider) = &p.provider {
        properties.insert("provider".into(), json!(provider));
    }
    if let Some(provider_session_id) = &p.provider_session_id {
        properties.insert("provider_session_id".into(), json!(provider_session_id));
    }
    if let Some(tags) = &p.tags {
        properties.insert("tags".into(), json!(tags));
    }

    let core = runtime.core();
    let note = core
        .create_note(
            token,
            SESSION_KIND,
            p.title.as_deref(),
            &p.content,
            None,
            Some(Value::Object(properties)),
            vec![],
        )
        .await?;

    let result = StoreResult {
        ok: true,
        session: to_session_record(&note),
    };
    Ok(serde_json::to_value(result).expect("StoreResult serializes"))
}
