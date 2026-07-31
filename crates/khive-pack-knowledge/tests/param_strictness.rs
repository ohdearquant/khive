// FILE SIZE JUSTIFICATION: covers issue #89 (deny_unknown_fields audit across every verb in
// the pack) and issue #105 (suggest -> fold -> compose wireability) in one file. Both fixes
// touch the same params-deserialization surface; splitting them would duplicate the same
// runtime/fixture boilerplate for no benefit.

//! Regression coverage for:
//! - #89: every `knowledge.*` verb rejects unknown params instead of silently ignoring them.
//! - #105: `knowledge.suggest` output feeds `knowledge.fold`'s `candidates` unmodified, and
//!   `fold`'s `selected` output feeds `knowledge.compose`'s `domain_ids` unmodified.

use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{KhiveRuntime, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use serde_json::{json, Value};

fn rt() -> KhiveRuntime {
    KhiveRuntime::memory().expect("memory runtime")
}

struct Fixture {
    registry: VerbRegistry,
}

impl Fixture {
    async fn dispatch(&self, verb: &str, args: Value) -> Result<Value, RuntimeError> {
        self.registry.dispatch(verb, args).await
    }
}

fn pack(rt: KhiveRuntime) -> Fixture {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(KnowledgePack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");
    rt.install_edge_rules(registry.all_edge_rules());
    Fixture { registry }
}

/// Assert that dispatching `verb` with `params` (which carries a bogus
/// `unknown_field_xyz` key alongside otherwise-valid required fields) fails,
/// and that the error names the bad field or says "unknown field" — i.e. it
/// enumerates rather than silently swallows.
async fn assert_rejects_unknown_field(f: &Fixture, verb: &str, mut params: Value) {
    params
        .as_object_mut()
        .expect("params must be an object")
        .insert("unknown_field_xyz".into(), json!("should cause rejection"));
    let result = f.dispatch(verb, params).await;
    let err = match result {
        Err(e) => e,
        Ok(v) => panic!("{verb}: expected rejection of unknown field, got Ok({v:?})"),
    };
    let msg = err.to_string();
    assert!(
        msg.contains("unknown_field_xyz") || msg.contains("unknown field"),
        "{verb}: unknown field must be rejected; got: {msg}"
    );
}

// ── #89: unknown-field rejection, one test per verb ─────────────────────────

#[tokio::test]
async fn upsert_atoms_rejects_unknown_top_level_field() {
    let f = pack(rt());
    assert_rejects_unknown_field(
        &f,
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "a", "name": "A", "content": "x".repeat(200) }] }),
    )
    .await;
}

#[tokio::test]
async fn upsert_atoms_rejects_unknown_atom_input_field() {
    let f = pack(rt());
    let err = f
        .dispatch(
            "knowledge.upsert_atoms",
            json!({ "atoms": [{
                "slug": "a", "name": "A", "content": "x".repeat(200),
                "unknown_field_xyz": "boom"
            }] }),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("unknown_field_xyz") || msg.contains("unknown field"),
        "AtomInput: unknown field must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn upsert_domains_rejects_unknown_top_level_field() {
    let f = pack(rt());
    assert_rejects_unknown_field(
        &f,
        "knowledge.upsert_domains",
        json!({ "domains": [{
            "slug": "d", "name": "D",
            "description": "A domain with enough words to pass the twenty word minimum content requirement for testing."
        }] }),
    )
    .await;
}

#[tokio::test]
async fn get_rejects_unknown_field() {
    let f = pack(rt());
    assert_rejects_unknown_field(&f, "knowledge.get", json!({ "id": "nonexistent" })).await;
}

#[tokio::test]
async fn list_rejects_unknown_field() {
    let f = pack(rt());
    assert_rejects_unknown_field(&f, "knowledge.list", json!({})).await;
}

#[tokio::test]
async fn delete_atoms_rejects_unknown_field() {
    let f = pack(rt());
    assert_rejects_unknown_field(
        &f,
        "knowledge.delete_atoms",
        json!({ "ids": ["nonexistent"] }),
    )
    .await;
}

#[tokio::test]
async fn stats_rejects_unknown_field() {
    let f = pack(rt());
    assert_rejects_unknown_field(&f, "knowledge.stats", json!({})).await;
}

#[tokio::test]
async fn index_rejects_unknown_field() {
    let f = pack(rt());
    assert_rejects_unknown_field(&f, "knowledge.index", json!({})).await;
}

#[tokio::test]
async fn fold_rejects_unknown_top_level_field() {
    let f = pack(rt());
    assert_rejects_unknown_field(
        &f,
        "knowledge.fold",
        json!({ "candidates": [{ "id": "a", "score": 1.0, "size": 10 }], "budget": 100 }),
    )
    .await;
}

#[tokio::test]
async fn fold_rejects_unknown_candidate_field() {
    let f = pack(rt());
    let err = f
        .dispatch(
            "knowledge.fold",
            json!({
                "candidates": [{
                    "id": "a", "score": 1.0, "size": 10,
                    "unknown_field_xyz": "boom"
                }],
                "budget": 100
            }),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("unknown_field_xyz") || msg.contains("unknown field"),
        "FoldCandidate: unknown field must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn search_rejects_unknown_field() {
    let f = pack(rt());
    assert_rejects_unknown_field(
        &f,
        "knowledge.search",
        json!({ "query": "vector database" }),
    )
    .await;
}

#[tokio::test]
async fn suggest_rejects_unknown_field() {
    let f = pack(rt());
    assert_rejects_unknown_field(
        &f,
        "knowledge.suggest",
        json!({ "query": "vector database retrieval systems" }),
    )
    .await;
}

#[tokio::test]
async fn compose_rejects_unknown_field() {
    let f = pack(rt());
    assert_rejects_unknown_field(
        &f,
        "knowledge.compose",
        json!({ "query": "vector database retrieval systems", "atom_ids": ["nonexistent"] }),
    )
    .await;
}

#[tokio::test]
async fn edit_rejects_unknown_top_level_field() {
    let f = pack(rt());
    assert_rejects_unknown_field(
        &f,
        "knowledge.edit",
        json!({ "id": "nonexistent", "sections": [{ "section_type": "overview", "content": "x" }] }),
    )
    .await;
}

#[tokio::test]
async fn edit_rejects_unknown_section_update_field() {
    let f = pack(rt());
    let err = f
        .dispatch(
            "knowledge.edit",
            json!({
                "id": "nonexistent",
                "sections": [{
                    "section_type": "overview", "content": "x",
                    "unknown_field_xyz": "boom"
                }]
            }),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("unknown_field_xyz") || msg.contains("unknown field"),
        "SectionUpdate: unknown field must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn import_rejects_unknown_field() {
    let f = pack(rt());
    assert_rejects_unknown_field(&f, "knowledge.import", json!({ "path": "/nonexistent" })).await;
}

#[tokio::test]
async fn challenge_rejects_unknown_field() {
    let f = pack(rt());
    assert_rejects_unknown_field(
        &f,
        "knowledge.challenge",
        json!({ "atom_id": "nonexistent", "section_type": "overview" }),
    )
    .await;
}

#[tokio::test]
async fn adjudicate_rejects_unknown_field() {
    let f = pack(rt());
    assert_rejects_unknown_field(
        &f,
        "knowledge.adjudicate",
        json!({ "atom_id": "nonexistent", "section_type": "overview", "resolution": "accept" }),
    )
    .await;
}

#[tokio::test]
async fn learn_rejects_unknown_field() {
    let f = pack(rt());
    assert_rejects_unknown_field(&f, "knowledge.learn", json!({ "name": "Test Concept" })).await;
}

#[tokio::test]
async fn cite_rejects_unknown_field() {
    let f = pack(rt());
    assert_rejects_unknown_field(
        &f,
        "knowledge.cite",
        json!({ "concept_id": "nonexistent", "source_id": "nonexistent" }),
    )
    .await;
}

#[tokio::test]
async fn topic_rejects_unknown_field() {
    let f = pack(rt());
    assert_rejects_unknown_field(&f, "knowledge.topic", json!({})).await;
}

#[tokio::test]
async fn feedback_rejects_unknown_top_level_field() {
    let f = pack(rt());
    assert_rejects_unknown_field(
        &f,
        "knowledge.feedback",
        json!({ "section_signals": { "overview": "useful" } }),
    )
    .await;
}

/// `feedback`'s `section_signals` map keys stay free-form (validated dynamically
/// against `SectionType`, not via `deny_unknown_fields`) — an unrecognized
/// section-type key must still be rejected, just with the pack's own
/// domain-specific error, not a generic serde one.
#[tokio::test]
async fn feedback_rejects_unknown_section_type_key_with_domain_error() {
    let f = pack(rt());
    let err = f
        .dispatch(
            "knowledge.feedback",
            json!({ "section_signals": { "not_a_real_section": "useful" } }),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("unknown section_type"),
        "expected domain-specific section_type error; got: {msg}"
    );
}

// ── #89: documented aliases must keep working under deny_unknown_fields ─────

#[tokio::test]
async fn learn_content_alias_still_works_under_deny_unknown_fields() {
    let f = pack(rt());
    let resp = f
        .dispatch(
            "knowledge.learn",
            json!({ "name": "Aliased Concept", "content": "described via the content alias" }),
        )
        .await
        .expect("content alias must still be accepted for LearnParams.description");
    assert_eq!(resp["name"], "Aliased Concept");
    assert_eq!(resp["description"], "described via the content alias");
}

#[tokio::test]
async fn list_kind_alias_still_works_under_deny_unknown_fields() {
    let f = pack(rt());
    let resp = f
        .dispatch("knowledge.list", json!({ "kind": "domain", "limit": 5 }))
        .await
        .expect("kind alias must still be accepted for ListParams.type");
    assert_eq!(resp["results"].as_array().map(Vec::len), Some(0));
}

// ── #105: suggest -> fold -> compose wires without caller-side construction ─

#[tokio::test]
async fn suggest_output_wires_into_fold_which_wires_into_compose() {
    let f = pack(rt());

    // Seed a member atom per domain (compose renders domain members, not the
    // domain's own mirror description) plus the two domains that reference them.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [
            {
                "slug": "retrieval-atom",
                "name": "Retrieval Atom",
                "content": "Dense and sparse retrieval systems for vector search, embeddings, ranking, and hybrid fusion pipelines over large corpora of documents, spanning approximate nearest neighbor indexes and reranking stages.",
                "finalized": true
            },
            {
                "slug": "training-atom",
                "name": "Training Atom",
                "content": "Distributed training infrastructure covering gradient accumulation, checkpointing, mixed precision, and cluster scheduling for large model runs across many GPU nodes and regions.",
                "finalized": true
            }
        ]}),
    )
    .await
    .expect("seed atoms");

    f.dispatch(
        "knowledge.upsert_domains",
        json!({ "domains": [
            {
                "slug": "retrieval-systems",
                "name": "Retrieval Systems",
                "description": "Dense and sparse retrieval systems for vector search, embeddings, ranking, and hybrid fusion pipelines over large corpora of documents, spanning approximate nearest neighbor indexes and reranking stages.",
                "members": ["retrieval-atom"]
            },
            {
                "slug": "training-infra",
                "name": "Training Infra",
                "description": "Distributed training infrastructure covering gradient accumulation, checkpointing, mixed precision, and cluster scheduling for large model runs across many GPU nodes and regions.",
                "members": ["training-atom"]
            }
        ]}),
    )
    .await
    .expect("seed domains");

    let suggest_resp = f
        .dispatch(
            "knowledge.suggest",
            json!({ "query": "vector search retrieval embeddings ranking", "limit": 8 }),
        )
        .await
        .expect("suggest");
    let results = suggest_resp["results"]
        .as_array()
        .expect("suggest results must be an array")
        .clone();
    assert!(
        !results.is_empty(),
        "suggest must return at least one domain"
    );
    for r in &results {
        assert!(r.get("id").is_some(), "suggest result missing id: {r:?}");
        assert!(
            r.get("score").is_some(),
            "suggest result missing score: {r:?}"
        );
        assert!(
            r.get("size").and_then(Value::as_u64).is_some(),
            "suggest result missing numeric size (issue #105): {r:?}"
        );
    }

    // suggest's `results` array feeds fold's `candidates` UNMODIFIED — no
    // caller-side field renaming, no size synthesis.
    let fold_resp = f
        .dispatch(
            "knowledge.fold",
            json!({ "candidates": results, "budget": 100_000 }),
        )
        .await
        .expect("fold must accept suggest's output unmodified");
    let selected = fold_resp["selected"]
        .as_array()
        .expect("fold selected must be an array")
        .clone();
    assert!(
        !selected.is_empty(),
        "fold must select at least one candidate"
    );

    // fold's `selected` items feed compose's `domain_ids` UNMODIFIED — pull the
    // `id` field straight off each selected item, no other construction.
    let domain_ids: Vec<Value> = selected.iter().map(|s| s["id"].clone()).collect();
    let compose_resp = f
        .dispatch(
            "knowledge.compose",
            json!({ "domain_ids": domain_ids, "query": "vector search retrieval embeddings ranking", "blend_kg": false }),
        )
        .await
        .expect("compose must accept fold's selected ids unmodified");
    assert_eq!(compose_resp["status"], "ok");
    assert!(
        compose_resp["data"]["count"].as_u64().unwrap_or(0) > 0,
        "compose must render at least one atom from the wired domain_ids: {compose_resp:?}"
    );
}

#[tokio::test]
async fn suggest_size_prices_member_atoms_instead_of_domain_description() {
    let f = pack(rt());
    let first_name = "Large Retrieval Member";
    let second_name = "Large Ranking Member";
    let first_member_content =
        "vector search retrieval embeddings ranking first member body ".repeat(120);
    let second_member_content =
        "vector search retrieval embeddings ranking second member body ".repeat(80);
    let description = "Vector search retrieval embeddings ranking systems improve relevant document discovery with hybrid indexes and carefully tuned reranking across production corpora.";
    let old_description_size = description.chars().count().div_ceil(4);
    let expected_member_size = (first_name.len() + first_member_content.len() + 40).div_ceil(4)
        + (second_name.len() + second_member_content.len() + 40).div_ceil(4);

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [
            {
                "slug": "large-retrieval-member",
                "name": first_name,
                "content": first_member_content,
                "finalized": true
            },
            {
                "slug": "large-ranking-member",
                "name": second_name,
                "content": second_member_content,
                "finalized": true
            }
        ]}),
    )
    .await
    .expect("seed large member atom");

    f.dispatch(
        "knowledge.upsert_domains",
        json!({ "domains": [{
            "slug": "small-description-domain",
            "name": "Small Description Domain",
            "description": description,
            "members": ["large-retrieval-member", "large-ranking-member"]
        }]}),
    )
    .await
    .expect("seed domain with small description");

    let suggest_resp = f
        .dispatch(
            "knowledge.suggest",
            json!({ "query": "vector search retrieval embeddings ranking", "limit": 1 }),
        )
        .await
        .expect("suggest");
    let result = &suggest_resp["results"][0];
    assert_eq!(
        result["size"].as_u64(),
        Some(expected_member_size as u64),
        "suggest size must aggregate the member body cost, not the domain description"
    );
    assert!(expected_member_size > old_description_size);

    let fold_resp = f
        .dispatch(
            "knowledge.fold",
            json!({
                "candidates": [result.clone()],
                "budget": old_description_size
            }),
        )
        .await
        .expect("fold under the old description-sized budget");
    assert_eq!(
        fold_resp["selected_count"], 0,
        "the member-heavy domain must not fit the old description-sized budget"
    );
}
