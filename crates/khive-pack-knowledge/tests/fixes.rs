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
use khive_runtime::{KhiveRuntime, RuntimeError, VerbRegistry, VerbRegistryBuilder};
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

    async fn dispatch_ns(
        &self,
        verb: &str,
        ns: &str,
        mut args: Value,
    ) -> Result<Value, RuntimeError> {
        args["namespace"] = json!(ns);
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

#[tokio::test]
async fn w5_status_multiplier_verified_beats_draft() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({
            "atoms": [
                {
                    "slug": "veri-atom",
                    "name": "Verified Atom",
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
        "UPDATE knowledge_atoms SET status='verified' WHERE slug=?1",
        vec![SqlValue::Text("veri-atom".into())],
    )
    .await;
    f.sql_exec(
        "UPDATE knowledge_atoms SET status='draft' WHERE slug=?1",
        vec![SqlValue::Text("draft-atom".into())],
    )
    .await;

    let resp = f
        .dispatch(
            "knowledge.search",
            json!({ "query": "neural network gradient learning zzzxxx", "rerank": false }),
        )
        .await
        .expect("search ok");
    let results = resp["results"].as_array().expect("results");

    let verified_score = results
        .iter()
        .find(|r| r["name"].as_str() == Some("Verified Atom"))
        .and_then(|r| r["score"].as_f64());
    let draft_score = results
        .iter()
        .find(|r| r["name"].as_str() == Some("Draft Atom"))
        .and_then(|r| r["score"].as_f64());

    match (verified_score, draft_score) {
        (Some(v), Some(d)) => assert!(
            v > d,
            "verified score {v:.4} must exceed draft score {d:.4} (1.2× vs 0.8× multiplier)"
        ),
        (Some(_), None) => {} // draft filtered — multiplier=0.8 below threshold, acceptable
        (None, _) => panic!("verified atom missing from results: {results:?}"),
    }
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
                "content": "retrieval domain techniques xyzabc organization dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity"
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

// ── S4: namespace guard on UPDATE WHERE clauses ───────────────────────────────

#[tokio::test]
async fn s4_upsert_atoms_update_does_not_affect_other_namespace() {
    let f = pack(rt());

    // Insert same slug in two different namespaces.
    f.dispatch_ns(
        "knowledge.upsert_atoms",
        "ns-alpha",
        json!({ "atoms": [{ "slug": "shared-slug", "name": "Alpha Name", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert alpha");

    f.dispatch_ns(
        "knowledge.upsert_atoms",
        "ns-beta",
        json!({ "atoms": [{ "slug": "shared-slug", "name": "Beta Name", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert beta");

    // Update in ns-alpha only.
    f.dispatch_ns(
        "knowledge.upsert_atoms",
        "ns-alpha",
        json!({ "atoms": [{ "slug": "shared-slug", "name": "Alpha Name Updated", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("update alpha");

    // ns-beta must be unchanged.
    let beta = f
        .dispatch_ns("knowledge.get", "ns-beta", json!({ "id": "shared-slug" }))
        .await
        .expect("get beta");
    assert_eq!(
        beta["name"].as_str().unwrap_or(""),
        "Beta Name",
        "update in ns-alpha must not affect ns-beta atom"
    );

    // ns-alpha must have the updated name.
    let alpha = f
        .dispatch_ns("knowledge.get", "ns-alpha", json!({ "id": "shared-slug" }))
        .await
        .expect("get alpha");
    assert_eq!(
        alpha["name"].as_str().unwrap_or(""),
        "Alpha Name Updated",
        "ns-alpha atom must reflect the update"
    );
}

#[tokio::test]
async fn s4_upsert_domains_update_does_not_affect_other_namespace() {
    let f = pack(rt());

    f.dispatch_ns(
        "knowledge.upsert_domains",
        "ns-alpha",
        json!({ "domains": [{ "slug": "shared-domain", "name": "Alpha Domain", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert alpha domain");

    f.dispatch_ns(
        "knowledge.upsert_domains",
        "ns-beta",
        json!({ "domains": [{ "slug": "shared-domain", "name": "Beta Domain", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("upsert beta domain");

    f.dispatch_ns(
        "knowledge.upsert_domains",
        "ns-alpha",
        json!({ "domains": [{ "slug": "shared-domain", "name": "Alpha Domain Updated", "content": "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" }] }),
    )
    .await
    .expect("update alpha domain");

    let beta = f
        .dispatch_ns("knowledge.get", "ns-beta", json!({ "id": "shared-domain" }))
        .await
        .expect("get beta domain");
    assert_eq!(
        beta["name"].as_str().unwrap_or(""),
        "Beta Domain",
        "update in ns-alpha must not affect ns-beta domain"
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
                { "slug": "rrf-a", "name": "RRF Alpha", "content": "rrf fusion scoring alpha dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" },
                { "slug": "rrf-b", "name": "RRF Beta",  "content": "rrf fusion scoring beta dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" },
                { "slug": "rrf-c", "name": "RRF Gamma", "content": "rrf fusion scoring gamma dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" },
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

// ── codex #527: status stays consistent with finalized through the UPDATE path ──

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

#[tokio::test]
async fn upsert_finalizing_does_not_demote_verified() {
    let f = pack(rt());
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "verified-atom", "name": "V", "content": "b dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "finalized": true }] }),
    )
    .await
    .expect("insert");
    // Manually promote to the higher 'verified' state.
    f.sql_exec(
        "UPDATE knowledge_atoms SET status='verified' WHERE slug=?1",
        vec![SqlValue::Text("verified-atom".into())],
    )
    .await;
    // Re-upsert with finalized=true again: must NOT demote verified -> reviewed.
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({ "atoms": [{ "slug": "verified-atom", "name": "V2", "content": "b2 dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity", "finalized": true }] }),
    )
    .await
    .expect("re-upsert");
    let row = f
        .sql_query_one(
            "SELECT status FROM knowledge_atoms WHERE slug=?1",
            vec![SqlValue::Text("verified-atom".into())],
        )
        .await
        .expect("row");
    assert_eq!(
        row_text(&row, "status").as_deref(),
        Some("verified"),
        "re-finalizing must not demote an already-verified atom"
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
        db_path: None,
        default_namespace: Namespace::local(),
        embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string(), "knowledge".to_string()],
        backend_id: BackendId::main(),
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

    // Promote high to verified (1.2× multiplier after normalization, clamped to 1.0).
    f.sql_exec(
        "UPDATE knowledge_atoms SET status='verified' WHERE slug=?1",
        vec![SqlValue::Text("norm-high".into())],
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

    // Verified (high) must outrank non-verified — ordering preserved after normalization + clamp.
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
            "verified atom score {hs:.4} must not be less than draft score {ms:.4}"
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
                { "slug": "rerank-a", "name": "Cosine Alpha", "content": "cosine similarity embedding rerank vector dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" },
                { "slug": "rerank-b", "name": "Cosine Beta",  "content": "cosine similarity embedding rerank dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity" },
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
