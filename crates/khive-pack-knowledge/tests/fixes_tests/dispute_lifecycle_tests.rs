// ── W9: challenge increments dispute_count / adjudicate decrements ─────────────

#[tokio::test]
async fn w9_challenge_increments_dispute_count() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "challenge-atom", "name": "Challengeable Atom", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert");

    // Create a section via edit (required for challenge section status update).
    f.dispatch(
        "knowledge.edit",
        json!({
            "id": "challenge-atom",
            "sections": [{ "section_type": "overview", "content": "Section content for challenge test — this text is sufficiently long to satisfy the 80-character minimum. dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }]
        }),
    )
    .await
    .expect("edit ok");

    f.dispatch(
        "knowledge.challenge",
        json!({ "atom_id": "challenge-atom", "section_type": "overview", "reason": "disputed claim" }),
    )
    .await
    .expect("challenge ok");

    let atom = f
        .dispatch("knowledge.get", json!({ "id": "challenge-atom" }))
        .await
        .expect("get ok");
    let dispute_count = atom["properties"]["dispute_count"]
        .as_i64()
        .expect("dispute_count should be integer");
    assert_eq!(
        dispute_count, 1,
        "challenge must increment dispute_count to 1"
    );
}

#[tokio::test]
async fn w9_challenge_on_atom_with_no_prior_dispute_count_starts_at_one() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "fresh-atom", "name": "Fresh Atom", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert");
    f.dispatch(
        "knowledge.edit",
        json!({
            "id": "fresh-atom",
            "sections": [{ "section_type": "formalism", "content": "Formalism content for fresh-atom challenge test — this text satisfies the 80-character minimum length requirement. dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }]
        }),
    )
    .await
    .expect("edit");

    f.dispatch(
        "knowledge.challenge",
        json!({ "atom_id": "fresh-atom", "section_type": "formalism" }),
    )
    .await
    .expect("challenge ok");

    let atom = f
        .dispatch("knowledge.get", json!({ "id": "fresh-atom" }))
        .await
        .expect("get ok");
    let count = atom["properties"]["dispute_count"]
        .as_i64()
        .expect("dispute_count");
    assert_eq!(
        count, 1,
        "first challenge on atom with no prior dispute_count must start at 1"
    );
}

#[tokio::test]
async fn w9_adjudicate_decrements_dispute_count() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "adjud-atom", "name": "Adjudicate Atom", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert");
    f.dispatch(
        "knowledge.edit",
        json!({
            "id": "adjud-atom",
            "sections": [{ "section_type": "core_model", "content": "Core model content for adjudication test — this text satisfies the 80-character minimum length requirement. dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }]
        }),
    )
    .await
    .expect("edit");

    f.dispatch(
        "knowledge.challenge",
        json!({ "atom_id": "adjud-atom", "section_type": "core_model" }),
    )
    .await
    .expect("challenge");

    // Verify dispute_count = 1 before adjudicate.
    let before = f
        .dispatch("knowledge.get", json!({ "id": "adjud-atom" }))
        .await
        .expect("get");
    assert_eq!(before["properties"]["dispute_count"].as_i64(), Some(1));

    f.dispatch(
        "knowledge.adjudicate",
        json!({ "atom_id": "adjud-atom", "section_type": "core_model", "resolution": "accept" }),
    )
    .await
    .expect("adjudicate ok");

    let after = f
        .dispatch("knowledge.get", json!({ "id": "adjud-atom" }))
        .await
        .expect("get");
    let after_count = after["properties"]["dispute_count"].as_i64().unwrap_or(0);
    assert_eq!(
        after_count, 0,
        "adjudicate must decrement dispute_count from 1 to 0"
    );
}

// ── W9 edge cases: challenge/adjudicate lifecycle guards ──────────────────────

#[tokio::test]
async fn w9_double_challenge_is_rejected() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "dbl-chal", "name": "Double Challenge", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert");
    f.dispatch(
        "knowledge.edit",
        json!({ "id": "dbl-chal", "sections": [{ "section_type": "overview", "content": "Some content for double-challenge test — this text is sufficiently long to satisfy the 80-character minimum. dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("edit");

    f.dispatch(
        "knowledge.challenge",
        json!({ "atom_id": "dbl-chal", "section_type": "overview" }),
    )
    .await
    .expect("first challenge ok");

    let err = f
        .dispatch(
            "knowledge.challenge",
            json!({ "atom_id": "dbl-chal", "section_type": "overview" }),
        )
        .await;
    assert!(err.is_err(), "double challenge must fail");
}

#[tokio::test]
async fn w9_challenge_missing_section_is_rejected() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "no-sec", "name": "No Section", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert");

    let err = f
        .dispatch(
            "knowledge.challenge",
            json!({ "atom_id": "no-sec", "section_type": "overview" }),
        )
        .await;
    assert!(err.is_err(), "challenge on nonexistent section must fail");
}

#[tokio::test]
async fn w9_adjudicate_non_disputed_section_is_rejected() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "adj-nodis", "name": "Not Disputed", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert");
    f.dispatch(
        "knowledge.edit",
        json!({ "id": "adj-nodis", "sections": [{ "section_type": "overview", "content": "Content for adjudicate-non-disputed test — this text is long enough to satisfy the 80-character minimum requirement. dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("edit");

    let err = f
        .dispatch(
            "knowledge.adjudicate",
            json!({ "atom_id": "adj-nodis", "section_type": "overview", "resolution": "accept" }),
        )
        .await;
    assert!(err.is_err(), "adjudicate on non-disputed section must fail");
}

#[tokio::test]
async fn w9_challenge_disambiguates_same_type_siblings() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "sib-atom", "name": "Sibling Atom", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert");

    // Two distinct-content overview sections are valid siblings under
    // UNIQUE(atom_id, content_hash), so section_type alone no longer targets one.
    let edit = f
        .dispatch(
            "knowledge.edit",
            json!({ "id": "sib-atom", "sections": [
                { "section_type": "overview", "content": "First overview variant — long enough to clear the 80-character minimum. dense sparse retrieval corpus benchmark search latency gradient transformer attention vector index" },
                { "section_type": "overview", "content": "Second overview variant — also long enough to clear the 80-character minimum. ranking fusion pipeline embedding rerank cosine similarity nearest neighbor corpus benchmark" }
            ] }),
        )
        .await
        .expect("edit two siblings");
    let sections = edit["sections"].as_array().expect("sections array");
    assert_eq!(sections.len(), 2, "two distinct overviews must be siblings");
    let hash0 = sections[0]["content_hash"]
        .as_str()
        .expect("content_hash")
        .to_string();

    // Without a disambiguator the challenge is ambiguous and must be rejected.
    let ambiguous = f
        .dispatch(
            "knowledge.challenge",
            json!({ "atom_id": "sib-atom", "section_type": "overview" }),
        )
        .await;
    assert!(
        ambiguous.is_err(),
        "ambiguous same-type challenge without content_hash must be rejected"
    );

    // Targeting by content_hash disputes exactly one section.
    let res = f
        .dispatch(
            "knowledge.challenge",
            json!({ "atom_id": "sib-atom", "section_type": "overview", "content_hash": hash0 }),
        )
        .await
        .expect("targeted challenge ok");
    assert_eq!(
        res["disputed"].as_i64(),
        Some(1),
        "exactly one section disputed"
    );
    let atom = f
        .dispatch("knowledge.get", json!({ "id": "sib-atom" }))
        .await
        .expect("get");
    assert_eq!(
        atom["properties"]["dispute_count"].as_i64(),
        Some(1),
        "dispute_count increments once, not once per sibling"
    );

    // The other sibling is still the only eligible overview now, so an un-hashed
    // challenge resolves it and the counter advances to 2.
    let res2 = f
        .dispatch(
            "knowledge.challenge",
            json!({ "atom_id": "sib-atom", "section_type": "overview" }),
        )
        .await
        .expect("second sibling is independently challengeable");
    assert_eq!(res2["disputed"].as_i64(), Some(1));
    let atom2 = f
        .dispatch("knowledge.get", json!({ "id": "sib-atom" }))
        .await
        .expect("get2");
    assert_eq!(
        atom2["properties"]["dispute_count"].as_i64(),
        Some(2),
        "each sibling disputes independently"
    );
}
