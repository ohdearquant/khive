//! `session.store` - store a session record as a `kind=session` note.

use serde_json::{json, Value};

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};

use super::{deser, to_session_record, StoreParams, StoreResult};
use crate::vocab::SESSION_KIND;

const VERB: &str = "session.store";

pub(crate) async fn handle_store(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: StoreParams = deser(params)?;
    p.validate(VERB)?;

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

    // Session content is caller-supplied and often longer than the embedder's
    // input budget. The report variant keeps the committed record a success and
    // discloses the truncation, as comm, kg and knowledge writes do, instead of
    // failing a write that has already been stored.
    let core = runtime.core();
    let (note, embedding_truncation) = core
        .create_note_with_embedding_content_and_report(
            token,
            SESSION_KIND,
            p.title.as_deref(),
            &p.content,
            None,
            None,
            Some(Value::Object(properties)),
            vec![],
        )
        .await?;

    let result = StoreResult {
        ok: true,
        session: to_session_record(&note),
    };
    let mut response = serde_json::to_value(result).expect("StoreResult serializes");
    if embedding_truncation.any_truncated() {
        response["warnings"] = json!([khive_runtime::retrieval::EMBEDDING_INPUT_TRUNCATED_WARNING]);
    }
    Ok(response)
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
