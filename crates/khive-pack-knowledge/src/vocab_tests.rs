use super::*;

fn find_handler(name: &str) -> &'static HandlerDef {
    KNOWLEDGE_HANDLERS
        .iter()
        .find(|h| h.name == name)
        .unwrap_or_else(|| panic!("handler {name:?} not found in KNOWLEDGE_HANDLERS"))
}

/// knowledge.edit sections param must document the closed section_type enum (8 values) and
/// the 80-character content minimum (#160).
#[test]
fn knowledge_edit_sections_documents_enum_and_content_minimum() {
    let h = find_handler("knowledge.edit");
    let sections = h
        .params
        .iter()
        .find(|p| p.name == "sections")
        .expect("knowledge.edit must have a sections param");
    assert!(
        sections.description.contains("80"),
        "knowledge.edit sections description must document the 80-character content minimum"
    );
    // Every member of the closed enum is listed, and the retired names are not.
    for name in khive_brain_core::SectionType::NAMES {
        assert!(
            sections.description.contains(name),
            "knowledge.edit sections description must list '{name}' as a valid section_type"
        );
    }
    for retired in khive_brain_core::SectionType::RETIRED_NAMES {
        assert!(
            !sections.description.contains(retired),
            "knowledge.edit sections description must not list the retired '{retired}'"
        );
    }
    // Verify the description calls out the closed-enum nature so callers know unrecognized
    // values are rejected (not silently coerced).
    assert!(
        sections.description.contains("closed enum"),
        "knowledge.edit sections description must state that section_type is a closed enum"
    );
}

/// Parameter-parity guard (cross-model tooling evaluation, "compose budget
/// discoverability"): the top-level field names each handler's `#[derive(Deserialize)]`
/// params struct actually accepts, hand-kept in sync with
/// `src/knowledge/schema.rs` and `src/handlers.rs`. Every verb's entry here must be
/// exactly the set of top-level param names advertised in `KNOWLEDGE_HANDLERS` for
/// that verb (order-independent) -- so a field added to a params struct without a
/// matching `ParamDef` (or a stale `ParamDef` for a removed field) fails this test
/// instead of silently drifting from the actual accepted request shape.
///
/// Nested/child structs (e.g. `AtomInput` within `upsert_atoms.atoms`,
/// `FoldCandidate` within `fold.candidates`) are documented in prose inside the
/// parent `ParamDef.description` rather than as separate top-level entries here.
fn expected_params() -> &'static [(&'static str, &'static [&'static str])] {
    &[
        (
            "knowledge.upsert_atoms",
            &["atoms", "chunk_size", "dry_run"],
        ),
        ("knowledge.upsert_domains", &["domains"]),
        ("knowledge.get", &["id", "include_sections"]),
        ("knowledge.export", &["format"]),
        ("knowledge.lint", &["rules", "fix", "severity"]),
        (
            "knowledge.list",
            &[
                "type",
                "limit",
                "offset",
                "after",
                "fields",
                "status",
                "exclude_status",
            ],
        ),
        ("knowledge.delete_atoms", &["ids", "cascade"]),
        ("knowledge.stats", &[]),
        ("knowledge.eval_retrieval", &["query_set"]),
        (
            "knowledge.index",
            &["ids", "batch_size", "insert_only", "rebuild_ann"],
        ),
        (
            "knowledge.fold",
            &[
                "candidates",
                "budget",
                "min_score",
                "category_weights",
                "diversity_bias",
                "epistemic_weight",
            ],
        ),
        (
            "knowledge.search",
            &[
                "query",
                "type",
                "include_drafts",
                "status",
                "exclude_status",
                "role",
                "limit",
                "min_score",
                "weights",
                "decompose",
                "decompose_threshold",
                "intersection_bonus",
                "rerank",
                "rerank_alpha",
            ],
        ),
        ("knowledge.suggest", &["query", "role", "limit"]),
        (
            "knowledge.compose",
            &[
                "namespace",
                "domain_ids",
                "atom_ids",
                "query",
                "blend_kg",
                "auto_limit",
                "max_tokens",
                "explain",
            ],
        ),
        ("knowledge.edit", &["id", "sections"]),
        ("knowledge.import", &["path", "format", "chunk_strategy"]),
        (
            "knowledge.challenge",
            &["atom_id", "section_type", "content_hash", "reason"],
        ),
        (
            "knowledge.adjudicate",
            &["atom_id", "section_type", "content_hash", "resolution"],
        ),
        (
            "knowledge.learn",
            &["name", "description", "domain", "tags"],
        ),
        ("knowledge.cite", &["concept_id", "source_id", "weight"]),
        ("knowledge.topic", &["domain", "query", "limit"]),
        (
            "knowledge.feedback",
            &[
                "section_signals",
                "target_id",
                "signal",
                "served_by_profile_id",
            ],
        ),
    ]
}

#[test]
fn advertised_params_match_expected_deserialized_fields_for_every_verb() {
    for (verb, expected) in expected_params() {
        let h = find_handler(verb);
        let mut advertised: Vec<&str> = h.params.iter().map(|p| p.name).collect();
        let mut expected: Vec<&str> = expected.to_vec();
        advertised.sort_unstable();
        expected.sort_unstable();
        assert_eq!(
            advertised, expected,
            "{verb}: advertised params {advertised:?} do not match the params \
                 struct's deserialized fields {expected:?} -- update either the \
                 `ParamDef`s in KNOWLEDGE_HANDLERS or the `expected_params()` table \
                 in this test (whichever is stale)"
        );
    }
}

/// Every verb declared in `KNOWLEDGE_HANDLERS` must have a parity row above --
/// otherwise a newly added verb could silently escape this guard.
#[test]
fn every_handler_has_a_parity_row() {
    let covered: std::collections::HashSet<&str> =
        expected_params().iter().map(|(name, _)| *name).collect();
    for h in KNOWLEDGE_HANDLERS.iter() {
        assert!(
            covered.contains(h.name),
            "{}: no entry in expected_params() -- add one so parameter drift \
                 on this verb cannot silently return",
            h.name
        );
    }
}
