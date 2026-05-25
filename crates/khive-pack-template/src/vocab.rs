//! Vocabulary for the template pack.
//!
//! Define your pack's note kinds and entity kinds here.
//! These are registered with the runtime at link time via `Pack` impl constants.
//!
//! ADR-023 §1: packs own closed sets of kinds declared as `&'static [&'static str]`.
//! Kinds must not overlap with other packs in the same binary (boot-time check).

/// Note kinds this pack contributes to the vocabulary.
///
/// Example: `"my_note_kind"`.
/// Leave empty (`&[]`) if your pack has no custom note kinds.
pub const NOTE_KINDS: &[&str] = &["template_note"];

/// Entity kinds this pack contributes to the vocabulary.
///
/// Example: `"my_entity_kind"`.
/// Leave empty (`&[]`) if your pack has no custom entity kinds.
pub const ENTITY_KINDS: &[&str] = &[];
