//! CRUD handlers: upsert_atoms, upsert_domains, get, list, delete_atoms, stats.

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};
use uuid::Uuid;

use khive_runtime::secret_gate::{mask_for_redaction_surface, RedactionSurface};
use khive_runtime::{
    micros_to_iso, uuid_prefix_bounds, KhiveRuntime, NamespaceToken, RuntimeError,
};
use khive_storage::types::{SqlStatement, SqlValue};

use super::schema::{
    AtomWrite, DeleteAtomsParams, GetParams, ListParams, StatsParams, UpsertAtomsParams,
    UpsertDomainsParams,
};
use super::search::HYDRATION_ID_CHUNK;
use super::sections::{section_from_row, section_to_json};
use super::util::{
    atom_from_row, atom_to_json, compute_embedding_coverage, deser, domain_from_row,
    domain_to_json, has_domain_mirror_tag, new_id, now_us, row_i64, row_str, sql_err,
    status_sql_clause, status_values, tags_to_json, validate_atom_content,
};
use super::KnowledgeHandlers;

#[path = "cursor_query.rs"]
mod cursor_query;
use cursor_query::cursor_query;

const ATOM_LIST_FIELDS: &[&str] = &[
    "id",
    "namespace",
    "slug",
    "name",
    "content",
    "tags",
    "properties",
    "status",
    "source_uri",
    "source_type",
    "finalized",
    "kind",
    "created_at",
    "updated_at",
];

const DOMAIN_LIST_FIELDS: &[&str] = &[
    "id",
    "namespace",
    "slug",
    "name",
    "description",
    "tags",
    "members",
    "kind",
    "created_at",
    "updated_at",
];

const LIST_CURSOR_ORDER: &str = "created_at_asc_id_asc";
const LIST_OFFSET_ORDER: &str = "created_at_desc_id_desc";

fn invalidate_section_vectors_for_atom_rename(
    id: &str,
    namespace: &str,
    new_name: &str,
    now: i64,
) -> SqlStatement {
    SqlStatement {
        sql: khive_runtime::sql!("knowledge_sections_invalidate_atom_rename").into(),
        params: vec![
            SqlValue::Integer(now),
            SqlValue::Text(id.to_string()),
            SqlValue::Text(namespace.to_string()),
            SqlValue::Text(new_name.to_string()),
        ],
        label: None,
    }
}

#[derive(Clone, Copy)]
enum KnowledgeListKind {
    Atom,
    Domain,
}

impl KnowledgeListKind {
    fn name(self) -> &'static str {
        match self {
            Self::Atom => "atom",
            Self::Domain => "domain",
        }
    }

    fn projection_fields(self) -> &'static [&'static str] {
        match self {
            Self::Atom => ATOM_LIST_FIELDS,
            Self::Domain => DOMAIN_LIST_FIELDS,
        }
    }
}

fn parse_list_after(raw: &str) -> Result<Option<Uuid>, RuntimeError> {
    if raw.is_empty() {
        return Ok(None);
    }
    Uuid::parse_str(raw).map(Some).map_err(|error| {
        RuntimeError::InvalidInput(
            mask_for_redaction_surface(
                RedactionSurface::GateProbe,
                &format!(
                    "knowledge.list: `after` must be a full UUID returned as `next_after`; \
                     prefixes are unsafe for keyset pagination; got {raw:?}: {error}"
                ),
            )
            .into_owned(),
        )
    })
}

fn validate_list_fields(
    kind: KnowledgeListKind,
    fields: Option<&[String]>,
) -> Result<(), RuntimeError> {
    let Some(fields) = fields else {
        return Ok(());
    };
    if fields.is_empty() {
        return Err(RuntimeError::InvalidInput(
            "knowledge.list: `fields` must be non-empty".into(),
        ));
    }
    if let Some(unknown) = fields
        .iter()
        .find(|field| !kind.projection_fields().contains(&field.as_str()))
    {
        return Err(RuntimeError::InvalidInput(
            mask_for_redaction_surface(
                RedactionSurface::GateProbe,
                &format!(
                    "knowledge.list: unknown {} projection field {unknown:?}; expected one of: {}",
                    kind.name(),
                    kind.projection_fields().join(", ")
                ),
            )
            .into_owned(),
        ));
    }
    Ok(())
}

/// Projection reads include the sort key internally, but the renderer below
/// emits only caller-requested fields. This is the byte-saving boundary: an
/// `id`/`slug` walk never selects atom `content` from SQLite.
fn list_select_columns(fields: Option<&[String]>) -> String {
    let Some(fields) = fields else {
        return "*".into();
    };

    let mut columns: Vec<&str> = Vec::with_capacity(fields.len() + 2);
    for field in fields {
        if field != "kind" && !columns.contains(&field.as_str()) {
            columns.push(field);
        }
    }
    for pagination_column in ["id", "created_at"] {
        if !columns.contains(&pagination_column) {
            columns.push(pagination_column);
        }
    }
    columns.join(", ")
}

fn json_text_column(row: &khive_storage::types::SqlRow, column: &str) -> Value {
    row_str(row, column).map_or(Value::Null, Value::String)
}

fn json_array_column(row: &khive_storage::types::SqlRow, column: &str) -> Value {
    row_str(row, column)
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .unwrap_or_else(|| Value::Array(vec![]))
}

fn json_object_column(row: &khive_storage::types::SqlRow, column: &str) -> Value {
    row_str(row, column)
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .unwrap_or(Value::Null)
}

fn json_timestamp_column(row: &khive_storage::types::SqlRow, column: &str) -> Value {
    row_i64(row, column)
        .map(micros_to_iso)
        .map_or(Value::Null, Value::String)
}

fn project_list_row(
    row: &khive_storage::types::SqlRow,
    kind: KnowledgeListKind,
    fields: Option<&[String]>,
) -> Option<Value> {
    let Some(fields) = fields else {
        return match kind {
            KnowledgeListKind::Atom => atom_from_row(row).map(|atom| atom_to_json(&atom)),
            KnowledgeListKind::Domain => domain_from_row(row).map(|domain| domain_to_json(&domain)),
        };
    };

    let mut projected = serde_json::Map::new();
    for field in fields {
        let value = match field.as_str() {
            "id" | "namespace" | "slug" | "name" | "content" | "description" | "status"
            | "source_uri" | "source_type" => json_text_column(row, field),
            "tags" | "members" => json_array_column(row, field),
            "properties" => json_object_column(row, field),
            "finalized" => Value::Bool(row_i64(row, field).is_some_and(|value| value == 1)),
            "kind" => Value::String(kind.name().into()),
            "created_at" | "updated_at" => json_timestamp_column(row, field),
            _ => unreachable!("projection field was validated"),
        };
        projected.insert(field.clone(), value);
    }
    Some(Value::Object(projected))
}

fn knowledge_get_prefix_statement(prefix: &str) -> Option<SqlStatement> {
    let (lower, upper) = uuid_prefix_bounds(prefix)?;
    Some(SqlStatement {
        // A domain's FTS mirror atom has the same UUID. UNION (rather than
        // UNION ALL) deduplicates that legitimate cross-table duplicate so
        // one domain is not reported as an ambiguous prefix.
        sql: khive_runtime::sql!("knowledge_get_prefix").into(),
        params: vec![SqlValue::Text(lower), SqlValue::Text(upper)],
        label: Some("knowledge.get.resolve_prefix".into()),
    })
}

/// Match a chunk of ids-or-slugs against the live rows of one table, as one
/// statement per key column. A single `id IN (..) OR slug IN (..)` predicate
/// makes the planner scan the namespace, so each column gets its own
/// statement: the primary key drives the id probe (`+namespace` keeps the
/// namespace term out of index selection, as in `hydrate_atoms_statement`)
/// and `(namespace, slug)` drives the slug probe. `+deleted_at` keeps the
/// liveness term out of index selection too, so the count index on
/// `(namespace, deleted_at, ..)` cannot win the slug probe and filter the
/// whole live namespace. `head` is the statement up
/// to `WHERE`; `lead` holds the binds before the keys, and its first entry is
/// the namespace (`?1`).
fn key_match_statements(head: &str, lead: Vec<SqlValue>, keys: &[String]) -> [SqlStatement; 2] {
    let first_key = lead.len() + 1;
    let placeholders = (first_key..first_key + keys.len())
        .map(|n| format!("?{n}"))
        .collect::<Vec<_>>()
        .join(",");
    let mut params = lead;
    params.extend(keys.iter().cloned().map(SqlValue::Text));
    [("id", "+namespace"), ("slug", "namespace")].map(|(column, namespace)| SqlStatement {
        sql: format!(
            "{head} WHERE {column} IN ({placeholders}) AND {namespace} = ?1 AND +deleted_at IS NULL"
        ),
        params: params.clone(),
        label: None,
    })
}

// Reader preflight protects known domains before writer admission; the same
// queries run again under the applying writer to guard concurrent changes.
async fn preflight_atom_delete<R: khive_storage::SqlReader + ?Sized>(
    reader: &mut R,
    namespace: &str,
    keys: &[String],
) -> khive_storage::StorageResult<Result<HashSet<String>, String>> {
    let mut atom_ids = HashSet::new();
    for chunk in keys.chunks(HYDRATION_ID_CHUNK) {
        let mut domains = HashSet::new();
        for statement in key_match_statements(
            "SELECT id, slug FROM knowledge_domains",
            vec![SqlValue::Text(namespace.to_string())],
            chunk,
        ) {
            for row in reader.query_all(statement).await? {
                domains.extend(row_str(&row, "id"));
                domains.extend(row_str(&row, "slug"));
            }
        }
        let mut mirrors = HashSet::new();
        for statement in key_match_statements(
            "SELECT id, slug, tags FROM knowledge_atoms",
            vec![SqlValue::Text(namespace.to_string())],
            chunk,
        ) {
            for row in reader.query_all(statement).await? {
                let id = row_str(&row, "id").ok_or_else(|| {
                    khive_storage::StorageError::Internal("atom has no id".into())
                })?;
                if has_domain_mirror_tag(&row_str(&row, "tags").unwrap_or_default()) {
                    mirrors.insert(id.clone());
                    mirrors.extend(row_str(&row, "slug"));
                }
                atom_ids.insert(id);
            }
        }
        for key in chunk {
            if domains.contains(key) {
                return Ok(Err(format!("knowledge.delete_atoms cannot delete domain {key:?}; use the generic delete verb by domain UUID")));
            }
            if mirrors.contains(key) {
                return Ok(Err(format!("knowledge.delete_atoms cannot delete domain mirror {key:?}; use the generic delete verb by domain UUID")));
            }
        }
    }
    Ok(Ok(atom_ids))
}

impl KnowledgeHandlers {
    /// Feedback accepts IDs from knowledge retrieval only; no KG table lookup
    /// and no slug fallback. Domain mirror atoms share the domain UUID.
    pub(crate) async fn resolve_feedback_target(
        runtime: &KhiveRuntime,
        input: &str,
    ) -> Result<(Uuid, &'static str), RuntimeError> {
        let sql = runtime.sql();
        let mut reader = sql
            .reader()
            .await
            .map_err(|e| sql_err("feedback target reader", e))?;
        let id = if let Ok(id) = input.parse::<Uuid>() {
            id
        } else if let Some(statement) = knowledge_get_prefix_statement(input) {
            let rows = reader
                .query_all(statement)
                .await
                .map_err(|e| sql_err("feedback target prefix", e))?;
            let ids: Vec<Uuid> = rows
                .iter()
                .filter_map(|r| row_str(r, "id"))
                .map(|id| {
                    id.parse()
                        .map_err(|_| RuntimeError::Internal("invalid stored knowledge UUID".into()))
                })
                .collect::<Result<_, _>>()?;
            match ids.as_slice() {
                [] => {
                    return Err(
                        khive_types::KhiveError::not_found("knowledge target", input).into(),
                    )
                }
                [id] => *id,
                _ => {
                    return Err(RuntimeError::AmbiguousPrefix {
                        prefix: input.into(),
                        matches: ids,
                    })
                }
            }
        } else {
            return Err(khive_types::KhiveError::invalid_input(
                "knowledge feedback target must be a full UUID or 8+ hex prefix",
            )
            .into());
        };
        // UUID reads, like knowledge.get, are namespace-agnostic (ADR-007).
        for (table, kind) in [("knowledge_domains", "domain"), ("knowledge_atoms", "atom")] {
            let row = reader
                .query_row(SqlStatement {
                    sql: format!(
                        "SELECT id FROM {table} WHERE id = ?1 AND deleted_at IS NULL LIMIT 1"
                    ),
                    params: vec![SqlValue::Text(id.to_string())],
                    label: Some("knowledge.feedback.target".into()),
                })
                .await
                .map_err(|e| sql_err("feedback target", e))?;
            if row.is_some() {
                return Ok((id, kind));
            }
        }
        Err(khive_types::KhiveError::not_found("knowledge target", input).into())
    }

    pub(crate) async fn upsert_atoms(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        Self::upsert_atoms_with_content_policy(runtime, token, params, false).await
    }

    /// Import has already validated the complete source document and must retain its
    /// boundary whitespace. The public upsert verb keeps its established trim behavior.
    pub(super) async fn upsert_import_atoms(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        Self::upsert_atoms_with_content_policy(runtime, token, params, true).await
    }

    async fn upsert_atoms_with_content_policy(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
        preserve_content_whitespace: bool,
    ) -> Result<Value, RuntimeError> {
        Self::upsert_atoms_with_sql(
            runtime,
            token,
            params,
            preserve_content_whitespace,
            runtime.sql(),
        )
        .await
    }

    async fn upsert_atoms_with_sql(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
        preserve_content_whitespace: bool,
        sql: std::sync::Arc<dyn khive_storage::SqlAccess>,
    ) -> Result<Value, RuntimeError> {
        use khive_runtime::error::{BatchAdmissionReport, BatchRefusal};
        let prewrite = |error: RuntimeError| {
            if preserve_content_whitespace {
                error
            } else {
                error.before_requested_write()
            }
        };
        let mut failures = Vec::new();
        let mut refusals = Vec::new();
        let (atoms, dry_run, chunk_size): (Vec<Option<AtomWrite>>, bool, Option<usize>) =
            if preserve_content_whitespace {
                let p: UpsertAtomsParams = deser(params)?;
                failures.resize_with(p.atoms.len(), || None);
                refusals.resize_with(p.atoms.len(), || None);
                (
                    p.atoms.into_iter().map(Some).collect(),
                    p.dry_run,
                    p.chunk_size,
                )
            } else {
                let p: super::schema::UpsertAtomsAdmissionParams =
                    deser(params).map_err(prewrite)?;
                let atoms = p
                    .atoms
                    .into_iter()
                    .enumerate()
                    .map(|(index, value)| match deser::<AtomWrite>(value) {
                        Ok(atom) => {
                            failures.push(None);
                            refusals.push(None);
                            Some(atom)
                        }
                        Err(error) => {
                            refusals.push(Some(BatchRefusal {
                                index,
                                field: "item",
                                reason: "invalid_input",
                            }));
                            failures.push(Some(error));
                            None
                        }
                    })
                    .collect();
                (atoms, p.dry_run, p.chunk_size)
            };
        if chunk_size.is_some() {
            tracing::warn!(?chunk_size, "upsert_atoms: chunk_size is accepted but not yet implemented; server-side chunking is not performed");
        }
        if atoms.is_empty() {
            return Err(prewrite(RuntimeError::InvalidInput(
                "atoms list must not be empty".into(),
            )));
        }
        if atoms.len() > 5000 {
            return Err(prewrite(RuntimeError::InvalidInput(
                "max 5000 atoms per request".into(),
            )));
        }
        if dry_run {
            if let Some(error) = failures.into_iter().flatten().next() {
                return Err(prewrite(error));
            }
            let atoms: Vec<_> = atoms.into_iter().flatten().collect();
            return super::atom_validation::dry_run(
                runtime,
                token,
                &atoms,
                preserve_content_whitespace,
            )
            .await;
        }
        for (index, atom) in atoms.iter().enumerate() {
            let Some(atom) = atom else {
                continue;
            };
            if let Err(failure) = super::refusal::validate_submission_located(
                atom,
                index,
                preserve_content_whitespace,
            ) {
                let reason = match failure.source.refusal_source() {
                    RuntimeError::SecretDetected(_) => "secret_detected",
                    _ => "invalid_input",
                };
                refusals[index] = Some(BatchRefusal {
                    index,
                    field: failure.field,
                    reason,
                });
                failures[index] = Some(failure.source);
            }
        }
        if preserve_content_whitespace && failures.iter().any(Option::is_some) {
            return Err(super::refusal::refuse_batch(runtime, token, &atoms, failures, true).await);
        }
        let ns = token.namespace().as_str().to_owned();
        let now = now_us();
        let mut ids_by_slug: HashMap<String, String> = HashMap::new();
        let mut operations = vec![None; atoms.len()];
        let mut checked: Vec<bool> = failures.iter().map(Option::is_some).collect();
        let report = |checked: &[bool], refusals: &[Option<BatchRefusal>]| BatchAdmissionReport {
            refused: refusals.iter().flatten().cloned().collect(),
            validation_complete: checked.iter().all(|checked| *checked),
            unchecked_indices: checked
                .iter()
                .enumerate()
                .filter_map(|(index, checked)| (!checked).then_some(index))
                .collect(),
        };
        if checked.iter().any(|checked| !checked) {
            let mut reader = sql.reader().await.map_err(|error| {
                if preserve_content_whitespace {
                    sql_err("upsert_atoms reader", error)
                } else {
                    RuntimeError::Storage(error).with_batch_admission(report(&checked, &refusals))
                }
            })?;
            for (index, atom) in atoms.iter().enumerate() {
                if checked[index] {
                    continue;
                }
                let atom = atom
                    .as_ref()
                    .expect("unchecked admission has a decoded item");
                if let AtomWrite::Upsert(input) = atom {
                    if let Some(id) = ids_by_slug.get(input.slug.trim()) {
                        operations[index] = Some((id.clone(), false));
                        checked[index] = true;
                        continue;
                    }
                }
                let target = if preserve_content_whitespace {
                    super::atom_validation::target(reader.as_mut(), &ns, atom).await
                } else {
                    super::atom_validation::target_for_admission(reader.as_mut(), &ns, atom).await
                };
                match target {
                    Ok(target) => {
                        let (id, insert) =
                            target.map_or_else(|| (new_id(), true), |id| (id, false));
                        if let AtomWrite::Upsert(input) = atom {
                            ids_by_slug.insert(input.slug.trim().to_owned(), id.clone());
                        }
                        operations[index] = Some((id, insert));
                        checked[index] = true;
                    }
                    Err(error @ (RuntimeError::InvalidInput(_) | RuntimeError::NotFound(_))) => {
                        if preserve_content_whitespace {
                            return Err(error);
                        }
                        let field = match atom {
                            AtomWrite::Upsert(_) => "slug",
                            AtomWrite::PropertiesOnly(_) => "id",
                        };
                        let reason = if matches!(error, RuntimeError::NotFound(_)) {
                            "not_found"
                        } else {
                            "invalid_input"
                        };
                        refusals[index] = Some(BatchRefusal {
                            index,
                            field,
                            reason,
                        });
                        failures[index] = Some(error);
                        checked[index] = true;
                    }
                    Err(error) => {
                        if preserve_content_whitespace {
                            return Err(error);
                        }
                        return Err(error.with_batch_admission(report(&checked, &refusals)));
                    }
                }
            }
        }
        if failures.iter().any(Option::is_some) {
            let admission = report(&checked, &refusals);
            let error = super::refusal::refuse_batch(runtime, token, &atoms, failures, false).await;
            return Err(error.with_batch_admission(admission));
        }
        let p = UpsertAtomsParams {
            atoms: atoms
                .into_iter()
                .map(|atom| atom.expect("successful admission decoded every item"))
                .collect(),
            dry_run,
            chunk_size,
        };
        let operations: Vec<_> = operations
            .into_iter()
            .map(|operation| operation.expect("successful admission prepared every target"))
            .collect();

        let mut created = 0usize;
        let mut updated = 0usize;
        let mut statements = Vec::with_capacity(p.atoms.len());
        for (atom_in, (id, insert)) in p.atoms.iter().zip(operations) {
            let atom_in = match atom_in {
                AtomWrite::Upsert(atom_in) => atom_in,
                AtomWrite::PropertiesOnly(atom_in) => {
                    statements.push(SqlStatement {
                        sql: khive_runtime::sql!("knowledge_atom_properties_update").into(),
                        params: vec![
                            if atom_in.properties.is_null() {
                                SqlValue::Null
                            } else {
                                SqlValue::Text(atom_in.properties.to_string())
                            },
                            SqlValue::Integer(now),
                            SqlValue::Text(id),
                        ],
                        label: None,
                    });
                    updated += 1;
                    continue;
                }
            };
            let slug = atom_in.slug.trim().to_string();
            let raw_content = atom_in.content.as_deref().unwrap_or("");
            let content = if preserve_content_whitespace {
                raw_content.to_string()
            } else {
                raw_content.trim().to_string()
            };
            let tags_json = tags_to_json(atom_in.tags.as_ref());
            let props_json = atom_in
                .properties
                .as_ref()
                .map(|v| serde_json::to_string(v).unwrap_or_default());
            let source_uri = atom_in
                .source_uri
                .as_ref()
                .and_then(Option::as_ref)
                .map(|s| s.trim())
                .filter(|s| !s.is_empty());
            // Blank strings keep their established update behavior (preserve),
            // while JSON null is now an explicit clear.
            let source_uri_present =
                matches!(&atom_in.source_uri, Some(None)) || source_uri.is_some();
            let source_type = atom_in
                .source_type
                .as_ref()
                .and_then(Option::as_ref)
                .map(|s| s.trim())
                .filter(|s| !s.is_empty());
            let source_type_present =
                matches!(&atom_in.source_type, Some(None)) || source_type.is_some();
            let finalized_present = atom_in.finalized.is_some();
            let finalized = atom_in.finalized.flatten().unwrap_or(false);

            if insert {
                statements.push(SqlStatement {
                    sql: khive_runtime::sql!("knowledge_atom_insert").into(),
                    params: vec![
                        SqlValue::Text(id),
                        SqlValue::Text(ns.clone()),
                        SqlValue::Text(slug.clone()),
                        SqlValue::Text(atom_in.name.clone()),
                        SqlValue::Text(content.clone()),
                        SqlValue::Text(tags_json.clone()),
                        props_json
                            .as_ref()
                            .map_or(SqlValue::Null, |p| SqlValue::Text(p.clone())),
                        source_uri.map_or(SqlValue::Null, |s| SqlValue::Text(s.to_string())),
                        source_type.map_or(SqlValue::Null, |s| SqlValue::Text(s.to_string())),
                        // status mirrors the lifecycle backfill (finalized => reviewed) so a
                        // freshly-finalized atom is never left at the 'draft' default.
                        SqlValue::Text(if finalized { "reviewed" } else { "draft" }.to_string()),
                        SqlValue::Integer(finalized as i64),
                        SqlValue::Integer(now),
                        SqlValue::Integer(now),
                    ],
                    label: None,
                });
                created += 1;
            } else {
                statements.push(invalidate_section_vectors_for_atom_rename(
                    &id,
                    &ns,
                    &atom_in.name,
                    now,
                ));
                statements.push(SqlStatement {
                    // Presence/value pairs distinguish omission from explicit JSON
                    // null. Finalized is non-nullable, so its null value is bound as
                    // false. Only true promotes draft -> reviewed; clearing the flag
                    // does not demote an independent lifecycle status.
                    sql: khive_runtime::sql!("knowledge_atom_content_update").into(),
                    params: vec![
                        SqlValue::Text(atom_in.name.clone()),
                        SqlValue::Text(content),
                        SqlValue::Text(tags_json),
                        props_json.map_or(SqlValue::Null, SqlValue::Text),
                        SqlValue::Integer(source_uri_present as i64),
                        source_uri.map_or(SqlValue::Null, |s| SqlValue::Text(s.to_string())),
                        SqlValue::Integer(source_type_present as i64),
                        source_type.map_or(SqlValue::Null, |s| SqlValue::Text(s.to_string())),
                        SqlValue::Integer(finalized_present as i64),
                        SqlValue::Integer(finalized as i64),
                        SqlValue::Integer(now),
                        SqlValue::Text(id.clone()),
                        SqlValue::Text(ns.clone()),
                    ],
                    label: None,
                });
                updated += 1;
            }
        }

        let mut writer = sql.writer().await.map_err(|error| {
            if preserve_content_whitespace {
                sql_err("upsert_atoms writer", error)
            } else {
                RuntimeError::Storage(error).before_requested_write()
            }
        })?;
        writer.execute_batch(statements).await.map_err(|error| {
            if preserve_content_whitespace {
                sql_err("upsert_atoms batch", error)
            } else {
                RuntimeError::atomic_write_failure(error)
            }
        })?;
        Ok(json!({
            "created": created,
            "updated": updated,
            "total": p.atoms.len(),
        }))
    }

    pub(crate) async fn upsert_domains(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: UpsertDomainsParams = deser(params).map_err(RuntimeError::before_requested_write)?;
        if p.domains.is_empty() {
            return Err(
                RuntimeError::InvalidInput("domains list must not be empty".into())
                    .before_requested_write(),
            );
        }

        let ns = token.namespace().as_str().to_owned();
        let sql = runtime.sql();
        let now = now_us();
        let mut created = 0usize;
        let mut updated = 0usize;

        for domain_in in &p.domains {
            let prewrite = |error: RuntimeError| {
                if created == 0 && updated == 0 {
                    error.before_requested_write()
                } else {
                    error
                }
            };
            let slug = domain_in.slug.trim().to_string();
            let name = domain_in.name.trim().to_string();
            if slug.is_empty() {
                return Err(prewrite(RuntimeError::InvalidInput(
                    "domain slug must not be empty".into(),
                )));
            }
            if name.is_empty() {
                return Err(prewrite(RuntimeError::InvalidInput(
                    "domain name must not be empty".into(),
                )));
            }
            // Secret gate: scan slug and name first (before content-length validation)
            // so security violations short-circuit before business logic errors.
            khive_runtime::secret_gate::check_at(&slug, "domain", "slug").map_err(prewrite)?;
            khive_runtime::secret_gate::check_at(&name, "domain", "name").map_err(prewrite)?;
            // Domain mirror atoms are written to knowledge_atoms with the description
            // as content. Enforce the same 20-word minimum that normal atoms must satisfy
            // so the FTS and embedding surfaces receive adequate content.
            let mirror_content = domain_in.description.as_deref().unwrap_or("").trim();
            validate_atom_content(mirror_content).map_err(|e| {
                prewrite(RuntimeError::InvalidInput(format!(
                    "domain {slug:?}: description {e}"
                )))
            })?;
            // Secret gate: scan remaining caller-supplied text.
            khive_runtime::secret_gate::check_at(mirror_content, "domain", "description")
                .map_err(prewrite)?;
            if let Some(ref tags_vec) = domain_in.tags {
                khive_runtime::secret_gate::check_tags_at(tags_vec, "domain", "tags")
                    .map_err(prewrite)?;
            }
            if let Some(ref members_vec) = domain_in.members {
                khive_runtime::secret_gate::check_tags_at(members_vec, "domain", "members")
                    .map_err(prewrite)?;
            }

            let mut tags: Vec<String> = domain_in.tags.clone().unwrap_or_default();
            if !tags.iter().any(|t| t == "type:domain") {
                tags.push("type:domain".to_string());
            }
            let tags_json = serde_json::to_string(&tags).unwrap_or_else(|_| "[]".into());
            let members_json = match &domain_in.members {
                Some(m) => serde_json::to_string(m).unwrap_or_else(|_| "[]".into()),
                None => "[]".to_string(),
            };
            // The mirror's properties are synthesized entirely from `members`
            // (a `Vec<String>`, no arbitrary-key input); `DomainInput` carries no
            // `properties` field, so the top-level reserved key is unreachable here.
            let properties_json = serde_json::to_string(
                &serde_json::json!({ "members": domain_in.members.as_deref().unwrap_or(&[]) }),
            )
            .unwrap_or_else(|_| "{}".into());

            let mut reader = sql
                .reader()
                .await
                .map_err(|e| sql_err("upsert_domains reader", e))?;
            // Look up by slug WITHOUT the deleted_at filter so a tombstoned domain
            // that still owns the (namespace, slug) unique index is detected before
            // the insert path runs, instead of leaking a raw unique-constraint error.
            let existing = reader
                .query_row(SqlStatement {
                    sql: khive_runtime::sql!("knowledge_domain_slug_probe").into(),
                    params: vec![SqlValue::Text(ns.clone()), SqlValue::Text(slug.clone())],
                    label: None,
                })
                .await
                .map_err(|e| sql_err("upsert_domains lookup", e))?;
            if let Some(row) = &existing {
                if row_i64(row, "deleted_at").is_some() {
                    return Err(prewrite(RuntimeError::InvalidInput(format!(
                        "domain slug {slug:?} was previously deleted; choose a new slug"
                    ))));
                }
            }

            let id = match &existing {
                Some(row) => row_str(row, "id").ok_or_else(|| {
                    RuntimeError::Internal("missing id in existing domain row".into())
                })?,
                None => new_id(),
            };

            // Preflight: a normal atom (or a tombstoned atom that still owns the
            // unique (namespace, slug) index) must never share this domain's slug.
            // The mirror atom for THIS domain shares its id with the domain, so the
            // collision is only real when the colliding row belongs to a different
            // id. This must run BEFORE any write so a collision never leaves a
            // partially-committed domain row behind.
            let atom_collision = reader
                .query_row(SqlStatement {
                    sql: khive_runtime::sql!("knowledge_atom_slug_probe").into(),
                    params: vec![SqlValue::Text(ns.clone()), SqlValue::Text(slug.clone())],
                    label: None,
                })
                .await
                .map_err(|e| sql_err("upsert_domains atom collision check", e))?;
            if let Some(row) = &atom_collision {
                let collision_id = row_str(row, "id").ok_or_else(|| {
                    RuntimeError::Internal("missing id in colliding atom row".into())
                })?;
                if collision_id != id {
                    return Err(prewrite(RuntimeError::InvalidInput(format!(
                        "domain slug {slug:?} collides with an existing atom of the same slug"
                    ))));
                }
            }

            // Mirror write is keyed by id (shared with the domain), never by slug —
            // ON CONFLICT(namespace, slug) would blind-overwrite an unrelated atom
            // that merely happens to share this slug.
            let mirror_stmt = SqlStatement {
                sql: khive_runtime::sql!("knowledge_domain_mirror_upsert").into(),
                params: vec![
                    SqlValue::Text(id.clone()),
                    SqlValue::Text(ns.clone()),
                    SqlValue::Text(slug.clone()),
                    SqlValue::Text(name.clone()),
                    SqlValue::Text(domain_in.description.clone().unwrap_or_default()),
                    SqlValue::Text(tags_json.clone()),
                    SqlValue::Text(properties_json.clone()),
                    SqlValue::Integer(now),
                    SqlValue::Integer(now),
                ],
                label: None,
            };

            let mut writer = sql
                .writer()
                .await
                .map_err(|e| sql_err("upsert_domains writer", e))?;
            if existing.is_some() {
                let domain_stmt = SqlStatement {
                    sql: khive_runtime::sql!("knowledge_domain_update").into(),
                    params: vec![
                        SqlValue::Text(name.clone()),
                        domain_in
                            .description
                            .as_ref()
                            .map_or(SqlValue::Null, |d| SqlValue::Text(d.clone())),
                        SqlValue::Text(tags_json.clone()),
                        SqlValue::Text(members_json.clone()),
                        SqlValue::Integer(now),
                        SqlValue::Text(id.clone()),
                        SqlValue::Text(ns.clone()),
                    ],
                    label: None,
                };
                // Atomic: the canonical domain row and its FTS mirror atom are one
                // logical record; a mirror-write failure must roll back the domain
                // update too.
                writer
                    .execute_batch(vec![
                        domain_stmt,
                        invalidate_section_vectors_for_atom_rename(&id, &ns, &name, now),
                        mirror_stmt,
                    ])
                    .await
                    .map_err(|e| sql_err("upsert_domains update batch", e))?;
                updated += 1;
            } else {
                let domain_stmt = SqlStatement {
                    sql: khive_runtime::sql!("knowledge_domain_insert").into(),
                    params: vec![
                        SqlValue::Text(id.clone()),
                        SqlValue::Text(ns.clone()),
                        SqlValue::Text(slug.clone()),
                        SqlValue::Text(name.clone()),
                        domain_in
                            .description
                            .as_ref()
                            .map_or(SqlValue::Null, |d| SqlValue::Text(d.clone())),
                        SqlValue::Text(tags_json.clone()),
                        SqlValue::Text(members_json.clone()),
                        SqlValue::Integer(now),
                        SqlValue::Integer(now),
                    ],
                    label: None,
                };
                // Atomic: a mirror-insert failure must roll back the domain insert
                // too, so no domain row is ever left committed without its mirror.
                writer
                    .execute_batch(vec![
                        domain_stmt,
                        invalidate_section_vectors_for_atom_rename(&id, &ns, &name, now),
                        mirror_stmt,
                    ])
                    .await
                    .map_err(|e| sql_err("upsert_domains insert batch", e))?;
                created += 1;
            }
        }

        Ok(json!({
            "created": created,
            "updated": updated,
            "total": p.domains.len(),
        }))
    }

    pub(crate) async fn get(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: GetParams = deser(params)?;
        let ns = token.namespace().as_str().to_owned();
        let sql = runtime.sql();
        let id = p.id.trim().to_string();
        let with_sections = p.include_sections.unwrap_or(false);

        let mut reader = sql.reader().await.map_err(|e| sql_err("get reader", e))?;

        // ADR-007 Rule 2: UUID and short-prefix forms are by-ID reads, so they
        // are namespace-agnostic. Parse a complete UUID first so 32-character
        // compact UUIDs are complete identifiers, not prefixes. For non-UUID
        // input, preserve the pack's registered-slug contract: an exact slug in
        // the caller namespace wins before an all-hex value is interpreted as a
        // UUID prefix.
        let resolved_id = if let Ok(uuid) = id.parse::<Uuid>() {
            Some(uuid.to_string())
        } else {
            // Exact slug lookup precedes prefix interpretation so a registered
            // all-hex slug remains addressable. Domains are authoritative over
            // their same-slug mirror atoms.
            let row = reader
                .query_row(SqlStatement {
                    sql: khive_runtime::sql!("knowledge_domain_get_slug").into(),
                    params: vec![SqlValue::Text(ns.clone()), SqlValue::Text(id.clone())],
                    label: Some("knowledge.get.domain_by_slug".into()),
                })
                .await
                .map_err(|e| sql_err("get domain by slug", e))?;
            if let Some(r) = row {
                return domain_from_row(&r)
                    .map(|d| domain_to_json(&d))
                    .ok_or_else(|| RuntimeError::Internal("domain row parse failed".into()));
            }

            let row = reader
                .query_row(SqlStatement {
                    sql: khive_runtime::sql!("knowledge_atom_get_slug").into(),
                    params: vec![SqlValue::Text(ns.clone()), SqlValue::Text(id.clone())],
                    label: Some("knowledge.get.atom_by_slug".into()),
                })
                .await
                .map_err(|e| sql_err("get atom by slug", e))?;
            if let Some(r) = row {
                let atom = atom_from_row(&r)
                    .ok_or_else(|| RuntimeError::Internal("atom row parse failed".into()))?;
                let atom_id = atom.id.to_string();
                let mut out = atom_to_json(&atom);
                if with_sections {
                    out["sections"] = fetch_sections(runtime, &ns, &atom_id).await?;
                }
                return Ok(out);
            }

            if id.len() >= 8 && id.chars().all(|c| c.is_ascii_hexdigit()) {
                let rows = if let Some(statement) = knowledge_get_prefix_statement(&id) {
                    reader
                        .query_all(statement)
                        .await
                        .map_err(|e| sql_err("get by prefix", e))?
                } else {
                    Vec::new()
                };
                let matches: Vec<String> =
                    rows.iter().filter_map(|row| row_str(row, "id")).collect();
                match matches.as_slice() {
                    [] => {
                        return Err(RuntimeError::InvalidInput(format!(
                            "no knowledge record matches prefix: {id:?}"
                        )))
                    }
                    [only] => Some(only.clone()),
                    _ => {
                        return Err(RuntimeError::AmbiguousPrefix {
                            prefix: id.clone(),
                            matches: matches
                                .iter()
                                .filter_map(|matched| matched.parse::<Uuid>().ok())
                                .collect(),
                        })
                    }
                }
            } else {
                None
            }
        };

        if let Some(resolved_id) = resolved_id {
            // Domain-first: a domain's canonical row and its FTS mirror atom share
            // the same UUID, so the UUID branch must match the slug branch below
            // and prefer knowledge_domains — otherwise a domain UUID resolves to
            // its own mirror atom instead of the canonical domain record.
            let row = reader
                .query_row(SqlStatement {
                    sql: khive_runtime::sql!("knowledge_domain_get_id").into(),
                    params: vec![SqlValue::Text(resolved_id.clone())],
                    label: Some("knowledge.get.domain_by_id".into()),
                })
                .await
                .map_err(|e| sql_err("get domain by id", e))?;
            if let Some(r) = row {
                return domain_from_row(&r)
                    .map(|d| domain_to_json(&d))
                    .ok_or_else(|| RuntimeError::Internal("domain row parse failed".into()));
            }
            let row = reader
                .query_row(SqlStatement {
                    sql: khive_runtime::sql!("knowledge_atom_get_id").into(),
                    params: vec![SqlValue::Text(resolved_id)],
                    label: Some("knowledge.get.atom_by_id".into()),
                })
                .await
                .map_err(|e| sql_err("get atom by id", e))?;
            if let Some(r) = row {
                let atom = atom_from_row(&r)
                    .ok_or_else(|| RuntimeError::Internal("atom row parse failed".into()))?;
                let atom_id = atom.id.to_string();
                let atom_namespace = atom.namespace.clone();
                let mut out = atom_to_json(&atom);
                if with_sections {
                    out["sections"] = fetch_sections(runtime, &atom_namespace, &atom_id).await?;
                }
                return Ok(out);
            }

            // An ID-shaped input names one record or misses. It must not fall
            // through and accidentally resolve a slug with the same spelling.
            return Err(RuntimeError::NotFound(format!(
                "atom or domain not found: {id:?}"
            )));
        }

        Err(RuntimeError::NotFound(format!(
            "atom or domain not found: {id:?}"
        )))
    }

    pub(crate) async fn list(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let p: ListParams = deser(params)?;
        let kind = match p.kind.as_deref() {
            Some("domain") => KnowledgeListKind::Domain,
            Some("atom") | None => KnowledgeListKind::Atom,
            Some(other) => {
                return Err(RuntimeError::InvalidInput(
                    mask_for_redaction_surface(
                        RedactionSurface::GateProbe,
                        &format!("unknown type {other:?}; valid: atom | domain"),
                    )
                    .into_owned(),
                ))
            }
        };
        let cursor_mode = p.after.is_some();
        if cursor_mode && p.offset.is_some() {
            return Err(RuntimeError::InvalidInput(
                "knowledge.list: `after` and `offset` are mutually exclusive".into(),
            ));
        }
        validate_list_fields(kind, p.fields.as_deref())?;
        let after_id = p
            .after
            .as_deref()
            .map(parse_list_after)
            .transpose()?
            .flatten();

        if matches!(kind, KnowledgeListKind::Atom) {
            if let Some(status) = p.status.as_ref() {
                let valid = match status {
                    Value::String(_) => true,
                    Value::Array(items) => items.iter().all(Value::is_string),
                    _ => false,
                };
                if !valid {
                    return Err(RuntimeError::InvalidInput(
                        "status must be a string or an array of strings".into(),
                    ));
                }
            }
        }

        let ns = token.namespace().as_str().to_owned();
        let sql = runtime.sql();
        let limit_usize = p.limit.unwrap_or(20).clamp(1, 500);
        let limit = limit_usize as i64;
        let offset = i64::try_from(p.offset.unwrap_or(0)).map_err(|_| {
            RuntimeError::InvalidInput("knowledge.list offset exceeds the supported range".into())
        })?;
        let select_columns = list_select_columns(p.fields.as_deref());

        let mut reader = sql.reader().await.map_err(|e| sql_err("list reader", e))?;

        // A full cursor UUID resolves to the immutable seek tuple. Deliberately
        // allow a soft-deleted boundary to keep an in-flight walk resumable;
        // hard deletion, the wrong namespace, or the wrong list type fails.
        let cursor_key = if let Some(after_id) = after_id {
            let cursor_sql = match kind {
                KnowledgeListKind::Atom => {
                    khive_runtime::sql!("knowledge_atom_cursor_key")
                }
                KnowledgeListKind::Domain => {
                    khive_runtime::sql!("knowledge_domain_cursor_key")
                }
            };
            let row = reader
                .query_row(SqlStatement {
                    sql: cursor_sql.into(),
                    params: vec![
                        SqlValue::Text(ns.clone()),
                        SqlValue::Text(after_id.to_string()),
                    ],
                    label: Some(format!("knowledge.list.{}.cursor", kind.name())),
                })
                .await
                .map_err(|e| sql_err("resolve list cursor", e))?
                .ok_or_else(|| {
                    RuntimeError::NotFound(format!(
                        "knowledge.list: {} cursor {} not found in namespace {ns:?}",
                        kind.name(),
                        after_id
                    ))
                })?;
            let created_at = row_i64(&row, "created_at").ok_or_else(|| {
                RuntimeError::Internal("knowledge.list cursor row lacks created_at".into())
            })?;
            let id = row_str(&row, "id").ok_or_else(|| {
                RuntimeError::Internal("knowledge.list cursor row lacks id".into())
            })?;
            Some((created_at, id))
        } else {
            None
        };

        match kind {
            KnowledgeListKind::Domain => {
                let (data_sql, data_params) = if cursor_mode {
                    let fetch_limit = limit.saturating_add(1);
                    match cursor_key.as_ref() {
                        Some((created_at, id)) => (
                            cursor_query("knowledge_domains", &select_columns, true, ""),
                            vec![
                                SqlValue::Text(ns.clone()),
                                SqlValue::Integer(*created_at),
                                SqlValue::Text(id.clone()),
                                SqlValue::Integer(fetch_limit),
                            ],
                        ),
                        None => (
                            cursor_query("knowledge_domains", &select_columns, false, ""),
                            vec![SqlValue::Text(ns.clone()), SqlValue::Integer(fetch_limit)],
                        ),
                    }
                } else {
                    (
                        // #1671: `id` tiebreak — deterministic total order for offset pages.
                        format!(
                            "SELECT {select_columns} FROM knowledge_domains \
                             WHERE namespace = ?1 AND deleted_at IS NULL \
                             ORDER BY created_at DESC, id DESC LIMIT ?2 OFFSET ?3"
                        ),
                        vec![
                            SqlValue::Text(ns.clone()),
                            SqlValue::Integer(limit),
                            SqlValue::Integer(offset),
                        ],
                    )
                };
                let mut rows = reader
                    .query_all(SqlStatement {
                        sql: data_sql,
                        params: data_params,
                        label: Some("knowledge.list.domains".into()),
                    })
                    .await
                    .map_err(|e| sql_err("list domains", e))?;

                let has_more = cursor_mode && rows.len() > limit_usize;
                if has_more {
                    rows.truncate(limit_usize);
                }
                let next_after = has_more
                    .then(|| rows.last().and_then(|row| row_str(row, "id")))
                    .flatten();
                let items: Vec<Value> = rows
                    .iter()
                    .filter_map(|row| project_list_row(row, kind, p.fields.as_deref()))
                    .collect();

                // A cursor page carries no total: counting the namespace is a
                // full scan per page, and next_after is the completion signal.
                if cursor_mode {
                    Ok(json!({
                        "results": items,
                        "limit": limit,
                        "order": LIST_CURSOR_ORDER,
                        "next_after": next_after,
                    }))
                } else {
                    let total = reader
                        .count(SqlStatement {
                            sql: khive_runtime::sql!("knowledge_domain_count").into(),
                            params: vec![SqlValue::Text(ns)],
                            label: None,
                        })
                        .await
                        .map_err(|e| sql_err("list domains count", e))?;
                    Ok(json!({
                        "results": items,
                        "total": total,
                        "limit": limit,
                        "offset": offset,
                        "order": LIST_OFFSET_ORDER,
                    }))
                }
            }
            KnowledgeListKind::Atom => {
                let requested_statuses = status_values(p.status.as_ref());
                let exclude_buf: Vec<&str> = p
                    .exclude_status
                    .as_deref()
                    .filter(|s| !s.trim().is_empty())
                    .into_iter()
                    .collect();
                let (count_status_clause, count_status_params) =
                    status_sql_clause(&requested_statuses, &exclude_buf, 2);

                let (sql_str, row_params) = if cursor_mode {
                    let fetch_limit = limit.saturating_add(1);
                    match cursor_key.as_ref() {
                        Some((created_at, id)) => {
                            let (data_status_clause, data_status_params) =
                                status_sql_clause(&requested_statuses, &exclude_buf, 5);
                            let mut row_params = vec![
                                SqlValue::Text(ns.clone()),
                                SqlValue::Integer(*created_at),
                                SqlValue::Text(id.clone()),
                                SqlValue::Integer(fetch_limit),
                            ];
                            row_params.extend(data_status_params);
                            (
                                cursor_query(
                                    "knowledge_atoms",
                                    &select_columns,
                                    true,
                                    &data_status_clause,
                                ),
                                row_params,
                            )
                        }
                        None => {
                            let (data_status_clause, data_status_params) =
                                status_sql_clause(&requested_statuses, &exclude_buf, 3);
                            let mut row_params =
                                vec![SqlValue::Text(ns.clone()), SqlValue::Integer(fetch_limit)];
                            row_params.extend(data_status_params);
                            (
                                cursor_query(
                                    "knowledge_atoms",
                                    &select_columns,
                                    false,
                                    &data_status_clause,
                                ),
                                row_params,
                            )
                        }
                    }
                } else {
                    let (data_status_clause, data_status_params) =
                        status_sql_clause(&requested_statuses, &exclude_buf, 4);
                    let mut row_params = vec![
                        SqlValue::Text(ns.clone()),
                        SqlValue::Integer(limit),
                        SqlValue::Integer(offset),
                    ];
                    row_params.extend(data_status_params);
                    (
                        format!(
                            "SELECT {select_columns} FROM knowledge_atoms \
                             WHERE namespace = ?1 AND deleted_at IS NULL \
                             AND NOT khive_tag_contains(tags, 'type:domain'){} \
                             ORDER BY created_at DESC, id DESC LIMIT ?2 OFFSET ?3",
                            data_status_clause
                        ),
                        row_params,
                    )
                };
                let count_sql = format!(
                    "SELECT COUNT(*) FROM knowledge_atoms WHERE namespace = ?1 AND deleted_at IS NULL AND NOT khive_tag_contains(tags, 'type:domain'){}",
                    count_status_clause
                );

                let mut rows = reader
                    .query_all(SqlStatement {
                        sql: sql_str,
                        params: row_params,
                        label: Some("knowledge.list.atoms".into()),
                    })
                    .await
                    .map_err(|e| sql_err("list atoms", e))?;

                let has_more = cursor_mode && rows.len() > limit_usize;
                if has_more {
                    rows.truncate(limit_usize);
                }
                let next_after = has_more
                    .then(|| rows.last().and_then(|row| row_str(row, "id")))
                    .flatten();
                let items: Vec<Value> = rows
                    .iter()
                    .filter_map(|row| project_list_row(row, kind, p.fields.as_deref()))
                    .collect();

                // A cursor page carries no total: counting the namespace is a
                // full scan per page, and next_after is the completion signal.
                if cursor_mode {
                    Ok(json!({
                        "results": items,
                        "limit": limit,
                        "order": LIST_CURSOR_ORDER,
                        "next_after": next_after,
                    }))
                } else {
                    let mut count_params = vec![SqlValue::Text(ns)];
                    count_params.extend(count_status_params);
                    let total = reader
                        .count(SqlStatement {
                            sql: count_sql,
                            params: count_params,
                            label: Some("knowledge.list.atom_count".into()),
                        })
                        .await
                        .map_err(|e| sql_err("list atoms count", e))?;
                    Ok(json!({
                        "results": items,
                        "total": total,
                        "limit": limit,
                        "offset": offset,
                        "order": LIST_OFFSET_ORDER,
                    }))
                }
            }
        }
    }

    pub(crate) async fn delete_atoms(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        Self::delete_atoms_with_sql(runtime, token, params, runtime.sql()).await
    }

    async fn delete_atoms_with_sql(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
        sql: std::sync::Arc<dyn khive_storage::SqlAccess>,
    ) -> Result<Value, RuntimeError> {
        let p: DeleteAtomsParams = deser(params)?;
        if p.cascade.is_some() {
            tracing::warn!(
                cascade = ?p.cascade,
                "delete_atoms: cascade is accepted but not yet implemented; \
                 sections are not cascade-deleted when atoms are soft-deleted"
            );
        }
        if p.ids.is_empty() {
            return Err(RuntimeError::InvalidInput("ids must not be empty".into()));
        }

        let ns = token.namespace().as_str().to_owned();
        let now = now_us();
        // Keys are matched a chunk at a time so the namespace bind plus the key
        // binds stay below SQLite's portable variable ceiling.
        let keys: Vec<String> = p.ids.iter().map(|id| id.trim().to_string()).collect();

        // Preserve the established refusal boundary: a known domain or mirror
        // is rejected without acquiring a writer. Do not retain the reader
        // lease across writer admission.
        let mut reader = sql
            .reader()
            .await
            .map_err(|e| sql_err("delete_atoms reader", e))?;
        preflight_atom_delete(reader.as_mut(), &ns, &keys)
            .await
            .map_err(|e| sql_err("delete_atoms preflight", e))?
            .map_err(RuntimeError::InvalidInput)?;
        drop(reader);

        // Re-resolve under the applying writer: slug/domain state may have
        // changed after the preflight reader released its lease.
        let requested = keys.len();
        let runtime = runtime.clone();
        let outcome = sql.atomic_unit(Box::new(move |writer| Box::pin(async move {
            let atom_ids = match preflight_atom_delete(writer, &ns, &keys).await? {
                Ok(ids) => ids,
                Err(message) => return Ok(Box::new(Err::<usize, String>(message)) as Box<dyn std::any::Any + Send>),
            };
            let tables = khive_runtime::atomic_prepare::discover_vector_tables_in_writer(writer, &runtime).await?;
            // Keep the exact stored spelling; parsing and re-serializing a UUID
            // could redirect cleanup away from a historical text key.
            let ids: Vec<String> = atom_ids.into_iter().collect();
            let mut deleted = 0usize;
            for chunk in ids.chunks(128) {
                let placeholders = (3..chunk.len() + 3).map(|i| format!("?{i}"))
                    .collect::<Vec<_>>().join(",");
                let mut params = vec![SqlValue::Text(ns.clone()), SqlValue::Integer(now)];
                params.extend(chunk.iter().cloned().map(SqlValue::Text));
                let update_head = "UPDATE knowledge_atoms SET deleted_at = ?2";
                deleted += writer.execute(SqlStatement::new(format!(
                    "{update_head} WHERE id IN ({placeholders}) AND +namespace = ?1 AND +deleted_at IS NULL"), params,
                )).await? as usize;
                for statement in khive_runtime::atomic_prepare::vector_purge_statements(
                    &tables, &ns, chunk, Some("knowledge.atom"), "knowledge-delete-atoms",
                )? {
                    writer.execute(statement).await?;
                }
            }
            Ok(Box::new(Ok::<usize, String>(deleted)) as Box<dyn std::any::Any + Send>)
        }))).await.map_err(|e| sql_err("delete_atoms update", e))?;
        let deleted = *outcome
            .downcast::<Result<usize, String>>()
            .map_err(|_| RuntimeError::Internal("invalid delete_atoms outcome".into()))?;
        let deleted = deleted.map_err(RuntimeError::InvalidInput)?;

        Ok(json!({
            "deleted": deleted,
            "requested": requested,
        }))
    }

    pub(crate) async fn stats(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let _: StatsParams = deser(params)?;
        let ns = token.namespace().as_str().to_owned();
        let sql = runtime.sql();
        let mut reader = sql.reader().await.map_err(|e| sql_err("stats reader", e))?;

        // Atom totals and finalized coverage share the same expensive
        // namespace/live/non-domain predicate. Keep them in one aggregate so
        // large corpora pay for a single table pass and one request-deadline
        // slice rather than two sequential scans (#2218).
        let atom_stats = reader
            .query_row(SqlStatement {
                sql: khive_runtime::sql!("knowledge_atom_stats").into(),
                params: vec![SqlValue::Text(ns.clone())],
                label: Some("knowledge.stats.atom_aggregates".into()),
            })
            .await
            .map_err(|e| sql_err("stats atom aggregates", e))?;

        let total_domains = reader
            .count(SqlStatement {
                sql: khive_runtime::sql!("knowledge_domain_count").into(),
                params: vec![SqlValue::Text(ns.clone())],
                label: None,
            })
            .await
            .map_err(|e| sql_err("stats domains", e))?;

        let total_events = reader
            .count(SqlStatement {
                sql: khive_runtime::sql!("knowledge_event_count").into(),
                params: vec![SqlValue::Text(ns.clone())],
                label: Some("knowledge.stats.event_count".into()),
            })
            .await
            .map_err(|e| sql_err("stats events", e))?;

        let retrieval_eval_run_count = reader
            .count(SqlStatement {
                sql: khive_runtime::sql!("knowledge_eval_run_count").into(),
                params: vec![SqlValue::Text(ns.clone())],
                label: Some("knowledge.stats.eval_run_count".into()),
            })
            .await
            .map_err(|e| sql_err("stats eval run count", e))?;

        let latest_retrieval_eval = reader
            .query_row(SqlStatement {
                sql: khive_runtime::sql!("knowledge_eval_latest").into(),
                params: vec![SqlValue::Text(ns.clone())],
                label: Some("knowledge.stats.latest_eval_run".into()),
            })
            .await
            .map_err(|e| sql_err("stats latest eval run", e))?;

        let total_atoms = atom_stats
            .as_ref()
            .and_then(|row| row_i64(row, "total_atoms"))
            .unwrap_or(0);
        let finalized = atom_stats
            .as_ref()
            .and_then(|row| row_i64(row, "finalized_atoms"))
            .unwrap_or(0);
        let retrieval_eval_coverage = latest_retrieval_eval
            .as_ref()
            .and_then(|row| row.f64("precision_at_5").ok())
            .unwrap_or(0.0);
        let retrieval_eval_last_run_at = latest_retrieval_eval
            .as_ref()
            .and_then(|row| row.i64_or_none("run_at"));
        let retrieval_eval_last_mrr = latest_retrieval_eval
            .as_ref()
            .and_then(|row| row.f64("mrr").ok());

        let eval_coverage = if total_atoms > 0 {
            finalized as f64 / total_atoms as f64
        } else {
            0.0
        };

        let embedding_coverage =
            compute_embedding_coverage(runtime, token, &ns, total_atoms).await?;

        Ok(json!({
            "total_atoms": total_atoms,
            "total_domains": total_domains,
            "total_events": total_events,
            "eval_coverage": eval_coverage,
            "embedding_coverage": embedding_coverage,
            "retrieval_eval_coverage": retrieval_eval_coverage,
            "retrieval_eval_run_count": retrieval_eval_run_count,
            "retrieval_eval_last_run_at": retrieval_eval_last_run_at,
            "retrieval_eval_last_mrr": retrieval_eval_last_mrr,
            "namespace": ns,
        }))
    }
}

/// Fetch all sections for `atom_id` scoped to `ns`, ordered by `sort_order`.
/// Namespace isolation is preserved: `atom_id` was resolved under `ns` by the
/// caller, and we additionally filter `knowledge_sections.namespace = ns`.
async fn fetch_sections(
    runtime: &KhiveRuntime,
    ns: &str,
    atom_id: &str,
) -> Result<Value, RuntimeError> {
    let sql = runtime.sql();
    let mut reader = sql
        .reader()
        .await
        .map_err(|e| sql_err("get sections reader", e))?;

    let rows = reader
        .query_all(SqlStatement {
            sql: khive_runtime::sql!("knowledge_sections_for_atom").into(),
            params: vec![
                SqlValue::Text(atom_id.to_owned()),
                SqlValue::Text(ns.to_owned()),
            ],
            label: None,
        })
        .await
        .map_err(|e| sql_err("get sections query", e))?;

    let mut sections: Vec<Value> = Vec::with_capacity(rows.len());
    for r in &rows {
        match section_from_row(r) {
            Some(s) => sections.push(section_to_json(&s)),
            None => {
                return Err(RuntimeError::Internal(
                    "knowledge_sections row is malformed (invalid UUID or section_type); \
                     data integrity check required"
                        .into(),
                ));
            }
        }
    }

    Ok(Value::Array(sections))
}

#[cfg(test)]
#[path = "crud_tests.rs"]
mod tests;
