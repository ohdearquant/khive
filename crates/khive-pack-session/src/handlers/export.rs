//! `session.export` - serialize one stored session as json or markdown.

use serde_json::Value;

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};

use super::{
    deser, fetch_session_note, resolve_session_uuid, to_session_record, ExportJsonResult,
    ExportMarkdownResult, ExportParams,
};
use crate::vocab::VALID_EXPORT_FORMATS;

const VERB: &str = "session.export";

pub(crate) async fn handle_export(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    params: Value,
) -> Result<Value, RuntimeError> {
    let p: ExportParams = deser(params)?;
    let format = p.format.as_deref().unwrap_or("json");
    if !VALID_EXPORT_FORMATS.contains(&format) {
        return Err(RuntimeError::InvalidInput(format!(
            "{VERB}: format must be one of {VALID_EXPORT_FORMATS:?}; got {format:?}"
        )));
    }

    let uuid = resolve_session_uuid(runtime, token, &p.id, VERB).await?;
    let note = fetch_session_note(runtime, token, uuid, VERB).await?;
    let record = to_session_record(&note);

    match format {
        "markdown" => {
            let title = record
                .title
                .clone()
                .unwrap_or_else(|| format!("Session {}", &record.id[..8]));
            let tags = if record.tags.is_empty() {
                String::new()
            } else {
                record.tags.join(", ")
            };
            let content = format!(
                "# {title}\n\n\
                 - id: {id}\n\
                 - provider: {provider}\n\
                 - provider_session_id: {provider_session_id}\n\
                 - created_at: {created_at}\n\
                 - updated_at: {updated_at}\n\
                 - tags: {tags}\n\n\
                 ## Content\n\n\
                 {body}",
                id = record.id,
                provider = record.provider.as_deref().unwrap_or("null"),
                provider_session_id = record.provider_session_id.as_deref().unwrap_or("null"),
                created_at = record.created_at,
                updated_at = record.updated_at,
                body = record.content,
            );
            let result = ExportMarkdownResult {
                ok: true,
                format: "markdown",
                content,
            };
            Ok(serde_json::to_value(result).expect("ExportMarkdownResult serializes"))
        }
        _ => {
            let result = ExportJsonResult {
                ok: true,
                format: "json",
                session: record,
            };
            Ok(serde_json::to_value(result).expect("ExportJsonResult serializes"))
        }
    }
}

#[cfg(test)]
mod tests {
    use khive_runtime::{KhiveRuntime, Namespace};
    use serde_json::json;
    use uuid::Uuid;

    use super::handle_export;

    #[tokio::test]
    async fn invalid_format_rejected_before_resolution() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let token = rt.authorize(Namespace::local()).expect("authorize local");

        // `id` is also malformed; a resolution-first implementation would
        // fail with a different error. Getting the format error here proves
        // format validation runs before id resolution.
        let err = handle_export(&rt, &token, json!({ "id": "not-an-id!", "format": "xml" }))
            .await
            .unwrap_err();

        let khive_runtime::RuntimeError::InvalidInput(msg) = err else {
            panic!("expected InvalidInput, got {err:?}");
        };
        assert!(
            msg.contains("format must be one of") && msg.contains("xml"),
            "format must be validated before id resolution; got: {msg}",
        );
    }

    #[tokio::test]
    async fn json_format_accepted_proceeds_to_resolution() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let token = rt.authorize(Namespace::local()).expect("authorize local");
        let missing = Uuid::new_v4().to_string();

        let err = handle_export(&rt, &token, json!({ "id": missing, "format": "json" }))
            .await
            .unwrap_err();

        let khive_runtime::RuntimeError::NotFound(msg) = err else {
            panic!("expected NotFound (format accepted, id resolution reached), got {err:?}");
        };
        assert!(
            msg.contains("session not found"),
            "format=json must pass validation and reach id resolution; got: {msg}",
        );
    }

    #[tokio::test]
    async fn markdown_format_accepted_proceeds_to_resolution() {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let token = rt.authorize(Namespace::local()).expect("authorize local");
        let missing = Uuid::new_v4().to_string();

        let err = handle_export(&rt, &token, json!({ "id": missing, "format": "markdown" }))
            .await
            .unwrap_err();

        let khive_runtime::RuntimeError::NotFound(msg) = err else {
            panic!("expected NotFound (format accepted, id resolution reached), got {err:?}");
        };
        assert!(
            msg.contains("session not found"),
            "format=markdown must pass validation and reach id resolution; got: {msg}",
        );
    }
}
