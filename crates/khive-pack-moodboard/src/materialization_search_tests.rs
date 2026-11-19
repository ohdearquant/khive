use super::*;
use khive_runtime::Namespace;
use khive_storage::{Attachment, AttachmentSubstrate};

#[tokio::test]
async fn one_shot_window_underfills_even_with_a_live_vector_beyond_it() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let descriptor = DescriptorIdentity::fixture(4);
    let root = tempfile::tempdir().unwrap();
    let blob_store =
        khive_db::stores::blob::FsBlobStore::new(root.path().to_path_buf(), 0).unwrap();
    let content_ref = blob_store.put(b"live candidate".to_vec()).await.unwrap();
    let mut live = Entity::new(
        token.namespace().as_str(),
        "artifact",
        "beyond the one-shot window",
    )
    .with_entity_type(Some("visual_asset"));
    live.id = Uuid::from_u128(6);
    runtime
        .entities(&token)
        .unwrap()
        .upsert_entity(live.clone())
        .await
        .unwrap();
    runtime
        .attachments()
        .unwrap()
        .upsert_attachment(Attachment {
            record_uuid: live.id,
            substrate: AttachmentSubstrate::Entity,
            role: "content".into(),
            content_ref: content_ref.clone(),
            media_type: None,
            size_bytes: None,
            created_at: live.created_at,
        })
        .await
        .unwrap();
    // The first five vector rows have no live graph entity. Every vector is
    // normalized, with strictly decreasing cosine similarity to the query.
    for id in 1_u128..=6 {
        let cosine = 1.0_f32 - id as f32 / 10.0;
        let embedding = [cosine, (1.0 - cosine * cosine).sqrt(), 0.0, 0.0];
        index_embedding(
            &runtime,
            &token,
            &descriptor,
            Uuid::from_u128(id),
            &embedding,
        )
        .await
        .unwrap();
    }
    let query = [1.0, 0.0, 0.0, 0.0];
    let full = search_embedding(&runtime, &token, &descriptor, &query, 6)
        .await
        .unwrap();
    assert_eq!(
        full.len(),
        6,
        "the sixth live candidate exists in the backend"
    );
    assert_eq!(full[5].subject_id, live.id);
    let result = search_materialized_hits(
        &runtime,
        &runtime,
        &token,
        &blob_store,
        Uuid::nil(),
        &descriptor,
        &query,
        1,
    )
    .await
    .unwrap();
    assert!(
        result.is_empty(),
        "4 * K + 1 admits only five stale candidates; do not refill"
    );
    assert_eq!(candidate_limit(1), 5);
    assert_eq!(candidate_limit(100), 401);
}
