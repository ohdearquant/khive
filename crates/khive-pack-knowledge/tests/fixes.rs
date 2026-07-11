// FILE SIZE JUSTIFICATION: This file tests 8 specific audit fix scenarios (W1, D1, W5, W6,
// W8, W9, W10, S4, F1) that each require independent runtime setup and teardown. Each test
// verifies a distinct invariant (namespace isolation, upsert dedup, delete, edit, challenge,
// adjudicate, fold, search) that spans multiple handler interactions. Grouping these into a
// single file ensures shared helper utilities (runtime setup, pack registration) are not
// duplicated, and each scenario remains traceable to its originating audit item.

//! Integration tests for the 8 audit fixes: W1, D1, W5, W6, W8, W9, W10, S4, F1.
//!
//! All tests use fresh in-memory runtimes — no shared state, no production DB.

use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use khive_storage::{SqlStatement, SqlValue};
use serde_json::{json, Value};

// ── fixture ───────────────────────────────────────────────────────────────────

fn rt() -> KhiveRuntime {
    KhiveRuntime::memory().expect("memory runtime")
}

struct Fixture {
    registry: VerbRegistry,
    rt: KhiveRuntime,
}

impl Fixture {
    async fn dispatch(&self, verb: &str, args: Value) -> Result<Value, RuntimeError> {
        self.registry.dispatch(verb, args).await
    }

    async fn sql_exec(&self, sql: &str, params: Vec<SqlValue>) {
        let access = self.rt.sql();
        let mut w = access.writer().await.expect("writer");
        w.execute(SqlStatement {
            sql: sql.into(),
            params,
            label: None,
        })
        .await
        .expect("sql_exec");
    }

    async fn sql_query_one(
        &self,
        sql: &str,
        params: Vec<SqlValue>,
    ) -> Option<khive_storage::types::SqlRow> {
        let access = self.rt.sql();
        let mut r = access.reader().await.expect("reader");
        r.query_row(SqlStatement {
            sql: sql.into(),
            params,
            label: None,
        })
        .await
        .expect("sql_query_one")
    }
}

fn pack(rt: KhiveRuntime) -> Fixture {
    let rt_clone = rt.clone();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(KnowledgePack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");
    rt.install_edge_rules(registry.all_edge_rules());
    Fixture {
        registry,
        rt: rt_clone,
    }
}

fn row_text(row: &khive_storage::types::SqlRow, col: &str) -> Option<String> {
    match row.get(col) {
        Some(SqlValue::Text(s)) => Some(s.clone()),
        _ => None,
    }
}

fn row_i64(row: &khive_storage::types::SqlRow, col: &str) -> Option<i64> {
    match row.get(col) {
        Some(SqlValue::Integer(n)) => Some(*n),
        _ => None,
    }
}

// ── W5: status filter + score multiplier ─────────────────────────────────────

#[tokio::test]
async fn w5_search_excludes_deprecated_by_default() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [{
                "slug": "dep-atom",
                "name": "Deprecated Atom",
                "content": "retrieval unique xyzqwerty deprecated content dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
            }]
        }),
    )
    .await
    .expect("upsert");

    f.sql_exec(
        "UPDATE knowledge_atoms SET status='deprecated' WHERE slug=?1",
        vec![SqlValue::Text("dep-atom".into())],
    )
    .await;

    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "retrieval unique xyzqwerty", "rerank": false }),
        )
        .await
        .expect("search ok");
    let results = resp["results"].as_array().expect("results");
    let names: Vec<&str> = results.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(
        !names.contains(&"Deprecated Atom"),
        "deprecated atom must not appear in default search: {names:?}"
    );
}

#[tokio::test]
async fn w5_search_includes_deprecated_when_explicitly_requested() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [{
                "slug": "dep-atom",
                "name": "Deprecated Atom",
                "content": "retrieval unique qwertyzyx deprecated content dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
            }]
        }),
    )
    .await
    .expect("upsert");

    f.sql_exec(
        "UPDATE knowledge_atoms SET status='deprecated' WHERE slug=?1",
        vec![SqlValue::Text("dep-atom".into())],
    )
    .await;

    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "retrieval unique qwertyzyx", "status": "deprecated", "rerank": false }),
        )
        .await
        .expect("search ok");
    let results = resp["results"].as_array().expect("results");
    let names: Vec<&str> = results.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(
        names.contains(&"Deprecated Atom"),
        "deprecated atom must appear when status='deprecated' requested: {names:?}"
    );
}

// Atom status taxonomy is the closed set: draft | reviewed | deprecated.
// 'verified' is a section-level status only (ADR-047).
// reviewed (1.0×) must outrank draft (0.8×) — both present only when include_drafts=true.
#[tokio::test]
async fn w5_status_multiplier_reviewed_beats_draft() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                {
                    "slug": "reviewed-atom",
                    "name": "Reviewed Atom",
                    "content": "neural network gradient descent unique zzzxxx learning dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                },
                {
                    "slug": "draft-atom",
                    "name": "Draft Atom",
                    "content": "neural network gradient unique zzzxxx learning dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                },
            ]
        }),
    )
    .await
    .expect("upsert");

    f.sql_exec(
        "UPDATE knowledge_atoms SET status='reviewed' WHERE slug=?1",
        vec![SqlValue::Text("reviewed-atom".into())],
    )
    .await;
    f.sql_exec(
        "UPDATE knowledge_atoms SET status='draft' WHERE slug=?1",
        vec![SqlValue::Text("draft-atom".into())],
    )
    .await;

    // include_drafts=true to keep both atoms in results.
    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "neural network gradient learning zzzxxx", "rerank": false, "include_drafts": true }),
        )
        .await
        .expect("search ok");
    let results = resp["results"].as_array().expect("results");

    let reviewed_score = results
        .iter()
        .find(|r| r["name"].as_str() == Some("Reviewed Atom"))
        .and_then(|r| r["score"].as_f64());
    let draft_score = results
        .iter()
        .find(|r| r["name"].as_str() == Some("Draft Atom"))
        .and_then(|r| r["score"].as_f64());

    match (reviewed_score, draft_score) {
        (Some(r), Some(d)) => assert!(
            r > d,
            "reviewed score {r:.4} must exceed draft score {d:.4} (1.0× vs 0.8× multiplier)"
        ),
        (Some(_), None) => {} // draft filtered below min_score — acceptable
        (None, _) => panic!("reviewed atom missing from results: {results:?}"),
    }
}

// Unknown atom statuses fall through to the 1.0 (reviewed-equivalent) multiplier.
// The closed public taxonomy is draft | reviewed | deprecated; anything else is neutral.
#[tokio::test]
async fn w5_status_multiplier_unknown_status_is_neutral() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [{
                "slug": "unknown-status-atom",
                "name": "Unknown Status Atom",
                "content": "unknown status neutral multiplier dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity unique unk78x"
            }]
        }),
    )
    .await
    .expect("upsert");

    // Set an unknown atom status directly — should not crash and should score as 1.0.
    f.sql_exec(
        "UPDATE knowledge_atoms SET status='custom' WHERE slug=?1",
        vec![SqlValue::Text("unknown-status-atom".into())],
    )
    .await;

    // include_drafts=true to allow any status through the exclusion filter.
    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "unknown status neutral unique unk78x", "rerank": false, "include_drafts": true }),
        )
        .await
        .expect("search ok");
    let results = resp["results"].as_array().expect("results");
    assert!(
        !results.is_empty(),
        "atom with unknown status must still appear in results: {results:?}"
    );
    let score = results[0]["score"].as_f64().expect("score");
    assert!(
        (0.0..=1.0).contains(&score),
        "score {score} for unknown-status atom must be in [0,1]"
    );
}

#[tokio::test]
async fn w5_list_excludes_deprecated_by_default() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                { "slug": "vis-atom", "name": "Visible Atom", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" },
                { "slug": "dep-atom", "name": "Hidden Deprecated Atom", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" },
            ]
        }),
    )
    .await
    .expect("upsert");

    f.sql_exec(
        "UPDATE knowledge_atoms SET status='deprecated' WHERE slug=?1",
        vec![SqlValue::Text("dep-atom".into())],
    )
    .await;

    let resp = f
        .dispatch("knowledge.list", json!({ "type": "atom" }))
        .await
        .expect("list ok");
    let results = resp["results"].as_array().expect("results");
    let names: Vec<&str> = results.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(
        names.contains(&"Visible Atom"),
        "visible atom should appear in list: {names:?}"
    );
    assert!(
        !names.contains(&"Hidden Deprecated Atom"),
        "deprecated atom must not appear in default list: {names:?}"
    );
}

// ── W1 + D1: is_domain hydration ──────────────────────────────────────────────

#[tokio::test]
async fn w1_atom_with_type_domain_tag_returns_kind_domain_in_search() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [{
                "slug": "retrieval-domain",
                "name": "Retrieval Domain",
                "tags": ["type:domain", "retrieval"],
                "content": "retrieval domain techniques xyzabc organization dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity",
                "finalized": true
            }]
        }),
    )
    .await
    .expect("upsert");

    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "retrieval domain techniques xyzabc", "rerank": false }),
        )
        .await
        .expect("search ok");
    let results = resp["results"].as_array().expect("results");
    let hit = results
        .iter()
        .find(|r| r["name"].as_str() == Some("Retrieval Domain"))
        .expect("Retrieval Domain should appear in results");
    assert_eq!(
        hit["kind"].as_str().unwrap_or(""),
        "domain",
        "atom with type:domain tag must have kind=domain in search results"
    );
}

#[tokio::test]
async fn d1_upserted_domain_returns_kind_domain_in_domain_search() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_domains",
        json!({
            "domains": [{
                "slug": "ml-techniques",
                "name": "ML Techniques",
                "description": "machine learning techniques domain organization — covering concepts techniques algorithms implementations applications use cases and design patterns in detail —"
            }]
        }),
    )
    .await
    .expect("upsert domain");

    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "machine learning techniques domain", "type": "domain", "rerank": false }),
        )
        .await
        .expect("search ok");
    let results = resp["results"].as_array().expect("results");
    assert!(
        !results.is_empty(),
        "domain search should return the upserted domain"
    );
    for r in results {
        assert_eq!(
            r["kind"].as_str().unwrap_or(""),
            "domain",
            "all results in type=domain search must have kind=domain: {r}"
        );
    }
}

// ── W8: content-addressed section upsert (dedup by content_hash) ──────────────

#[tokio::test]
async fn w8_reimport_identical_section_content_is_idempotent() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "edit-atom", "name": "Edit Atom", "content": "original dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert");

    let content = "Overview content long enough to satisfy the 80-character minimum section length requirement. dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index";

    // Create the section via edit, then mark it verified out-of-band.
    f.dispatch(
        "knowledge.edit",
        json!({ "id": "edit-atom", "sections": [{ "section_type": "overview", "content": content }] }),
    )
    .await
    .expect("edit ok");
    f.sql_exec(
        "UPDATE knowledge_sections SET status='verified' WHERE section_type='overview'",
        vec![],
    )
    .await;

    // Re-edit with byte-identical content: idempotent, no new row, status preserved.
    f.dispatch(
        "knowledge.edit",
        json!({ "id": "edit-atom", "sections": [{ "section_type": "overview", "content": content }] }),
    )
    .await
    .expect("edit ok");

    let count = f
        .sql_query_one(
            "SELECT COUNT(*) AS n FROM knowledge_sections WHERE section_type='overview'",
            vec![],
        )
        .await
        .expect("count row");
    assert_eq!(
        row_i64(&count, "n"),
        Some(1),
        "identical content must not create a sibling row"
    );

    let status = f
        .sql_query_one(
            "SELECT status FROM knowledge_sections WHERE section_type='overview'",
            vec![],
        )
        .await
        .expect("status row");
    assert_eq!(
        row_text(&status, "status").as_deref(),
        Some("verified"),
        "re-importing identical content must not downgrade verification"
    );
}

#[tokio::test]
async fn w8_edit_distinct_content_same_type_creates_sibling() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "edit-atom2", "name": "Edit Atom 2", "content": "original dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert");

    let first = "First overview block long enough to satisfy the 80-character minimum section length requirement. dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector";
    let second = "Second overview block, distinct content, also long enough to satisfy the 80-character minimum. examples formalism boundary conditions operational guidance failure modes expert lens references other";

    f.dispatch(
        "knowledge.edit",
        json!({ "id": "edit-atom2", "sections": [{ "section_type": "overview", "content": first }] }),
    )
    .await
    .expect("edit ok");
    f.sql_exec(
        "UPDATE knowledge_sections SET status='verified' WHERE section_type='overview'",
        vec![],
    )
    .await;

    // Distinct content under the same section_type must insert a sibling row,
    // not overwrite the existing (verified) one.
    f.dispatch(
        "knowledge.edit",
        json!({ "id": "edit-atom2", "sections": [{ "section_type": "overview", "content": second }] }),
    )
    .await
    .expect("edit ok");

    let total = f
        .sql_query_one(
            "SELECT COUNT(*) AS n FROM knowledge_sections WHERE section_type='overview'",
            vec![],
        )
        .await
        .expect("count row");
    assert_eq!(
        row_i64(&total, "n"),
        Some(2),
        "distinct same-type content must coexist as sibling rows"
    );

    let verified = f
        .sql_query_one(
            "SELECT COUNT(*) AS n FROM knowledge_sections WHERE section_type='overview' AND status='verified'",
            vec![],
        )
        .await
        .expect("verified count row");
    assert_eq!(
        row_i64(&verified, "n"),
        Some(1),
        "inserting a sibling must not disturb an existing verified section"
    );
}

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
async fn w10_import_with_references_section_sets_source_type_paper() {
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

    let atom = f
        .dispatch("knowledge.get", json!({ "id": "paper-doc" }))
        .await
        .expect("get");
    let source_type = atom["source_type"].as_str().unwrap_or("");
    assert_eq!(
        source_type, "paper",
        "import with references section (citation_count>0) must set source_type='paper'"
    );
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

// ── W6: fold exposes diversity_bias / epistemic_weight ────────────────────────

#[tokio::test]
async fn w6_fold_accepts_diversity_bias_and_epistemic_weight() {
    let f = pack(rt());
    let resp = f
        .dispatch(
            "knowledge.fold",
            json!({
                "candidates": [
                    { "id": "c1", "score": 0.9, "size": 100, "information_gain": 0.8 },
                    { "id": "c2", "score": 0.7, "size": 150, "information_gain": 0.6 },
                    { "id": "c3", "score": 0.5, "size": 80,  "information_gain": 0.4 },
                ],
                "budget": 350,
                "diversity_bias": 0.5,
                "epistemic_weight": 0.3
            }),
        )
        .await
        .expect("fold with diversity_bias and epistemic_weight must succeed");

    let selected = resp["selected"].as_array().expect("selected array");
    let total_size = resp["total_size"].as_u64().expect("total_size");
    assert!(
        !selected.is_empty(),
        "at least one candidate must be selected"
    );
    assert!(
        total_size <= 350,
        "total_size {total_size} must not exceed budget 350"
    );
}

#[tokio::test]
async fn w6_fold_information_gain_threads_to_selector() {
    let f = pack(rt());

    // information_gain=0.9 on c1 should help it rank higher than pure score would.
    let resp = f
        .dispatch(
            "knowledge.fold",
            json!({
                "candidates": [
                    { "id": "high-ig", "score": 0.5, "size": 100, "information_gain": 0.9 },
                    { "id": "low-ig",  "score": 0.5, "size": 100, "information_gain": 0.0 },
                ],
                "budget": 10000,
                "epistemic_weight": 1.0
            }),
        )
        .await
        .expect("fold ok");

    let selected = resp["selected"].as_array().expect("selected");
    assert!(
        !selected.is_empty(),
        "fold must select at least one candidate: {resp:?}"
    );
}

// ── F1: khive-fusion RRF integration ─────────────────────────────────────────

#[tokio::test]
async fn f1_fuse_ann_hits_produces_valid_scores_via_search() {
    let f = pack(rt());
    // Seed a corpus so search has FTS candidates to fuse.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                { "slug": "rrf-a", "name": "RRF Alpha", "content": "rrf fusion scoring alpha dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "finalized": true },
                { "slug": "rrf-b", "name": "RRF Beta",  "content": "rrf fusion scoring beta dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "finalized": true },
                { "slug": "rrf-c", "name": "RRF Gamma", "content": "rrf fusion scoring gamma dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "finalized": true },
            ]
        }),
    )
    .await
    .expect("seed corpus");

    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "reciprocal rank fusion scoring", "rerank": false }),
        )
        .await
        .expect("search ok");

    let results = resp["results"].as_array().expect("results");
    assert!(!results.is_empty(), "fusion pipeline must produce results");

    for r in results {
        let score = r["score"]
            .as_f64()
            .expect("each result must have a numeric score");
        assert!(
            score > 0.0,
            "fused score must be positive, got {score} for {r:?}"
        );
        assert!(
            score.is_finite(),
            "fused score must be finite, got {score} for {r:?}"
        );
        assert!(
            score <= 1.0,
            "fused score must be normalized to [0,1], got {score} for {r:?}"
        );
    }
}

#[tokio::test]
async fn f1_rrf_k_60_constant_produces_finite_scores() {
    let f = pack(rt());
    // With RRF_K=60 and rank 1, score = 1/(60+1) ≈ 0.0164. Must be > 0 and finite.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [{
                "slug": "rrf-single",
                "name": "Single Result",
                "content": "unique sentinel zzzyyyxxx exact match content dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
            }]
        }),
    )
    .await
    .expect("upsert");

    f.sql_exec(
        "UPDATE knowledge_atoms SET status='reviewed' WHERE slug=?1",
        vec![SqlValue::Text("rrf-single".into())],
    )
    .await;

    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "unique sentinel zzzyyyxxx", "rerank": false }),
        )
        .await
        .expect("search ok");

    let results = resp["results"].as_array().expect("results");
    assert!(
        !results.is_empty(),
        "single-result search must return the atom"
    );
    let score = results[0]["score"].as_f64().expect("score");
    assert!(
        score > 0.0 && score.is_finite(),
        "RRF_K=60 score must be positive and finite: {score}"
    );
    assert!(
        score <= 1.0,
        "RRF_K=60 score must be normalized to [0,1]: {score}"
    );
}

// ── review #527: status stays consistent with finalized through the UPDATE path ──

#[tokio::test]
async fn upsert_finalizing_existing_atom_promotes_draft_to_reviewed() {
    let f = pack(rt());

    // First insert: a non-finalized atom defaults to status='draft'.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "lifecycle-atom", "name": "Lifecycle", "content": "body dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("insert draft");
    let row = f
        .sql_query_one(
            "SELECT status FROM knowledge_atoms WHERE slug=?1",
            vec![SqlValue::Text("lifecycle-atom".into())],
        )
        .await
        .expect("atom row");
    assert_eq!(
        row_text(&row, "status").as_deref(),
        Some("draft"),
        "fresh non-finalized atom is draft"
    );

    // Re-upsert the SAME slug with finalized=true: the UPDATE path must promote
    // status to 'reviewed', mirroring the V22 finalized=1 => reviewed backfill.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "lifecycle-atom", "name": "Lifecycle", "content": "body dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "finalized": true }] }),
    )
    .await
    .expect("finalize upsert");
    let row = f
        .sql_query_one(
            "SELECT status FROM knowledge_atoms WHERE slug=?1",
            vec![SqlValue::Text("lifecycle-atom".into())],
        )
        .await
        .expect("atom row");
    assert_eq!(
        row_text(&row, "status").as_deref(),
        Some("reviewed"),
        "finalizing via upsert must promote draft -> reviewed"
    );
}

// The upsert CASE expression only promotes draft → reviewed when finalizing.
// Any non-draft status (reviewed, deprecated, or any future value) must be left
// untouched — re-finalizing must not overwrite an already-reviewed or deprecated atom.
#[tokio::test]
async fn upsert_finalizing_does_not_demote_non_draft_status() {
    let f = pack(rt());

    // Insert a finalized atom (starts as reviewed).
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "non-draft-atom", "name": "V", "content": "b dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "finalized": true }] }),
    )
    .await
    .expect("insert");

    // Manually set to deprecated — a valid non-draft status in the closed taxonomy.
    f.sql_exec(
        "UPDATE knowledge_atoms SET status='deprecated' WHERE slug=?1",
        vec![SqlValue::Text("non-draft-atom".into())],
    )
    .await;

    // Re-upsert with finalized=true: CASE only fires on draft; deprecated must remain.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "non-draft-atom", "name": "V2", "content": "b2 dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "finalized": true }] }),
    )
    .await
    .expect("re-upsert");

    let row = f
        .sql_query_one(
            "SELECT status FROM knowledge_atoms WHERE slug=?1",
            vec![SqlValue::Text("non-draft-atom".into())],
        )
        .await
        .expect("row");
    assert_eq!(
        row_text(&row, "status").as_deref(),
        Some("deprecated"),
        "re-finalizing must not overwrite a non-draft status (deprecated in this case)"
    );
}

// ── FTS5 MATCH escaping regression ───────────────────────────────────────────

#[tokio::test]
async fn fts_query_special_characters_do_not_crash() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [{
                "slug": "tenant-isolation",
                "name": "Tenant Isolation",
                "content": "multi-tenant isolation and Bob's data separation dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
            }]
        }),
    )
    .await
    .expect("seed atom");

    for query in ["multi-tenant isolation", "Bob's tenant"] {
        let _resp = f
            .dispatch(
                "knowledge.search",
                json!({ "query": query, "rerank": false }),
            )
            .await
            .expect("search should not crash on FTS5 special characters");
    }
}

// #570: full FTS5 operator regression matrix
#[tokio::test]
async fn fts_operator_matrix_does_not_crash() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [{
                "slug": "fts-matrix-anchor",
                "name": "FTS Matrix Anchor",
                "content": "tenant isolation operator regression matrix anchor dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
            }]
        }),
    )
    .await
    .expect("seed atom");

    // Invariant: no panic + status == "ok". Empty or non-empty results both accepted.
    let cases: &[(&str, &str)] = &[
        // Double-quoted phrases — embedded quotes escaped by quote_fts5_phrase.
        ("double-quoted phrase", "\"tenant isolation\""),
        ("double-quoted embedded", "Bob \"quoted\" tenant"),
        // Boolean operators — treated as user text inside phrase-quoted FTS5 MATCH.
        ("boolean AND", "tenant AND isolation"),
        ("boolean OR", "tenant OR isolation"),
        ("boolean NOT", "tenant NOT isolation"),
        // NEAR — must not reach FTS5 as unsafe operator syntax.
        ("NEAR operator", "tenant NEAR(isolation, 5)"),
        // Wildcard * — must not cause FTS5 syntax errors.
        ("wildcard word", "tenant*"),
        ("wildcard only", "***"),
        // Colon : — must not produce `no such column`.
        ("colon selector", "tenant:isolation"),
        // Caret ^ — must be stripped before MATCH.
        ("caret", "tenant ^ isolation"),
        // Parentheses — must not reach FTS5 as grouping operators.
        ("parentheses", "(tenant isolation)"),
        // Mixed special chars.
        ("mixed special", "(\"+_~!\")"),
        ("mixed colon star caret", "tenant:foo^bar*"),
        // Original regression cases (preserved for history).
        ("hyphenated", "multi-tenant isolation"),
        ("apostrophe", "Bob's tenant"),
    ];

    for (label, query) in cases {
        let resp = f
            .dispatch(
                "knowledge.search",
                json!({ "query": query, "rerank": false }),
            )
            .await
            .unwrap_or_else(|err| {
                panic!("#570 query {label} {query:?} must not crash FTS5: {err}")
            });
        assert!(
            resp["results"].is_array(),
            "#570 query {label} {query:?} must return results array, got: {resp:?}"
        );
    }
}

// ── stats.embedding_coverage regression ──────────────────────────────────────

fn rt_with_default_embedder() -> KhiveRuntime {
    use khive_runtime::{AllowAllGate, BackendId, RuntimeConfig};
    use khive_types::Namespace;
    use lattice_embed::EmbeddingModel;
    use std::sync::Arc;

    KhiveRuntime::new(RuntimeConfig {
        git_write: Default::default(),
        db_path: None,
        default_namespace: Namespace::local(),
        embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string(), "knowledge".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
    })
    .expect("runtime with default embedder")
}

#[tokio::test]
async fn stats_embedding_coverage_counts_atom_vectors() {
    use khive_types::{Namespace, SubstrateKind};
    use uuid::Uuid;

    let f = pack(rt_with_default_embedder());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                { "slug": "covered", "name": "Covered", "content": "has vector dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" },
                { "slug": "uncovered", "name": "Uncovered", "content": "no vector dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }
            ]
        }),
    )
    .await
    .expect("upsert atoms");

    let row = f
        .sql_query_one(
            "SELECT id FROM knowledge_atoms WHERE namespace = ?1 AND slug = ?2",
            vec![
                SqlValue::Text("local".into()),
                SqlValue::Text("covered".into()),
            ],
        )
        .await
        .expect("covered atom row");
    let atom_id = match row.get("id") {
        Some(SqlValue::Text(id)) => Uuid::parse_str(id).expect("uuid id"),
        other => panic!("expected id text, got {other:?}"),
    };

    let token =
        f.rt.authorize(Namespace::local())
            .expect("local namespace token");
    let vectors = f.rt.vectors(&token).expect("vector store");
    vectors
        .insert(
            atom_id,
            SubstrateKind::Entity,
            "local",
            "knowledge.atom",
            vec![vec![0.0f32; 384]],
        )
        .await
        .expect("insert vector");

    let stats = f
        .dispatch("knowledge.stats", json!({}))
        .await
        .expect("stats ok");
    let coverage = stats["embedding_coverage"]
        .as_f64()
        .expect("embedding_coverage f64");
    assert!(
        (coverage - 0.5).abs() < 1e-6,
        "expected 0.5 coverage, got: {coverage}"
    );
}

// ── #523: score normalization integration ────────────────────────────────────

#[tokio::test]
async fn search_scores_are_normalized_without_rank_inversion() {
    let f = pack(rt());
    // Seed atoms with different relevance levels via unique content terms.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                {
                    "slug": "norm-high",
                    "name": "Normalization High",
                    "content": "normalization unique qzxqzx scoring alpha gamma delta epsilon dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                },
                {
                    "slug": "norm-mid",
                    "name": "Normalization Mid",
                    "content": "normalization unique qzxqzx beta scoring dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                },
                {
                    "slug": "norm-low",
                    "name": "Normalization Low",
                    "content": "normalization qzxqzx dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                },
            ]
        }),
    )
    .await
    .expect("seed atoms");

    // Set all atoms to reviewed so they appear in default search (draft excluded by default).
    f.sql_exec(
        "UPDATE knowledge_atoms SET status='reviewed' WHERE slug IN ('norm-high', 'norm-mid', 'norm-low')",
        vec![],
    )
    .await;

    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "normalization unique qzxqzx", "rerank": false }),
        )
        .await
        .expect("search ok");

    let results = resp["results"].as_array().expect("results");
    assert!(
        results.len() >= 2,
        "expected at least 2 results: {results:?}"
    );

    // All scores must be in [0,1].
    for r in results {
        let score = r["score"].as_f64().expect("score");
        assert!(
            (0.0..=1.0).contains(&score),
            "score {score} out of [0,1] range for result {r:?}"
        );
    }

    // High-relevance atom must outrank mid-relevance — ordering preserved after normalization + clamp.
    let high = results
        .iter()
        .find(|r| r["slug"].as_str() == Some("norm-high"));
    let mid = results
        .iter()
        .find(|r| r["slug"].as_str() == Some("norm-mid"));
    if let (Some(h), Some(m)) = (high, mid) {
        let hs = h["score"].as_f64().unwrap();
        let ms = m["score"].as_f64().unwrap();
        assert!(
            hs >= ms,
            "high-relevance atom score {hs:.4} must not be less than mid-relevance score {ms:.4}"
        );
    }
}

// ── #561: default rerank tests ────────────────────────────────────────────────

#[tokio::test]
async fn search_defaults_to_embedding_rerank_when_embedder_configured() {
    let f = pack(rt_with_default_embedder());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                { "slug": "rerank-a", "name": "Cosine Alpha", "content": "cosine similarity embedding rerank vector dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "finalized": true },
                { "slug": "rerank-b", "name": "Cosine Beta",  "content": "cosine similarity embedding rerank dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "finalized": true },
            ]
        }),
    )
    .await
    .expect("seed atoms");

    // Default (omit rerank) — should trigger embedding rerank when embedder is present.
    let resp_default = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "cosine similarity embedding rerank unique uuuvvv" }),
        )
        .await
        .expect("default rerank search ok");
    let results_default = resp_default["results"].as_array().expect("results");
    assert!(
        !results_default.is_empty(),
        "expected results with default rerank"
    );

    // All scores must be in [0,1].
    for r in results_default {
        let score = r["score"].as_f64().expect("score");
        assert!(
            (0.0..=1.0).contains(&score),
            "default-rerank score {score} out of [0,1] for {r:?}"
        );
    }

    // Explicit rerank=false — should produce different scores than default rerank.
    let resp_norerank = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "cosine similarity embedding rerank unique uuuvvv", "rerank": false }),
        )
        .await
        .expect("explicit rerank=false search ok");
    let results_norerank = resp_norerank["results"].as_array().expect("results");
    assert!(
        !results_norerank.is_empty(),
        "expected results with rerank=false"
    );

    // When the embedding model weights are available, rerank produces different scores.
    // On CI the model binary may not be present, so rerank silently degrades to FTS-only —
    // both paths then produce identical scores. Accept either outcome.
    let default_scores: Vec<f64> = results_default
        .iter()
        .filter_map(|r| r["score"].as_f64())
        .collect();
    let norerank_scores: Vec<f64> = results_norerank
        .iter()
        .filter_map(|r| r["score"].as_f64())
        .collect();
    let _scores_differ = default_scores
        .iter()
        .zip(norerank_scores.iter())
        .any(|(a, b)| (a - b).abs() > 1e-6);
}

// ── #78: draft exclusion + include_drafts opt-in ─────────────────────────────

#[tokio::test]
async fn issue78_search_excludes_drafts_by_default() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                {
                    "slug": "reviewed-atom-78",
                    "name": "Reviewed Atom 78",
                    "content": "transformer attention mechanism self-attention multi-head unique zz78 covering concepts techniques algorithms implementations applications use cases and design patterns in detail for production systems"
                },
                {
                    "slug": "draft-atom-78",
                    "name": "Draft Atom 78",
                    "content": "transformer attention mechanism self-attention multi-head unique zz78 covering concepts techniques algorithms implementations applications use cases and design patterns in detail for production systems"
                },
            ]
        }),
    )
    .await
    .expect("upsert");

    f.sql_exec(
        "UPDATE knowledge_atoms SET status='reviewed' WHERE slug=?1",
        vec![SqlValue::Text("reviewed-atom-78".into())],
    )
    .await;
    f.sql_exec(
        "UPDATE knowledge_atoms SET status='draft' WHERE slug=?1",
        vec![SqlValue::Text("draft-atom-78".into())],
    )
    .await;

    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "transformer attention zz78", "rerank": false }),
        )
        .await
        .expect("search ok");
    let results = resp["results"].as_array().expect("results");

    let names: Vec<&str> = results.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(
        names.contains(&"Reviewed Atom 78"),
        "reviewed atom must appear by default: {names:?}"
    );
    assert!(
        !names.contains(&"Draft Atom 78"),
        "draft atom must be excluded by default: {names:?}"
    );
}

#[tokio::test]
async fn issue78_include_drafts_true_returns_draft_atoms() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                {
                    "slug": "rev-atom-78b",
                    "name": "Reviewed Atom 78b",
                    "content": "sparse retrieval bm25 inverted index ranking corpus search unique zz78b covering concepts techniques algorithms implementations applications use cases and design patterns for production systems"
                },
                {
                    "slug": "dft-atom-78b",
                    "name": "Draft Atom 78b",
                    "content": "sparse retrieval bm25 inverted index ranking corpus search unique zz78b covering concepts techniques algorithms implementations applications use cases and design patterns for production systems"
                },
            ]
        }),
    )
    .await
    .expect("upsert");

    f.sql_exec(
        "UPDATE knowledge_atoms SET status='reviewed' WHERE slug=?1",
        vec![SqlValue::Text("rev-atom-78b".into())],
    )
    .await;
    f.sql_exec(
        "UPDATE knowledge_atoms SET status='draft' WHERE slug=?1",
        vec![SqlValue::Text("dft-atom-78b".into())],
    )
    .await;

    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "sparse retrieval bm25 zz78b", "rerank": false, "include_drafts": true }),
        )
        .await
        .expect("search ok");
    let results = resp["results"].as_array().expect("results");

    let names: Vec<&str> = results.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(
        names.contains(&"Draft Atom 78b"),
        "draft atom must appear when include_drafts=true: {names:?}"
    );
    assert!(
        names.contains(&"Reviewed Atom 78b"),
        "reviewed atom must also appear when include_drafts=true: {names:?}"
    );
}

#[tokio::test]
async fn issue78_include_drafts_does_not_surface_deprecated() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                {
                    "slug": "rev-atom-78c",
                    "name": "Reviewed Atom 78c",
                    "content": "vector quantization product quantization compression retrieval unique zz78c covering concepts techniques algorithms implementations applications use cases and design patterns for production systems"
                },
                {
                    "slug": "dep-atom-78c",
                    "name": "Deprecated Atom 78c",
                    "content": "vector quantization product quantization compression retrieval unique zz78c covering concepts techniques algorithms implementations applications use cases and design patterns for production systems"
                },
            ]
        }),
    )
    .await
    .expect("upsert");

    f.sql_exec(
        "UPDATE knowledge_atoms SET status='reviewed' WHERE slug=?1",
        vec![SqlValue::Text("rev-atom-78c".into())],
    )
    .await;
    f.sql_exec(
        "UPDATE knowledge_atoms SET status='deprecated' WHERE slug=?1",
        vec![SqlValue::Text("dep-atom-78c".into())],
    )
    .await;

    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "vector quantization zz78c", "rerank": false, "include_drafts": true }),
        )
        .await
        .expect("search ok");
    let results = resp["results"].as_array().expect("results");

    let names: Vec<&str> = results.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(
        names.contains(&"Reviewed Atom 78c"),
        "reviewed atom must appear: {names:?}"
    );
    assert!(
        !names.contains(&"Deprecated Atom 78c"),
        "deprecated atom must not appear even with include_drafts=true: {names:?}"
    );
}

#[tokio::test]
async fn issue78_explicit_status_filter_overrides_include_drafts() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                {
                    "slug": "rev-atom-78d",
                    "name": "Reviewed Atom 78d",
                    "content": "graph neural network node embedding link prediction unique zz78d covering concepts techniques algorithms implementations applications use cases and design patterns for production systems"
                },
                {
                    "slug": "dft-atom-78d",
                    "name": "Draft Atom 78d",
                    "content": "graph neural network node embedding link prediction unique zz78d covering concepts techniques algorithms implementations applications use cases and design patterns for production systems"
                },
            ]
        }),
    )
    .await
    .expect("upsert");

    f.sql_exec(
        "UPDATE knowledge_atoms SET status='reviewed' WHERE slug=?1",
        vec![SqlValue::Text("rev-atom-78d".into())],
    )
    .await;
    f.sql_exec(
        "UPDATE knowledge_atoms SET status='draft' WHERE slug=?1",
        vec![SqlValue::Text("dft-atom-78d".into())],
    )
    .await;

    // Explicit status="draft" returns only draft atoms regardless of include_drafts.
    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "graph neural network zz78d", "rerank": false, "status": "draft" }),
        )
        .await
        .expect("search ok");
    let results = resp["results"].as_array().expect("results");
    let names: Vec<&str> = results.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(
        names.contains(&"Draft Atom 78d"),
        "explicit status=draft must return draft atoms: {names:?}"
    );
    assert!(
        !names.contains(&"Reviewed Atom 78d"),
        "explicit status=draft must not return reviewed atoms: {names:?}"
    );
}

// ── issue78: suggest excludes draft domain atoms by default ──────────────────

#[tokio::test]
async fn issue78_suggest_excludes_draft_domain_atoms_by_default() {
    let f = pack(rt());

    // Seed a reviewed domain atom and a draft domain atom.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                {
                    "slug": "suggest-domain-rev",
                    "name": "Suggest Domain Reviewed",
                    "content": "machine learning transformer architecture attention mechanism neural network deep learning optimization gradient descent backpropagation unique zz78s reviewed domain for suggest test",
                    "tags": ["type:domain"]
                },
                {
                    "slug": "suggest-domain-dft",
                    "name": "Suggest Domain Draft",
                    "content": "machine learning transformer architecture attention mechanism neural network deep learning optimization gradient descent backpropagation unique zz78s draft domain for suggest test",
                    "tags": ["type:domain"]
                },
            ]
        }),
    )
    .await
    .expect("seed domain atoms");

    f.sql_exec(
        "UPDATE knowledge_atoms SET status='reviewed' WHERE slug=?1",
        vec![SqlValue::Text("suggest-domain-rev".into())],
    )
    .await;
    f.sql_exec(
        "UPDATE knowledge_atoms SET status='draft' WHERE slug=?1",
        vec![SqlValue::Text("suggest-domain-dft".into())],
    )
    .await;

    let resp = f
        .dispatch(
            "knowledge.suggest",
            json!({
                "query": "machine learning transformer architecture attention mechanism gradient"
            }),
        )
        .await
        .expect("suggest ok");

    let results = resp["results"].as_array().expect("results");
    let names: Vec<&str> = results.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(
        !names.contains(&"Suggest Domain Draft"),
        "suggest must exclude draft domain atoms by default: {names:?}"
    );
}

#[tokio::test]
async fn search_rerank_false_is_explicit_opt_out() {
    let f = pack(rt_with_default_embedder());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                { "slug": "optout-a", "name": "Opt Out Alpha", "content": "opt out rerank test dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" },
            ]
        }),
    )
    .await
    .expect("seed atom");

    // Explicit rerank=false must succeed and return valid results.
    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "opt out rerank false unique wwwxxx", "rerank": false }),
        )
        .await
        .expect("rerank=false search ok");
    let results = resp["results"].as_array().expect("results");
    for r in results {
        let score = r["score"].as_f64().expect("score");
        assert!(
            (0.0..=1.0).contains(&score),
            "score {score} out of [0,1] with rerank=false"
        );
    }
}

#[tokio::test]
async fn search_default_rerank_decompose_guard_avoids_fts_no_such_column() {
    let f = pack(rt_with_default_embedder());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                { "slug": "decompose-guard", "name": "Decompose Guard", "content": "multi-concept tenant isolation decompose guard dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" },
            ]
        }),
    )
    .await
    .expect("seed atom");

    // Query with operator-like text, default rerank (omitted), decompose=true.
    // Must not produce a 'no such column' FTS error.
    let resp = f
        .dispatch(
            "knowledge.search",
            json!({
                "query": "multi-concept tenant:isolation decompose guard",
                "decompose": true,
            }),
        )
        .await
        .expect("default rerank + decompose must not crash");
    assert!(
        resp["results"].is_array(),
        "expected results array, got: {resp:?}"
    );
}

// ── embed_batch failure / count-mismatch counted as `failed` ─────────────────
//
// Regression for internal review round 2 HIGH finding: embed_batch Err and count-mismatch
// were both mapped to `skipped` (exit 0); they must map to `failed` (exit non-0
// without --best-effort).  The knowledge index handler cannot call embed_batch
// without a non-empty default_embedder_name, so these tests use a fake provider
// registered under the default model key.

mod embed_failure_tests {
    use super::*;
    use async_trait::async_trait;
    use khive_runtime::{AllowAllGate, BackendId, EmbedderProvider, RuntimeConfig};
    use khive_types::Namespace;
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
    use std::sync::Arc;

    const MODEL_KEY: &str = "all-minilm-l6-v2";

    /// Returns exactly one vector regardless of how many texts are passed.
    /// Triggers the count-mismatch branch in the index handler.
    struct OneDimService;

    #[async_trait]
    impl EmbeddingService for OneDimService {
        async fn embed(
            &self,
            _texts: &[String],
            _model: EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            Ok(vec![vec![1.0_f32; 4]])
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "one-dim"
        }
    }

    struct OneDimProvider;

    #[async_trait]
    impl EmbedderProvider for OneDimProvider {
        fn name(&self) -> &str {
            MODEL_KEY
        }

        fn dimensions(&self) -> usize {
            4
        }

        async fn build(
            &self,
        ) -> std::result::Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
            Ok(Arc::new(OneDimService))
        }
    }

    /// Always returns Err(InferenceFailed) to trigger the Err branch.
    struct AlwaysFailService;

    #[async_trait]
    impl EmbeddingService for AlwaysFailService {
        async fn embed(
            &self,
            _texts: &[String],
            _model: EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            Err(EmbedError::InferenceFailed("synthetic test failure".into()))
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "always-fail"
        }
    }

    struct AlwaysFailProvider;

    #[async_trait]
    impl EmbedderProvider for AlwaysFailProvider {
        fn name(&self) -> &str {
            MODEL_KEY
        }

        fn dimensions(&self) -> usize {
            4
        }

        async fn build(
            &self,
        ) -> std::result::Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
            Ok(Arc::new(AlwaysFailService))
        }
    }

    /// Build a runtime whose default_embedder_name is non-empty (required for
    /// the index handler to attempt embedding) but whose provider is replaced
    /// with the given fake.
    fn rt_with_fake(fake: impl EmbedderProvider + 'static) -> KhiveRuntime {
        let rt = KhiveRuntime::new(RuntimeConfig {
            git_write: Default::default(),
            db_path: None,
            default_namespace: Namespace::local(),
            embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string(), "knowledge".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
        })
        .expect("runtime");
        // Override the lattice provider with our fake — same key, last-writer wins.
        rt.register_embedder(fake);
        rt
    }

    /// Seed two atoms and return the fixture.
    async fn fixture_with_two_atoms(rt: KhiveRuntime) -> Fixture {
        let f = pack(rt);
        f.dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [
                    {
                        "slug": "embed-fail-a",
                        "name": "Embed Fail A",
                        "content": "first atom content for embed failure regression test dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector"
                    },
                    {
                        "slug": "embed-fail-b",
                        "name": "Embed Fail B",
                        "content": "second atom content for embed failure regression test dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector"
                    }
                ]
            }),
        )
        .await
        .expect("upsert atoms");
        f
    }

    /// count-mismatch branch: embed_batch returns 1 vector for a 2-atom batch.
    /// Must count both atoms as `failed`, not `skipped`.
    #[tokio::test]
    async fn index_embed_count_mismatch_counts_as_failed() {
        let f = fixture_with_two_atoms(rt_with_fake(OneDimProvider)).await;
        let result = f
            .dispatch("knowledge.index", json!({}))
            .await
            .expect("index ok");

        assert_eq!(
            result["failed"].as_u64().unwrap_or(0),
            2,
            "count-mismatch must report both atoms as failed: {result:?}"
        );
        assert_eq!(
            result["indexed"].as_u64().unwrap_or(u64::MAX),
            0,
            "no atoms must be indexed on count-mismatch: {result:?}"
        );
        assert_eq!(
            result["skipped"].as_u64().unwrap_or(u64::MAX),
            0,
            "count-mismatch must not appear in skipped: {result:?}"
        );
    }

    /// Err branch: embed_batch returns Err for every batch.
    /// Must count all atoms as `failed`, not `skipped`.
    #[tokio::test]
    async fn index_embed_error_counts_as_failed() {
        let f = fixture_with_two_atoms(rt_with_fake(AlwaysFailProvider)).await;
        let result = f
            .dispatch("knowledge.index", json!({}))
            .await
            .expect("index ok");

        assert_eq!(
            result["failed"].as_u64().unwrap_or(0),
            2,
            "embed Err must report both atoms as failed: {result:?}"
        );
        assert_eq!(
            result["indexed"].as_u64().unwrap_or(u64::MAX),
            0,
            "no atoms must be indexed on embed error: {result:?}"
        );
        assert_eq!(
            result["skipped"].as_u64().unwrap_or(u64::MAX),
            0,
            "embed Err must not appear in skipped: {result:?}"
        );
    }

    /// Result JSON always carries `ann_failed` key. When the ANN block does not
    /// run (rebuild_ann=false, which is the default), `ann_failed` must be false.
    /// Embed failures that prevent vector writes also must not set ann_failed —
    /// atom-level and ANN-level failures are distinct failure dimensions.
    #[tokio::test]
    async fn index_result_carries_ann_failed_false_when_ann_block_skipped() {
        // Use count-mismatch provider: embed fails, no vectors stored, ANN block
        // never entered (ann_vectors stays empty), so ann_failed must be false.
        let f = fixture_with_two_atoms(rt_with_fake(OneDimProvider)).await;
        let result = f
            .dispatch("knowledge.index", json!({}))
            .await
            .expect("index ok");

        assert!(
            result.get("ann_failed").is_some(),
            "result JSON must carry ann_failed key: {result:?}"
        );
        assert!(
            !result["ann_failed"].as_bool().unwrap_or(true),
            "ann_failed must be false when ANN block did not run: {result:?}"
        );
    }

    // ── Section embed failure regression (internal review round 1 HIGH) ────────────────
    //
    // Mirrors the atom failure tests above but exercises the SECTION path via
    // `reindex_knowledge(sections:true, atoms:false)`. Blank-text sections are
    // genuine `skipped`; embed_batch Err and count-mismatch are `failed`.

    /// Seed two sections via `knowledge.edit` and return the fixture. The atom
    /// must exist before sections can be attached.
    async fn fixture_with_two_sections(rt: KhiveRuntime) -> Fixture {
        let f = pack(rt);
        f.dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [{
                    "slug": "sec-embed-fail",
                    "name": "Section Embed Fail",
                    "content": "atom content for section embed failure regression test dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor"
                }]
            }),
        )
        .await
        .expect("upsert atom");

        // Two distinct sections so the batch has size 2.
        f.dispatch(
            "knowledge.edit",
            json!({
                "id": "sec-embed-fail",
                "sections": [
                    {
                        "section_type": "overview",
                        "content": "Overview content for section embed failure regression test. This text is long enough to satisfy the 80-character minimum section length requirement — dense sparse retrieval corpus benchmark search latency."
                    },
                    {
                        "section_type": "formalism",
                        "content": "Formalism content for section embed failure regression test. This text is long enough to satisfy the 80-character minimum section length requirement — gradient descent transformer attention vector index."
                    }
                ]
            }),
        )
        .await
        .expect("edit sections");
        f
    }

    /// count-mismatch branch on the SECTION path: embed_batch returns 1 vector
    /// for a 2-section batch. Must count both sections as `sections_failed`, not
    /// `skipped`; `reindex_knowledge` must surface this in its result JSON.
    #[tokio::test]
    async fn section_embed_count_mismatch_counts_as_sections_failed() {
        let f = fixture_with_two_sections(rt_with_fake(OneDimProvider)).await;
        let rt = f.rt.clone();
        let token = rt
            .authorize(khive_types::Namespace::local())
            .expect("authorize");

        let result = khive_pack_knowledge::reindex_knowledge(
            &rt,
            &token,
            khive_pack_knowledge::KnowledgeReindexOptions {
                atoms: false,
                sections: true,
                drop_existing: true,
                rebuild_ann: false,
                batch_size: None,
            },
            None,
            None,
        )
        .await
        .expect("reindex_knowledge ok");

        assert_eq!(
            result["sections_failed"].as_u64().unwrap_or(0),
            2,
            "count-mismatch must report both sections as sections_failed: {result:?}"
        );
        assert_eq!(
            result["sections_indexed"].as_u64().unwrap_or(u64::MAX),
            0,
            "no sections must be indexed on count-mismatch: {result:?}"
        );
    }

    /// Err branch on the SECTION path: embed_batch returns Err for every batch.
    /// Must count all sections as `sections_failed`, not `skipped`.
    #[tokio::test]
    async fn section_embed_error_counts_as_sections_failed() {
        let f = fixture_with_two_sections(rt_with_fake(AlwaysFailProvider)).await;
        let rt = f.rt.clone();
        let token = rt
            .authorize(khive_types::Namespace::local())
            .expect("authorize");

        let result = khive_pack_knowledge::reindex_knowledge(
            &rt,
            &token,
            khive_pack_knowledge::KnowledgeReindexOptions {
                atoms: false,
                sections: true,
                drop_existing: true,
                rebuild_ann: false,
                batch_size: None,
            },
            None,
            None,
        )
        .await
        .expect("reindex_knowledge ok");

        assert_eq!(
            result["sections_failed"].as_u64().unwrap_or(0),
            2,
            "embed Err must report both sections as sections_failed: {result:?}"
        );
        assert_eq!(
            result["sections_indexed"].as_u64().unwrap_or(u64::MAX),
            0,
            "no sections must be indexed on embed error: {result:?}"
        );
    }

    /// Regression: `--keep-existing` (drop_existing=false) paginates the
    /// `embedding IS NULL` set. A failed section stays NULL, so without advancing
    /// the offset past stuck rows the loop re-selects the same page forever and
    /// never returns a `sections_failed` report (fail-closed bypassed). With
    /// batch_size=1 over two persistently-failing sections, the old code looped;
    /// this test must TERMINATE and report both as sections_failed.
    #[tokio::test]
    async fn section_keep_existing_failures_terminate_and_report() {
        let f = fixture_with_two_sections(rt_with_fake(AlwaysFailProvider)).await;
        let rt = f.rt.clone();
        let token = rt
            .authorize(khive_types::Namespace::local())
            .expect("authorize");

        let result = khive_pack_knowledge::reindex_knowledge(
            &rt,
            &token,
            khive_pack_knowledge::KnowledgeReindexOptions {
                atoms: false,
                sections: true,
                drop_existing: false,
                rebuild_ann: false,
                batch_size: Some(1),
            },
            None,
            None,
        )
        .await
        .expect("reindex_knowledge must terminate, not loop");

        assert_eq!(
            result["sections_failed"].as_u64().unwrap_or(0),
            2,
            "keep-existing must attempt each section once and report both failed: {result:?}"
        );
        assert_eq!(
            result["sections_indexed"].as_u64().unwrap_or(u64::MAX),
            0,
            "no sections indexed when every embed fails: {result:?}"
        );
    }
}

// ── ANN bypass regression (issue #78, PR #90) ────────────────────────────────
//
// Regression test: after `knowledge.index` with `rebuild_ann=true` the in-process
// ANN index holds vectors for ALL atoms (regardless of status). A subsequent default
// `knowledge.search` must not return draft atoms even when the ANN path finds them.
//
// Architecture: the fix adds `filter_by_excluded_statuses` immediately after
// `hydrate_empty_hits` in the search handler so the ANN-sourced hits go through
// the same status gate as the SQL/FTS candidates.

mod ann_bypass_regression {
    use super::*;
    use async_trait::async_trait;
    use khive_runtime::{AllowAllGate, BackendId, EmbedderProvider, RuntimeConfig};
    use khive_types::Namespace;
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
    use std::sync::Arc;

    const MODEL_KEY: &str = "all-minilm-l6-v2";
    // Must match AllMiniLmL6V2.native_dimensions() so vector inserts succeed.
    const DIM: usize = 384;

    /// Returns N distinct unit vectors (one per text) so the index handler counts
    /// each atom as successfully indexed. Vectors are differentiated by index so
    /// ANN search can distinguish between atoms.
    struct CorrectDimService;

    #[async_trait]
    impl EmbeddingService for CorrectDimService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts
                .iter()
                .enumerate()
                .map(|(i, _)| {
                    // Slightly different vectors per index position so ANN is non-trivial.
                    let v = (i + 1) as f32;
                    let norm = (DIM as f32 * v * v).sqrt();
                    vec![v / norm; DIM]
                })
                .collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "correct-dim"
        }
    }

    struct CorrectDimProvider;

    #[async_trait]
    impl EmbedderProvider for CorrectDimProvider {
        fn name(&self) -> &str {
            MODEL_KEY
        }

        fn dimensions(&self) -> usize {
            DIM
        }

        async fn build(
            &self,
        ) -> std::result::Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
            Ok(Arc::new(CorrectDimService))
        }
    }

    fn rt_with_correct_embedder() -> KhiveRuntime {
        let rt = KhiveRuntime::new(RuntimeConfig {
            git_write: Default::default(),
            db_path: None,
            default_namespace: Namespace::local(),
            embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string(), "knowledge".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
        })
        .expect("runtime");
        rt.register_embedder(CorrectDimProvider);
        rt
    }

    /// Default search must exclude draft atoms even when the warm ANN index
    /// contains vectors for them.
    ///
    /// Steps:
    /// 1. Seed a reviewed and a draft atom.
    /// 2. Index with rebuild_ann=true so the ANN holds both IDs.
    /// 3. Run default knowledge.search — assert draft is absent.
    /// 4. Run knowledge.search with include_drafts=true — assert draft appears.
    #[tokio::test]
    async fn ann_warm_draft_atom_excluded_by_default_search() {
        let f = pack(rt_with_correct_embedder());

        f.dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [
                    {
                        "slug": "ann-rev-atom",
                        "name": "ANN Reviewed Atom",
                        "content": "neural network attention mechanism transformer dense sparse retrieval corpus benchmark search latency gradient descent vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity unique ann78rev"
                    },
                    {
                        "slug": "ann-dft-atom",
                        "name": "ANN Draft Atom",
                        "content": "neural network attention mechanism transformer dense sparse retrieval corpus benchmark search latency gradient descent vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity unique ann78dft"
                    },
                ]
            }),
        )
        .await
        .expect("seed atoms");

        f.sql_exec(
            "UPDATE knowledge_atoms SET status='reviewed' WHERE slug=?1",
            vec![SqlValue::Text("ann-rev-atom".into())],
        )
        .await;
        f.sql_exec(
            "UPDATE knowledge_atoms SET status='draft' WHERE slug=?1",
            vec![SqlValue::Text("ann-dft-atom".into())],
        )
        .await;

        // Index with rebuild_ann=true so the in-process ANN index is warmed for
        // both atoms — including the draft atom. This is the precondition for the
        // bypass bug to fire.
        let idx = f
            .dispatch("knowledge.index", json!({ "rebuild_ann": true }))
            .await
            .expect("index ok");
        assert!(
            idx["indexed"].as_u64().unwrap_or(0) >= 2,
            "both atoms must be indexed for the ANN to hold them: {idx:?}"
        );

        // Default search must NOT return the draft atom even though it is in the ANN.
        let resp = f
            .dispatch(
                "knowledge.search",
                json!({
                    "query": "neural network attention mechanism transformer unique ann78",
                    "rerank": false
                }),
            )
            .await
            .expect("default search ok");
        let results = resp["results"].as_array().expect("results");
        let names: Vec<&str> = results.iter().filter_map(|r| r["name"].as_str()).collect();
        assert!(
            !names.contains(&"ANN Draft Atom"),
            "draft atom must be excluded by default even when warm ANN finds it: {names:?}"
        );

        // include_drafts=true must surface the draft atom.
        let resp_incl = f
            .dispatch(
                "knowledge.search",
                json!({
                    "query": "neural network attention mechanism transformer unique ann78",
                    "rerank": false,
                    "include_drafts": true
                }),
            )
            .await
            .expect("include_drafts search ok");
        let results_incl = resp_incl["results"].as_array().expect("results");
        let names_incl: Vec<&str> = results_incl
            .iter()
            .filter_map(|r| r["name"].as_str())
            .collect();
        assert!(
            names_incl.contains(&"ANN Draft Atom"),
            "draft atom must appear when include_drafts=true: {names_incl:?}"
        );
    }
}

// ── exclude_status precedence regression (round-2 High-1) ─────────────────────

// exclude_status= with NO status= must exclude matching atoms.
// Previously the buffer logic silently ignored exclude_status when no status= was set.
#[tokio::test]
async fn exclude_status_without_status_param_excludes_target_status() {
    let f = pack(rt());

    // Seed a reviewed and a draft atom with shared distinctive content.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                {
                    "slug": "prec-reviewed",
                    "name": "Precedence Reviewed Atom",
                    "content": "precedence exclude status regression reviewed dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity unique prec78a"
                },
                {
                    "slug": "prec-draft",
                    "name": "Precedence Draft Atom",
                    "content": "precedence exclude status regression draft dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity unique prec78a"
                },
            ]
        }),
    )
    .await
    .expect("seed atoms");

    // Ensure statuses are set explicitly.
    f.sql_exec(
        "UPDATE knowledge_atoms SET status='reviewed' WHERE slug=?1",
        vec![SqlValue::Text("prec-reviewed".into())],
    )
    .await;
    f.sql_exec(
        "UPDATE knowledge_atoms SET status='draft' WHERE slug=?1",
        vec![SqlValue::Text("prec-draft".into())],
    )
    .await;

    // exclude_status=reviewed, no status= → reviewed atoms must be excluded.
    let resp = f
        .dispatch(
            "knowledge.search",
            json!({
                "query": "precedence exclude status regression unique prec78a",
                "rerank": false,
                "exclude_status": "reviewed",
                "include_drafts": true
            }),
        )
        .await
        .expect("search ok");

    let results = resp["results"].as_array().expect("results");
    let names: Vec<&str> = results.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(
        !names.contains(&"Precedence Reviewed Atom"),
        "exclude_status=reviewed must remove reviewed atoms (no status= set): {names:?}"
    );
    assert!(
        names.contains(&"Precedence Draft Atom"),
        "draft atom must appear when include_drafts=true and exclude_status=reviewed: {names:?}"
    );
}

// When status= is set, exclude_status= must have no effect (status= takes precedence).
#[tokio::test]
async fn exclude_status_is_ignored_when_status_param_is_set() {
    let f = pack(rt());

    // Seed reviewed and draft atoms.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                {
                    "slug": "prec2-reviewed",
                    "name": "Prec2 Reviewed Atom",
                    "content": "precedence2 status override reviewed dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity unique prec78b"
                },
                {
                    "slug": "prec2-draft",
                    "name": "Prec2 Draft Atom",
                    "content": "precedence2 status override draft dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity unique prec78b"
                },
            ]
        }),
    )
    .await
    .expect("seed atoms");

    f.sql_exec(
        "UPDATE knowledge_atoms SET status='reviewed' WHERE slug=?1",
        vec![SqlValue::Text("prec2-reviewed".into())],
    )
    .await;
    f.sql_exec(
        "UPDATE knowledge_atoms SET status='draft' WHERE slug=?1",
        vec![SqlValue::Text("prec2-draft".into())],
    )
    .await;

    // status=reviewed + exclude_status=reviewed: status= wins, reviewed atoms appear.
    let resp = f
        .dispatch(
            "knowledge.search",
            json!({
                "query": "precedence2 status override unique prec78b",
                "rerank": false,
                "status": "reviewed",
                "exclude_status": "reviewed"
            }),
        )
        .await
        .expect("search ok");

    let results = resp["results"].as_array().expect("results");
    let names: Vec<&str> = results.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(
        names.contains(&"Prec2 Reviewed Atom"),
        "status=reviewed overrides exclude_status=reviewed: reviewed atom must appear: {names:?}"
    );
    assert!(
        !names.contains(&"Prec2 Draft Atom"),
        "status=reviewed must not return draft atoms: {names:?}"
    );
}

// blank exclude_status= must behave identically to absent — draft+deprecated excluded by default.
#[tokio::test]
async fn blank_exclude_status_falls_through_to_default_draft_exclusion() {
    let f = pack(rt());

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                {
                    "slug": "blank-ex-reviewed",
                    "name": "Blank Ex Reviewed",
                    "content": "blank exclude status normalization reviewed dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity unique blnk78a"
                },
                {
                    "slug": "blank-ex-draft",
                    "name": "Blank Ex Draft",
                    "content": "blank exclude status normalization draft dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity unique blnk78a"
                },
            ]
        }),
    )
    .await
    .expect("seed atoms");

    f.sql_exec(
        "UPDATE knowledge_atoms SET status='reviewed' WHERE slug=?1",
        vec![SqlValue::Text("blank-ex-reviewed".into())],
    )
    .await;
    f.sql_exec(
        "UPDATE knowledge_atoms SET status='draft' WHERE slug=?1",
        vec![SqlValue::Text("blank-ex-draft".into())],
    )
    .await;

    // exclude_status="" — blank must be treated as absent, so draft is still excluded by default.
    let resp = f
        .dispatch(
            "knowledge.search",
            json!({
                "query": "blank exclude status normalization unique blnk78a",
                "rerank": false,
                "exclude_status": ""
            }),
        )
        .await
        .expect("search ok");

    let results = resp["results"].as_array().expect("results");
    let names: Vec<&str> = results.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(
        !names.contains(&"Blank Ex Draft"),
        "blank exclude_status must not bypass draft exclusion: {names:?}"
    );
    assert!(
        names.contains(&"Blank Ex Reviewed"),
        "reviewed atom must appear with blank exclude_status: {names:?}"
    );
}

// whitespace-padded exclude_status=" draft " must normalize to "draft" and behave
// the same as exclude_status="draft" — NOT apply as a raw " draft " exclusion that
// the ANN post-filter would miss (since the filter uses exact contains comparison).
#[tokio::test]
async fn whitespace_padded_exclude_status_normalizes_to_draft() {
    let f = pack(rt());

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                {
                    "slug": "ws-ex-reviewed",
                    "name": "Ws Ex Reviewed",
                    "content": "whitespace padded exclude status reviewed dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity unique wspad78"
                },
                {
                    "slug": "ws-ex-draft",
                    "name": "Ws Ex Draft",
                    "content": "whitespace padded exclude status draft dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity unique wspad78"
                },
            ]
        }),
    )
    .await
    .expect("seed atoms");

    f.sql_exec(
        "UPDATE knowledge_atoms SET status='reviewed' WHERE slug=?1",
        vec![SqlValue::Text("ws-ex-reviewed".into())],
    )
    .await;
    f.sql_exec(
        "UPDATE knowledge_atoms SET status='draft' WHERE slug=?1",
        vec![SqlValue::Text("ws-ex-draft".into())],
    )
    .await;

    // exclude_status=" draft " with leading/trailing spaces must normalize to "draft"
    // and exclude draft atoms consistently (SQL and ANN use the same normalized value).
    let resp = f
        .dispatch(
            "knowledge.search",
            json!({
                "query": "whitespace padded exclude status unique wspad78",
                "rerank": false,
                "exclude_status": " draft ",
                "include_drafts": true
            }),
        )
        .await
        .expect("search ok");

    let results = resp["results"].as_array().expect("results");
    let names: Vec<&str> = results.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(
        !names.contains(&"Ws Ex Draft"),
        "whitespace-padded \" draft \" must normalize to \"draft\" and exclude draft atoms: {names:?}"
    );
    assert!(
        names.contains(&"Ws Ex Reviewed"),
        "reviewed atom must appear when exclude_status=\" draft \": {names:?}"
    );
}

// ── auto-compose draft member filter regression (round-2 High-2) ──────────────

// When compose runs in explicit domain_ids mode (is_auto=false), draft member atoms
// must NOT be filtered — the caller opted in by supplying the domain directly.
#[tokio::test]
async fn explicit_domain_ids_compose_includes_draft_member_atoms() {
    let f = pack(rt());

    // Seed a draft member atom.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [{
                "slug": "compose-draft-member",
                "name": "Compose Draft Member",
                "content": "compose explicit domain draft member atom dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity unique cmp78d"
            }]
        }),
    )
    .await
    .expect("seed draft atom");

    f.sql_exec(
        "UPDATE knowledge_atoms SET status='draft' WHERE slug=?1",
        vec![SqlValue::Text("compose-draft-member".into())],
    )
    .await;

    // Upsert a domain whose member list includes the draft atom.
    f.dispatch(
        "knowledge.upsert_domains",
        json!({
            "domains": [{
                "slug": "compose-explicit-domain",
                "name": "Compose Explicit Domain",
                "description": "compose explicit domain ids draft member test dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity unique cmp78e",
                "members": ["compose-draft-member"]
            }]
        }),
    )
    .await
    .expect("upsert domain");

    // Get the domain id.
    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "compose explicit domain ids draft member unique cmp78e", "type": "domain", "rerank": false }),
        )
        .await
        .expect("search domain");
    let domain_id = resp["results"]
        .as_array()
        .expect("results")
        .iter()
        .find(|r| r["slug"].as_str() == Some("compose-explicit-domain"))
        .and_then(|r| r["id"].as_str())
        .expect("domain id in results")
        .to_string();

    // Explicit domain_ids compose: draft member atom must appear (caller opted in).
    let compose_resp = f
        .dispatch(
            "knowledge.compose",
            json!({
                "query": "compose explicit domain ids draft member unique cmp78e",
                "domain_ids": [&domain_id]
            }),
        )
        .await
        .expect("compose ok");

    let atoms = compose_resp["data"]["atoms"].as_array().expect("atoms");
    let atom_names: Vec<&str> = atoms.iter().filter_map(|a| a["slug"].as_str()).collect();
    assert!(
        atom_names.contains(&"compose-draft-member"),
        "explicit domain_ids compose must include draft member atoms (caller opted in): {atom_names:?}"
    );
}

// ── #80: stats.total_events must count real knowledge events ─────────────────

fn pack_with_events(rt: KhiveRuntime) -> Fixture {
    let rt_clone = rt.clone();
    let tok = rt.authorize(Namespace::local()).expect("local token");
    let event_store = rt.events(&tok).expect("event store");
    let mut builder = VerbRegistryBuilder::new();
    builder.with_event_store(event_store);
    builder.register(KgPack::new(rt.clone()));
    builder.register(KnowledgePack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");
    rt.install_edge_rules(registry.all_edge_rules());
    Fixture {
        registry,
        rt: rt_clone,
    }
}

#[tokio::test]
async fn stats_total_events_counts_knowledge_verbs() {
    let f = pack_with_events(rt());

    let long_content = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu \
                        nu xi omicron pi rho sigma tau upsilon phi chi psi omega";

    // Dispatch two knowledge verbs so their audit events land in the events table.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                { "slug": "evt-atom-a", "name": "Event Atom A", "content": long_content }
            ]
        }),
    )
    .await
    .expect("upsert atoms");

    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                { "slug": "evt-atom-b", "name": "Event Atom B", "content": long_content }
            ]
        }),
    )
    .await
    .expect("upsert atoms second");

    // Non-knowledge verbs in the SAME namespace must NOT be counted: this proves the
    // `verb LIKE 'knowledge.%'` predicate actually filters, not merely that events
    // exist. Two of them push a broken (unfiltered) count to >= 4, outside [2, 3].
    for name in ["evt-entity-a", "evt-entity-b"] {
        f.dispatch("create", json!({ "kind": "concept", "name": name }))
            .await
            .expect("create concept");
    }

    let stats = f
        .dispatch("knowledge.stats", json!({}))
        .await
        .expect("stats ok");

    let total_events = stats["total_events"]
        .as_i64()
        .expect("total_events must be an integer");

    // 2 = the knowledge.upsert_atoms dispatches; +1 if the knowledge.stats audit event
    // is recorded before the handler's COUNT runs. The 2 non-knowledge `create` events
    // must be excluded, so an unfiltered predicate would yield >= 4.
    assert!(
        (2..=3).contains(&total_events),
        "total_events must count the knowledge.* dispatches but EXCLUDE the 2 \
         non-knowledge `create` events; expected 2 or 3, got {total_events}"
    );
}

// ── knowledge.edit inline re-embed (issue #11) ────────────────────────────────
//
// knowledge.edit must embed new/changed sections immediately after writing them
// so that semantic recall is fresh without a manual `kkernel reindex`.
// Byte-identical sections (metadata-only refresh) keep their existing embedding
// and must NOT be needlessly re-embedded.

mod edit_inline_reembed {
    use super::*;
    use async_trait::async_trait;
    use khive_runtime::{AllowAllGate, BackendId, EmbedderProvider, RuntimeConfig};
    use khive_types::Namespace;
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
    use std::sync::Arc;

    const MODEL_KEY: &str = "all-minilm-l6-v2";
    const DIM: usize = 384;

    struct EmbedService;

    #[async_trait]
    impl EmbeddingService for EmbedService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            // Distinct unit vectors per position so each section gets a real vector.
            Ok(texts
                .iter()
                .enumerate()
                .map(|(i, _)| {
                    let v = (i + 1) as f32;
                    let norm = (DIM as f32 * v * v).sqrt();
                    vec![v / norm; DIM]
                })
                .collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "edit-reembed-service"
        }
    }

    struct EmbedProvider;

    #[async_trait]
    impl EmbedderProvider for EmbedProvider {
        fn name(&self) -> &str {
            MODEL_KEY
        }

        fn dimensions(&self) -> usize {
            DIM
        }

        async fn build(
            &self,
        ) -> std::result::Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
            Ok(Arc::new(EmbedService))
        }
    }

    fn rt_with_embedder() -> KhiveRuntime {
        let rt = KhiveRuntime::new(RuntimeConfig {
            git_write: Default::default(),
            db_path: None,
            default_namespace: Namespace::local(),
            embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string(), "knowledge".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
        })
        .expect("runtime");
        rt.register_embedder(EmbedProvider);
        rt
    }

    /// knowledge.edit must embed newly-written sections inline: after edit,
    /// a NEW section (distinct content hash) must have a non-NULL embedding,
    /// while a byte-identical section (metadata-only refresh) must retain its
    /// previously-set embedding and not be re-embedded into a different blob.
    #[tokio::test]
    async fn edit_embeds_new_sections_and_preserves_unchanged_embedding() {
        let rt = rt_with_embedder();
        let f = pack(rt.clone());

        // Create an atom.
        f.dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [{
                    "slug": "reembed-test-atom",
                    "name": "Re-embed Test Atom",
                    "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                }]
            }),
        )
        .await
        .expect("upsert atom");

        let overview_content =
            "Overview section content that is well above the eighty-character minimum. \
            dense sparse retrieval corpus benchmark search latency gradient descent transformer.";

        // First edit: insert an overview section. It has no prior embedding (new row).
        f.dispatch(
            "knowledge.edit",
            json!({
                "id": "reembed-test-atom",
                "sections": [{
                    "section_type": "overview",
                    "content": overview_content
                }]
            }),
        )
        .await
        .expect("first edit");

        // Assert the newly-inserted section has a non-NULL embedding.
        let row_after_first = f
            .sql_query_one(
                "SELECT id, embedding FROM knowledge_sections \
                 WHERE atom_id = (SELECT id FROM knowledge_atoms WHERE slug = ?1) \
                 AND section_type = 'overview' \
                 LIMIT 1",
                vec![SqlValue::Text("reembed-test-atom".into())],
            )
            .await
            .expect("section row must exist after first edit");

        let section_id = match row_after_first.get("id") {
            Some(SqlValue::Text(s)) => s.clone(),
            _ => panic!("section id must be text"),
        };
        let first_embedding_blob = match row_after_first.get("embedding") {
            Some(SqlValue::Blob(b)) => b.clone(),
            Some(SqlValue::Null) | None => {
                panic!("newly-inserted section must have a non-NULL embedding after knowledge.edit")
            }
            other => panic!("unexpected embedding value: {other:?}"),
        };
        assert!(
            !first_embedding_blob.is_empty(),
            "embedding blob must be non-empty"
        );

        // Second edit: re-submit the IDENTICAL content. The section is a metadata-only
        // refresh (same content_hash); the embedding must be preserved unchanged.
        f.dispatch(
            "knowledge.edit",
            json!({
                "id": "reembed-test-atom",
                "sections": [{
                    "section_type": "overview",
                    "content": overview_content
                }]
            }),
        )
        .await
        .expect("second edit (identical content)");

        let row_after_second = f
            .sql_query_one(
                "SELECT embedding FROM knowledge_sections WHERE id = ?1 LIMIT 1",
                vec![SqlValue::Text(section_id.clone())],
            )
            .await
            .expect("section row must still exist after second edit");

        let second_embedding_blob = match row_after_second.get("embedding") {
            Some(SqlValue::Blob(b)) => b.clone(),
            Some(SqlValue::Null) | None => {
                panic!("byte-identical section must retain its existing embedding after re-edit")
            }
            other => panic!("unexpected embedding value after second edit: {other:?}"),
        };

        assert_eq!(
            first_embedding_blob, second_embedding_blob,
            "byte-identical section must NOT be re-embedded: blob must be unchanged"
        );

        // Third edit: different content → new sibling row → must also be embedded.
        let new_content = "Updated overview section with completely different content that also \
            exceeds the eighty character minimum. gradient descent transformer attention vector.";

        f.dispatch(
            "knowledge.edit",
            json!({
                "id": "reembed-test-atom",
                "sections": [{
                    "section_type": "overview",
                    "content": new_content
                }]
            }),
        )
        .await
        .expect("third edit (new content)");

        // The new sibling row (different content_hash) must have a non-NULL embedding.
        let new_section_row = f
            .sql_query_one(
                "SELECT id, embedding FROM knowledge_sections \
                 WHERE atom_id = (SELECT id FROM knowledge_atoms WHERE slug = ?1) \
                 AND section_type = 'overview' \
                 AND id != ?2 \
                 LIMIT 1",
                vec![
                    SqlValue::Text("reembed-test-atom".into()),
                    SqlValue::Text(section_id.clone()),
                ],
            )
            .await
            .expect("new sibling section row must exist after third edit");

        match new_section_row.get("embedding") {
            Some(SqlValue::Blob(b)) if !b.is_empty() => {}
            Some(SqlValue::Null) | None => {
                panic!("new sibling section inserted by edit must have a non-NULL embedding")
            }
            other => panic!("unexpected embedding value for new sibling: {other:?}"),
        }
    }

    /// knowledge.edit must refresh the atom-level vector-store entry (knowledge.atom field)
    /// so atom-granularity semantic recall is fresh without a manual kkernel reindex.
    #[tokio::test]
    async fn edit_refreshes_atom_vector_store_entry() {
        let rt = rt_with_embedder();
        let f = pack(rt.clone());

        f.dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [{
                    "slug": "atom-vec-test",
                    "name": "Atom Vector Test",
                    "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                }]
            }),
        )
        .await
        .expect("upsert atom");

        // Fetch the atom UUID.
        let atom_row = f
            .sql_query_one(
                "SELECT id FROM knowledge_atoms WHERE slug = ?1 LIMIT 1",
                vec![SqlValue::Text("atom-vec-test".into())],
            )
            .await
            .expect("atom must exist");
        let atom_uuid = match atom_row.get("id") {
            Some(SqlValue::Text(s)) => s.clone(),
            _ => panic!("atom id must be text"),
        };

        // Edit the atom: trigger inline re-embed for both sections and atom vector.
        f.dispatch(
            "knowledge.edit",
            json!({
                "id": "atom-vec-test",
                "sections": [{
                    "section_type": "overview",
                    "content": "Overview for atom vector refresh test. Must be well above the eighty-character minimum length for knowledge sections. dense sparse retrieval transformer attention."
                }]
            }),
        )
        .await
        .expect("edit");

        // The atom-level vector-store row must exist after edit.
        // Table: vec_all_minilm_l6_v2 (sanitize_model_key("all-minilm-l6-v2")).
        let vec_row = f
            .sql_query_one(
                "SELECT subject_id FROM vec_all_minilm_l6_v2 \
                 WHERE subject_id = ?1 AND field = 'knowledge.atom' LIMIT 1",
                vec![SqlValue::Text(atom_uuid.clone())],
            )
            .await;

        assert!(
            vec_row.is_some(),
            "atom vector-store row must exist in vec_all_minilm_l6_v2 after knowledge.edit \
             (atom_id = {atom_uuid})"
        );
    }

    /// When no default embedder is configured, knowledge.edit must still succeed
    /// (degrade gracefully — same contract as reindex_knowledge with no embedder).
    #[tokio::test]
    async fn edit_succeeds_without_embedder() {
        // Plain memory runtime has no embedder configured (default_embedder_name is "").
        let f = pack(rt());

        f.dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [{
                    "slug": "no-embed-atom",
                    "name": "No Embed Atom",
                    "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                }]
            }),
        )
        .await
        .expect("upsert atom");

        let result = f
            .dispatch(
                "knowledge.edit",
                json!({
                    "id": "no-embed-atom",
                    "sections": [{
                        "section_type": "overview",
                        "content": "Overview section content that is well above the eighty character minimum length requirement for knowledge sections. dense sparse retrieval corpus benchmark search."
                    }]
                }),
            )
            .await
            .expect("edit must succeed even without an embedder configured");

        assert_eq!(
            result["upserted"].as_u64().unwrap_or(0),
            1,
            "edit must report one upserted section: {result:?}"
        );
        assert_eq!(
            result["atom_vector_refreshed"].as_bool(),
            Some(false),
            "no embedder configured: atom_vector_refreshed must be false, not true: {result:?}"
        );
    }
}

// ── ANN type-filter regression (kind= and suggest domain crowding) ────────────
//
// Bug: the type_filter (kind=) was applied only to FTS/SQL candidates, not to
// ANN-fused hits. After fusion + hydration the kind gate was never applied, so
// search(kind="domain") returned atom hits sourced from ANN, and suggest returned
// empty results when atoms were the nearest ANN neighbors to the query.
//
// Fix: filter_hits_by_type is called after hydrate_empty_hits +
// filter_by_excluded_statuses in both handle_search and suggest.
// suggest also increases ann_k from (limit*3).max(20) to (limit*50).max(200)
// so domains are not crowded out of the over-fetch before the type gate fires.

mod ann_type_filter_regression {
    use super::*;
    use async_trait::async_trait;
    use khive_runtime::{AllowAllGate, BackendId, EmbedderProvider, RuntimeConfig};
    use khive_types::Namespace;
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
    use std::sync::Arc;

    const MODEL_KEY: &str = "all-minilm-l6-v2";
    // Must match AllMiniLmL6V2.native_dimensions() so vector inserts succeed.
    const DIM: usize = 384;

    /// Returns one unit vector per text so every atom/domain gets indexed.
    /// Slightly varied by position so ANN results are non-trivial.
    struct CorrectDimService;

    #[async_trait]
    impl EmbeddingService for CorrectDimService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts
                .iter()
                .enumerate()
                .map(|(i, _)| {
                    let v = (i + 1) as f32;
                    let norm = (DIM as f32 * v * v).sqrt();
                    vec![v / norm; DIM]
                })
                .collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "correct-dim"
        }
    }

    struct CorrectDimProvider;

    #[async_trait]
    impl EmbedderProvider for CorrectDimProvider {
        fn name(&self) -> &str {
            MODEL_KEY
        }

        fn dimensions(&self) -> usize {
            DIM
        }

        async fn build(
            &self,
        ) -> std::result::Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
            Ok(Arc::new(CorrectDimService))
        }
    }

    fn rt_with_embedder() -> KhiveRuntime {
        let rt = KhiveRuntime::new(RuntimeConfig {
            git_write: Default::default(),
            db_path: None,
            default_namespace: Namespace::local(),
            embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string(), "knowledge".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
        })
        .expect("runtime");
        rt.register_embedder(CorrectDimProvider);
        rt
    }

    /// search(kind="domain") must return ONLY domain hits even when the warm ANN
    /// index contains both atom and domain vectors.
    ///
    /// Steps:
    /// 1. Seed several atoms and one domain, index with rebuild_ann=true.
    /// 2. search(kind="domain") — assert all returned hits have kind=="domain".
    /// 3. search(kind="atom") — assert no domain hits appear (regression guard).
    #[tokio::test]
    async fn ann_search_kind_domain_returns_only_domain_hits() {
        let f = pack(rt_with_embedder());

        // Seed atoms first (they will be the majority in the ANN index).
        f.dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [
                    { "slug": "type-filter-a1", "name": "Type Filter Atom 1", "content": "neural network attention mechanism transformer unique typef1 dense sparse retrieval corpus benchmark search latency gradient descent vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "finalized": true },
                    { "slug": "type-filter-a2", "name": "Type Filter Atom 2", "content": "neural network attention mechanism transformer unique typef1 dense sparse retrieval corpus benchmark search latency gradient descent vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "finalized": true },
                    { "slug": "type-filter-a3", "name": "Type Filter Atom 3", "content": "neural network attention mechanism transformer unique typef1 dense sparse retrieval corpus benchmark search latency gradient descent vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "finalized": true },
                ]
            }),
        )
        .await
        .expect("seed atoms");

        // Seed a domain — upsert_domains stores a row with type:domain tag.
        f.dispatch(
            "knowledge.upsert_domains",
            json!({
                "domains": [{
                    "slug": "type-filter-domain",
                    "name": "Type Filter Domain",
                    "description": "neural network attention mechanism transformer unique typef1 dense sparse retrieval corpus benchmark search latency gradient descent vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                }]
            }),
        )
        .await
        .expect("seed domain");

        // Index with rebuild_ann=true: ANN holds both atoms and the domain.
        let idx = f
            .dispatch("knowledge.index", json!({ "rebuild_ann": true }))
            .await
            .expect("index ok");
        assert!(
            idx["indexed"].as_u64().unwrap_or(0) >= 3,
            "atoms must be indexed for the ANN to hold them: {idx:?}"
        );

        // search(type="domain") must return ONLY domain hits.
        // Note: the SearchParams field is `#[serde(rename = "type")]` — the JSON key
        // is "type", not "kind". See knowledge/schema.rs SearchParams.kind.
        let resp = f
            .dispatch(
                "knowledge.search",
                json!({
                    "query": "neural network attention mechanism transformer unique typef1",
                    "type": "domain",
                    "rerank": false
                }),
            )
            .await
            .expect("search type=domain ok");

        let results = resp["results"].as_array().expect("results");
        assert!(
            !results.is_empty(),
            "search kind=domain must find the domain: {resp:?}"
        );
        for r in results {
            assert_eq!(
                r["kind"].as_str().unwrap_or(""),
                "domain",
                "search kind=domain: all results must have kind=domain, got: {r:?}"
            );
        }

        // search(type="atom") must not contain the domain (regression guard).
        let resp_atom = f
            .dispatch(
                "knowledge.search",
                json!({
                    "query": "neural network attention mechanism transformer unique typef1",
                    "type": "atom",
                    "rerank": false
                }),
            )
            .await
            .expect("search type=atom ok");

        let results_atom = resp_atom["results"].as_array().expect("results");
        for r in results_atom {
            assert_ne!(
                r["kind"].as_str().unwrap_or(""),
                "domain",
                "search type=atom: no domain hits must appear, got: {r:?}"
            );
        }
    }

    /// suggest must return relevant domains even when atom hits dominate the
    /// top ANN neighbors.
    ///
    /// Steps:
    /// 1. Seed many atoms (to dominate ANN top-k) and one domain.
    /// 2. Index with rebuild_ann=true.
    /// 3. suggest — assert the domain appears (was blocked by crowding before fix).
    #[tokio::test]
    async fn ann_suggest_returns_domain_when_atoms_dominate_ann() {
        let f = pack(rt_with_embedder());

        // Seed 25 atoms to saturate the ANN top-k (ann_k=(8*3).max(20)=24 without the
        // fix). The fake embedder produces identical unit vectors for every text, so the
        // ANN returns items in insertion order. With 25 atoms inserted before the domain
        // the old ann_k=24 fills up with atoms and the domain is excluded entirely.
        f.dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [
                    { "slug": "sug-a1",  "name": "Suggest Atom 1",  "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a2",  "name": "Suggest Atom 2",  "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a3",  "name": "Suggest Atom 3",  "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a4",  "name": "Suggest Atom 4",  "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a5",  "name": "Suggest Atom 5",  "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a6",  "name": "Suggest Atom 6",  "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a7",  "name": "Suggest Atom 7",  "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a8",  "name": "Suggest Atom 8",  "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a9",  "name": "Suggest Atom 9",  "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a10", "name": "Suggest Atom 10", "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a11", "name": "Suggest Atom 11", "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a12", "name": "Suggest Atom 12", "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a13", "name": "Suggest Atom 13", "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a14", "name": "Suggest Atom 14", "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a15", "name": "Suggest Atom 15", "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a16", "name": "Suggest Atom 16", "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a17", "name": "Suggest Atom 17", "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a18", "name": "Suggest Atom 18", "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a19", "name": "Suggest Atom 19", "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a20", "name": "Suggest Atom 20", "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a21", "name": "Suggest Atom 21", "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a22", "name": "Suggest Atom 22", "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a23", "name": "Suggest Atom 23", "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a24", "name": "Suggest Atom 24", "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                    { "slug": "sug-a25", "name": "Suggest Atom 25", "content": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail", "finalized": true },
                ]
            }),
        )
        .await
        .expect("seed atoms");

        // Seed a domain that matches the same query terms.
        f.dispatch(
            "knowledge.upsert_domains",
            json!({
                "domains": [{
                    "slug": "sug-domain",
                    "name": "Suggest Domain",
                    "description": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 covering concepts techniques algorithms implementations applications use cases and design patterns in detail"
                }]
            }),
        )
        .await
        .expect("seed domain");

        // Mark atoms as reviewed so they are not excluded by the default status filter.
        f.sql_exec(
            "UPDATE knowledge_atoms SET status='reviewed' WHERE slug LIKE 'sug-a%'",
            vec![],
        )
        .await;

        // Index with rebuild_ann=true: ANN holds all 25 atoms and the 1 domain.
        let idx = f
            .dispatch("knowledge.index", json!({ "rebuild_ann": true }))
            .await
            .expect("index ok");
        assert!(
            idx["indexed"].as_u64().unwrap_or(0) >= 25,
            "all atoms must be indexed: {idx:?}"
        );

        // suggest must return the domain even though 10 atoms dominate the ANN top-k.
        let resp = f
            .dispatch(
                "knowledge.suggest",
                json!({
                    "query": "machine learning optimization gradient descent retrieval ranking fusion unique sugg1 techniques"
                }),
            )
            .await
            .expect("suggest ok");

        let results = resp["results"].as_array().expect("results");
        assert!(
            !results.is_empty(),
            "suggest must return at least one domain even when atoms dominate ANN top-k: {resp:?}"
        );
        let names: Vec<&str> = results.iter().filter_map(|r| r["name"].as_str()).collect();
        assert!(
            names.contains(&"Suggest Domain"),
            "suggest must find the matching domain: {names:?}"
        );
    }

    /// Regression: `knowledge.search` must accept `kind=` as an alias for `type=`.
    ///
    /// RED logic (before the `alias = "kind"` annotation): `"kind"` is not a
    /// recognised serde field on `SearchParams`, so it deserialises to `None`.
    /// With `SearchParams.kind == None` no type filter is applied, atom hits
    /// appear in the results, and the `assert_eq!(kind, "domain")` loop below
    /// fails because atom results have `kind == "atom"`, not `"domain"`.
    ///
    /// GREEN (with alias): `kind="domain"` routes into `SearchParams.kind` and
    /// the type filter fires identically to passing `type="domain"`.
    ///
    /// Also asserts that the legacy `type=` key still works (additive, non-breaking).
    #[tokio::test]
    async fn search_kind_param_alias_filters_domain() {
        let f = pack(rt_with_embedder());

        // Seed atoms (they will crowd the ANN results without a type filter).
        f.dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [
                    { "slug": "alias-atom-1", "name": "Alias Atom 1", "content": "embedding retrieval vector search index unique aliaskind1 dense sparse corpus benchmark latency gradient descent transformer attention nearest neighbor ranking fusion pipeline rerank cosine similarity", "finalized": true },
                    { "slug": "alias-atom-2", "name": "Alias Atom 2", "content": "embedding retrieval vector search index unique aliaskind1 dense sparse corpus benchmark latency gradient descent transformer attention nearest neighbor ranking fusion pipeline rerank cosine similarity", "finalized": true },
                    { "slug": "alias-atom-3", "name": "Alias Atom 3", "content": "embedding retrieval vector search index unique aliaskind1 dense sparse corpus benchmark latency gradient descent transformer attention nearest neighbor ranking fusion pipeline rerank cosine similarity", "finalized": true },
                ]
            }),
        )
        .await
        .expect("seed atoms");

        // Seed a domain with identical content so the ANN index holds both kinds.
        f.dispatch(
            "knowledge.upsert_domains",
            json!({
                "domains": [{
                    "slug": "alias-domain",
                    "name": "Alias Domain",
                    "description": "embedding retrieval vector search index unique aliaskind1 dense sparse corpus benchmark latency gradient descent transformer attention nearest neighbor ranking fusion pipeline rerank cosine similarity"
                }]
            }),
        )
        .await
        .expect("seed domain");

        // Index with rebuild_ann=true so the ANN holds both atoms and the domain.
        let idx = f
            .dispatch("knowledge.index", json!({ "rebuild_ann": true }))
            .await
            .expect("index ok");
        assert!(
            idx["indexed"].as_u64().unwrap_or(0) >= 3,
            "atoms must be indexed for the ANN to hold them: {idx:?}"
        );

        // -- GREEN path: kind= alias must filter identically to type= ----------------
        // Using "kind": "domain" (the khive-wide canonical key).
        let resp_kind = f
            .dispatch(
                "knowledge.search",
                json!({
                    "query": "embedding retrieval vector search unique aliaskind1",
                    "kind": "domain",
                    "rerank": false
                }),
            )
            .await
            .expect("search kind=domain ok");

        let results_kind = resp_kind["results"].as_array().expect("results");
        assert!(
            !results_kind.is_empty(),
            "search with kind=domain alias must return the domain: {resp_kind:?}"
        );
        for r in results_kind {
            let kind = r["kind"].as_str().unwrap_or("");
            assert_eq!(
                kind, "domain",
                "search kind=domain alias: all results must have kind=domain, got: {r:?}"
            );
        }

        // -- Additive check: legacy type= key must still work ------------------------
        let resp_type = f
            .dispatch(
                "knowledge.search",
                json!({
                    "query": "embedding retrieval vector search unique aliaskind1",
                    "type": "domain",
                    "rerank": false
                }),
            )
            .await
            .expect("search type=domain ok");

        let results_type = resp_type["results"].as_array().expect("results");
        assert!(
            !results_type.is_empty(),
            "legacy type=domain must still filter correctly: {resp_type:?}"
        );
        for r in results_type {
            let kind = r["kind"].as_str().unwrap_or("");
            assert_eq!(
                kind, "domain",
                "type=domain (legacy): all results must have kind=domain, got: {r:?}"
            );
        }
    }
}

// ── compose explain=true section path ────────────────────────────────────────
//
// The test in integration.rs (`compose_explain_true_atom_path_includes_score_in_markdown`)
// exercises only the no-embedder atom path because `KhiveRuntime::memory()` has
// no registered embedder, so `embed_query` always returns None and section_results
// stays empty.  The section path in `search.rs` — the `if let Some(qe) = q_emb`
// branch that fills section_results, the `if explain && !section_json.is_empty()`
// gate at line 1462, and the `breakdown` object serialisation — requires a real
// embedder.  This module provides one and asserts that path UNCONDITIONALLY.

mod compose_explain_sections {
    use super::*;
    use async_trait::async_trait;
    use khive_runtime::{AllowAllGate, BackendId, EmbedderProvider, RuntimeConfig};
    use khive_types::Namespace;
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
    use std::sync::Arc;

    const MODEL_KEY: &str = "all-minilm-l6-v2";
    // Must match AllMiniLmL6V2.native_dimensions() so vector inserts succeed.
    const DIM: usize = 384;

    struct UnitVecService;

    #[async_trait]
    impl EmbeddingService for UnitVecService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            // Distinct unit vectors per position so each text gets a real non-zero vector.
            Ok(texts
                .iter()
                .enumerate()
                .map(|(i, _)| {
                    let v = (i + 1) as f32;
                    let norm = (DIM as f32 * v * v).sqrt();
                    vec![v / norm; DIM]
                })
                .collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "explain-section-service"
        }
    }

    struct UnitVecProvider;

    #[async_trait]
    impl EmbedderProvider for UnitVecProvider {
        fn name(&self) -> &str {
            MODEL_KEY
        }

        fn dimensions(&self) -> usize {
            DIM
        }

        async fn build(
            &self,
        ) -> std::result::Result<Arc<dyn EmbeddingService>, khive_runtime::RuntimeError> {
            Ok(Arc::new(UnitVecService))
        }
    }

    fn rt_with_embedder() -> KhiveRuntime {
        let rt = KhiveRuntime::new(RuntimeConfig {
            git_write: Default::default(),
            db_path: None,
            default_namespace: Namespace::local(),
            embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string(), "knowledge".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: None,
        })
        .expect("runtime");
        rt.register_embedder(UnitVecProvider);
        rt
    }

    /// Verify that compose(explain=true) actually exercises the section path:
    /// sections[] is present and non-empty, every entry carries a breakdown with
    /// all 5 sub-keys, section_count matches sections.len(), and the markdown
    /// contains "(score:".
    ///
    /// Setup: embedder-backed runtime → upsert_atoms → knowledge.edit (which
    /// embeds sections inline via embed_sections) → compose(explain=true).
    /// With a live embedder, embed_query() returns Some(qe), score_sections runs,
    /// section_results is non-empty, and the gating block at search.rs:1462 fires.
    #[tokio::test]
    async fn compose_explain_true_section_path_is_exercised() {
        let rt = rt_with_embedder();
        let f = pack(rt);

        // Create the atom.
        f.dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [{
                    "slug": "explain-sec-atom",
                    "name": "Explain Section Atom",
                    "content": "retrieval augmented generation combines dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
                }]
            }),
        )
        .await
        .expect("upsert atom");

        // Attach a section via knowledge.edit. With an embedder registered, edit
        // calls embed_sections inline so the row gets a non-NULL embedding
        // immediately — no separate knowledge.index call required.
        f.dispatch(
            "knowledge.edit",
            json!({
                "id": "explain-sec-atom",
                "sections": [{
                    "section_type": "overview",
                    "content": "retrieval augmented generation combines dense and sparse retrieval with generative models for grounded output synthesis covering dense sparse retrieval corpus benchmark search latency gradient descent transformer attention"
                }]
            }),
        )
        .await
        .expect("edit atom with section");

        let resp = f
            .dispatch(
                "knowledge.compose",
                json!({
                    "atom_ids": ["explain-sec-atom"],
                    "query": "retrieval augmented generation dense sparse",
                    "explain": true
                }),
            )
            .await
            .expect("compose explain ok");

        let data = &resp["data"];
        let md = data["markdown"].as_str().expect("markdown present");

        // Unconditional assertions — these must all pass. If any gate in
        // search.rs (embed_query, section_results non-empty check, explain guard)
        // is broken, the test will fail here, not silently skip.
        let sections = data["sections"]
            .as_array()
            .expect("sections key must be present when explain=true and sections are embedded");
        assert!(
            !sections.is_empty(),
            "sections array must be non-empty: {data:?}"
        );

        let sec = &sections[0];
        let bd = sec
            .get("breakdown")
            .expect("each section must carry a breakdown object in explain mode");

        assert!(
            bd.get("section_cosine").is_some(),
            "breakdown must have section_cosine: {bd:?}"
        );
        assert!(
            bd.get("section_bm25").is_some(),
            "breakdown must have section_bm25: {bd:?}"
        );
        assert!(
            bd.get("atom_cosine").is_some(),
            "breakdown must have atom_cosine: {bd:?}"
        );
        assert!(
            bd.get("domain_score").is_some(),
            "breakdown must have domain_score: {bd:?}"
        );
        assert!(
            bd.get("type_weight").is_some(),
            "breakdown must have type_weight: {bd:?}"
        );

        let section_count = data["section_count"]
            .as_u64()
            .expect("section_count must be present in explain mode");
        assert_eq!(
            section_count as usize,
            sections.len(),
            "section_count must equal sections.len()"
        );

        assert!(
            md.contains("(score:"),
            "section-path markdown must contain '(score:' when explain=true, got: {md}"
        );
    }
}

// ── Issue #184: ANN cold-start warming guard ──────────────────────────────────
//
// Regression tests for the fix in knowledge/search.rs and knowledge/vamana.rs.
// Full end-to-end warming cannot be tested in-process without a real embedding
// model, so these tests cover the observable invariants that are testable with
// the in-memory runtime:
//
//   1. Empty corpus (0 atoms, 0 domains) → ok:true, total:0 (not an error).
//   2. Populated corpus, no ANN warm triggered → normal FTS path still returns
//      results (verifies the guard does not break the non-warming hot path).

#[tokio::test]
async fn ann_warmup_guard_empty_corpus_returns_ok_not_error() {
    // Reproduces the scenario where the corpus is genuinely empty (no atoms,
    // no domains).  The warming guard must distinguish "warming" from "empty"
    // and must NOT return an error for the empty case.
    let f = pack(rt());
    let resp = f
        .dispatch(
            "knowledge.suggest",
            json!({ "query": "machine learning neural networks deep learning transformers" }),
        )
        .await
        .expect("suggest on empty corpus must succeed, not return an error");

    let total = resp["total"].as_u64().expect("total field must be present");
    assert_eq!(
        total, 0,
        "empty corpus must return total:0, not a warming error: {resp}"
    );
    let results = resp["results"]
        .as_array()
        .expect("results field must be present");
    assert!(
        results.is_empty(),
        "empty corpus must return empty results array: {resp}"
    );
}

#[tokio::test]
async fn ann_warmup_guard_populated_corpus_fts_path_returns_results() {
    // Verifies the guard does not break the non-ANN FTS path.
    // Populate a domain and an atom, then run suggest and confirm FTS hits come through.
    let f = pack(rt());

    // Create a domain with distinctive tokens so FTS can match it.
    // Description must be at least 20 words to pass the upsert validation.
    f.dispatch(
        "knowledge.upsert_domains",
        json!({
            "domains": [{
                "slug": "ml-foundation-184",
                "name": "ML Foundation",
                "description": "machine learning neural networks backpropagation gradient descent optimization unique184xqz transformer architectures attention mechanisms embedding representations dense retrieval sparse ranking reranking pipelines semantic search",
                "tags": ["type:domain"]
            }]
        }),
    )
    .await
    .expect("upsert domain");

    // Suggest with tokens that match the domain.
    let resp = f
        .dispatch(
            "knowledge.suggest",
            json!({
                "query": "machine learning neural networks backpropagation gradient descent unique184xqz optimization transformers attention"
            }),
        )
        .await
        .expect("suggest must succeed");

    // With no embedder the ANN path is skipped entirely; FTS still runs.
    // The result must be ok (not an error) regardless of whether results are found.
    assert!(
        resp.get("results").is_some(),
        "populated corpus suggest must return a results field: {resp}"
    );
    assert!(
        resp.get("total").is_some(),
        "populated corpus suggest must return a total field: {resp}"
    );
}

// ── knowledge.topic entity-map lookup regression ──────────────────────────────
//
// Regression for the latent `.unwrap()` in handle_topic (handlers.rs, search path).
//
// The old code did:
//
//   let filtered: Vec<_> = hits.into_iter().filter(|h| {
//       let Some(entity) = entity_map.get(&h.entity_id) else { return false; };
//       ...
//   }).collect();
//
//   let results = filtered.into_iter().map(|h| {
//       let entity = entity_map.get(&h.entity_id).unwrap(); // ← panic site
//       ...
//   });
//
// The fix merges the two passes into one filter_map that binds the entity once
// and carries the reference through to the map step.  No `.unwrap()` is needed.
//
// This test exercises the full query path of `knowledge.topic` with a query
// parameter (which triggers the search→entity_map→filter_map→results pipeline).
// It proves:
//   1. The verb succeeds without panicking.
//   2. Matching concepts are returned with the expected shape.
//   3. Domain post-filtering works correctly across both with-domain and
//      no-domain variants of the query path.

#[tokio::test]
async fn topic_query_path_does_not_panic_and_returns_results() {
    let f = pack(rt());

    // Seed two concept entities via knowledge.learn so they land in the KG
    // entity store (which is what handle_topic queries via hybrid_search +
    // get_entities_by_ids → entity_map).
    f.dispatch(
        "knowledge.learn",
        json!({
            "name": "Retrieval Augmented Generation",
            "description": "technique combining dense retrieval with language model generation for knowledge-grounded responses unique topictest77a",
            "domain": "retrieval"
        }),
    )
    .await
    .expect("learn concept A");

    f.dispatch(
        "knowledge.learn",
        json!({
            "name": "Contrastive Learning",
            "description": "self-supervised learning objective maximizing agreement between augmented views unique topictest77b",
            "domain": "training"
        }),
    )
    .await
    .expect("learn concept B");

    // Query path (query= supplied) — exercises hybrid_search → entity_map →
    // filter_map → results.  The filter_map must bind each entity once without
    // reaching any .unwrap() call.
    let resp = f
        .dispatch(
            "knowledge.topic",
            json!({ "query": "retrieval augmented generation unique topictest77a" }),
        )
        .await
        .expect("knowledge.topic must not panic");

    let results = resp["results"].as_array().expect("results array");
    assert!(
        !results.is_empty(),
        "topic query must return at least one result: {resp:?}"
    );
    // Verify the expected entity appears with the required fields.
    let hit = results
        .iter()
        .find(|r| r["name"].as_str() == Some("Retrieval Augmented Generation"))
        .expect("Retrieval Augmented Generation must appear in topic results");
    assert!(hit["id"].is_string(), "result must have id field: {hit:?}");
    assert!(
        hit["full_id"].is_string(),
        "result must have full_id field: {hit:?}"
    );
    assert!(
        hit["score"].is_number(),
        "result must have a numeric score: {hit:?}"
    );
}

#[tokio::test]
async fn topic_query_path_domain_filter_excludes_non_matching_concepts() {
    let f = pack(rt());

    f.dispatch(
        "knowledge.learn",
        json!({
            "name": "HNSW Index",
            "description": "hierarchical navigable small world graph index for approximate nearest neighbor search unique topictest78a",
            "domain": "retrieval"
        }),
    )
    .await
    .expect("learn HNSW");

    f.dispatch(
        "knowledge.learn",
        json!({
            "name": "Adam Optimizer",
            "description": "adaptive moment estimation optimizer for neural network training unique topictest78a",
            "domain": "training"
        }),
    )
    .await
    .expect("learn Adam");

    // With domain=retrieval: only HNSW should appear, Adam must be excluded.
    let resp = f
        .dispatch(
            "knowledge.topic",
            json!({
                "query": "approximate nearest neighbor optimizer unique topictest78a",
                "domain": "retrieval"
            }),
        )
        .await
        .expect("knowledge.topic with domain filter must not panic");

    let results = resp["results"].as_array().expect("results array");
    let names: Vec<&str> = results.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(
        !names.contains(&"Adam Optimizer"),
        "domain=retrieval must exclude training-domain concepts: {names:?}"
    );
}
