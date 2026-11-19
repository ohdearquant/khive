/// Issue #2974. Standalone `stream.append` now reaches the memory pack's creation hook, while
/// the approved AddNote route above remains intentionally outside that hook.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn stream_append_refuses_memory_kind_and_names_memory_remember() {
    let rt = make_runtime();
    let registry = make_registry(rt);

    let refusal = registry
        .dispatch(
            "stream.append",
            json!({
                "stream": "acceptance-2974-memory",
                "record": {"content": "the hook must refuse this route"},
                "note_kind": "memory"
            }),
        )
        .await
        .expect_err("stream.append must refuse memory creation");
    assert!(
        refusal.to_string().contains("memory.remember"),
        "the refusal must name the owning writer: {refusal}"
    );
}

/// Bulk `create(items=[...])` reaches the same owner refusal as a singleton create. Under
/// `atomic: false` each memory item is its own failure naming `memory.remember` while an ordinary
/// sibling commits; an atomic batch holding one memory item is refused as a whole.
#[tokio::test]
async fn bulk_create_refuses_a_memory_note_item_in_both_modes() {
    let registry = make_registry(make_runtime());

    let resp = registry
        .dispatch(
            "create",
            json!({
                "atomic": false,
                "items": [
                    {"kind": "observation", "content": "an ordinary sibling in a bulk batch"},
                    {"kind": "memory", "content": "a memory through bulk create"},
                    {"kind": "note", "note_kind": "memory", "content": "a memory through the note spelling"}
                ]
            }),
        )
        .await
        .expect("best-effort bulk create must return per-item results");
    assert_eq!(resp["created"], 1, "{resp}");
    assert_eq!(resp["failed"], 2, "{resp}");
    let results = resp["results"].as_array().expect("results array");
    assert_eq!(results[0]["ok"], true, "{resp}");
    for idx in [1, 2] {
        assert_eq!(results[idx]["ok"], false, "{resp}");
        assert_eq!(
            results[idx]["domain_disposition"], "not_committed",
            "{resp}"
        );
        let message = results[idx]["error"]["message"].as_str().unwrap_or("");
        assert!(
            message.contains("memory.remember"),
            "item {idx} must name the verb that owns the kind: {message}"
        );
    }

    let refusal = registry
        .dispatch(
            "create",
            json!({
                "items": [
                    {"kind": "observation", "content": "an ordinary sibling in an atomic batch"},
                    {"kind": "memory", "content": "a memory in an atomic batch"}
                ]
            }),
        )
        .await
        .expect_err("an atomic batch holding a memory item must be refused");
    assert!(
        refusal.to_string().contains("memory.remember"),
        "the atomic refusal must name the verb that owns the kind: {refusal}"
    );

    let memories = registry
        .dispatch("list", json!({"kind": "memory"}))
        .await
        .expect("listing memories must succeed");
    assert_eq!(
        memories["items"].as_array().map(Vec::len),
        Some(0),
        "no memory row may be written by bulk create: {memories}"
    );
    let observations = registry
        .dispatch("list", json!({"kind": "observation"}))
        .await
        .expect("listing observations must succeed");
    assert_eq!(
        observations["items"].as_array().map(Vec::len),
        Some(1),
        "only the best-effort sibling may be stored: {observations}"
    );
}

/// A memory whose content exceeds the embedder input budget is stored, and the
/// response discloses the truncation instead of failing after the commit.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn remember_over_embedding_budget_succeeds_and_discloses_truncation() {
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: vec![],
        ..RuntimeConfig::default()
    })
    .expect("runtime");
    rt.register_embedder(ConstVecProvider::new("truncation-enc", 4, 0.9));
    let registry = make_registry(rt);

    let content = "x".repeat(lattice_embed::MAX_TEXT_BYTES + 1);
    let result = registry
        .dispatch("memory.remember", json!({ "content": content }))
        .await
        .expect("an over-budget memory must not fail after commit");

    assert_eq!(
        result["warnings"],
        json!([khive_runtime::retrieval::EMBEDDING_INPUT_TRUNCATED_WARNING]),
        "remember must disclose the truncated embedding input: {result}"
    );
    let note_id = result["id"].as_str().expect("note id present");
    let stored = registry
        .dispatch("get", json!({ "id": note_id }))
        .await
        .expect("the committed memory must be readable");
    assert_eq!(stored["id"], json!(note_id));
}
