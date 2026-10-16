// =============================================================================
// Note (memory) operations
// =============================================================================

#[tokio::test]
async fn create_note_and_list_notes() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();

    rt.create_note(
        &tok,
        "observation",
        None,
        "LoRA is a fine-tuning technique",
        Some(0.9),
        None,
        vec![],
    )
    .await
    .unwrap();
    rt.create_note(
        &tok,
        "observation",
        None,
        "QLoRA uses quantization",
        Some(0.8),
        None,
        vec![],
    )
    .await
    .unwrap();
    rt.create_note(
        &tok,
        "question",
        None,
        "Review LoRA paper",
        Some(0.7),
        None,
        vec![],
    )
    .await
    .unwrap();

    let observations = rt
        .list_notes(&tok, Some("observation"), 50, 0)
        .await
        .unwrap();
    assert_eq!(observations.len(), 2);

    let questions = rt.list_notes(&tok, Some("question"), 50, 0).await.unwrap();
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].content, "Review LoRA paper");

    let all = rt.list_notes(&tok, None, 50, 0).await.unwrap();
    assert_eq!(all.len(), 3);
}

#[tokio::test]
async fn create_all_note_kinds() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    for kind in [
        "observation",
        "insight",
        "question",
        "decision",
        "reference",
        "head",
    ] {
        let content = if kind == "head" { "{}" } else { "content" };
        rt.create_note(&tok, kind, None, content, Some(0.5), None, vec![])
            .await
            .unwrap();
    }
    let all = rt.list_notes(&tok, None, 50, 0).await.unwrap();
    assert_eq!(all.len(), 6);
}
