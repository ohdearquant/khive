struct FailingEmbedService;

#[async_trait]
impl EmbeddingService for FailingEmbedService {
    async fn embed(
        &self,
        _texts: &[String],
        _model: EmbeddingModel,
    ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
        Err(EmbedError::InferenceFailed(
            "simulated inference failure (issue #764 regression)".into(),
        ))
    }

    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "failing-test-embedder"
    }
}

struct FailingEmbedProvider;

#[async_trait]
impl EmbedderProvider for FailingEmbedProvider {
    fn name(&self) -> &str {
        "failing-test-embedder"
    }

    fn dimensions(&self) -> usize {
        8
    }

    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(Arc::new(FailingEmbedService))
    }
}

/// An over-cap commit whose vector embedding genuinely fails (a real
/// `EmbedError::InferenceFailed` from the registered embedder -- the existing
/// test-embedder seam, not a production fault-injection flag) must not leave
/// a partial record behind: `create_note_inner`'s existing compensation
/// removes the note row and its FTS document, the pass reports the failure
/// as a create-commit warning, and neither `commits_ingested` nor
/// `commit_embeddings_truncated` moves for that commit, since both only ever
/// advance on the successful-create arm (a prior fix).
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_over_cap_commit_with_failing_embedder_creates_nothing_and_warns() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "failing-embed-repo"}),
    )
    .await;

    rt.register_embedder(FailingEmbedProvider);

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    write(repo, "README.md", "hello\n");

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
    .expect("ingest ok -- a per-commit create failure is a warning, not a hard pass error");

    assert_eq!(
        report.commits_ingested, 0,
        "the failed create must not be counted as ingested"
    );
    assert_eq!(
        report.commit_embeddings_truncated, 0,
        "truncation is only reported on the successful-create arm, which a failed \
         embed never reaches"
    );
    assert!(
        report.warnings.iter().any(|w| w.contains("create commit")
            && w.contains("embedding inference failed")
            && w.contains("simulated inference failure")),
        "the embed failure must surface as an explicit create-commit warning: {:?}",
        report.warnings
    );

    // No note row survives the compensated create.
    let list = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list ok");
    assert_eq!(
        list_items(&list).len(),
        0,
        "a compensated create must leave no commit note row behind"
    );

    // No FTS hit either -- the head term never resolves anything, since the
    // FTS document was compensated away along with the note row.
    let head_hits = registry
        .dispatch(
            "search",
            json!({"kind": "commit", "query": "head term unique"}),
        )
        .await
        .expect("search ok");
    assert_eq!(
        head_hits.as_array().expect("array").len(),
        0,
        "no FTS document must survive the compensated create"
    );
}

/// Semantic-only retrieval: an explicit
/// deterministic query vector matching a real registered embedder's
/// constant output, paired with query text that is lexically absent from
/// the commit message, can only resolve the note through the vector leg --
/// the FTS leg cannot contribute a hit. This proves the stored vector
/// genuinely retrieves the created commit note by ID/kind through the
/// public `KhiveRuntime::search_notes` surface, not merely that a vector
/// row of some count exists or that FTS happens to also work.
///
/// `search_notes`'s vector leg always searches the store keyed by the
/// runtime's *configured default* embedding model
/// (`config().embedding_model`), so unlike the sibling truncation test's
/// custom-named `CapturingEmbedProvider` (whose vectors live in a
/// per-provider store `search_notes` cannot reach), this test registers its
/// deterministic fixture embedder under a real `EmbeddingModel` variant's
/// canonical name -- `EmbedderRegistry::register` is last-writer-wins, so
/// this replaces the default `LatticeEmbedderProvider` khive-runtime
/// auto-registers for a configured model.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_over_cap_commit_embedding_is_semantically_retrievable() {
    const MODEL: EmbeddingModel = EmbeddingModel::BgeSmallEnV15;
    let dims = MODEL.dimensions();

    struct FixtureEmbedService {
        dims: usize,
    }

    #[async_trait]
    impl EmbeddingService for FixtureEmbedService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts.iter().map(|_| vec![1.0_f32; self.dims]).collect())
        }

        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }

        fn name(&self) -> &'static str {
            "fixture-semantic-embedder"
        }
    }

    struct FixtureEmbedProvider {
        dims: usize,
    }

    #[async_trait]
    impl EmbedderProvider for FixtureEmbedProvider {
        fn name(&self) -> &str {
            "bge-small-en-v1.5"
        }

        fn dimensions(&self) -> usize {
            self.dims
        }

        async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
            Ok(Arc::new(FixtureEmbedService { dims: self.dims }))
        }
    }

    let _guard = ENV_MUTEX.lock().await;
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
        embedding_model: Some(MODEL),
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
        exec: Default::default(),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .expect("runtime with a configured default model");
    rt.register_embedder(FixtureEmbedProvider { dims });

    let token = rt.authorize(Namespace::local()).expect("authorize local");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(GitPack::new(rt.clone()));
    builder
        .with_runtime_event_store(&rt)
        .expect("configure trusted runtime audit store");
    let registry = builder.build().expect("registry builds");
    rt.install_edge_rules(registry.all_edge_rules());
    registry.apply_schema_plans(rt.backend());
    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "semantic-retrieval-repo"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    write(repo, "README.md", "hello\n");

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
    assert_eq!(report.commit_embeddings_truncated, 1);

    let list = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list ok");
    let items = list_items(&list);
    assert_eq!(items.len(), 1);
    let commit_note_id =
        Uuid::parse_str(items[0]["id"].as_str().expect("commit id")).expect("commit id is uuid");

    let semantic_hits = rt
        .search_notes(
            &token,
            "lexically-absent-query-term-zzz",
            Some(vec![1.0_f32; dims]),
            10,
            Some("commit"),
            false,
            &[],
            None,
        )
        .await
        .expect("semantic search ok");
    assert!(
        semantic_hits.iter().any(|h| h.note_id == commit_note_id),
        "an explicit deterministic query vector matching the embedded head must \
         retrieve the commit note by ID even though the query text is lexically \
         absent from its content: {semantic_hits:?}"
    );
}

/// Under-cap commit content must not be truncated: the embedder sees the
/// full text and the report's truncation counter stays at zero. See
/// [`ingest_does_not_truncate_exact_cap_commit_embedding`] for the
/// exact-cap-boundary sibling of this test.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_does_not_truncate_under_cap_commit_embedding() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    // Create the project anchor entity BEFORE registering the capturing
    // embedder, so only the commit note's embed call is captured below.
    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "small-commit-repo"}),
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
    commit(repo, &["README.md"], "A short, unremarkable commit message");

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
        report.commit_embeddings_truncated, 0,
        "an under-cap commit must not be reported as truncated"
    );

    let seen = captured.lock().expect("captured mutex").clone();
    assert_eq!(seen.len(), 1);
    assert!(
        seen[0].contains("A short, unremarkable commit message"),
        "the embedder must receive the full message unchanged: {:?}",
        seen[0]
    );
}

/// A commit message whose masked content lands EXACTLY on the 32,768-byte
/// cap (not one byte over) must not be truncated either: the design draws
/// the line at "at or below the cap", and `truncated_embedding_head`'s own
/// exact-cap unit test only proves the pure helper's behavior, not this
/// full `run_ingest` pipeline's counter and embedder-input wiring.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_does_not_truncate_exact_cap_commit_embedding() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "exact-cap-commit-repo"}),
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

    // A single-line, body-less subject of exactly 32,768 ASCII bytes: `git`'s
    // `%s`/`%b` split leaves `body` empty, so `raw_content == subject` and
    // `content.len()` (post-mask, a no-op here) is exactly the cap.
    let message = "x".repeat(32_768);
    assert_eq!(
        message.len(),
        32_768,
        "fixture must land exactly on the cap"
    );
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
        report.commit_embeddings_truncated, 0,
        "an exact-cap commit must not be reported as truncated"
    );

    let seen = captured.lock().expect("captured mutex").clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].len(),
        32_768,
        "the embedder must receive the full, untruncated exact-cap message"
    );
}

/// With no embedder registered at all (the acceptance fixture's default),
/// an over-cap commit must still store the full note and report the
/// truncation — the counter reflects the capped candidate input regardless
/// of whether any embedder is configured to consume it.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_reports_truncation_even_with_no_embedder_configured() {
    let _guard = ENV_MUTEX.lock().await;
    let (rt, token, registry) = fixture().await;

    let project_id = create(
        &registry,
        json!({"kind": "project", "name": "no-embedder-repo"}),
    )
    .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    write(repo, "README.md", "hello\n");
    let message = format!("head-of-message {}", "y".repeat(40_000));
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
    assert_eq!(report.commit_embeddings_truncated, 1);

    let list = registry
        .dispatch("list", json!({"kind": "commit", "limit": 10}))
        .await
        .expect("list ok");
    let stored_content = list_items(&list)[0]["content"]
        .as_str()
        .expect("content is string");
    assert!(stored_content.contains(&message));
}

/// A `Closes #N` reference placed in the tail of an over-cap commit message —
/// past the 32,768-byte embedding cap — must still resolve to a
/// `reference_edges_created` annotates edge. `link_references` reads
/// `NewRecordForRef.text`, which is the complete masked content (not the
/// capped embedding head), so a beyond-cap reference must be exactly as
/// resolvable as one in the head.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn ingest_resolves_over_cap_commit_reference_beyond_the_embedding_cap() {
    let _guard = ENV_MUTEX.lock().await;
    let (_rt, _token, registry) = fixture().await;

    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_repo(repo);
    write(repo, "README.md", "hello\n");
    commit(repo, &["README.md"], "Initial commit");

    let first = registry
        .dispatch(
            "git.digest",
            json!({"source": repo.to_str().unwrap(), "max_items": 10}),
        )
        .await
        .expect("digest ok (pass 1)");
    let project_id = first["project_id"]
        .as_str()
        .expect("project_id present")
        .to_string();

    let issue_id = create(
        &registry,
        json!({
            "kind": "issue",
            "content": "",
            "properties": {"number": 4242, "title": "Tail-referenced bug", "project_id": project_id},
            "annotates": [project_id],
        }),
    )
    .await;

    // The `Closes #4242` reference sits well past the 32,768-byte cap: the
    // filler alone already exceeds the cap before the reference paragraph.
    let filler = "z".repeat(40_000);
    let message = format!("Fix the tail bug\n\n{filler}\n\nCloses #4242");
    assert!(
        message.rfind("Closes #4242").unwrap() > 32_768,
        "fixture must place the reference beyond the embedding cap"
    );
    write(repo, "src/lib.rs", "// fix\n");
    commit(repo, &["src/lib.rs"], &message);

    let second = registry
        .dispatch(
            "git.digest",
            json!({"source": repo.to_str().unwrap(), "project": project_id, "max_items": 10}),
        )
        .await
        .expect("digest ok (pass 2)");
    assert_eq!(second["commits_ingested"], 1, "{second}");
    assert_eq!(
        second["commit_embeddings_truncated"], 1,
        "the referencing commit is itself over-cap: {second}"
    );
    assert_eq!(
        second["reference_edges_created"], 1,
        "the beyond-cap Closes #4242 reference must still resolve: {second}"
    );

    let issue_neighbors = registry
        .dispatch(
            "neighbors",
            json!({"id": issue_id.to_string(), "direction": "incoming", "relations": ["annotates"]}),
        )
        .await
        .expect("neighbors ok");
    let hits = issue_neighbors.as_array().expect("array");
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0]["kind"], "commit");
}
