//! Note storage capability — temporal-referential record CRUD.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::types::{BatchWriteSummary, DeleteMode, Page, PageRequest, StorageResult};

pub use khive_types::NoteKind;

/// A storage-level note record. Flat, SQL-friendly representation.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Note {
    pub id: Uuid,
    pub namespace: String,
    pub kind: NoteKind,
    pub content: String,
    pub salience: f64,
    pub decay_factor: f64,
    pub expires_at: Option<i64>,
    pub properties: Option<Value>,
    pub created_at: i64,
    pub updated_at: i64,
    pub deleted_at: Option<i64>,
}

impl Note {
    pub fn new(namespace: impl Into<String>, kind: NoteKind, content: impl Into<String>) -> Self {
        let now = chrono::Utc::now().timestamp_micros();
        Self {
            id: Uuid::new_v4(),
            namespace: namespace.into(),
            kind,
            content: content.into(),
            salience: 0.5,
            decay_factor: 0.0,
            expires_at: None,
            properties: None,
            created_at: now,
            updated_at: now,
            deleted_at: None,
        }
    }

    pub fn with_salience(mut self, s: f64) -> Self {
        self.salience = s.clamp(0.0, 1.0);
        self
    }

    pub fn with_decay(mut self, d: f64) -> Self {
        self.decay_factor = d.max(0.0);
        self
    }

    pub fn with_properties(mut self, p: Value) -> Self {
        self.properties = Some(p);
        self
    }
}

#[async_trait]
pub trait NoteStore: Send + Sync + 'static {
    async fn upsert_note(&self, note: Note) -> StorageResult<()>;
    async fn upsert_notes(&self, notes: Vec<Note>) -> StorageResult<BatchWriteSummary>;
    async fn get_note(&self, id: Uuid) -> StorageResult<Option<Note>>;
    async fn delete_note(&self, id: Uuid, mode: DeleteMode) -> StorageResult<bool>;
    async fn query_notes(
        &self,
        namespace: &str,
        kind: Option<NoteKind>,
        page: PageRequest,
    ) -> StorageResult<Page<Note>>;
    async fn count_notes(&self, namespace: &str, kind: Option<NoteKind>) -> StorageResult<u64>;

    async fn upsert_note_if_below_quota(&self, note: Note, max_notes: u64) -> StorageResult<bool> {
        let count = self.count_notes(&note.namespace, None).await?;
        if count >= max_notes {
            return Ok(false);
        }
        self.upsert_note(note).await?;
        Ok(true)
    }
}
