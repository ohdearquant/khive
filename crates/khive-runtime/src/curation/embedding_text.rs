use super::{Entity, Note, NoteFtsScalars, SubstrateKind, TextDocument};

// ---------------------------------------------------------------------------
// FTS document construction
// ---------------------------------------------------------------------------

/// Build the canonical text embedded for an entity on create, update, merge,
/// and repair paths.
pub fn entity_embedding_text(entity: &Entity) -> String {
    match &entity.description {
        Some(description) if !description.is_empty() => {
            format!("{} {description}", entity.name)
        }
        _ => entity.name.clone(),
    }
}

/// Build the canonical text embedded for a note when no explicit bounded
/// embedding prefix was supplied at creation time.
pub fn note_embedding_text(note: &Note) -> String {
    note_embedding_text_ref(note).to_owned()
}

/// Borrow the canonical note embedding text for runtime paths that do not
/// require ownership.
pub(crate) fn note_embedding_text_ref(note: &Note) -> &str {
    &note.content
}

/// Build the `TextDocument` for an entity. This is the single source of truth for
/// entity FTS document shape; all write paths (create, update, merge, reindex, backfill)
/// must go through this function so search parity is guaranteed.
///
/// Body rule: when the entity has a non-empty description, prepend the name
/// (`"<name> <description>"`). Otherwise the body is just the name. This
/// matches the FTS index contract: `title` and `body` are the ranked columns;
/// `tags`, `metadata`, and `namespace` are UNINDEXED.
///
/// `updated_at` is taken from the entity's own timestamp so that backfill and
/// reindex runs record the entity's actual mutation time rather than the
/// reindex execution time.
pub fn entity_fts_document(entity: &Entity) -> TextDocument {
    let updated_at =
        chrono::DateTime::from_timestamp_micros(entity.updated_at).unwrap_or_else(chrono::Utc::now);
    TextDocument {
        subject_id: entity.id,
        kind: SubstrateKind::Entity,
        record_kind: Some(entity.kind.clone()),
        title: Some(entity.name.clone()),
        body: entity_embedding_text(entity),
        tags: entity.tags.clone(),
        namespace: entity.namespace.clone(),
        metadata: entity.properties.clone(),
        updated_at,
    }
}

/// Build the `TextDocument` for a note. This is the single source of truth for
/// note FTS document shape; all write paths (create, update, reindex) must go
/// through this function so recall parity is guaranteed. Changes here apply to
/// every caller automatically.
///
/// Body rule: when the note has a `name`, prepend it to the content
/// (`"<name> <content>"`). This matches the FTS index contract: title and body
/// both contribute to ranking, and the name is the most salient signal.
///
/// `updated_at` is taken from the note's own timestamp (not `Utc::now()`) so
/// that backfill and reindex runs record the note's actual mutation time rather
/// than the reindex execution time.
pub fn note_fts_document(note: &Note) -> TextDocument {
    let body = match &note.name {
        Some(n) => format!("{n} {}", note.content),
        None => note.content.clone(),
    };
    let updated_at =
        chrono::DateTime::from_timestamp_micros(note.updated_at).unwrap_or_else(chrono::Utc::now);
    TextDocument {
        subject_id: note.id,
        kind: SubstrateKind::Note,
        record_kind: Some(note.kind.clone()),
        title: note.name.clone(),
        body,
        tags: vec![],
        namespace: note.namespace.clone(),
        metadata: note.properties.clone(),
        updated_at,
    }
}

/// Derive [`NoteFtsScalars`] from a [`Note`].
///
/// All values match the encoding that [`Fts5TextSearch::upsert_document`]
/// applies when given the output of [`note_fts_document`].
pub(crate) fn note_fts_scalars(note: &Note) -> NoteFtsScalars {
    let doc = note_fts_document(note);
    NoteFtsScalars {
        record_kind: doc.record_kind.unwrap_or_default(),
        title: doc.title.unwrap_or_default(),
        body: doc.body,
        tags: "[]".to_string(),
        metadata: doc
            .metadata
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default()),
        updated_at_micros: doc.updated_at.timestamp_micros(),
    }
}
