//! `session.resume` - fetch one session's full content by UUID or short prefix.

use serde_json::Value;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};

use super::{
    deser, fetch_session_note, resolve_session_uuid, to_session_record, ResumeParams, ResumeResult,
};

const VERB: &str = "session.resume";

pub(crate) async fn handle_resume(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: ResumeParams = deser(params)?;

    let uuid = resolve_session_uuid(runtime, token, &p.id, VERB).await?;
    let note = fetch_session_note(runtime, token, uuid, VERB).await?;

    let result = ResumeResult {
        ok: true,
        session: to_session_record(&note),
    };
    Ok(serde_json::to_value(result).expect("ResumeResult serializes"))
}
