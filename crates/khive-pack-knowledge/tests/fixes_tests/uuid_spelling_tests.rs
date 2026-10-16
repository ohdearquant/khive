// ── #3728: any spelling `Uuid` accepts names the atom in edit / challenge / adjudicate ──

const SPELLING_SECTION_TYPES: [&str; 4] = ["overview", "core_model", "formalism", "examples"];

/// Spellings `Uuid` parses that differ from the stored lowercase hyphenated text.
fn non_canonical_spellings(id: &str) -> [String; 4] {
    [
        id.to_uppercase(),
        id.replace('-', ""),
        format!("{{{id}}}"),
        format!("urn:uuid:{id}"),
    ]
}

/// Upsert an atom, give it one section per `SPELLING_SECTION_TYPES` entry, and
/// return the atom id exactly as stored.
async fn seed_atom_with_sections(f: &Fixture, slug: &str) -> String {
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": slug, "name": slug, "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert");
    let sections: Vec<Value> = SPELLING_SECTION_TYPES
        .iter()
        .map(|section_type| {
            json!({
                "section_type": section_type,
                "content": format!("Section {section_type} for the id spelling test, long enough to satisfy the 80-character minimum section length requirement."),
            })
        })
        .collect();
    f.dispatch(
        "knowledge.edit",
        json!({ "id": slug, "sections": sections }),
    )
    .await
    .expect("seed sections");
    let row = f
        .sql_query_one(
            "SELECT id FROM knowledge_atoms WHERE slug = ?1",
            vec![SqlValue::Text(slug.into())],
        )
        .await
        .expect("atom row");
    row_text(&row, "id").expect("atom id")
}

#[tokio::test]
async fn non_canonical_uuid_spellings_resolve_in_edit() {
    let f = pack(rt());
    let id = seed_atom_with_sections(&f, "spell-edit").await;

    for spelling in non_canonical_spellings(&id) {
        let res = f
            .dispatch(
                "knowledge.edit",
                json!({ "id": spelling, "sections": [{
                    "section_type": "overview",
                    "content": "Edited overview reached through a non-canonical id spelling, long enough to satisfy the 80-character minimum."
                }] }),
            )
            .await
            .unwrap_or_else(|e| panic!("edit by id spelled {spelling:?} must resolve: {e}"));
        assert_eq!(
            res["atom_id"].as_str(),
            Some(id.as_str()),
            "edit by {spelling:?} must act on the stored atom"
        );
    }
}

#[tokio::test]
async fn non_canonical_uuid_spellings_resolve_in_challenge() {
    let f = pack(rt());
    let id = seed_atom_with_sections(&f, "spell-challenge").await;

    for (spelling, section_type) in non_canonical_spellings(&id)
        .iter()
        .zip(SPELLING_SECTION_TYPES)
    {
        let res = f
            .dispatch(
                "knowledge.challenge",
                json!({ "atom_id": spelling, "section_type": section_type }),
            )
            .await
            .unwrap_or_else(|e| panic!("challenge by id spelled {spelling:?} must resolve: {e}"));
        assert_eq!(
            res["atom_id"].as_str(),
            Some(id.as_str()),
            "challenge by {spelling:?} must act on the stored atom"
        );
        assert_eq!(res["disputed"].as_i64(), Some(1));
    }

    let atom = f
        .dispatch("knowledge.get", json!({ "id": id }))
        .await
        .expect("get");
    assert_eq!(
        atom["properties"]["dispute_count"].as_i64(),
        Some(4),
        "each spelled challenge must land on the stored atom row"
    );
}

#[tokio::test]
async fn non_canonical_uuid_spellings_resolve_in_adjudicate() {
    let f = pack(rt());
    let id = seed_atom_with_sections(&f, "spell-adjudicate").await;
    for section_type in SPELLING_SECTION_TYPES {
        f.dispatch(
            "knowledge.challenge",
            json!({ "atom_id": id, "section_type": section_type }),
        )
        .await
        .expect("challenge by stored id");
    }

    for (spelling, section_type) in non_canonical_spellings(&id)
        .iter()
        .zip(SPELLING_SECTION_TYPES)
    {
        let res = f
            .dispatch(
                "knowledge.adjudicate",
                json!({ "atom_id": spelling, "section_type": section_type, "resolution": "accept" }),
            )
            .await
            .unwrap_or_else(|e| panic!("adjudicate by id spelled {spelling:?} must resolve: {e}"));
        assert_eq!(
            res["atom_id"].as_str(),
            Some(id.as_str()),
            "adjudicate by {spelling:?} must act on the stored atom"
        );
        assert_eq!(res["resolved"].as_i64(), Some(1));
    }

    let atom = f
        .dispatch("knowledge.get", json!({ "id": id }))
        .await
        .expect("get");
    assert_eq!(
        atom["properties"]["dispute_count"].as_i64(),
        Some(0),
        "each spelled adjudication must land on the stored atom row"
    );
}

#[tokio::test]
async fn absent_atom_id_is_not_found_in_edit_challenge_and_adjudicate() {
    let f = pack(rt());
    seed_atom_with_sections(&f, "spell-absent").await;

    let absent = "00000000-0000-4000-8000-000000000000";
    for reference in [
        absent.to_string(),
        absent.to_uppercase(),
        absent.replace('-', ""),
        "no-such-slug".to_string(),
    ] {
        let edit = f
            .dispatch(
                "knowledge.edit",
                json!({ "id": reference, "sections": [{
                    "section_type": "overview",
                    "content": "Overview for an atom that does not exist, long enough to satisfy the 80-character minimum."
                }] }),
            )
            .await;
        assert!(
            matches!(edit, Err(RuntimeError::NotFound(_))),
            "edit of absent {reference:?} must be NotFound, got: {edit:?}"
        );
        let challenge = f
            .dispatch(
                "knowledge.challenge",
                json!({ "atom_id": reference, "section_type": "overview" }),
            )
            .await;
        assert!(
            matches!(challenge, Err(RuntimeError::NotFound(_))),
            "challenge of absent {reference:?} must be NotFound, got: {challenge:?}"
        );
        let adjudicate = f
            .dispatch(
                "knowledge.adjudicate",
                json!({ "atom_id": reference, "section_type": "overview", "resolution": "accept" }),
            )
            .await;
        assert!(
            matches!(adjudicate, Err(RuntimeError::NotFound(_))),
            "adjudicate of absent {reference:?} must be NotFound, got: {adjudicate:?}"
        );
    }
}
