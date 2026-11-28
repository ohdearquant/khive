use super::{Entity, Note, SubstrateKind, TextDocument};

// ---------------------------------------------------------------------------
// FTS document construction
// ---------------------------------------------------------------------------

/// Build the canonical text embedded for an entity on create, update, merge,
/// and repair paths.
///
/// The text is the name, then each alias in the record's order, then the
/// description when it is non-empty, joined by single spaces. An entity with
/// no aliases gets exactly the text it got before aliases existed.
pub fn entity_embedding_text(entity: &Entity) -> String {
    let mut text = entity.name.clone();
    for alias in &entity.aliases {
        text.push(' ');
        text.push_str(&alias.alias);
    }
    if let Some(description) = entity.description.as_deref().filter(|d| !d.is_empty()) {
        text.push(' ');
        text.push_str(description);
    }
    text
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
/// Body rule: the body is [`entity_embedding_text`]: the name, then the
/// entity's aliases, then the description when it is non-empty. The title is
/// the name alone. This matches the FTS index contract: `title` and `body` are
/// the ranked columns; `tags`, `metadata`, and `namespace` are UNINDEXED.
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
