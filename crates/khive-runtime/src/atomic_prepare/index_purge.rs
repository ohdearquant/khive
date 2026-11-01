use super::{
    delete_document_statements, event_insert_statements, EventKind, KhiveRuntime, NamespaceToken,
    PlanStatement, RuntimeError, RuntimeResult, SqlStatement, SqlValue, SubstrateKind, Uuid, Value,
};

#[cfg(doc)]
use crate::atomic_runner::apply_plan;

/// Every registered embedding model's vector table name, in the exact format
/// `curation::merge_entity_sql` uses (`"vec_{sanitize_key(model_name)}"`) —
/// reused here so atomic delete/merge purge the same tables the non-atomic
/// paths do.
pub(crate) fn vector_table_names(runtime: &KhiveRuntime) -> Vec<String> {
    runtime
        .registered_embedding_model_names()
        .iter()
        .map(|name| format!("vec_{}", crate::config::sanitize_key(name)))
        .collect()
}

/// A guarded (`guard: None` — best-effort mirror, matching the non-atomic
/// index-cleanup calls which don't assert a row existed) `DELETE` statement
/// against one vector table for a single subject, scoped by namespace.
///
/// Vector tables carry a real index on `(subject_id, namespace)`
/// ([`khive_db::stores::vectors`]) — this row-scan predicate is not the FTS
/// full-table-scan class this module's `purge_fts_document_statements`
/// exists to avoid, so it is left as a direct `namespace = ? AND subject_id =
/// ?` predicate.
fn purge_index_row_statement(
    table: &str,
    namespace: &str,
    subject_id: &str,
    label: &str,
) -> PlanStatement {
    let quoted = format!("\"{}\"", table.replace('"', "\"\""));
    PlanStatement {
        statement: SqlStatement {
            sql: format!("DELETE FROM main.{quoted} WHERE namespace = ?1 AND subject_id = ?2"),
            params: vec![
                SqlValue::Text(namespace.to_string()),
                SqlValue::Text(subject_id.to_string()),
            ],
            label: Some(label.to_string()),
        },
        guard: None,
    }
}

fn purge_vector_provenance_statement(
    table: &str,
    namespace: &str,
    subject_id: &str,
    label: &str,
) -> PlanStatement {
    let model_key = &table[4..];
    PlanStatement {
        statement: SqlStatement {
            sql: "DELETE FROM main.vector_provenance \
                  WHERE model_key = ?1 AND subject_id = ?2 AND namespace = ?3"
                .to_string(),
            params: vec![
                SqlValue::Text(model_key.to_string()),
                SqlValue::Text(subject_id.to_string()),
                SqlValue::Text(namespace.to_string()),
            ],
            label: Some(label.to_string()),
        },
        guard: None,
    }
}

/// The FTS-document half of an index purge: `fts_table`'s row for `subject_id`
/// (looked up via `khive_db::stores::text::rowid_map_table`, not a
/// `namespace`/`subject_id` scan — those columns are `UNINDEXED` in every
/// FTS5 DDL) plus that row's own entry in the sidecar map. Order-sensitive:
/// index 0 must run before index 1 — see `delete_document_statements`'s
/// adjacency contract.
fn purge_fts_document_statements(
    fts_table: &str,
    namespace: &str,
    subject_id: Uuid,
    label_prefix: &str,
) -> [PlanStatement; 2] {
    let [mut fts_stmt, mut map_stmt] = delete_document_statements(fts_table, namespace, subject_id);
    fts_stmt.label = Some(label_prefix.to_string());
    map_stmt.label = Some(format!("{label_prefix}-map"));
    [
        PlanStatement {
            statement: fts_stmt,
            guard: None,
        },
        PlanStatement {
            statement: map_stmt,
            guard: None,
        },
    ]
}

fn log_vector_row_delete_statement(
    table: &str,
    namespace: &str,
    subject_id: &str,
    label: &str,
) -> PlanStatement {
    let quoted = format!("\"{}\"", table.replace('"', "\"\""));
    PlanStatement {
        statement: SqlStatement {
            sql: format!(
                "INSERT INTO main.ann_write_log \
                 (namespace, embedding_model, kind, field, subject_id, op) \
                 SELECT namespace, embedding_model, kind, field, subject_id, 'delete' \
                 FROM main.{quoted} WHERE namespace = ?1 AND subject_id = ?2"
            ),
            params: vec![
                SqlValue::Text(namespace.to_string()),
                SqlValue::Text(subject_id.to_string()),
            ],
            label: Some(label.to_string()),
        },
        guard: None,
    }
}

/// `true` iff a table named `table` currently exists in the backing SQLite
/// database (`sqlite_master` probe, read-only — safe in async prepare, does
/// NOT open/create the vector store, so it cannot lazily create the table
/// itself).
async fn vector_table_exists(runtime: &KhiveRuntime, table: &str) -> RuntimeResult<bool> {
    let mut reader = runtime
        .sql()
        .reader()
        .await
        .map_err(RuntimeError::Storage)?;
    let row = reader
        .query_scalar(SqlStatement {
            sql: "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1".to_string(),
            params: vec![SqlValue::Text(table.to_string())],
            label: Some("atomic-delete-vec-table-exists".to_string()),
        })
        .await
        .map_err(RuntimeError::Storage)?;
    Ok(row.is_some())
}

/// Append the FTS + every registered model's vector-row purge for `subject_id`
/// (scoped to the RECORD's own namespace, matching `delete_entity`/
/// `delete_note`'s `record_tok`/`record_ns` convention: not the caller
/// token's namespace, per by-ID namespace-agnosticism) onto `statements`.
///
/// FTS tables (`fts_entities`/`fts_notes`) always exist (created at schema
/// migration time) so their purge is unconditional. `vec_*` tables are
/// created lazily on first vector-store open, so a default runtime can
/// register embedding models before any vector table necessarily exists:
/// a raw unconditional `DELETE FROM vec_*` can hit `no such table` on a
/// fresh DB. Only push the vec purge for tables that actually exist:
/// absence means the record definitionally has no vector row for that
/// model, so skipping is data-parity-correct (the non-atomic path would
/// lazily create the table then delete zero rows: same data outcome,
/// without this read-only prepare pass performing an init side effect).
pub(super) async fn push_index_purge_statements(
    runtime: &KhiveRuntime,
    statements: &mut Vec<PlanStatement>,
    fts_table: &str,
    namespace: &str,
    subject_id: Uuid,
    label_prefix: &str,
) -> RuntimeResult<()> {
    let mut tables = Vec::new();
    for table in vector_table_names(runtime) {
        if vector_table_exists(runtime, &table).await? {
            tables.push(table);
        }
    }
    push_index_purge_for_tables(
        statements,
        fts_table,
        namespace,
        subject_id,
        label_prefix,
        &tables,
    );
    Ok(())
}

/// Resolve optional vector tables under the applying writer, so a truncation
/// cannot miss a table created by an append before its transaction begins.
/// This only drives the supplied SQL handle; it never acquires another lease.
pub(crate) async fn vector_tables_in_writer(
    writer: &mut dyn khive_storage::SqlWriter,
    candidates: &[String],
) -> Result<Vec<String>, khive_storage::StorageError> {
    let mut tables = Vec::new();
    for table in candidates {
        let present = writer
            .query_scalar(
                SqlStatement::new(
                    "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    vec![SqlValue::Text(table.clone())],
                )
                .labelled("stream-truncate-vector-table"),
            )
            .await?;
        if present.is_some() {
            tables.push(table.clone());
        }
    }
    Ok(tables)
}

/// Shared DML construction once the caller has resolved the vector tables.
pub(crate) fn push_index_purge_for_tables(
    statements: &mut Vec<PlanStatement>,
    fts_table: &str,
    namespace: &str,
    subject_id: Uuid,
    label_prefix: &str,
    tables: &[String],
) {
    statements.extend(purge_fts_document_statements(
        fts_table,
        namespace,
        subject_id,
        &format!("{label_prefix}-purge-fts"),
    ));
    statements.extend(
        vector_purge_statements(
            tables,
            namespace,
            &[subject_id.to_string()],
            None,
            label_prefix,
        )
        .expect("registered vector tables have a vec_ prefix")
        .into_iter()
        .map(|statement| PlanStatement {
            statement,
            guard: None,
        }),
    );
}

// Tokenize enough of SQLite CREATE VIRTUAL TABLE DDL to avoid interpreting
// USING inside a quoted table name or comment as the module declaration.
fn virtual_module(ddl: &str) -> Result<Option<String>, khive_storage::StorageError> {
    let mut chars = ddl.chars().peekable();
    let mut tokens: Vec<(String, bool)> = Vec::new();
    while let Some(c) = chars.next() {
        if c.is_whitespace() {
            continue;
        }
        if c == '-' && chars.peek() == Some(&'-') {
            chars.next();
            for next in chars.by_ref() {
                if next == '\n' {
                    break;
                }
            }
            continue;
        }
        if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            let mut closed = false;
            while let Some(next) = chars.next() {
                if next == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    closed = true;
                    break;
                }
            }
            if !closed {
                return Err(khive_storage::StorageError::Internal(
                    "unterminated DDL comment".into(),
                ));
            }
            continue;
        }
        if matches!(c, '"' | '\'' | '`' | '[') {
            let end = if c == '[' { ']' } else { c };
            let mut token = String::new();
            let mut closed = false;
            while let Some(next) = chars.next() {
                if next == end {
                    if end != ']' && chars.peek() == Some(&end) {
                        chars.next();
                        token.push(end);
                        continue;
                    }
                    closed = true;
                    break;
                }
                token.push(next);
            }
            if !closed {
                return Err(khive_storage::StorageError::Internal(
                    "unterminated DDL identifier".into(),
                ));
            }
            tokens.push((token, true));
            continue;
        }
        if matches!(c, '(' | ')' | ',' | ';' | '.') {
            tokens.push((c.to_string(), false));
            continue;
        }
        let mut token = String::from(c);
        while let Some(&next) = chars.peek() {
            if next.is_whitespace()
                || matches!(next, '(' | ')' | ',' | ';' | '.' | '"' | '\'' | '`' | '[')
            {
                break;
            }
            if matches!(next, '-' | '/') {
                let mut lookahead = chars.clone();
                lookahead.next();
                if (next == '-' && lookahead.peek() == Some(&'-'))
                    || (next == '/' && lookahead.peek() == Some(&'*'))
                {
                    break;
                }
            }
            token.push(next);
            chars.next();
        }
        tokens.push((token, false));
    }
    let keyword = |index: usize, word: &str| {
        tokens
            .get(index)
            .is_some_and(|(token, quoted)| !quoted && token.eq_ignore_ascii_case(word))
    };
    if !(keyword(0, "create") && keyword(1, "virtual") && keyword(2, "table")) {
        return Ok(None);
    }
    let using = (3..tokens.len())
        .find(|&i| keyword(i, "using"))
        .ok_or_else(|| {
            khive_storage::StorageError::Internal("virtual table DDL missing USING".into())
        })?;
    let module = tokens
        .get(using + 1)
        .map(|(token, _)| token.clone())
        .filter(|token| !token.is_empty() && token != "(")
        .ok_or_else(|| {
            khive_storage::StorageError::Internal("virtual table DDL missing module".into())
        })?;
    Ok(Some(module))
}

fn insert_casefolded_table_name(
    seen: &mut std::collections::BTreeSet<String>,
    name: &str,
) -> Result<(), khive_storage::StorageError> {
    if !seen.insert(name.to_ascii_lowercase()) {
        return Err(khive_storage::StorageError::Internal(format!(
            "duplicate case-insensitive vector table name {name}"
        )));
    }
    Ok(())
}

/// Classify on the applying writer, before mutation, so historical unregistered
/// models are included without mistaking vec0 shadow or fixture tables for models.
pub async fn discover_vector_tables_in_writer(
    writer: &mut dyn khive_storage::SqlWriter,
    runtime: &KhiveRuntime,
) -> Result<Vec<String>, khive_storage::StorageError> {
    use std::collections::{BTreeMap, BTreeSet};
    let rows = writer.query_all(SqlStatement::new(
        "SELECT name, sql FROM main.sqlite_master WHERE type='table' AND name LIKE 'vec\\_%' ESCAPE '\\'",
        vec![],
    ).labelled("vector-purge-discover-tables")).await?;
    let mut names: BTreeSet<String> = vector_table_names(runtime).into_iter().collect();
    let mut schema = BTreeMap::new();
    for row in rows {
        let name = match row.get("name") {
            Some(SqlValue::Text(name)) => name.clone(),
            _ => {
                return Err(khive_storage::StorageError::Internal(
                    "invalid vector table name".into(),
                ))
            }
        };
        let ddl = match row.get("sql") {
            Some(SqlValue::Text(sql)) => sql.clone(),
            _ => {
                return Err(khive_storage::StorageError::Internal(format!(
                    "missing DDL for {name}"
                )))
            }
        };
        names.insert(name.clone());
        schema.insert(name, ddl);
    }
    let mut tables = Vec::new();
    let mut folded_names = BTreeSet::new();
    for name in names {
        let Some(ddl) = schema.get(&name) else {
            continue;
        };
        insert_casefolded_table_name(&mut folded_names, &name)?;
        let Some(module) = virtual_module(ddl)? else {
            continue;
        };
        if !module.eq_ignore_ascii_case("vec0") {
            return Err(khive_storage::StorageError::Internal(format!(
                "virtual vector table {name} uses unsupported module {module}"
            )));
        }
        let suffix = name.get(4..).unwrap_or("");
        if suffix.is_empty()
            || !suffix
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(khive_storage::StorageError::Internal(format!(
                "vector table {name} has an invalid model-key suffix"
            )));
        }
        let columns = writer
            .query_all(
                SqlStatement::new(
                    "SELECT name FROM pragma_table_info(?1, 'main')",
                    vec![SqlValue::Text(name.clone())],
                )
                .labelled("vector-purge-table-columns"),
            )
            .await?;
        for required in [
            "subject_id",
            "namespace",
            "field",
            "kind",
            "embedding_model",
        ] {
            if !columns.iter().any(|row| matches!(row.get("name"), Some(SqlValue::Text(column)) if column.eq_ignore_ascii_case(required))) {
                return Err(khive_storage::StorageError::Internal(format!(
                    "vector table {name} is missing required column {required}"
                )));
            }
        }
        tables.push(name);
    }
    Ok(tables)
}

/// Construct vector-only cleanup in log-before-delete order. `field = None`
/// removes every field, matching ordinary index cleanup; a field limits both
/// the log and vector deletion to that field. For field-limited cleanup,
/// provenance remains if this model still has a row for the subject.
/// Table names must have the `vec_` prefix and should come from writer-side
/// discovery. Identifiers are quoted independently of bound values.
pub fn vector_purge_statements(
    tables: &[String],
    namespace: &str,
    subject_ids: &[String],
    field: Option<&str>,
    label_prefix: &str,
) -> Result<Vec<SqlStatement>, khive_storage::StorageError> {
    let mut statements = Vec::with_capacity(tables.len() * 3);
    if subject_ids.is_empty() {
        return Ok(statements);
    }
    for table in tables {
        if !table
            .get(..4)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("vec_"))
        {
            return Err(khive_storage::StorageError::Internal(
                "invalid vector table name".into(),
            ));
        }
        // The legacy all-field caller supplies one subject, preserving labels
        // and the original all-field and provenance behavior.
        if field.is_none() {
            for subject_id in subject_ids {
                statements.push(
                    log_vector_row_delete_statement(
                        table,
                        namespace,
                        subject_id,
                        &format!("{label_prefix}-log-delete-vec-{table}"),
                    )
                    .statement,
                );
                statements.push(
                    purge_index_row_statement(
                        table,
                        namespace,
                        subject_id,
                        &format!("{label_prefix}-purge-vec-{table}"),
                    )
                    .statement,
                );
                statements.push(
                    purge_vector_provenance_statement(
                        table,
                        namespace,
                        subject_id,
                        &format!("{label_prefix}-purge-vec-provenance-{table}"),
                    )
                    .statement,
                );
            }
            continue;
        }
        let field = field.expect("field checked above");
        let model_key = &table[4..];
        let quoted = format!("main.\"{}\"", table.replace('"', "\"\""));
        let ids = (3..subject_ids.len() + 3)
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(",");
        let mut params = vec![
            SqlValue::Text(namespace.to_owned()),
            SqlValue::Text(field.to_owned()),
        ];
        params.extend(subject_ids.iter().map(|id| SqlValue::Text(id.to_string())));
        statements.push(SqlStatement {
            sql: format!("INSERT INTO main.ann_write_log (namespace, embedding_model, kind, field, subject_id, op) \
                 SELECT namespace, embedding_model, kind, field, subject_id, 'delete' \
                 FROM {quoted} WHERE namespace = ?1 AND field = ?2 AND subject_id IN ({ids})"),
            params: params.clone(),
            label: Some(format!("{label_prefix}-log-delete-vec-{table}")),
        });
        statements.push(SqlStatement {
            sql: format!("DELETE FROM {quoted} WHERE namespace = ?1 AND field = ?2 AND subject_id IN ({ids})"),
            params,
            label: Some(format!("{label_prefix}-purge-vec-{table}")),
        });
        let mut provenance_params = vec![
            SqlValue::Text(model_key.to_owned()),
            SqlValue::Text(namespace.to_owned()),
        ];
        provenance_params.extend(subject_ids.iter().map(|id| SqlValue::Text(id.to_string())));
        statements.push(SqlStatement {
            sql: format!("DELETE FROM main.vector_provenance WHERE model_key = ?1 COLLATE NOCASE AND namespace = ?2 \
                 AND subject_id IN ({ids}) AND NOT EXISTS (SELECT 1 FROM {quoted} AS remaining \
                 WHERE remaining.subject_id = vector_provenance.subject_id \
                 AND remaining.namespace = vector_provenance.namespace)"),
            params: provenance_params,
            label: Some(format!("{label_prefix}-purge-vec-provenance-{table}")),
        });
    }
    Ok(statements)
}

/// Event-store append parity for the canonical handlers that emit a
/// lifecycle event after their row mutation: `update_entity` ->
/// `EntityUpdated`, `delete_entity` -> `EntityDeleted`, `delete_note` ->
/// `NoteDeleted`, `update_edge` -> `EdgeUpdated`, `delete_edge` ->
/// `EdgeDeleted`, `link` -> `LinkCreated`/`EdgeUpdated`, and
/// `update_note` -> `NoteUpdated`. See
/// `docs/api/atomic_prepare.md#event_append_statements` for why
/// this is a `PlanStatement` rather than a `PostCommitEffect`.
///
/// Invariant: returned statements are unguarded — appended after the plan's
/// own guarded row statement, so [`apply_plan`]'s stop-on-first-failure
/// contract means they are only reached once that row mutation's guard has
/// already held. Committing the event row atomically with the mutation it
/// describes strengthens canonical's guarantee: the non-atomic handlers write
/// the event in a separate transaction, ordered but not atomic with the row
/// mutation.
pub(crate) fn event_append_statements(
    token: &NamespaceToken,
    namespace: &str,
    verb: &str,
    kind: EventKind,
    substrate: SubstrateKind,
    target_id: Uuid,
    payload: Value,
) -> RuntimeResult<Vec<PlanStatement>> {
    let record_token = token
        .with_namespace(crate::Namespace::parse(namespace).map_err(|error| {
            RuntimeError::Internal(format!("event namespace invalid: {error}"))
        })?);
    let event = crate::EventAttribution::from_token(&record_token).stamp(
        khive_storage::event::Event::new(namespace.to_string(), verb, kind, substrate, "")
            .with_target(target_id)
            .with_payload(payload),
    );
    let statements = event_insert_statements(&event)
        .map_err(|e| RuntimeError::Internal(format!("event_insert_statements: {e}")))?;
    Ok(statements
        .into_iter()
        .map(|statement| PlanStatement {
            statement,
            guard: None,
        })
        .collect())
}

#[cfg(test)]
mod virtual_module_tests {
    use super::virtual_module;

    #[test]
    fn duplicate_casefold_names_are_refused() {
        let mut seen = std::collections::BTreeSet::new();
        super::insert_casefolded_table_name(&mut seen, "vec_Qwen3").unwrap();
        let error = super::insert_casefolded_table_name(&mut seen, "vec_qwen3").unwrap_err();
        assert!(error.to_string().contains("vec_qwen3"));
    }

    #[test]
    fn ddl_classification_respects_quotes_case_and_comments() {
        for ddl in [
            "CREATE VIRTUAL TABLE vec_a USING vec0(subject_id TEXT PRIMARY KEY)",
            "CREATE VIRTUAL TABLE vec_a/* USING other */USING vec0(subject_id TEXT PRIMARY KEY)",
            "CREATE VIRTUAL TABLE vec_a-- USING other\nUSING vec0(subject_id TEXT PRIMARY KEY)",
            "create  virtual\ntable IF NOT EXISTS \"vec_odd USING fts5\" /* USING fts5 */ USING \"VEC0\" (subject_id TEXT PRIMARY KEY)",
        ] {
            assert!(virtual_module(ddl).unwrap().unwrap().eq_ignore_ascii_case("vec0"));
        }
        assert_eq!(
            virtual_module("CREATE TABLE vec_ordinary (value TEXT)").unwrap(),
            None
        );
        assert_eq!(
            virtual_module("CREATE VIRTUAL TABLE vec_odd USING fts5(content)")
                .unwrap()
                .as_deref(),
            Some("fts5")
        );
        assert!(virtual_module("CREATE VIRTUAL TABLE vec_bad").is_err());
    }
}
