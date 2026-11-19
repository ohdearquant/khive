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

    // Session notes live in the main shared-graph backend (ADR-083 §4:
    // store/list write and read through `core()`), so resolution must go
    // through `core()` too — on a secondary-backend runtime, resolving
    // against `runtime` itself looks in the pack backend and misses every
    // stored session.
    let core = runtime.core();
    let uuid = resolve_session_uuid(&core, token, &p.id, VERB).await?;
    let note = fetch_session_note(&core, token, uuid, VERB).await?;

    let result = ResumeResult {
        ok: true,
        session: to_session_record(&note),
    };
    Ok(serde_json::to_value(result).expect("ResumeResult serializes"))
}

#[cfg(test)]
#[path = "resume_tests.rs"]
mod tests;
