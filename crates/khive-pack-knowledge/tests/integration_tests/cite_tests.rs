// ── cite verb ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn cite_creates_introduced_by_edge() {
    let f = pack(rt());

    // Create concept via learn.
    let concept = f
        .dispatch(
            "knowledge.learn",
            json!({ "name": "LoRA", "domain": "fine-tuning" }),
        )
        .await
        .expect("learn concept");

    // Create paper as a `document` entity (base allowlist: concept -[introduced_by]-> document).
    let paper = f
        .dispatch(
            "create",
            json!({
                "kind": "document",
                "name": "Hu et al. 2021",
                "description": "LoRA: Low-Rank Adaptation paper — covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering"
            }),
        )
        .await
        .expect("create document");

    let concept_id = concept["full_id"].as_str().unwrap();
    // `create` serialises the raw Entity — id field is the full UUID string.
    let source_id = paper["id"].as_str().unwrap();

    let resp = f
        .dispatch(
            "knowledge.cite",
            json!({
                "concept_id": concept_id,
                "source_id": source_id,
                "weight": 1.0
            }),
        )
        .await
        .expect("cite ok");

    assert_eq!(resp["relation"], "introduced_by");
    assert_eq!(resp["concept_id"], concept_id);
    assert_eq!(resp["source_id"], source_id);
    assert_eq!(resp["weight"], 1.0);
    let id = resp["id"].as_str().expect("id");
    assert_eq!(id.len(), 8, "expected 8-char edge id, got: {id}");
}

#[tokio::test]
async fn cite_accepts_org_source() {
    let f = pack(rt());

    let concept = f
        .dispatch(
            "knowledge.learn",
            json!({ "name": "Hopper Architecture", "domain": "hardware" }),
        )
        .await
        .expect("learn concept");

    // Base allowlist (ADR-002 2026-07-08 amendment): concept -[introduced_by]-> org.
    let org = f
        .dispatch(
            "create",
            json!({
                "kind": "org",
                "name": "Example Hardware Vendor",
                "description": "Org entity originating an architecture concept — covering concepts techniques algorithms implementations applications use cases and design patterns in detail"
            }),
        )
        .await
        .expect("create org");

    let concept_id = concept["full_id"].as_str().unwrap();
    let source_id = org["id"].as_str().unwrap();

    let resp = f
        .dispatch(
            "knowledge.cite",
            json!({ "concept_id": concept_id, "source_id": source_id }),
        )
        .await
        .expect("cite org source ok");

    assert_eq!(resp["relation"], "introduced_by");
    assert_eq!(resp["source_id"], source_id);
}

/// Regression (PR #1623 round 3): the vocab promises unique 8+ hex prefix
/// resolution for `concept_id` and `source_id`. Both must resolve through
/// `resolve_uuid`'s prefix arm and return canonical full UUIDs.
#[tokio::test]
async fn cite_resolves_concept_and_source_by_unique_prefix() {
    let f = pack(rt());

    let concept = f
        .dispatch(
            "knowledge.learn",
            json!({ "name": "QuaRot", "domain": "quantization" }),
        )
        .await
        .expect("learn concept");
    let paper = f
        .dispatch(
            "create",
            json!({
                "kind": "document",
                "name": "Ashkboos et al. 2024",
                "description": "QuaRot paper — covering concepts techniques algorithms implementations applications use cases and design patterns in detail — covering"
            }),
        )
        .await
        .expect("create document");

    let concept_full = concept["full_id"].as_str().unwrap();
    let source_full = paper["id"].as_str().unwrap();
    let concept_prefix = &concept_full[..8];
    let source_prefix = &source_full[..8];

    let resp = f
        .dispatch(
            "knowledge.cite",
            json!({
                "concept_id": concept_prefix,
                "source_id": source_prefix,
                "weight": 0.7
            }),
        )
        .await
        .expect("prefixes must resolve on both cite parameters");

    assert_eq!(resp["concept_id"], concept_full);
    assert_eq!(resp["source_id"], source_full);
}

#[tokio::test]
async fn cite_rejects_unknown_id() {
    let f = pack(rt());
    let err = f
        .dispatch(
            "knowledge.cite",
            json!({
                "concept_id": "00000000-0000-0000-0000-000000000001",
                "source_id":  "00000000-0000-0000-0000-000000000002"
            }),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(!msg.is_empty(), "expected error for unknown IDs, got empty");
}

#[tokio::test]
async fn cite_rejects_missing_concept_id() {
    let f = pack(rt());
    let err = f
        .dispatch(
            "knowledge.cite",
            json!({ "source_id": "00000000-0000-0000-0000-000000000001" }),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(!msg.is_empty(), "expected deserialization error");
}
