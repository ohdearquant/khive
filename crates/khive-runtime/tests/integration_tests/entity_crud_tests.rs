// =============================================================================
// Entity operations
// =============================================================================

#[tokio::test]
async fn entity_create_and_get_roundtrip() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    let entity = rt
        .create_entity_with_embedding_report(
            &tok,
            "concept",
            None,
            "LoRA",
            Some("Low-Rank Adaptation"),
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();

    let fetched = rt.get_entity(&tok, entity.id).await.unwrap();
    assert_eq!(fetched.id, entity.id);
    assert_eq!(fetched.name, "LoRA");
    assert_eq!(fetched.kind, "concept");
    assert_eq!(fetched.description.as_deref(), Some("Low-Rank Adaptation"));
}

#[tokio::test]
async fn entity_create_with_properties_and_tags() {
    let rt = rt();
    let research_tok = rt.authorize(Namespace::parse("research").unwrap()).unwrap();

    let props = serde_json::json!({"domain": "fine-tuning", "type": "technique"});
    let entity = rt
        .create_entity_with_embedding_report(
            &research_tok,
            "concept",
            None,
            "QLoRA",
            Some("Quantized LoRA"),
            Some(props.clone()),
            vec!["fine-tuning".to_string(), "quantization".to_string()],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();

    let fetched = rt.get_entity(&research_tok, entity.id).await.unwrap();
    assert_eq!(fetched.properties, Some(props));
    assert_eq!(fetched.tags, vec!["fine-tuning", "quantization"]);
}

#[tokio::test]
async fn entity_create_with_content_attachment_roundtrip() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let blob_store = std::sync::Arc::new(
        khive_db::stores::blob::FsBlobStore::new(temp.path().to_path_buf(), 0).unwrap(),
    );
    rt.install_blob_store(blob_store.clone())
        .expect("install blob store");
    let content_ref = blob_store.put(b"asset bytes".to_vec()).await.unwrap();

    let entity = rt
        .create_entity_with_attachments(
            &tok,
            "artifact",
            Some("visual_asset"),
            "asset",
            None,
            None,
            vec![],
            vec![NewAttachment {
                role: "content".to_string(),
                content_ref: content_ref.clone(),
                media_type: None,
                size_bytes: Some(11),
            }],
        )
        .await
        .unwrap();

    let fetched = rt.get_entity(&tok, entity.id).await.unwrap();
    assert_eq!(fetched.content_ref.as_deref(), Some(content_ref.as_str()));
}

#[tokio::test]
async fn entity_create_with_content_attachment_rejects_unpublished_blob() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let temp = tempfile::tempdir().unwrap();
    rt.install_blob_store(std::sync::Arc::new(
        khive_db::stores::blob::FsBlobStore::new(temp.path().to_path_buf(), 0).unwrap(),
    ))
    .expect("install blob store");
    let missing = ContentRef::from_digest_bytes(&[7; 32]);

    let error = rt
        .create_entity_with_attachments(
            &tok,
            "artifact",
            Some("visual_asset"),
            "asset",
            None,
            None,
            vec![],
            vec![NewAttachment {
                role: "content".to_string(),
                content_ref: missing,
                media_type: None,
                size_bytes: None,
            }],
        )
        .await
        .expect_err("unpublished ref must fail");

    assert!(error.to_string().contains("requires a published blob"));
    assert!(rt
        .list_entities(&tok, None, None, 10, 0)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn entity_list_by_kind() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    rt.create_entity_with_embedding_report(
        &tok,
        "concept",
        None,
        "FlashAttention",
        None,
        None,
        vec![],
    )
    .await
    .map(|(record, _report)| record)
    .unwrap();
    rt.create_entity_with_embedding_report(&tok, "concept", None, "GQA", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();
    rt.create_entity_with_embedding_report(
        &tok,
        "document",
        None,
        "Attention Is All You Need",
        None,
        None,
        vec![],
    )
    .await
    .map(|(record, _report)| record)
    .unwrap();

    let concepts = rt
        .list_entities(&tok, Some("concept"), None, 50, 0)
        .await
        .unwrap();
    assert_eq!(concepts.len(), 2);
    assert!(concepts.iter().any(|e| e.name == "FlashAttention"));
    assert!(concepts.iter().any(|e| e.name == "GQA"));

    let docs = rt
        .list_entities(&tok, Some("document"), None, 50, 0)
        .await
        .unwrap();
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0].name, "Attention Is All You Need");

    let all = rt.list_entities(&tok, None, None, 50, 0).await.unwrap();
    assert_eq!(all.len(), 3);
}

#[tokio::test]
async fn entity_delete_soft() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    let entity = rt
        .create_entity_with_embedding_report(&tok, "concept", None, "to-delete", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();

    let deleted = rt.delete_entity(&tok, entity.id, false).await.unwrap();
    assert!(deleted);

    // Soft-deleted entity is not found via get_entity
    let fetched = rt.get_entity(&tok, entity.id).await;
    assert!(fetched.is_err());
}

#[tokio::test]
async fn entity_count_by_kind() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    for _ in 0..3 {
        rt.create_entity_with_embedding_report(
            &tok,
            "concept",
            None,
            "concept-X",
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .unwrap();
    }
    for _ in 0..2 {
        rt.create_entity_with_embedding_report(&tok, "document", None, "doc-Y", None, None, vec![])
            .await
            .map(|(record, _report)| record)
            .unwrap();
    }

    let concept_count = rt.count_entities(&tok, Some("concept")).await.unwrap();
    let doc_count = rt.count_entities(&tok, Some("document")).await.unwrap();
    let total = rt.count_entities(&tok, None).await.unwrap();

    assert_eq!(concept_count, 3);
    assert_eq!(doc_count, 2);
    assert_eq!(total, 5);
}
