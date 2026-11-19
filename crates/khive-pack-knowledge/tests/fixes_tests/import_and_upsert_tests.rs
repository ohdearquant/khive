// ── W10: import populates source_uri / source_type ────────────────────────────

#[tokio::test]
async fn w10_import_with_atlas_id_sets_source_uri() {
    let f = pack(rt());
    let dir = std::env::temp_dir().join("khive_fixes_test_w10a");
    std::fs::create_dir_all(&dir).ok();
    let md_path = dir.join("atlas-doc.md");
    std::fs::write(
        &md_path,
        "atlas_id: ATLAS-001\n\n# Atlas Doc\n\nContent about retrieval covering dense sparse vector search ranking fusion embedding reranking latency gradient transformer attention nearest neighbor index corpus benchmark pipeline cosine.\n",
    )
    .expect("write md");

    let resp = f
        .dispatch(
            "knowledge.import",
            json!({ "path": md_path.to_str().unwrap() }),
        )
        .await
        .expect("import ok");
    assert!(
        resp["imported_atoms"].as_i64().unwrap_or(0) > 0,
        "expected at least 1 imported atom"
    );

    let atom = f
        .dispatch("knowledge.get", json!({ "id": "atlas-doc" }))
        .await
        .expect("get");
    let source_uri = atom["source_uri"].as_str().unwrap_or("");
    assert_eq!(
        source_uri, "atlas:ATLAS-001",
        "import with atlas_id must set source_uri to 'atlas:{{id}}'"
    );
}

#[tokio::test]
async fn w10_import_with_references_section_refuses_it_and_sets_source_type_imported() {
    let f = pack(rt());
    let dir = std::env::temp_dir().join("khive_fixes_test_w10b");
    std::fs::create_dir_all(&dir).ok();
    let md_path = dir.join("paper-doc.md");
    std::fs::write(
        &md_path,
        "# Paper Doc\n\nContent about machine learning covering dense sparse vector search ranking fusion embedding reranking latency gradient transformer attention nearest neighbor index corpus benchmark pipeline cosine.\n\n## References\n\n1. Smith et al. 2023\n2. Jones et al. 2022\n",
    )
    .expect("write md");

    let resp = f
        .dispatch(
            "knowledge.import",
            json!({ "path": md_path.to_str().unwrap() }),
        )
        .await
        .expect("import ok");
    assert!(
        resp["imported_atoms"].as_i64().unwrap_or(0) > 0,
        "expected at least 1 imported"
    );
    // `references` is retired: the section is refused and counted, and sets no source type.
    assert_eq!(resp["sections_unknown_type"], 1);
    assert_eq!(resp["imported_sections"], 0);
    let atom = f
        .dispatch("knowledge.get", json!({ "id": "paper-doc" }))
        .await
        .expect("get");
    let source_type = atom["source_type"].as_str().unwrap_or("");
    assert_eq!(source_type, "imported");
}

#[tokio::test]
async fn w10_import_without_references_sets_source_type_imported() {
    let f = pack(rt());
    let dir = std::env::temp_dir().join("khive_fixes_test_w10c");
    std::fs::create_dir_all(&dir).ok();
    let md_path = dir.join("plain-doc.md");
    std::fs::write(
        &md_path,
        "# Plain Doc\n\nContent without any references section covering dense sparse vector search ranking fusion embedding reranking latency gradient transformer attention nearest neighbor index corpus benchmark pipeline cosine.\n",
    )
    .expect("write md");

    let resp = f
        .dispatch(
            "knowledge.import",
            json!({ "path": md_path.to_str().unwrap() }),
        )
        .await
        .expect("import ok");
    assert!(
        resp["imported_atoms"].as_i64().unwrap_or(0) > 0,
        "expected at least 1 imported"
    );

    let atom = f
        .dispatch("knowledge.get", json!({ "id": "plain-doc" }))
        .await
        .expect("get");
    let source_type = atom["source_type"].as_str().unwrap_or("");
    assert_eq!(
        source_type, "imported",
        "import without references must set source_type='imported'"
    );
}

#[tokio::test]
async fn w10_import_section_only_markdown_synthesizes_atom_content() {
    let f = pack(rt());
    let dir = std::env::temp_dir().join("khive_fixes_test_w10_section_only");
    std::fs::create_dir_all(&dir).ok();
    let md_path = dir.join("section-only.md");
    // All useful text lives under `##` sections; the pre-section body is empty.
    std::fs::write(
        &md_path,
        "# Section Only\n\n## Overview\n\nThis overview section is long enough to satisfy the eighty character minimum section length requirement, covering dense sparse retrieval corpus benchmark search latency.\n\n## Formalism\n\nThe formalism section also exceeds eighty characters with gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity.\n",
    )
    .expect("write md");

    let resp = f
        .dispatch(
            "knowledge.import",
            json!({ "path": md_path.to_str().unwrap(), "chunk_strategy": "section" }),
        )
        .await
        .expect("section-only import should succeed");
    assert_eq!(
        resp["imported_atoms"].as_i64().unwrap_or(0),
        1,
        "atom must be imported even though the pre-section body is empty"
    );
    assert!(
        resp["imported_sections"].as_i64().unwrap_or(0) >= 2,
        "section bodies must be imported"
    );

    // Atom content is synthesized from the section bodies (>= 20 words).
    let atom = f
        .dispatch("knowledge.get", json!({ "id": "section-only" }))
        .await
        .expect("get");
    let content = atom["content"].as_str().unwrap_or("");
    assert!(
        content.split_whitespace().count() >= 20,
        "atom content should be synthesized from sections: {content:?}"
    );
}

// ── issue #25: import must finalize atoms so search/suggest can retrieve them ──

#[tokio::test]
async fn issue25_imported_atom_is_finalized_and_retrievable_via_search() {
    let f = pack(rt());
    let dir = std::env::temp_dir().join("khive_fixes_test_issue25");
    std::fs::create_dir_all(&dir).ok();
    let md_path = dir.join("retrievable-doc.md");
    std::fs::write(
        &md_path,
        "# Retrievable Doc\n\nContent about zephyroquine dosage covering dense sparse vector search ranking fusion embedding reranking latency gradient transformer attention nearest neighbor index corpus benchmark pipeline cosine.\n",
    )
    .expect("write md");

    let resp = f
        .dispatch(
            "knowledge.import",
            json!({ "path": md_path.to_str().unwrap() }),
        )
        .await
        .expect("import ok");
    assert!(
        resp["imported_atoms"].as_i64().unwrap_or(0) > 0,
        "expected at least 1 imported atom"
    );

    let atom = f
        .dispatch("knowledge.get", json!({ "id": "retrievable-doc" }))
        .await
        .expect("get");
    assert_eq!(
        atom["status"].as_str(),
        Some("reviewed"),
        "imported atoms must be finalized (status=reviewed), not left at the draft default \
         that knowledge.search/suggest exclude"
    );

    // The regression this fix closes: default knowledge.search (no include_drafts, no
    // status=) must actually surface a freshly-imported atom by its unique content term.
    let search_resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "zephyroquine dosage", "rerank": false }),
        )
        .await
        .expect("search ok");
    let results = search_resp["results"].as_array().expect("results array");
    assert!(
        results.iter().any(|r| r["id"] == atom["id"]),
        "imported atom must be retrievable via default knowledge.search, got: {search_resp}"
    );
}

// ── S4: upsert-on-duplicate-slug updates in place (ADR-007 Rev 2: single local ns) ──

#[tokio::test]
async fn s4_upsert_atoms_update_on_duplicate_slug() {
    let f = pack(rt());

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "s4-atom", "name": "Original", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("initial upsert");

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "s4-atom", "name": "Updated", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("update upsert");

    let atom = f
        .dispatch("knowledge.get", json!({ "id": "s4-atom" }))
        .await
        .expect("get atom");
    assert_eq!(
        atom["name"].as_str().unwrap_or(""),
        "Updated",
        "upsert on duplicate slug must update the name"
    );
}

#[tokio::test]
async fn s4_upsert_domains_update_on_duplicate_slug() {
    let f = pack(rt());

    f.dispatch(
        "knowledge.upsert_domains",
        json!({ "domains": [{ "slug": "s4-domain", "name": "Original Domain", "description": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("initial domain upsert");

    f.dispatch(
        "knowledge.upsert_domains",
        json!({ "domains": [{ "slug": "s4-domain", "name": "Updated Domain", "description": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("update domain upsert");

    let domain = f
        .dispatch("knowledge.get", json!({ "id": "s4-domain" }))
        .await
        .expect("get domain");
    assert_eq!(
        domain["name"].as_str().unwrap_or(""),
        "Updated Domain",
        "upsert on duplicate slug must update the domain name"
    );
}
