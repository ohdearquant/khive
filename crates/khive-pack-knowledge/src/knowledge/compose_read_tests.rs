use super::*;
use khive_runtime::RuntimeConfig;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

#[derive(Default, Debug)]
struct Work {
    readers: usize,
    queries: usize,
    physical_checkouts: u64,
}
tokio::task_local! { static WORK: Arc<Mutex<Work>>; static AFTER_WINDOW: Arc<Pause>; }
pub(super) fn observe_reader() {
    let _ = WORK.try_with(|work| work.lock().expect("work").readers += 1);
}
pub(super) fn observe_query(runtime: &KhiveRuntime, before: u64) {
    let after = runtime
        .backend()
        .pool()
        .reader_acquisition_snapshot()
        .pooled_checkouts;
    let _ = WORK.try_with(|work| {
        let mut work = work.lock().expect("work");
        work.queries += 1;
        work.physical_checkouts += after - before;
    });
}
struct Pause {
    entered: tokio::sync::Barrier,
    release: tokio::sync::Barrier,
    used: AtomicBool,
}
impl Pause {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: tokio::sync::Barrier::new(2),
            release: tokio::sync::Barrier::new(2),
            used: AtomicBool::new(false),
        })
    }
}
pub(super) async fn pause_after_window() {
    if let Ok(pause) = AFTER_WINDOW.try_with(Arc::clone) {
        if !pause.used.swap(true, Ordering::AcqRel) {
            pause.entered.wait().await;
            pause.release.wait().await;
        }
    }
}
fn runtime(path: &std::path::Path) -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        db_path: Some(path.into()),
        packs: vec![],
        ..RuntimeConfig::no_embeddings()
    })
    .expect("file-backed runtime")
}
async fn execute(runtime: &KhiveRuntime, sql: &str, params: Vec<SqlValue>) {
    runtime
        .sql()
        .writer()
        .await
        .expect("writer")
        .execute(SqlStatement {
            sql: sql.into(),
            params,
            label: None,
        })
        .await
        .expect("fixture statement");
}
async fn seed_atom(
    runtime: &KhiveRuntime,
    id: &str,
    namespace: &str,
    slug: &str,
    status: &str,
    deleted: bool,
) {
    execute(runtime, "INSERT INTO knowledge_atoms(id,namespace,slug,name,content,tags,status,finalized,created_at,updated_at,deleted_at) VALUES(?1,?2,?3,?3,'fixture corpus content describing compile source code query detail','[]',?4,1,0,0,?5)", vec![SqlValue::Text(id.into()),SqlValue::Text(namespace.into()),SqlValue::Text(slug.into()),SqlValue::Text(status.into()),if deleted {SqlValue::Integer(1)} else {SqlValue::Null}]).await;
}
async fn seed_domain(runtime: &KhiveRuntime, slug: &str, members: &[String]) {
    execute(runtime, "INSERT INTO knowledge_domains(id,namespace,slug,name,members,status,created_at,updated_at) VALUES(?1,'local',?2,?2,?3,'reviewed',0,0)", vec![SqlValue::Text(Uuid::new_v5(&Uuid::NAMESPACE_OID,slug.as_bytes()).to_string()),SqlValue::Text(slug.into()),SqlValue::Text(serde_json::to_string(members).expect("members"))]).await;
}
async fn compose(runtime: &KhiveRuntime, params: Value) -> Result<Value, RuntimeError> {
    let token = runtime.authorize(Namespace::local()).expect("token");
    KnowledgeHandlers::compose(
        runtime,
        &token,
        params,
        &vamana::new_shared(),
        HashMap::new(),
    )
    .await
}
fn outcome(value: Result<Atom, RuntimeError>) -> String {
    match value {
        Ok(atom) => format!("OK {atom:?}"),
        Err(error) => format!("ERR {error}"),
    }
}
async fn differential(runtime: &KhiveRuntime, references: &[String]) {
    let mut actual = Vec::new();
    for chunk in references.chunks(KnowledgeHandlers::COMPOSE_ATOM_CHUNK_SIZE) {
        actual.extend(
            KnowledgeHandlers::load_compose_atom_window(runtime, "local", chunk)
                .await
                .expect("window"),
        );
    }
    assert_eq!(actual.len(), references.len());
    for (reference, result) in references.iter().zip(actual) {
        assert_eq!(
            outcome(result),
            outcome(load_atom_by_id_or_slug(runtime, "local", reference).await),
            "original occurrence spelling {reference:?}"
        );
    }
}

#[tokio::test]
async fn compose_window_chunk_boundaries_public_path_actual_reader_and_query_work() {
    let dir = TempDir::new().expect("directory");
    let rt = runtime(&dir.path().join("work.db"));
    let chunk = KnowledgeHandlers::COMPOSE_ATOM_CHUNK_SIZE;
    for count in [0, 1, chunk - 1, chunk, chunk + 1, 900, 1000] {
        let refs: Vec<_> = (0..count).map(|i| format!("member-{i}")).collect();
        for (i, reference) in refs.iter().enumerate().take(count) {
            let id = Uuid::from_u128(0x11110000000000000000000000000000 + i as u128).to_string();
            execute(
                &rt,
                "DELETE FROM knowledge_atoms WHERE id=?1",
                vec![SqlValue::Text(id.clone())],
            )
            .await;
            seed_atom(&rt, &id, "local", reference, "reviewed", false).await;
        }
        let domain = format!("domain-{count}");
        seed_domain(&rt, &domain, &refs).await;
        let work = Arc::new(Mutex::new(Work::default()));
        let response=WORK.scope(Arc::clone(&work),compose(&rt,json!({"query":"source code", "domain_ids":[domain],"blend_kg":false,"max_tokens":64}))).await.expect("shipping compose");
        assert_eq!(response["status"], "ok");
        let actual = work.lock().expect("work");
        assert_eq!(
            actual.readers,
            count.div_ceil(chunk),
            "actual completed SqlReader handles; serial domain/section work intentionally separate"
        );
        assert_eq!(
            actual.queries,
            count.div_ceil(chunk),
            "actual completed SqlReader statements, not per-reference wrapper counters"
        );
        assert_eq!(
            actual.physical_checkouts,
            count.div_ceil(chunk) as u64,
            "real pool-acquisition completions inside atom windows"
        );
        println!(
            "COMPOSE_WORK occurrences={count} chunk={chunk} handles={} statements={} physical_pool_checkouts={}",
            actual.readers, actual.queries, actual.physical_checkouts
        );
    }
}

#[tokio::test]
async fn compose_window_preserves_raw_uuid_slug_prefix_decoding_and_namespace_outcomes() {
    let dir = TempDir::new().expect("directory");
    let rt = runtime(&dir.path().join("resolve.db"));
    let canonical = "abcdef00-1111-2222-3333-444444444444";
    seed_atom(&rt, canonical, "local", "named", "reviewed", false).await;
    seed_atom(
        &rt,
        "12345678-0000-0000-0000-000000000001",
        "local",
        "abcdef00",
        "draft",
        false,
    )
    .await;
    seed_atom(
        &rt,
        "deadbeef-0000-0000-0000-000000000001",
        "local",
        "deleted",
        "deprecated",
        true,
    )
    .await;
    seed_atom(
        &rt,
        "feedface-0000-0000-0000-000000000001",
        "foreign",
        "foreign",
        "reviewed",
        false,
    )
    .await;
    // Raw corrupt prefix rows must count before decoding; TEXT id is not UUID constrained.
    seed_atom(
        &rt,
        "badc0ffe-raw-one",
        "local",
        "corrupt-one",
        "reviewed",
        false,
    )
    .await;
    seed_atom(
        &rt,
        "badc0ffe-raw-two",
        "local",
        "corrupt-two",
        "reviewed",
        false,
    )
    .await;
    seed_atom(
        &rt,
        "00000000-0000-0000-0000-000000000099",
        "local",
        "faceface",
        "reviewed",
        false,
    )
    .await;
    execute(
        &rt,
        "UPDATE knowledge_atoms SET name=X'ff' WHERE slug='faceface'",
        vec![],
    )
    .await;
    seed_atom(
        &rt,
        "faceface-0000-0000-0000-000000000001",
        "local",
        "prefix-target",
        "reviewed",
        false,
    )
    .await;
    let refs = vec![
        canonical.into(),
        canonical.to_uppercase(),
        format!("{{{canonical}}}"),
        canonical.replace('-', ""),
        format!("  {canonical}  "),
        "named".into(),
        " abcdef00 ".into(),
        "abcdef00-1111".into(),
        "badc0ffe".into(),
        "faceface".into(),
        "deleted".into(),
        "foreign".into(),
        "missing".into(),
        "missing".into(),
        "  ".into(),
    ];
    differential(&rt, &refs).await;
}

#[tokio::test]
async fn compose_member_omissions_explicit_errors_alias_dedup_and_first_error_order() {
    let dir = TempDir::new().expect("directory");
    let rt = runtime(&dir.path().join("fold.db"));
    let id = "abcdef00-1111-2222-3333-444444444444";
    seed_atom(&rt, id, "local", "named", "draft", false).await;
    seed_atom(&rt, "badc0ffe-one", "local", "bad-one", "reviewed", false).await;
    seed_atom(&rt, "badc0ffe-two", "local", "bad-two", "reviewed", false).await;
    seed_domain(
        &rt,
        "omissions",
        &[
            " missing ".into(),
            "named".into(),
            " missing ".into(),
            id.into(),
        ],
    )
    .await;
    let response=compose(&rt,json!({"query":"code", "domain_ids":["omissions"], "atom_ids":[id,"named"],"blend_kg":false})).await.expect("members tolerate missing; explicit draft opts in");
    assert_eq!(
        response["data"]["omissions"],
        json!([" missing ", " missing "])
    );
    assert_eq!(
        response["data"]["atoms"].as_array().expect("atoms").len(),
        1
    );
    let error = compose(
        &rt,
        json!({"query":"code","atom_ids":["missing","badc0ffe"],"blend_kg":false}),
    )
    .await
    .expect_err("first explicit error");
    assert_eq!(
        error.to_string(),
        load_atom_by_id_or_slug(&rt, "local", "missing")
            .await
            .expect_err("original missing")
            .to_string()
    );
    seed_domain(
        &rt,
        "ambiguous",
        &["missing".into(), "badc0ffe".into(), "named".into()],
    )
    .await;
    let error = compose(
        &rt,
        json!({"query":"code","domain_ids":["ambiguous"],"blend_kg":false}),
    )
    .await
    .expect_err("member prefix remains fatal");
    assert_eq!(
        error.to_string(),
        load_atom_by_id_or_slug(&rt, "local", "badc0ffe")
            .await
            .expect_err("original prefix")
            .to_string()
    );
}

#[test]
fn compose_slug_map_duplicate_slug_and_uuid_members_preserve_linear_first_oracle() {
    let mk = |n, slug: &str| Atom {
        id: Uuid::from_u128(n),
        namespace: "local".into(),
        slug: slug.into(),
        name: "name".into(),
        content: "content".into(),
        tags: "[]".into(),
        properties: None,
        status: None,
        source_uri: None,
        source_type: None,
        finalized: true,
        created_at: 0,
        updated_at: 0,
        deleted_at: None,
    };
    let atoms = vec![mk(1, "same"), mk(2, "other"), mk(3, "same")];
    let members = vec![
        "same".into(),
        "same".into(),
        atoms[1].id.to_string(),
        "absent".into(),
    ];
    let expected: HashSet<_> = members
        .iter()
        .filter_map(|slug| {
            atoms
                .iter()
                .find(|atom| atom.slug == *slug)
                .map(|atom| atom.id.to_string())
        })
        .collect();
    assert_eq!(
        KnowledgeHandlers::compose_domain_member_ids(&atoms, &members),
        expected
    );
    assert_eq!(expected, [atoms[0].id.to_string()].into_iter().collect());
}

#[tokio::test]
async fn compose_domains_stay_serial_and_parse_error_precedes_next_domain() {
    let dir = TempDir::new().expect("directory");
    let rt = runtime(&dir.path().join("domains.db"));
    seed_domain(&rt, "bad-domain", &[]).await;
    execute(
        &rt,
        "UPDATE knowledge_domains SET members='invalid-json' WHERE slug='bad-domain'",
        vec![],
    )
    .await;
    let work = Arc::new(Mutex::new(Work::default()));
    let error=WORK.scope(Arc::clone(&work),compose(&rt,json!({"query":"code","domain_ids":["bad-domain","missing-domain"],"blend_kg":false}))).await.expect_err("first domain parse");
    assert!(
        error.to_string().contains("bad-domain")
            && error.to_string().contains("invalid members JSON")
    );
    assert!(!error.to_string().contains("missing-domain"));
    assert_eq!(work.lock().expect("work").queries, 0);
}

#[tokio::test]
async fn compose_no_cross_window_or_request_memo_and_stop_at_first_fatal_window() {
    let dir = TempDir::new().expect("directory");
    let rt = runtime(&dir.path().join("memo.db"));
    let id = "abcdef00-1111-2222-3333-444444444444";
    seed_atom(&rt, id, "local", "named", "reviewed", false).await;
    let refs = vec!["named".to_owned()];
    let first = KnowledgeHandlers::load_compose_atom_window(&rt, "local", &refs)
        .await
        .expect("first")
        .remove(0)
        .expect("atom");
    execute(
        &rt,
        "UPDATE knowledge_atoms SET content='new committed content' WHERE id=?1",
        vec![SqlValue::Text(id.into())],
    )
    .await;
    let next = KnowledgeHandlers::load_compose_atom_window(&rt, "local", &refs)
        .await
        .expect("next window/request")
        .remove(0)
        .expect("atom");
    assert_ne!(first.content, next.content);
    assert_eq!(next.content, "new committed content");
    let mut inputs = vec!["missing".to_owned()];
    inputs.extend(std::iter::repeat_n(
        "named".to_owned(),
        2 * KnowledgeHandlers::COMPOSE_ATOM_CHUNK_SIZE,
    ));
    let work = Arc::new(Mutex::new(Work::default()));
    let error = WORK
        .scope(
            Arc::clone(&work),
            compose(
                &rt,
                json!({"query":"code","atom_ids":inputs,"blend_kg":false}),
            ),
        )
        .await
        .expect_err("fatal first occurrence");
    assert!(error.to_string().contains("missing"));
    assert_eq!(
        work.lock().expect("work").queries,
        1,
        "no successor window after ordinal fold abort"
    );
}

#[tokio::test]
async fn compose_deadline_is_fatal_without_sizing_degradation_or_successor_window() {
    let dir = TempDir::new().expect("directory");
    let rt = runtime(&dir.path().join("timeout.db"));
    let refs = vec!["missing".to_owned()];
    let error = khive_storage::scope_request_read_deadline(
        std::time::Duration::ZERO,
        KnowledgeHandlers::load_compose_atom_window(&rt, "local", &refs),
    )
    .await
    .expect_err("fatal deadline");
    assert!(
        matches!(
            &error,
            RuntimeError::Storage(khive_storage::StorageError::Timeout { operation })
                if operation.as_ref() == "knowledge.compose"
        ),
        "expired request must retain its typed knowledge.compose timeout: {error:?}; {error}"
    );
    let id = "abcdef00-1111-2222-3333-444444444444";
    seed_atom(&rt, id, "local", "named", "reviewed", false).await;
    let mut refs = vec!["named".to_owned(); KnowledgeHandlers::COMPOSE_ATOM_CHUNK_SIZE];
    refs.push("named".into());
    let pause = Pause::new();
    let work = Arc::new(Mutex::new(Work::default()));
    let (cancel, cancellation) = tokio::sync::watch::channel(false);
    let result = khive_storage::scope_request_read_cancellation(cancellation, async {
        let (result, ()) = tokio::join!(
            WORK.scope(
                Arc::clone(&work),
                AFTER_WINDOW.scope(
                    Arc::clone(&pause),
                    compose(
                        &rt,
                        json!({"query":"code","atom_ids":refs,"blend_kg":false})
                    )
                )
            ),
            async {
                pause.entered.wait().await;
                cancel
                    .send(true)
                    .expect("cancel after completed real window");
                pause.release.wait().await;
            }
        );
        result
    })
    .await;
    assert!(result.is_err());
    assert_eq!(
        work.lock().expect("work").queries,
        1,
        "request cancellation is fatal before second window"
    );
}

#[tokio::test]
async fn compose_real_pool_deadline_and_statement_failure_are_fatal_at_window_first_ordinal() {
    let dir = TempDir::new().expect("directory");
    let backend = Arc::new(
        khive_db::StorageBackend::sqlite_with_max_readers(dir.path().join("pool.db"), Some(1))
            .expect("one-reader backend"),
    );
    backend.prepare_core_schema().expect("schema");
    let rt = KhiveRuntime::from_backend(
        Arc::clone(&backend),
        RuntimeConfig {
            packs: vec![],
            ..RuntimeConfig::no_embeddings()
        },
    );
    let id = "abcdef00-1111-2222-3333-444444444444";
    seed_atom(&rt, id, "local", "named", "reviewed", false).await;
    let held = backend
        .pool()
        .reader()
        .expect("hold actual sole pooled reader");
    let work = Arc::new(Mutex::new(Work::default()));
    let refs = vec![id.to_owned(), "named".to_owned()];
    let error = WORK
        .scope(
            Arc::clone(&work),
            khive_storage::scope_request_read_deadline(
                std::time::Duration::from_millis(30),
                KnowledgeHandlers::load_compose_atom_window(&rt, "local", &refs),
            ),
        )
        .await
        .expect_err("actual pool admission cannot finish");
    assert!(error.to_string().contains("compose atom by id"), "{error}");
    assert_eq!(
        work.lock().expect("work").queries,
        0,
        "statement did not finish"
    );
    assert_eq!(work.lock().expect("work").physical_checkouts, 0);
    drop(held);
    KnowledgeHandlers::load_compose_atom_window(&rt, "local", &refs)
        .await
        .expect("healthy after permit release");
    execute(&rt, "DROP TABLE knowledge_atoms", vec![]).await;
    for inputs in [refs, vec!["named".to_owned(), id.to_owned()]] {
        let error = KnowledgeHandlers::load_compose_atom_window(&rt, "local", &inputs)
            .await
            .expect_err("real SQLite statement error");
        let context = if Uuid::parse_str(&inputs[0]).is_ok() {
            "compose atom by id"
        } else {
            "compose atom by slug"
        };
        assert!(
            error.to_string().contains(context) && error.to_string().contains("knowledge_atoms"),
            "{error}"
        );
    }
}

#[tokio::test]
async fn compose_public_wire_original_and_candidate_receipt() {
    let dir = TempDir::new().expect("directory");
    use async_trait::async_trait;
    use khive_runtime::EmbedderProvider;
    use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};

    const MODEL: &str = "all-minilm-l6-v2";
    const DIMENSIONS: usize = 384;
    struct WireEmbeddingService;
    #[async_trait]
    impl EmbeddingService for WireEmbeddingService {
        async fn embed(
            &self,
            texts: &[String],
            _model: EmbeddingModel,
        ) -> Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts.iter().map(|_| vec![0.5; DIMENSIONS]).collect())
        }
        fn supports_model(&self, _model: EmbeddingModel) -> bool {
            true
        }
        fn name(&self) -> &'static str {
            "compose-wire-fixture"
        }
    }
    struct WireEmbeddingProvider;
    #[async_trait]
    impl EmbedderProvider for WireEmbeddingProvider {
        fn name(&self) -> &str {
            MODEL
        }
        fn dimensions(&self) -> usize {
            DIMENSIONS
        }
        async fn build(&self) -> Result<Arc<dyn EmbeddingService>, RuntimeError> {
            Ok(Arc::new(WireEmbeddingService))
        }
    }
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(dir.path().join("wire.db")),
        packs: vec![],
        embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
        ..RuntimeConfig::no_embeddings()
    })
    .expect("file-backed wire runtime with query embedding");
    rt.register_embedder(WireEmbeddingProvider);
    let id = "abcdef00-1111-2222-3333-444444444444";
    seed_atom(&rt, id, "local", "named", "draft", false).await;
    seed_atom(&rt, "badc0ffe-one", "local", "bad-one", "reviewed", false).await;
    seed_atom(&rt, "badc0ffe-two", "local", "bad-two", "reviewed", false).await;
    seed_domain(
        &rt,
        "wire",
        &[
            " missing ".into(),
            "named".into(),
            " missing ".into(),
            id.into(),
        ],
    )
    .await;
    execute(&rt, "INSERT INTO knowledge_sections(id,atom_id,namespace,section_type,heading,content,content_hash,created_at,updated_at) VALUES('22730000-0000-4000-8000-000000000002',?1,'local','overview','Code overview','code query source detail overview','wire-hash',1,1)", vec![SqlValue::Text(id.into())]).await;
    for (label, params) in [
        (
            "success",
            json!({"query":"code","domain_ids":["wire"],"atom_ids":[id,"named"],"blend_kg":false,"explain":true}),
        ),
        (
            "missing-first",
            json!({"query":"code","atom_ids":["missing","badc0ffe"],"blend_kg":false}),
        ),
        (
            "ambiguous-first",
            json!({"query":"code","atom_ids":["badc0ffe","missing"],"blend_kg":false}),
        ),
        (
            "raw-uuid",
            json!({"query":"code","atom_ids":[id.to_uppercase()],"blend_kg":false}),
        ),
        (
            "namespace-refusal",
            json!({"query":"code","atom_ids":[id],"namespace":"foreign","blend_kg":false}),
        ),
        (
            "empty-domain",
            json!({"query":"code","domain_ids":["wire"],"blend_kg":false,"max_tokens":0}),
        ),
    ] {
        let receipt = match compose(&rt, params).await {
            Ok(value) => {
                if label == "success" {
                    let sections = value["data"]["sections"].as_array().unwrap_or_else(|| {
                        panic!("real section scorer must emit its wire rows: {value}")
                    });
                    assert_eq!(sections.len(), 1, "exact seeded section selected: {value}");
                    assert_eq!(
                        sections[0]["section_id"],
                        "22730000-0000-4000-8000-000000000002"
                    );
                    assert_eq!(sections[0]["atom_id"], id);
                    assert_eq!(sections[0]["breakdown"]["domain_score"], 1.0);
                    assert_eq!(value["data"]["section_count"], 1);
                }
                serde_json::to_string(&value).expect("success JSON bytes")
            }
            Err(error) => serde_json::to_string(&json!({"error":error.to_string()}))
                .expect("error JSON bytes"),
        };
        println!("COMPOSE_WIRE {label} {receipt}");
    }
}
