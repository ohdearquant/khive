//! Key and revision guards evaluated by the transaction owner.

use khive_storage::{SqlStatement, SqlValue, SqlWriter, StorageError};
use khive_types::{Details, KhiveError};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{KhiveRuntime, NamespaceToken, RuntimeError, RuntimeResult};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NoteFence {
    pub key: String,
    pub kind: String,
    pub expected_version: i64,
}

impl NoteFence {
    pub fn validate(&self) -> RuntimeResult<()> {
        crate::keyed_memory::validate_memory_key(&self.key)?;
        if self.kind.is_empty() || self.expected_version < 1 {
            return Err(RuntimeError::InvalidInput(
                "fence requires a note kind and a positive expected_version".into(),
            ));
        }
        Ok(())
    }
}

/// An object retains the original refusal shape; a list identifies its failing entry.
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum NoteFences {
    One(NoteFence),
    Many(Vec<NoteFence>),
}

impl From<NoteFence> for NoteFences {
    fn from(fence: NoteFence) -> Self {
        Self::One(fence)
    }
}

impl NoteFences {
    pub fn entries(&self) -> &[NoteFence] {
        match self {
            Self::One(fence) => std::slice::from_ref(fence),
            Self::Many(fences) => fences,
        }
    }

    pub fn validate(&self) -> RuntimeResult<()> {
        if self.entries().is_empty() {
            return Err(RuntimeError::InvalidInput(
                "fence list must not be empty".into(),
            ));
        }
        let mut seen = std::collections::HashMap::new();
        for (index, fence) in self.entries().iter().enumerate() {
            fence.validate()?;
            if let Some(first) = seen.insert((&fence.kind, &fence.key), index) {
                return Err(RuntimeError::InvalidInput(format!(
                    "duplicate fence (kind, key) at indices {first} and {index}"
                )));
            }
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for NoteFences {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Shape {
            One(NoteFence),
            Many(Vec<NoteFence>),
        }
        let fences = match Shape::deserialize(deserializer)? {
            Shape::One(fence) => Self::One(fence),
            Shape::Many(fences) => Self::Many(fences),
        };
        fences.validate().map_err(serde::de::Error::custom)?;
        Ok(fences)
    }
}

/// Missing optional fences default to None; explicitly supplied null is invalid.
pub fn deserialize_optional_fences<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<NoteFences>, D::Error> {
    NoteFences::deserialize(deserializer).map(Some)
}

#[derive(Clone, Debug, Default)]
pub struct NoteWriteOptions {
    pub key: Option<String>,
    pub expected_version: Option<i64>,
    pub fence: Option<NoteFences>,
    pub embed: Option<bool>,
}

impl NoteWriteOptions {
    pub fn validate(&self) -> RuntimeResult<()> {
        if let Some(key) = &self.key {
            crate::keyed_memory::validate_memory_key(key)?;
        }
        if self.expected_version.is_some_and(|version| version < 1) {
            return Err(RuntimeError::InvalidInput(
                "expected_version must be positive".into(),
            ));
        }
        if let Some(fence) = &self.fence {
            fence.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct NoteWriteGuard {
    pub namespace: String,
    pub target_id: Uuid,
    pub expected_version: Option<i64>,
    pub fence: Option<NoteFences>,
    pub create_key: Option<(String, String)>,
}

#[derive(Clone, Debug)]
pub(crate) struct NoteVectors {
    namespace: String,
    subject_id: Uuid,
}

impl NoteVectors {
    pub(crate) fn new(namespace: String, subject_id: Uuid) -> Self {
        Self {
            namespace,
            subject_id,
        }
    }

    async fn tables(writer: &mut dyn SqlWriter) -> Result<Vec<String>, StorageError> {
        // vec_* is the backend's reserved vector-table namespace. Catalog
        // types exclude sqlite-vec shadow tables without guessing suffixes.
        let tables = writer
            .query_all(statement(
                "SELECT name FROM pragma_table_list \
             WHERE schema='main' AND type='virtual' AND name GLOB 'vec_*' ORDER BY name",
                vec![],
            ))
            .await?;
        let mut names = Vec::with_capacity(tables.len());
        for row in tables {
            let Some(SqlValue::Text(table)) = row.get("name") else {
                return Err(StorageError::Internal(
                    "invalid vector table catalog row".into(),
                ));
            };
            if !table.strip_prefix("vec_").is_some_and(|key| {
                !key.is_empty() && key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
            }) {
                return Err(StorageError::Internal(
                    "invalid persisted vector table name".into(),
                ));
            }
            names.push(table.clone());
        }
        Ok(names)
    }

    pub(crate) async fn has_rows(&self, writer: &mut dyn SqlWriter) -> Result<bool, StorageError> {
        for table in Self::tables(writer).await? {
            if writer
                .query_scalar(statement(
                    format!(
                        "SELECT 1 FROM main.{table} WHERE namespace=?1 AND subject_id=?2 LIMIT 1"
                    ),
                    vec![
                        SqlValue::Text(self.namespace.clone()),
                        SqlValue::Text(self.subject_id.to_string()),
                    ],
                ))
                .await?
                .is_some()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(crate) async fn apply(&self, writer: &mut dyn SqlWriter) -> Result<(), StorageError> {
        for table in Self::tables(writer).await? {
            let scope = vec![
                SqlValue::Text(self.namespace.clone()),
                SqlValue::Text(self.subject_id.to_string()),
            ];
            writer
                .execute(statement(
                    format!(
                "INSERT INTO ann_write_log (namespace,embedding_model,kind,field,subject_id,op) \
                 SELECT namespace,embedding_model,kind,field,subject_id,'delete' \
                 FROM main.{table} WHERE namespace=?1 AND subject_id=?2"),
                    scope.clone(),
                ))
                .await?;
            writer
                .execute(statement(
                    format!("DELETE FROM main.{table} WHERE namespace=?1 AND subject_id=?2"),
                    scope,
                ))
                .await?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct NoteEmbeddingInheritance {
    pub vectors: NoteVectors,
    pub kind: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoteWriteConflict {
    Version {
        expected: i64,
        current: i64,
    },
    Fence {
        key: String,
        expected: i64,
        current: Option<i64>,
        index: Option<usize>,
    },
    Key {
        key: String,
        existing_id: String,
    },
}

impl NoteWriteConflict {
    pub fn into_error(self) -> KhiveError {
        let (message, details) = match self {
            Self::Version { expected, current } => (
                "note version precondition failed",
                vec![
                    ("reason", "version_conflict".into()),
                    ("expected_version", expected.to_string()),
                    ("current_version", current.to_string()),
                ],
            ),
            Self::Fence {
                key,
                expected,
                current,
                index,
            } => {
                let mut fields = vec![
                    ("reason", "fence_conflict".into()),
                    ("key", key),
                    ("expected_version", expected.to_string()),
                ];
                if let Some(current) = current {
                    fields.push(("current_version", current.to_string()));
                }
                if let Some(index) = index {
                    fields.push(("index", index.to_string()));
                }
                ("note fence precondition failed", fields)
            }
            Self::Key { key, existing_id } => (
                "a live note already holds this key",
                vec![
                    ("reason", "key_conflict".into()),
                    ("key", key),
                    ("existing_id", existing_id),
                ],
            ),
        };
        KhiveError::conflict(message).with_details(Details::new_owned(details))
    }
}

pub(crate) fn statement(sql: impl Into<String>, params: Vec<SqlValue>) -> SqlStatement {
    SqlStatement {
        sql: sql.into(),
        params,
        label: Some("note-write-guard".into()),
    }
}

impl NoteWriteGuard {
    pub(crate) async fn check_fence(
        &self,
        writer: &mut dyn SqlWriter,
    ) -> Result<Option<NoteWriteConflict>, StorageError> {
        let Some(fences) = &self.fence else {
            return Ok(None);
        };
        for (index, fence) in fences.entries().iter().enumerate() {
            let current = writer.query_scalar(statement(
            "SELECT version FROM notes WHERE namespace=?1 AND kind=?2 AND key=?3 AND deleted_at IS NULL",
            vec![SqlValue::Text(self.namespace.clone()), SqlValue::Text(fence.kind.clone()),
                 SqlValue::Text(fence.key.clone())],
        )).await?;
            let current = match current {
                None => None,
                Some(SqlValue::Integer(version)) => Some(version),
                Some(_) => {
                    return Err(StorageError::Internal(
                        "invalid persisted note version".into(),
                    ))
                }
            };
            if current != Some(fence.expected_version) {
                return Ok(Some(NoteWriteConflict::Fence {
                    key: fence.key.clone(),
                    expected: fence.expected_version,
                    current,
                    index: matches!(fences, NoteFences::Many(_)).then_some(index),
                }));
            }
        }
        Ok(None)
    }

    pub(crate) async fn classify_refusal(
        &self,
        writer: &mut dyn SqlWriter,
    ) -> Result<Option<NoteWriteConflict>, StorageError> {
        if let Some((kind, key)) = &self.create_key {
            let holder = writer.query_scalar(statement(
                "SELECT id FROM notes WHERE namespace=?1 AND kind=?2 AND key=?3 AND deleted_at IS NULL",
                vec![SqlValue::Text(self.namespace.clone()), SqlValue::Text(kind.clone()), SqlValue::Text(key.clone())],
            )).await?;
            if let Some(SqlValue::Text(existing_id)) = holder {
                return Ok(Some(NoteWriteConflict::Key {
                    key: key.clone(),
                    existing_id,
                }));
            }
        }
        if let Some(expected) = self.expected_version {
            let current = writer
                .query_scalar(statement(
                    "SELECT version FROM notes WHERE id=?1 AND deleted_at IS NULL",
                    vec![SqlValue::Text(self.target_id.to_string())],
                ))
                .await?;
            if let Some(SqlValue::Integer(current)) = current {
                if current != expected {
                    return Ok(Some(NoteWriteConflict::Version { expected, current }));
                }
            }
        }
        Ok(None)
    }
}

pub(crate) fn validate_head(note: &khive_storage::note::Note) -> RuntimeResult<()> {
    if note.kind != "head" {
        return Ok(());
    }
    if note.name.is_some() {
        return Err(RuntimeError::InvalidInput("head notes have no name".into()));
    }
    serde_json::from_str::<serde_json::Value>(&note.content).map_err(|error| {
        RuntimeError::InvalidInput(format!("head content must be JSON text: {error}"))
    })?;
    if let Some(tags) = note
        .properties
        .as_ref()
        .and_then(|p| p.get("tags"))
        .and_then(|t| t.as_array())
    {
        for tag in tags.iter().filter_map(|tag| tag.as_str()) {
            if let Some(kind) = tag.strip_prefix("kind:") {
                if kind.len() > 64 || kind.contains('\0') {
                    return Err(RuntimeError::InvalidInput(
                        "head document kind must be at most 64 bytes without U+0000".into(),
                    ));
                }
            }
        }
    }
    Ok(())
}

impl KhiveRuntime {
    pub async fn get_note_by_key(
        &self,
        token: &NamespaceToken,
        key: &str,
        kind: Option<&str>,
        after_key: bool,
    ) -> RuntimeResult<khive_storage::note::Note> {
        crate::keyed_memory::validate_memory_key(key)?;
        let mut matches = self
            .notes(token)?
            .get_live_notes_by_key(token.namespace().as_str(), key, kind)
            .await?;
        match matches.len() {
            0 => {
                let error = KhiveError::not_found("note key", key);
                Err(if after_key {
                    error.with_details(Details::new_owned([
                        ("reason", "after_key_missing".into()),
                        ("key", key.into()),
                    ]))
                } else {
                    error
                }
                .into())
            }
            1 => Ok(matches.remove(0)),
            _ => {
                let kinds = matches
                    .iter()
                    .map(|note| note.kind.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
                Err(KhiveError::conflict("note key is ambiguous")
                    .with_details(Details::new_owned([
                        ("reason", "key_ambiguous".into()),
                        ("key", key.into()),
                        ("kinds", kinds),
                    ]))
                    .into())
            }
        }
    }

    pub(crate) async fn prepare_versioned_note_update(
        &self,
        token: &NamespaceToken,
        snapshot: khive_storage::note::Note,
        patch: crate::curation::NotePatch,
    ) -> RuntimeResult<(khive_storage::note::Note, crate::atomic_plan::UpdatePlan)> {
        use crate::atomic_plan::{AffectedRowGuard, PlanStatement, PostCommitEffect, UpdatePlan};
        let options = patch.write_options.clone();
        options.validate()?;
        if options.key.is_some() {
            return Err(RuntimeError::InvalidInput("key is immutable".into()));
        }
        if let Some(fences) = &options.fence {
            for fence in fences.entries() {
                self.validate_note_kind(&fence.kind)?;
            }
        }
        let expected_updated_at = snapshot.updated_at;
        let expected_deleted_at = snapshot.deleted_at;
        let next_version = snapshot
            .version
            .checked_add(1)
            .ok_or_else(|| RuntimeError::InvalidInput("note version exhausted".into()))?;
        let (mut note, text_changed) = self
            .prepare_update_note_from_snapshot(token, snapshot, patch)
            .await?;
        validate_head(&note)?;
        note.version = next_version;
        let mut update = if self.stream_member_error(&note).await?.is_some() {
            khive_db::stores::note::note_metadata_replace_if_unchanged_statement(
                &note,
                expected_updated_at,
                expected_deleted_at,
            )
        } else {
            khive_db::stores::note::note_replace_if_unchanged_statement(
                &note,
                expected_updated_at,
                expected_deleted_at,
            )
        };
        if let Some(version) = options.expected_version {
            update.params.push(SqlValue::Integer(version));
            update
                .sql
                .push_str(&format!(" AND version = ?{}", update.params.len()));
        }
        let mut statements = vec![PlanStatement {
            statement: update,
            guard: Some(AffectedRowGuard::exactly(1)),
        }];
        if text_changed {
            for sql in khive_db::stores::text::delete_document_statements(
                "fts_notes",
                &note.namespace,
                note.id,
            )
            .into_iter()
            .chain(khive_db::stores::text::insert_document_statements(
                "fts_notes",
                &crate::curation::note_fts_document(&note),
            )) {
                statements.push(PlanStatement {
                    statement: sql,
                    guard: None,
                });
            }
        }
        // This is a potential reindex. Inherited membership is resolved by the
        // writer because vector publication can occur without a note revision.
        let post_commit =
            if options.embed != Some(false) && (text_changed || options.embed == Some(true)) {
                PostCommitEffect::ReindexNote {
                    note_id: note.id,
                    version: note.version,
                }
            } else if text_changed || options.embed == Some(false) {
                PostCommitEffect::NoteChanged {
                    note_id: note.id,
                    kind: note.kind.clone(),
                }
            } else {
                PostCommitEffect::None
            };
        let plan = UpdatePlan {
            target_id: note.id,
            statements,
            post_commit,
            edge_natural_key: None,
            note_guard: Some(NoteWriteGuard {
                namespace: token.namespace().as_str().into(),
                target_id: note.id,
                expected_version: options.expected_version,
                fence: options.fence,
                create_key: None,
            }),
            note_vector_purge: (options.embed == Some(false))
                .then(|| NoteVectors::new(note.namespace.clone(), note.id)),
            note_embedding_inheritance: (text_changed && options.embed.is_none()).then(|| {
                NoteEmbeddingInheritance {
                    vectors: NoteVectors::new(note.namespace.clone(), note.id),
                    kind: note.kind.clone(),
                }
            }),
        };
        Ok((note, plan))
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_note_with_options(
        &self,
        token: &NamespaceToken,
        kind: &str,
        name: Option<&str>,
        content: &str,
        embedding_content: Option<&str>,
        salience: Option<f64>,
        decay_factor: Option<f64>,
        properties: Option<serde_json::Value>,
        annotates: Vec<Uuid>,
        embedding_model: Option<&str>,
        options: NoteWriteOptions,
    ) -> RuntimeResult<(
        khive_storage::note::Note,
        crate::retrieval::EmbeddingTruncationReport,
    )> {
        use crate::atomic_message::{AtomicNoteOptions, AtomicNoteSpec};
        use crate::atomic_runner::{run_atomic_unit, AtomicOpFailure, AtomicRunOutcome};
        use crate::note_create::{prepare_note_create, KeyPublication};
        options.validate()?;
        if options.expected_version.is_some() {
            return Err(RuntimeError::InvalidInput(
                "expected_version applies only to update".into(),
            ));
        }
        if let Some(fences) = &options.fence {
            for fence in fences.entries() {
                self.validate_note_kind(&fence.kind)?;
            }
        }
        if let Some(prefix) = embedding_content {
            if prefix.is_empty() || prefix.len() >= content.len() || !content.starts_with(prefix) {
                return Err(RuntimeError::InvalidInput(
                    "embedding_content must be a non-empty proper prefix of content".into(),
                ));
            }
            crate::secret_gate::check(prefix)?;
        }
        let mut candidate =
            khive_storage::note::Note::new(token.namespace().as_str(), kind, content);
        candidate.name = name.map(str::to_owned);
        candidate.properties = properties.clone();
        validate_head(&candidate)?;
        let (mut prepared, _) = prepare_note_create(
            self,
            AtomicNoteSpec {
                token,
                id: None,
                kind,
                name,
                content,
                properties,
            },
            AtomicNoteOptions {
                salience,
                decay_factor,
                embedding_model,
                embedding_content,
                embed: Some(options.embed.unwrap_or(kind != "head")),
                key: options.key.as_deref(),
                fence: options.fence.as_ref(),
            },
            &annotates,
            KeyPublication::AtInsert,
        )
        .await?;
        let note = prepared.notes.remove(0);
        match run_atomic_unit(self.sql().as_ref(), prepared.plans).await {
            Ok(AtomicRunOutcome::Committed { .. }) => {
                self.fire_note_mutation_hook(&note.kind, note.id).await;
                Ok((note, prepared.embedding_truncation))
            }
            Ok(AtomicRunOutcome::RolledBack {
                failure: AtomicOpFailure::NoteConflict(conflict),
                ..
            }) => Err(conflict.into_error().into()),
            Ok(AtomicRunOutcome::RolledBack { failure, .. }) => Err(RuntimeError::Internal(
                format!("note creation rolled back: {failure:?}"),
            )),
            Err(error) => Err(RuntimeError::Storage(error.0)),
        }
    }
}
