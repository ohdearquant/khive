//! Compile-time census of the known public runtime embedding disclosure seams.
//! The legacy-return behavior is exercised with oversized inputs in the runtime
//! unit tests; this list does not purport to discover future methods.

use khive_runtime::KhiveRuntime;

#[test]
fn known_public_embedding_disclosure_surfaces_exist() {
    let _ = KhiveRuntime::embed_document;
    let _ = KhiveRuntime::embed_document_outcome;
    let _ = KhiveRuntime::embed_document_batch;
    let _ = KhiveRuntime::embed_document_batch_outcomes;
    let _ = KhiveRuntime::embed_document_batch_with_model;
    let _ = KhiveRuntime::embed_document_batch_with_model_outcomes;
    let _ = KhiveRuntime::create_entity_with_attachments;
    let _ = KhiveRuntime::create_entity_with_attachments_and_report;
    let _ = KhiveRuntime::update_entity_if_unchanged;
    let _ = KhiveRuntime::update_entity_if_unchanged_with_embedding_report;
    let _ = KhiveRuntime::create_note;
    let _ = KhiveRuntime::create_note_with_embedding_content;
    let _ = KhiveRuntime::create_note_with_embedding_content_and_report;
    let _ = KhiveRuntime::create_note_with_decay;
    let _ = KhiveRuntime::create_note_with_decay_and_report;
    let _ = KhiveRuntime::create_note_with_decay_for_embedding_model;
    let _ = KhiveRuntime::create_note_with_decay_for_embedding_model_and_report;
}
