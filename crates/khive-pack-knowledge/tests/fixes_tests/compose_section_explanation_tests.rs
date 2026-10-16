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
            wal_ceiling_bytes: 0,
            wal_ceiling_configured_bytes: 0,
            wal_ceiling_source: Default::default(),
            wal_ceiling_env_raw: None,
            web: Default::default(),
            telemetry: Default::default(),
            mounts: Vec::new(),
            brain: Default::default(),
            git_write: Default::default(),
            display_timezone: khive_runtime::config::resolve_default_display_timezone(),
            events_split: None,
            db_path: None,
            blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
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
            exec: Default::default(),
            ..khive_runtime::RuntimeConfig::no_embeddings()
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
