use khive_runtime::{NamespaceToken, RequestIdentity, RuntimeError, VerbRegistry};
use khive_storage::types::{SqlRow, SqlValue};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::GitPack;

pub(crate) mod snapshot;

const MAX_VALUE_BYTES: i64 = 256 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CursorParams {
    project: Uuid,
    source_kind: SourceKind,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SourceKind {
    Commits,
    Issues,
    PullRequests,
}

impl SourceKind {
    fn names(self) -> (&'static str, &'static str) {
        match self {
            Self::Commits => ("commits", "commits"),
            Self::Issues => ("issues", "issues"),
            Self::PullRequests => ("pull_requests", "prs"),
        }
    }
}

impl GitPack {
    pub(crate) async fn handle_ingest_cursor(
        &self,
        token: &NamespaceToken,
        registry: &VerbRegistry,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let params = crate::params::parse("git.ingest_cursor", params)?;
        let params: CursorParams = serde_json::from_value(params).map_err(|_| {
            RuntimeError::InvalidInput(
                "git.ingest_cursor requires a full project UUID and source_kind commits, issues, or pull_requests".into(),
            )
        })?;
        let project_id = params.project.to_string();
        // Keep project access at the canonical Gate seam, carrying the caller's
        // identity. A checkpoint's namespace describes continuation, not read isolation.
        let mut identity = RequestIdentity::from_token(token);
        // Implicit requests write to local even when Gate used another default.
        // Use the originating Gate namespace so this read cannot weaken that check.
        identity.namespace = token.gate_namespace().as_str().to_string();
        let project = registry
            .dispatch_with_identity(
                "get",
                json!({
                    "id":project_id,"namespace":identity.namespace,
                }),
                Some(identity),
            )
            .await?;
        if project["kind"] != "project" || !project["deleted_at"].is_null() {
            return Err(RuntimeError::InvalidInput(
                "project must identify a live project entity".into(),
            ));
        }
        let (source_kind, kind) = params.source_kind.names();
        let checkpoint_kind = format!("{kind}_checkpoint");
        // One SELECT snapshot cannot tear the atomically written pair. Limit
        // materialized value bytes even when a damaged row exceeds writer bounds.
        let rows = self
            .runtime()
            .sql()
            .reader()
            .await?
            .query_all(snapshot::statement(
                &project_id,
                kind,
                &checkpoint_kind,
                MAX_VALUE_BYTES,
                "git.ingest_cursor.snapshot",
            ))
            .await?;
        let mut cursor = Value::Null;
        let mut checkpoint = Value::Null;
        for row in rows {
            match row.get("kind") {
                Some(SqlValue::Text(name)) if name.as_str() == kind => cursor = render_row(&row)?,
                Some(SqlValue::Text(name)) if name.as_str() == checkpoint_kind.as_str() => {
                    checkpoint = render_row(&row)?
                }
                _ => return Err(invalid_row()),
            }
        }
        Ok(json!({
            "project_id": project_id,
            "source_kind": source_kind,
            "cursor": cursor,
            "checkpoint": checkpoint,
        }))
    }
}

fn invalid_row() -> RuntimeError {
    RuntimeError::InvalidInput(
        "stored ingest cursor row has invalid column types or text encoding".into(),
    )
}

fn render_row(row: &SqlRow) -> Result<Value, RuntimeError> {
    if !matches!(row.get("value_type"), Some(SqlValue::Text(t)) if t == "text" || t == "null") {
        return Err(invalid_row());
    }
    let value_bytes = snapshot::value_bytes(row).map_err(|_| invalid_row())?;
    let truncated = value_bytes.is_some_and(|n| n > MAX_VALUE_BYTES);
    let value = match snapshot::value(row).map_err(|_| invalid_row())? {
        Some(bytes) => Value::String(String::from_utf8(bytes.to_vec()).map_err(|_| invalid_row())?),
        None => Value::Null,
    };
    let updated_at = match row.get("updated_at") {
        Some(SqlValue::Integer(n)) => *n,
        _ => return Err(invalid_row()),
    };
    Ok(
        json!({"value": value, "updated_at": updated_at, "value_bytes": value_bytes, "truncated": truncated}),
    )
}
