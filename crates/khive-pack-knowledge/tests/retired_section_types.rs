//! Rows typed with a retired section type (`references`, `other`) stay in the store
//! unchanged. The sectioned atom read returns them marked retired; compose, search
//! and suggest never serve them; challenge and adjudicate refuse the retired names.

use async_trait::async_trait;
use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{
    EmbedderProvider, KhiveRuntime, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::{SqlStatement, SqlValue};
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use serde_json::{json, Value};
use std::sync::Arc;

const MODEL_KEY: &str = "all-minilm-l6-v2";
const DIM: usize = 384;
const SLUG: &str = "retired-section-atom";
const NAME: &str = "Retired Section Atom";
const MARKER: &str = "zzretiredmarker";
const QUERY: &str = "dense sparse retrieval corpus benchmark";
const ATOM: &str = "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity";
const CURRENT: &str = "Current overview section content, long enough to clear the 80-character minimum. dense sparse retrieval corpus benchmark search latency gradient descent transformer";

fn unit_vector() -> Vec<f32> {
    let mut vector = vec![0.0; DIM];
    vector[0] = 1.0;
    vector
}

/// Every text embeds to the same unit vector, so a section that reaches scoring is
/// always a perfect cosine match: only the retired-row filter can keep it out.
struct UnitService;

#[async_trait]
impl EmbeddingService for UnitService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| unit_vector()).collect())
    }

    async fn embed_query(
        &self,
        texts: &[String],
        model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.embed(texts, model).await
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "unit-vector-service"
    }
}

struct UnitProvider;

#[async_trait]
impl EmbedderProvider for UnitProvider {
    fn name(&self) -> &str {
        MODEL_KEY
    }

    fn dimensions(&self) -> usize {
        DIM
    }

    async fn build(&self) -> Result<Arc<dyn EmbeddingService>, RuntimeError> {
        Ok(Arc::new(UnitService))
    }
}

struct Fixture {
    runtime: KhiveRuntime,
    registry: VerbRegistry,
}

impl Fixture {
    fn new() -> Self {
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
            packs: vec!["kg".into(), "knowledge".into()],
            ..RuntimeConfig::no_embeddings()
        })
        .expect("in-memory knowledge runtime");
        runtime.register_embedder(UnitProvider);
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(runtime.clone()));
        builder.register(KnowledgePack::new(runtime.clone()));
        let registry = builder.build().expect("knowledge registry");
        registry.apply_schema_plans(runtime.backend());
        runtime.install_edge_rules(registry.all_edge_rules());
        Self { runtime, registry }
    }

    async fn dispatch(&self, verb: &str, args: Value) -> Result<Value, RuntimeError> {
        self.registry.dispatch(verb, args).await
    }

    /// Write a section row the API refuses to create: the section type is stored
    /// verbatim, as rows written before the type was retired are.
    async fn insert_section(&self, id: &str, atom_id: &str, section_type: &str, content: &str) {
        let mut embedding = Vec::with_capacity(DIM * 4);
        for value in unit_vector() {
            embedding.extend_from_slice(&value.to_le_bytes());
        }
        self.runtime
            .sql()
            .writer()
            .await
            .expect("writer")
            .execute(SqlStatement {
                sql: "INSERT INTO knowledge_sections(id, atom_id, namespace, section_type, \
                      heading, content, content_hash, embedding, created_at, updated_at) \
                      VALUES (?1, ?2, 'local', ?3, ?4, ?5, ?6, ?7, 1, 1)"
                    .into(),
                params: vec![
                    SqlValue::Text(id.into()),
                    SqlValue::Text(atom_id.into()),
                    SqlValue::Text(section_type.into()),
                    SqlValue::Text(format!("Stored as {section_type}")),
                    SqlValue::Text(content.into()),
                    SqlValue::Text(format!("hash-{section_type}")),
                    SqlValue::Blob(embedding),
                ],
                label: None,
            })
            .await
            .expect("insert section row");
    }

    async fn stored_retired_rows(&self) -> usize {
        let mut reader = self.runtime.sql().reader().await.expect("reader");
        reader
            .query_all(SqlStatement {
                sql: "SELECT id FROM knowledge_sections \
                      WHERE section_type IN ('references', 'other')"
                    .into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("retired rows")
            .len()
    }
}

#[tokio::test]
async fn retired_section_rows_are_read_marked_and_never_served() {
    let f = Fixture::new();
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({"atoms": [{"slug": SLUG, "name": NAME, "content": ATOM}]}),
    )
    .await
    .expect("upsert atom");
    f.dispatch(
        "knowledge.edit",
        json!({"id": SLUG, "sections": [{"section_type": "overview", "content": CURRENT}]}),
    )
    .await
    .expect("current section");
    let atom = f
        .dispatch("knowledge.get", json!({"id": SLUG}))
        .await
        .expect("get atom");
    let atom_id = atom["id"].as_str().expect("atom id").to_owned();
    f.insert_section(
        "22730000-0000-4000-8000-000000000001",
        &atom_id,
        "references",
        &format!("1. Smith 2023 {MARKER}\n2. Jones 2022\n3. Lee 2021"),
    )
    .await;
    f.insert_section(
        "22730000-0000-4000-8000-000000000002",
        &atom_id,
        "other",
        &format!("{MARKER} stray\nnotes\nmore"),
    )
    .await;

    // The sectioned read returns every row; the retired ones keep their stored type
    // and are marked, and the current row serializes as before.
    let got = f
        .dispatch(
            "knowledge.get",
            json!({"id": SLUG, "include_sections": true}),
        )
        .await
        .expect("a retired row no longer fails the whole read");
    let sections = got["sections"].as_array().expect("sections array");
    assert_eq!(sections.len(), 3, "{got}");
    let section = |section_type: &str| {
        sections
            .iter()
            .find(|s| s["section_type"] == section_type)
            .unwrap_or_else(|| panic!("no {section_type} section in {got}"))
    };
    assert_eq!(section("references")["retired"], true);
    assert_eq!(section("other")["retired"], true);
    assert!(section("overview").get("retired").is_none());
    assert_eq!(section("overview")["content"], CURRENT);

    // Compose serves the current section and neither retired one.
    let composed = f
        .dispatch(
            "knowledge.compose",
            json!({"atom_ids": [SLUG], "query": QUERY, "explain": true, "blend_kg": false}),
        )
        .await
        .expect("compose");
    let served: Vec<&str> = composed["data"]["sections"]
        .as_array()
        .expect("sections are reported when explain is set")
        .iter()
        .filter_map(|s| s["section_type"].as_str())
        .collect();
    assert_eq!(served, vec!["overview"], "{composed}");
    assert!(!composed.to_string().contains(MARKER), "{composed}");

    // Search reports the line count of the sections that can be served: the current
    // section is one line, the retired rows would add six more.
    let searched = f
        .dispatch(
            "knowledge.search",
            json!({"query": QUERY, "rerank": false, "include_drafts": true}),
        )
        .await
        .expect("search");
    let hit = searched["results"]
        .as_array()
        .expect("results")
        .iter()
        .find(|r| r["name"] == NAME)
        .unwrap_or_else(|| panic!("atom not found by search: {searched}"));
    assert_eq!(hit["body_lines"], 1, "{searched}");
    assert!(!searched.to_string().contains(MARKER), "{searched}");

    let suggested = f
        .dispatch("knowledge.suggest", json!({"query": QUERY}))
        .await
        .expect("suggest");
    assert!(!suggested.to_string().contains(MARKER), "{suggested}");

    // Challenge and adjudicate refuse a retired name as unknown; the same calls with
    // a current type succeed.
    for retired in ["references", "other"] {
        let challenge = json!({"atom_id": SLUG, "section_type": retired});
        let error = f
            .dispatch("knowledge.challenge", challenge)
            .await
            .expect_err("a retired section type cannot be challenged");
        assert!(
            error.to_string().contains("unknown section_type"),
            "{error}"
        );
        let adjudicate = json!({"atom_id": SLUG, "section_type": retired, "resolution": "accept"});
        let error = f
            .dispatch("knowledge.adjudicate", adjudicate)
            .await
            .expect_err("a retired section type cannot be adjudicated");
        assert!(
            error.to_string().contains("unknown section_type"),
            "{error}"
        );
    }
    f.dispatch(
        "knowledge.challenge",
        json!({"atom_id": SLUG, "section_type": "overview"}),
    )
    .await
    .expect("a current section type can be challenged");
    f.dispatch(
        "knowledge.adjudicate",
        json!({"atom_id": SLUG, "section_type": "overview", "resolution": "accept"}),
    )
    .await
    .expect("a current section type can be adjudicated");

    assert_eq!(f.stored_retired_rows().await, 2, "retired rows are kept");
}

#[tokio::test]
async fn retired_type_alias_spellings_are_read_marked_and_never_served() {
    // Stores written before the retirement also hold the spellings the earlier alias
    // table resolved to a retired type; they are recognised after normalization, not by
    // exact string. A non-canonical spelling of a current type still reads and serves.
    const ALIAS_MARKER: &str = "zzaliasretiredmarker";
    const RETIRED_SPELLINGS: [&str; 4] = ["reference", "See-Also", " Notes ", "misc"];
    let f = Fixture::new();
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({"atoms": [{"slug": SLUG, "name": NAME, "content": ATOM}]}),
    )
    .await
    .expect("upsert atom");
    f.dispatch(
        "knowledge.edit",
        json!({"id": SLUG, "sections": [{"section_type": "overview", "content": CURRENT}]}),
    )
    .await
    .expect("current section");
    let atom = f
        .dispatch("knowledge.get", json!({"id": SLUG}))
        .await
        .expect("get atom");
    let atom_id = atom["id"].as_str().expect("atom id").to_owned();
    for (i, spelling) in RETIRED_SPELLINGS.iter().enumerate() {
        f.insert_section(
            &format!("22730000-0000-4000-8000-00000000001{i}"),
            &atom_id,
            spelling,
            &format!("{ALIAS_MARKER} {spelling}\nsecond line"),
        )
        .await;
    }
    f.insert_section(
        "22730000-0000-4000-8000-000000000020",
        &atom_id,
        "example",
        "A worked example stored under a non-canonical spelling of a current type.",
    )
    .await;

    let got = f
        .dispatch(
            "knowledge.get",
            json!({"id": SLUG, "include_sections": true}),
        )
        .await
        .expect("a retired alias spelling does not fail the whole read");
    let sections = got["sections"].as_array().expect("sections array");
    assert_eq!(sections.len(), 6, "{got}");
    for spelling in RETIRED_SPELLINGS {
        let row = sections
            .iter()
            .find(|s| s["section_type"] == spelling)
            .unwrap_or_else(|| panic!("no row stored as {spelling:?} in {got}"));
        assert_eq!(row["retired"], true, "{spelling:?}: {got}");
    }
    let example = sections
        .iter()
        .find(|s| s["section_type"] == "examples")
        .unwrap_or_else(|| panic!("the example row reads as examples: {got}"));
    assert!(example.get("retired").is_none(), "{got}");

    let composed = f
        .dispatch(
            "knowledge.compose",
            json!({"atom_ids": [SLUG], "query": QUERY, "explain": true, "blend_kg": false}),
        )
        .await
        .expect("compose");
    let mut served: Vec<&str> = composed["data"]["sections"]
        .as_array()
        .expect("sections are reported when explain is set")
        .iter()
        .filter_map(|s| s["section_type"].as_str())
        .collect();
    served.sort_unstable();
    assert_eq!(served, vec!["example", "overview"], "{composed}");
    assert!(!composed.to_string().contains(ALIAS_MARKER), "{composed}");

    // One line each for the overview and the example; the four retired rows would add
    // eight more.
    let searched = f
        .dispatch(
            "knowledge.search",
            json!({"query": QUERY, "rerank": false, "include_drafts": true}),
        )
        .await
        .expect("search");
    let hit = searched["results"]
        .as_array()
        .expect("results")
        .iter()
        .find(|r| r["name"] == NAME)
        .unwrap_or_else(|| panic!("atom not found by search: {searched}"));
    assert_eq!(hit["body_lines"], 2, "{searched}");
    assert!(!searched.to_string().contains(ALIAS_MARKER), "{searched}");

    let suggested = f
        .dispatch("knowledge.suggest", json!({"query": QUERY}))
        .await
        .expect("suggest");
    assert!(!suggested.to_string().contains(ALIAS_MARKER), "{suggested}");
}

#[tokio::test]
async fn legacy_spellings_of_current_types_read_as_those_types() {
    // Bulk-imported stores hold `pitfall`, `definition` and `comparison`, which no
    // table resolved before; a single such row used to fail the whole atom read.
    const LEGACY: [(&str, &str); 3] = [
        ("pitfall", "failure_modes"),
        ("definition", "core_model"),
        ("comparison", "expert_lens"),
    ];
    let f = Fixture::new();
    f.dispatch(
        "knowledge.upsert_atoms",
        json!({"atoms": [{"slug": SLUG, "name": NAME, "content": ATOM}]}),
    )
    .await
    .expect("upsert atom");
    f.dispatch(
        "knowledge.edit",
        json!({"id": SLUG, "sections": [{"section_type": "overview", "content": CURRENT}]}),
    )
    .await
    .expect("current section");
    let atom = f
        .dispatch("knowledge.get", json!({"id": SLUG}))
        .await
        .expect("get atom");
    let atom_id = atom["id"].as_str().expect("atom id").to_owned();
    for (i, (stored, _)) in LEGACY.iter().enumerate() {
        f.insert_section(
            &format!("22730000-0000-4000-8000-00000000003{i}"),
            &atom_id,
            stored,
            &format!("A section stored under the legacy spelling {stored}."),
        )
        .await;
    }

    let got = f
        .dispatch(
            "knowledge.get",
            json!({"id": SLUG, "include_sections": true}),
        )
        .await
        .expect("a legacy spelling does not fail the whole read");
    let sections = got["sections"].as_array().expect("sections array");
    assert_eq!(sections.len(), 4, "{got}");
    for (stored, reads_as) in LEGACY {
        let row = sections
            .iter()
            .find(|s| s["section_type"] == reads_as)
            .unwrap_or_else(|| panic!("the {stored:?} row reads as {reads_as}: {got}"));
        assert!(row.get("retired").is_none(), "{stored:?}: {got}");
    }
}
