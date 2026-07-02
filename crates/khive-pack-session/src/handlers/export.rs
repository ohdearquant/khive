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
