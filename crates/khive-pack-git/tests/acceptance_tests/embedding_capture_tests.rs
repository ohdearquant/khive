// ── #764: over-cap commit payload truncation ────────────────────────────────

/// Test-only [`EmbeddingService`] that records every text it is asked to
/// embed (so a test can assert exactly what reached the "provider") and
/// returns a deterministic constant vector, never failing.
struct CapturingEmbedService {
    captured: Arc<Mutex<Vec<String>>>,
    dims: usize,
}

#[async_trait]
impl EmbeddingService for CapturingEmbedService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
        self.captured
            .lock()
            .expect("captured mutex")
            .extend(texts.iter().cloned());
        Ok(texts.iter().map(|_| vec![1.0_f32; self.dims]).collect())
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "capturing-test-embedder"
    }
}

struct CapturingEmbedProvider {
    captured: Arc<Mutex<Vec<String>>>,
    dims: usize,
}

#[async_trait]
impl EmbedderProvider for CapturingEmbedProvider {
    fn name(&self) -> &str {
        "capturing-test-embedder"
    }

    fn dimensions(&self) -> usize {
        self.dims
    }

    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(Arc::new(CapturingEmbedService {
            captured: Arc::clone(&self.captured),
            dims: self.dims,
        }))
    }
}

/// A commit message over the 32,768-byte embedding cap must still create the
/// complete note (full content stored/FTS-indexed), must send only a capped,
/// UTF-8-safe head prefix to the embedder, and must report the truncation.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_truncates_over_cap_commit_embedding_and_reports_it() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    // The project anchor entity is created (and, via `create_entity`, embedded)
    // BEFORE the capturing embedder is registered, so only the commit note's
    // embed call below is captured.
    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "big-commit-repo"}),
    )
    .await;

    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    rt.register_embedder(CapturingEmbedProvider {
        captured: Arc::clone(&captured),
        dims: 8,
    });

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    write(repo, "README.md", "hello\n");

    // 51,648 ASCII chars (matches the issue's first-consumer report), with a
    // unique term at the very start (head, must be embedded/searchable) and
    // a unique term at the very end (tail, must be stored/FTS-searchable but
    // is beyond the embedding cap).
    let filler = "x".repeat(51_648 - "head-term-unique ".len() - " tail-term-unique".len());
    let message = format!("head-term-unique {filler} tail-term-unique");
    assert!(message.len() > 32_768, "fixture must exceed the cap");
    commit(repo, &["README.md"], &message);

    let report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.to_path_buf(), project_id.to_string()),
    )
    .await
    .expect("ingest ok");
    assert_eq!(report.commits_ingested, 1);
    assert_eq!(
        report.commit_embeddings_truncated, 1,
        "over-cap commit must be reported as truncated"
    );

    // Full content remains stored (including the tail sentinel beyond the cap).
    let list = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list ok");
    let items = list_items(&list);
    assert_eq!(items.len(), 1);
    let stored_content = items[0]["content"].as_str().expect("content is string");
    assert!(stored_content.contains("head-term-unique"));
    assert!(
        stored_content.contains("tail-term-unique"),
        "full commit message, including the tail beyond the cap, must be stored"
    );

    // Exactly one embed call was made, with a capped, head-only prefix.
    let seen = captured.lock().expect("captured mutex").clone();
    assert_eq!(seen.len(), 1, "exactly one embed call: {seen:?}");
    let embedded_text = &seen[0];
    assert!(
        embedded_text.len() <= 32_768,
        "embedder input must be at or under the cap: {} bytes",
        embedded_text.len()
    );
    assert!(
        embedded_text.contains("head-term-unique"),
        "embedder input must retain the head term"
    );
    assert!(
        !embedded_text.contains("tail-term-unique"),
        "embedder input must not reach past the cap into the tail"
    );
    assert!(
        stored_content.starts_with(embedded_text.as_str()),
        "embedder input must be a proper prefix of the stored content"
    );

    // A vector was actually inserted for this note (not skipped).
    let vectors = rt
        .vectors_for_model(&token, "capturing-test-embedder")
        .expect("vector store for capturing embedder");
    assert_eq!(
        vectors.count().await.expect("vector count"),
        1,
        "the capped head must have produced exactly one vector row"
    );

    // The head term is retrievable through the `search` verb (the design's
    // "vector evidence/search finds the head" requirement, satisfied here via
    // its search form in addition to the direct embedder-capture evidence
    // above), and the tail term — beyond the embedding cap but still part of
    // the fully stored/FTS-indexed content — remains independently
    // searchable too.
    // FTS5's bareword query grammar treats a run of whitespace-separated
    // terms as an implicit (adjacency-required) phrase, so the query text
    // uses spaces rather than the fixture's hyphenated compound — both tokenize
    // identically against the indexed content (hyphens are FTS5 word
    // separators too), but only the space form parses as the intended
    // three-token phrase rather than one hyphenated bareword.
    let head_hits = registry
        .dispatch(
            "search",
            json!({"kind": "commit", "query": "head term unique"}),
        )
        .await
        .expect("search ok");
    let head_hits = head_hits.as_array().expect("array");
    assert!(
        head_hits.iter().any(|h| h["id"] == items[0]["id"]),
        "the head term must resolve the truncated commit note via search: {head_hits:?}"
    );

    let tail_hits = registry
        .dispatch(
            "search",
            json!({"kind": "commit", "query": "tail term unique"}),
        )
        .await
        .expect("search ok");
    let tail_hits = tail_hits.as_array().expect("array");
    assert!(
        tail_hits.iter().any(|h| h["id"] == items[0]["id"]),
        "the tail term, beyond the embedding cap, must still resolve via FTS on the \
         complete stored content: {tail_hits:?}"
    );

    // A duplicate digest pass that re-walks the same over-cap commit (its
    // natural-key sha already has a note) must count it as a skip, and must
    // NOT re-increment the truncation counter — that counter only ever moves
    // on the successful-CREATE arm, which a skip never reaches. Re-walking
    // requires clearing the persisted cursor directly (the `{sha}..HEAD`
    // range `walk_commits` uses is exclusive of an already-advanced cursor,
    // so an ordinary duplicate `run_ingest` call would simply see zero
    // commits and never exercise the skip path at all).
    let sql = rt.sql();
    let mut w = sql.writer().await.expect("sql writer");
    w.execute(SqlStatement {
        sql: "DELETE FROM git_mirror_cursor WHERE project_id=?1 AND kind IN ('commits','commits_checkpoint')".into(),
        params: vec![SqlValue::Text(project_id.to_string())],
        label: Some("test_reset_commits_cursor".into()),
    })
    .await
    .expect("reset cursor");
    drop(w);

    let second_report = run_ingest(
        &rt,
        &token,
        &registry,
        IngestOptions::unbounded(repo.to_path_buf(), project_id.to_string()),
    )
    .await
    .expect("second ingest ok");
    assert_eq!(
        second_report.commits_ingested, 0,
        "no new commits on the re-walked pass"
    );
    assert_eq!(
        second_report.commits_skipped_existing, 1,
        "the already-ingested over-cap commit must be a natural-key skip"
    );
    assert_eq!(
        second_report.commit_embeddings_truncated, 0,
        "a re-walked pass must not re-report truncation for an existing commit"
    );
}
