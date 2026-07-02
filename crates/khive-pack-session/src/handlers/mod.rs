//! Verb handlers for the session pack, one file per verb, plus shared
//! response shapes and the id-resolution helpers common to resume/export.

pub(crate) mod export;
pub(crate) mod list;
pub(crate) mod resume;
pub(crate) mod store;

use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::{micros_to_iso, KhiveRuntime, NamespaceToken, Resolved, RuntimeError};
use khive_storage::note::Note;

use crate::vocab::SESSION_KIND;

pub(crate) fn deser<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, RuntimeError> {
    serde_json::from_value(params)
        .map_err(|e| RuntimeError::InvalidInput(format!("bad params: {e}")))
}

#[derive(Debug, Serialize)]
pub(crate) struct SessionRecord {
    pub id: String,
    pub kind: &'static str,
    pub title: Option<String>,
    pub provider: Option<String>,
    pub provider_session_id: Option<String>,
    pub tags: Vec<String>,
    pub content: String,
    pub properties: Value,
    pub created_at: String,
    pub updated_at: String,
    pub namespace: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct SessionSummary {
    pub id: String,
    pub kind: &'static str,
    pub title: Option<String>,
    pub provider: Option<String>,
    pub provider_session_id: Option<String>,
    pub tags: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
    pub namespace: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct StoreResult {
    pub ok: bool,
    pub session: SessionRecord,
}

#[derive(Debug, Serialize)]
pub(crate) struct ListResult {
    pub ok: bool,
    pub sessions: Vec<SessionSummary>,
    pub count: usize,
    pub total: Option<u64>,
    pub limit: u32,
    pub offset: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct ResumeResult {
    pub ok: bool,
    pub session: SessionRecord,
}

#[derive(Debug, Serialize)]
pub(crate) struct ExportJsonResult {
    pub ok: bool,
    pub format: &'static str,
    pub session: SessionRecord,
}

#[derive(Debug, Serialize)]
pub(crate) struct ExportMarkdownResult {
    pub ok: bool,
    pub format: &'static str,
    pub content: String,
}

fn string_property(properties: &Value, key: &str) -> Option<String> {
    properties
        .get(key)
        .and_then(|v| v.as_str())
        .map(String::from)
}

fn tags_property(properties: &Value) -> Vec<String> {
    match properties.get("tags").and_then(|v| v.as_array()) {
        Some(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        None => Vec::new(),
    }
}

pub(crate) fn to_session_record(note: &Note) -> SessionRecord {
    let properties = note.properties.clone().unwrap_or_else(|| json!({}));
    SessionRecord {
        id: note.id.as_hyphenated().to_string(),
        kind: SESSION_KIND,
        title: note.name.clone(),
        provider: string_property(&properties, "provider"),
        provider_session_id: string_property(&properties, "provider_session_id"),
        tags: tags_property(&properties),
        content: note.content.clone(),
        properties,
        created_at: micros_to_iso(note.created_at),
        updated_at: micros_to_iso(note.updated_at),
        namespace: note.namespace.clone(),
    }
}

pub(crate) fn to_session_summary(note: &Note) -> SessionSummary {
    let properties = note.properties.clone().unwrap_or_else(|| json!({}));
    SessionSummary {
        id: note.id.as_hyphenated().to_string(),
        kind: SESSION_KIND,
        title: note.name.clone(),
        provider: string_property(&properties, "provider"),
        provider_session_id: string_property(&properties, "provider_session_id"),
        tags: tags_property(&properties),
        created_at: micros_to_iso(note.created_at),
        updated_at: micros_to_iso(note.updated_at),
        namespace: note.namespace.clone(),
    }
}

/// Resolve a caller-supplied id to a UUID: accepts a full UUID or an 8+ hex
/// short prefix, matching the runtime's `resolve_prefix` convention.
pub(crate) async fn resolve_session_uuid(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    raw: &str,
    verb: &str,
) -> Result<Uuid, RuntimeError> {
    if let Ok(uuid) = Uuid::from_str(raw) {
        return Ok(uuid);
    }

    if raw.len() >= 8 && raw.chars().all(|c| c.is_ascii_hexdigit()) {
        return match runtime.resolve_prefix(token, raw).await? {
            Some(uuid) => Ok(uuid),
            None => Err(RuntimeError::InvalidInput(format!(
                "{verb}: id prefix {raw:?} matched no records; valid values: full UUID or 8+ hex prefix"
            ))),
        };
    }

    Err(RuntimeError::InvalidInput(format!(
        "{verb}: id must be a full UUID or 8+ hex prefix; valid values: full UUID or 8+ hex prefix; got {raw:?}"
    )))
}

/// Fetch a session note by resolved UUID, rejecting non-session note kinds
/// and non-note substrates.
pub(crate) async fn fetch_session_note(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
    verb: &str,
) -> Result<Note, RuntimeError> {
    match runtime.resolve_primary(token, id).await? {
        Some(Resolved::Note(note)) if note.kind == SESSION_KIND => Ok(note),
        Some(Resolved::Note(note)) => Err(RuntimeError::InvalidInput(format!(
            "{verb}: expected note kind \"session\"; valid note kind: session; got {:?}",
            note.kind
        ))),
        Some(_) => Err(RuntimeError::InvalidInput(format!(
            "{verb}: id must resolve to a session note; valid substrate: note kind session"
        ))),
        None => Err(RuntimeError::NotFound(format!("session not found: {id}"))),
    }
}

/// Validate a caller-supplied optional string field: `None` passes through,
/// `Some` must be non-empty after trimming.
pub(crate) fn require_non_empty_if_present(
    value: &Option<String>,
    field: &str,
    verb: &str,
) -> Result<(), RuntimeError> {
    if let Some(s) = value {
        if s.trim().is_empty() {
            return Err(RuntimeError::InvalidInput(format!(
                "{verb}: {field} must be a non-empty string when provided"
            )));
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoreParams {
    pub content: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub provider_session_id: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListParams {
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub offset: Option<u32>,
    #[serde(default)]
    pub provider: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResumeParams {
    pub id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExportParams {
    pub id: String,
    #[serde(default)]
    pub format: Option<String>,
}
