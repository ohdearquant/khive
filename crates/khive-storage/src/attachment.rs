//! Role-keyed binary attachments for entity and note records.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::blob::ContentRef;
use crate::capability::StorageCapability;
use crate::error::StorageError;
use crate::types::StorageResult;

/// The record substrate that owns an attachment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AttachmentSubstrate {
    Entity,
    Note,
}

impl AttachmentSubstrate {
    /// Stable lowercase value stored in SQLite and exposed on the wire.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Entity => "entity",
            Self::Note => "note",
        }
    }
}

impl std::fmt::Display for AttachmentSubstrate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for AttachmentSubstrate {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "entity" => Ok(Self::Entity),
            "note" => Ok(Self::Note),
            other => Err(format!(
                "attachment substrate must be \"entity\" or \"note\", got {other:?}"
            )),
        }
    }
}

/// Caller-supplied metadata for one role-keyed attachment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewAttachment {
    pub role: String,
    pub content_ref: ContentRef,
    pub media_type: Option<String>,
    pub size_bytes: Option<u64>,
}

impl NewAttachment {
    /// Validate values that must be represented in the SQLite attachment row.
    pub fn validate(&self) -> StorageResult<()> {
        validate_attachment_role(&self.role)?;
        validate_attachment_size(self.size_bytes)
    }
}

/// A persisted attachment row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub record_uuid: Uuid,
    pub substrate: AttachmentSubstrate,
    pub role: String,
    pub content_ref: ContentRef,
    pub media_type: Option<String>,
    pub size_bytes: Option<u64>,
    pub created_at: i64,
}

impl Attachment {
    /// Bind caller-supplied metadata to one record and timestamp.
    pub fn from_new(
        record_uuid: Uuid,
        substrate: AttachmentSubstrate,
        new_attachment: NewAttachment,
        created_at: i64,
    ) -> Self {
        Self {
            record_uuid,
            substrate,
            role: new_attachment.role,
            content_ref: new_attachment.content_ref,
            media_type: new_attachment.media_type,
            size_bytes: new_attachment.size_bytes,
            created_at,
        }
    }

    /// Validate values that must be represented in the SQLite attachment row.
    pub fn validate(&self) -> StorageResult<()> {
        validate_attachment_role(&self.role)?;
        validate_attachment_size(self.size_bytes)
    }
}

/// Validate a role at every lookup and mutation boundary.
pub fn validate_attachment_role(role: &str) -> StorageResult<()> {
    if role.is_empty() {
        return Err(StorageError::InvalidInput {
            capability: StorageCapability::Attachments,
            operation: "validate_attachment_role".into(),
            message: "attachment role must not be empty".to_string(),
        });
    }
    if role.chars().any(char::is_control) {
        return Err(StorageError::InvalidInput {
            capability: StorageCapability::Attachments,
            operation: "validate_attachment_role".into(),
            message: "attachment role must not contain control characters".to_string(),
        });
    }
    Ok(())
}

fn validate_attachment_size(size_bytes: Option<u64>) -> StorageResult<()> {
    if size_bytes.is_some_and(|size| size > i64::MAX as u64) {
        return Err(StorageError::InvalidInput {
            capability: StorageCapability::Attachments,
            operation: "validate_attachment".into(),
            message: format!(
                "attachment size_bytes must fit SQLite INTEGER (maximum {} bytes)",
                i64::MAX
            ),
        });
    }
    Ok(())
}

/// Role-keyed attachment metadata within one backend.
///
/// This trait is placement-blind; runtime/host wiring chooses the canonical
/// main backend that participates in blob liveness.
#[async_trait]
pub trait AttachmentStore: Send + Sync + 'static {
    /// Insert or replace one attachment role.
    async fn upsert_attachment(&self, attachment: Attachment) -> StorageResult<()>;
    /// Fetch one attachment role for a record.
    async fn get_attachment(
        &self,
        record_uuid: Uuid,
        role: &str,
    ) -> StorageResult<Option<Attachment>>;
    /// List all attachment roles for a record in stable role order.
    async fn list_attachments(&self, record_uuid: Uuid) -> StorageResult<Vec<Attachment>>;
    /// Remove one attachment role without touching the referenced blob.
    async fn delete_attachment(&self, record_uuid: Uuid, role: &str) -> StorageResult<bool>;
}
