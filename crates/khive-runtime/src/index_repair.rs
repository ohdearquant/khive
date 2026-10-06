use std::any::Any;

use khive_storage::{note::Note, AtomicUnitOp, Entity, SqlStatement, SqlValue, TextDocument};
use khive_types::SubstrateKind;
use serde::Serialize;
use uuid::Uuid;

use crate::{KhiveRuntime, NamespaceToken, PostCommitDegradation, RuntimeError, RuntimeResult};

/// Per-record index work that actually committed. Failed stages can coexist
/// with repairs; rerunning repairs only the gaps that still remain.
#[derive(Clone, Debug, Serialize)]
pub struct IndexRepairReport {
    pub id: Uuid,
    pub substrate: SubstrateKind,
    pub namespace: String,
    pub repaired: Vec<String>,
    pub failures: Vec<PostCommitDegradation>,
}

enum Record {
    Entity(Entity),
    Note(Note),
}

impl Record {
    fn document(&self) -> TextDocument {
        match self {
            Self::Entity(entity) => crate::entity_fts_document(entity),
            Self::Note(note) => crate::note_fts_document(note),
        }
    }

    fn version(&self) -> i64 {
        match self {
            Self::Entity(entity) => entity.version,
            Self::Note(note) => note.version,
        }
    }

    fn tables(&self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Entity(_) => ("entities", "fts_entities", "entity.body"),
            Self::Note(_) => ("notes", "fts_notes", "note.content"),
        }
    }

    fn vector_statements(&self, table: &str, model: &str, vector: &[f32]) -> Vec<SqlStatement> {
        match self {
            Self::Entity(entity) => {
                KhiveRuntime::entity_vector_insert_statements(table, entity, model, vector)
            }
            Self::Note(note) => crate::atomic_message::vector_insert_statements(
                table,
                &note.namespace,
                note.id,
                "note.content",
                model,
                vector,
                "record-index-repair",
            )
            .into_iter()
            .map(|plan| plan.statement)
            .collect(),
        }
    }
}

#[derive(Clone, Copy)]
enum Publication {
    Repaired,
    Healthy,
    Changed,
    Occupied,
}

fn statement(sql: String, params: Vec<SqlValue>) -> SqlStatement {
    SqlStatement {
        sql,
        params,
        label: Some("record-index-repair".into()),
    }
}

fn vector_checks(record: &Record, doc: &TextDocument, model: &str) -> (SqlStatement, SqlStatement) {
    let table = format!("vec_{}", crate::config::sanitize_key(model));
    let healthy = statement(format!("SELECT 1 FROM {table} WHERE subject_id=?1 AND namespace=?2 AND kind=?3 AND field=?4 AND embedding_model=?5"), vec![
        SqlValue::Text(doc.subject_id.to_string()), SqlValue::Text(doc.namespace.clone()), SqlValue::Text(doc.kind.to_string()), SqlValue::Text(record.tables().2.into()), SqlValue::Text(model.into()),
    ]);
    // Even a mismatched identity may occupy the vec0 subject primary key.
    // Repair preserves it instead of silently replacing an existing vector.
    let occupied = statement(
        format!("SELECT 1 FROM {table} WHERE subject_id=?1"),
        vec![SqlValue::Text(doc.subject_id.to_string())],
    );
    (healthy, occupied)
}

fn same_document(a: &TextDocument, b: &TextDocument) -> bool {
    a.subject_id == b.subject_id
        && a.kind == b.kind
        && a.record_kind == b.record_kind
        && a.namespace == b.namespace
        && a.title.as_deref().unwrap_or_default() == b.title.as_deref().unwrap_or_default()
        && a.body == b.body
        && a.tags == b.tags
        && a.metadata == b.metadata
        && a.updated_at == b.updated_at
}

impl IndexRepairReport {
    fn failure(&mut self, stage: &'static str, error: impl ToString) {
        self.failures.push(PostCommitDegradation {
            stage,
            error: error.to_string(),
        });
    }

    fn publication(
        &mut self,
        result: RuntimeResult<Publication>,
        stage: &'static str,
        label: String,
    ) -> bool {
        match result {
            Ok(Publication::Repaired) => self.repaired.push(label),
            Ok(Publication::Healthy) => {},
            Ok(Publication::Changed) => {
                self.failure("source_revision", "record changed or was deleted before index publication; rerun against the current record");
                return false;
            }
            Ok(Publication::Occupied) => self.failure(stage, format!("{label}: another vector identity occupies the subject; existing vector was preserved")),
            Err(error) => self.failure(stage, format!("{label}: {error}")),
        }
        true
    }
}

impl KhiveRuntime {
    /// Repair indexes for one unambiguous live ID in the token namespace.
    /// Only missing/stale FTS and missing kind-selected vectors are repaired.
    /// Existing vectors, substrate rows and other records are never rewritten.
    /// A later failure does not undo earlier reported repairs.
    pub async fn repair_record_indexes(
        &self,
        token: &NamespaceToken,
        id: Uuid,
    ) -> RuntimeResult<IndexRepairReport> {
        if self.is_read_only() {
            return Err(RuntimeError::InvalidInput(
                "record index repair requires a writable runtime".into(),
            ));
        }
        let namespace = token.namespace().as_str();
        let entity = self
            .entities(token)?
            .get_entity(id)
            .await?
            .filter(|entity| entity.namespace == namespace);
        let note = self
            .notes(token)?
            .get_note(id)
            .await?
            .filter(|note| note.namespace == namespace);
        let record = match (entity, note) {
            (Some(entity), None) => Record::Entity(entity),
            (None, Some(note)) => Record::Note(note),
            (Some(_), Some(_)) => return Err(RuntimeError::InvalidInput(format!("record {id} is ambiguous: both an entity and a note exist in namespace {namespace}"))),
            (None, None) => return Err(RuntimeError::NotFound(format!("live entity or note {id} in namespace {namespace}"))),
        };
        let doc = record.document();
        let mut report = IndexRepairReport {
            id,
            substrate: doc.kind,
            namespace: doc.namespace.clone(),
            repaired: vec![],
            failures: vec![],
        };
        let mut models = match &record {
            Record::Entity(_) => self.registered_embedding_model_names(),
            Record::Note(note) => self.embedding_models_for_note_kind(&note.kind),
        };
        models.sort();
        models.dedup();
        let text = match &record {
            Record::Entity(_) => self.text(token),
            Record::Note(_) => self.text_for_notes(token),
        };
        match text {
            Ok(text) => match text.get_document(&doc.namespace, id).await {
                Ok(Some(current)) if same_document(&current, &doc) => {}
                Ok(_) => {
                    let result = self.repair_record_fts(&record, &doc).await;
                    if !report.publication(result, "fts", "fts".into()) {
                        return Ok(report);
                    }
                }
                Err(error) => report.failure("fts", error),
            },
            Err(error) => report.failure("fts", error),
        }
        let body = match &record {
            Record::Entity(_) => doc.body.as_str(),
            Record::Note(note) => crate::curation::note_embedding_text_ref(note),
        };
        if body.trim().is_empty() {
            return Ok(report);
        }
        for model in models {
            let (storage_model, dimensions) = match self.vector_model_metadata(&model) {
                Ok(metadata) => metadata,
                Err(error) => {
                    report.failure("vector_presence", format!("{model}: {error}"));
                    continue;
                }
            };
            let store = match self.backend().vectors_for_namespace(
                &crate::config::sanitize_key(&storage_model),
                &storage_model,
                dimensions,
                &doc.namespace,
            ) {
                Ok(store) => store,
                Err(error) => {
                    report.failure("vector_presence", format!("{model}: {error}"));
                    continue;
                }
            };
            let (healthy, occupied) = vector_checks(&record, &doc, &storage_model);
            let presence: RuntimeResult<Option<Publication>> = async {
                let mut reader = self.sql().reader().await?;
                if reader.query_scalar(healthy).await?.is_some() {
                    Ok(Some(Publication::Healthy))
                } else if reader.query_scalar(occupied).await?.is_some() {
                    Ok(Some(Publication::Occupied))
                } else {
                    Ok(None)
                }
            }
            .await;
            match presence {
                Ok(Some(Publication::Healthy)) => {
                    #[cfg(test)]
                    tests::pause("vector_read").await;
                    match store
                        .get_vectors(&[id], &doc.namespace, record.tables().2)
                        .await
                    {
                        Ok(vectors) if vectors.contains_key(&id) => continue,
                        Ok(_) => report.failure(
                            "vector_presence",
                            format!(
                                "{model}: vector identity was present for {id}, \
                                 but its vector was not returned"
                            ),
                        ),
                        Err(error) => {
                            report.failure("vector_presence", format!("{model}: {error}"))
                        }
                    }
                    continue;
                }
                Ok(Some(Publication::Occupied)) => {
                    report.failure("vector_presence", format!("{model}: another vector identity occupies the subject; existing vector was preserved"));
                    continue;
                }
                Ok(_) => {}
                Err(error) => {
                    report.failure("vector_presence", format!("{model}: {error}"));
                    continue;
                }
            }
            let outcome = match self
                .embed_document_with_model_outcome_for_token(token, &model, body)
                .await
            {
                Ok(outcome) => outcome,
                Err(error) => {
                    report.failure("embedding", format!("{model}: {error}"));
                    continue;
                }
            };
            if outcome.vector.len() != dimensions
                || outcome.vector.iter().any(|value| !value.is_finite())
            {
                report.failure(
                    "vector_publication",
                    format!("{model}: embedding has invalid dimensions or non-finite values"),
                );
                continue;
            }
            let result = self
                .repair_record_vector(&record, &doc, &storage_model, &outcome.vector)
                .await;
            if !report.publication(
                result,
                "vector_publication",
                format!("vector:{storage_model}"),
            ) {
                break;
            }
        }
        Ok(report)
    }

    async fn repair_record_fts(
        &self,
        record: &Record,
        doc: &TextDocument,
    ) -> RuntimeResult<Publication> {
        let (_, table, _) = record.tables();
        let canonical = khive_db::stores::text::insert_document_statement(table, doc);
        let map = khive_db::stores::text::rowid_map_table(table);
        let healthy = statement(
            format!(
                "SELECT 1 FROM {table} AS t JOIN {map} AS m ON m.rowid=t.rowid \
            WHERE m.subject_id=?1 AND m.namespace=?6 AND t.subject_id=?1 AND t.kind=?2 \
            AND t.title=?3 AND t.body=?4 AND t.tags=?5 AND t.namespace=?6 \
            AND t.metadata IS ?7 AND t.updated_at=?8 AND t.record_kind IS ?9"
            ),
            canonical.params,
        );
        let statements = khive_db::stores::text::delete_document_statements(
            table,
            &doc.namespace,
            doc.subject_id,
        )
        .into_iter()
        .chain(khive_db::stores::text::insert_document_statements(
            table, doc,
        ))
        .collect();
        #[cfg(test)]
        tests::pause("fts").await;
        self.publish_record_repair(record, doc, healthy, None, statements)
            .await
    }

    async fn repair_record_vector(
        &self,
        record: &Record,
        doc: &TextDocument,
        model: &str,
        vector: &[f32],
    ) -> RuntimeResult<Publication> {
        let table = format!("vec_{}", crate::config::sanitize_key(model));
        let (healthy, occupied) = vector_checks(record, doc, model);
        let statements = record.vector_statements(&table, model, vector);
        #[cfg(test)]
        tests::pause("vector").await;
        self.publish_record_repair(record, doc, healthy, Some(occupied), statements)
            .await
    }

    async fn publish_record_repair(
        &self,
        record: &Record,
        doc: &TextDocument,
        healthy: SqlStatement,
        occupied: Option<SqlStatement>,
        statements: Vec<SqlStatement>,
    ) -> RuntimeResult<Publication> {
        let source = statement(
            format!(
                "SELECT version FROM {} WHERE id=?1 AND namespace=?2 AND deleted_at IS NULL",
                record.tables().0
            ),
            vec![
                SqlValue::Text(doc.subject_id.to_string()),
                SqlValue::Text(doc.namespace.clone()),
            ],
        );
        let version = record.version();
        let op: AtomicUnitOp = Box::new(move |writer| {
            Box::pin(async move {
                let current = writer.query_scalar(source).await?;
                let result = if !matches!(current, Some(SqlValue::Integer(current)) if current == version)
                {
                    Publication::Changed
                } else if writer.query_scalar(healthy).await?.is_some() {
                    Publication::Healthy
                } else if let Some(occupied) = occupied {
                    if writer.query_scalar(occupied).await?.is_some() {
                        Publication::Occupied
                    } else {
                        for statement in statements {
                            writer.execute(statement).await?;
                        }
                        Publication::Repaired
                    }
                } else {
                    for statement in statements {
                        writer.execute(statement).await?;
                    }
                    Publication::Repaired
                };
                Ok(Box::new(result) as Box<dyn Any + Send>)
            })
        });
        self.sql()
            .atomic_unit(op)
            .await?
            .downcast::<Publication>()
            .map(|result| *result)
            .map_err(|_| RuntimeError::Internal("invalid record index repair outcome".into()))
    }
}

#[cfg(test)]
#[path = "index_repair_tests.rs"]
mod tests;
