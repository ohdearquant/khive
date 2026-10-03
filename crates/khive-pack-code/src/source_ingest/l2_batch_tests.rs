use super::*;
use crate::extractor::{CallRef, ExtractedImpl, TypeRef};
use khive_storage::types::{EdgeFilter, PageRequest};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use tempfile::TempDir;

#[derive(Default, Debug)]
struct Work {
    reads: HashMap<Uuid, usize>,
    writes: HashMap<Uuid, usize>,
    fts: HashMap<Uuid, usize>,
    visits: usize,
    normalizations: usize,
}
tokio::task_local! { static WORK: Arc<Mutex<Work>>; }
fn observe(action: impl FnOnce(&mut Work)) {
    let _ = WORK.try_with(|work| action(&mut work.lock().expect("work lock")));
}
pub(super) fn observe_row_read(id: Uuid) {
    observe(|work| *work.reads.entry(id).or_default() += 1);
}
pub(super) fn observe_row_write(id: Uuid) {
    observe(|work| *work.writes.entry(id).or_default() += 1);
}
pub(super) fn observe_fts_write(id: Uuid) {
    observe(|work| *work.fts.entry(id).or_default() += 1);
}
pub(super) fn observe_manifest_visit() {
    observe(|work| work.visits += 1);
}
pub(super) fn observe_manifest_normalization() {
    observe(|work| work.normalizations += 1);
}

struct Pause {
    entered: tokio::sync::Barrier,
    release: tokio::sync::Barrier,
    used: std::sync::atomic::AtomicBool,
}
impl Pause {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: tokio::sync::Barrier::new(2),
            release: tokio::sync::Barrier::new(2),
            used: std::sync::atomic::AtomicBool::new(false),
        })
    }
    async fn wait(&self) {
        if !self.used.swap(true, Ordering::AcqRel) {
            self.entered.wait().await;
            self.release.wait().await;
        }
    }
}
tokio::task_local! { static BEFORE_BATCH: Arc<Pause>; static BEFORE_REBASE: Arc<Pause>; }
pub(super) async fn pause_before_batch() {
    if let Ok(pause) = BEFORE_BATCH.try_with(Arc::clone) {
        pause.wait().await;
    }
}
pub(super) async fn pause_before_rebase() {
    if let Ok(pause) = BEFORE_REBASE.try_with(Arc::clone) {
        pause.wait().await;
    }
}

fn runtime(path: &Path, wal: bool) -> (KhiveRuntime, NamespaceToken) {
    let mode = if wal {
        super::tests::TestJournalMode::Wal
    } else {
        super::tests::TestJournalMode::Delete
    };
    super::tests::runtime_on_with_mode(path, mode)
}
fn fake_secret() -> String {
    ["AKIA", "ABCDEFGHIJKLMNOP"].concat()
}
fn reference(name: &str, evidence: &str) -> L2UnresolvedRef {
    L2UnresolvedRef {
        segments: vec![name.to_owned()],
        evidence: evidence.to_owned(),
    }
}
fn pending<T>(value: T, file: &str) -> PendingL2<T> {
    PendingL2 {
        value,
        file: file.to_owned(),
    }
}
fn time(seconds: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(seconds, 0).expect("fixture time")
}
fn declaration(name: &str, kind: DeclKind) -> ExtractedDeclaration {
    ExtractedDeclaration {
        kind,
        name: name.to_owned(),
        description: None,
        content_hash: "fixture-hash".into(),
        calls: vec![],
        type_refs: vec![],
        module_segments: vec![],
    }
}
async fn seed(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
    kind: &str,
    subtype: &str,
    name: &str,
    props: Value,
) {
    let mut entity =
        Entity::new(token.namespace().as_str(), kind, name).with_entity_type(Some(subtype));
    entity.id = id;
    entity.created_at = time(1).timestamp_micros();
    entity.updated_at = time(1).timestamp_micros();
    entity.properties = Some(props);
    rt.entities(token)
        .expect("entities")
        .upsert_entity(entity)
        .await
        .expect("seed");
}
async fn stored(rt: &KhiveRuntime, token: &NamespaceToken, id: Uuid) -> Entity {
    rt.entities(token)
        .expect("entities")
        .get_entity_including_deleted(id)
        .await
        .expect("read")
        .expect("owner")
}
async fn script(rt: &KhiveRuntime, sql: String) {
    rt.sql()
        .writer()
        .await
        .expect("writer")
        .execute_script(sql)
        .await
        .expect("fixture SQL");
}

async fn owner_batch<T>(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
    key: &str,
    entries: &[PendingL2<T>],
) -> (CodeSourceIngestReport, Arc<Mutex<Work>>)
where
    T: Clone + Eq + Hash + serde::Serialize + serde::de::DeserializeOwned,
{
    let work = Arc::new(Mutex::new(Work::default()));
    let mut report = CodeSourceIngestReport::default();
    WORK.scope(
        Arc::clone(&work),
        record_l2_pending_batch(rt, token, id, key, entries, &mut report),
    )
    .await
    .expect("batch");
    (report, work)
}

#[tokio::test]
async fn l2_reference_and_impl_batches_observe_actual_reads_writes_and_fts() {
    for k in [8, 64, 256] {
        let dir = TempDir::new().expect("directory");
        let (rt, token) = runtime(&dir.path().join("work.db"), true);
        let symbol = symbol_uuid("fixture", "rust", "crate", "owner", "function");
        let module = module_uuid("fixture", "rust", "crate");
        seed(
            &rt,
            &token,
            symbol,
            "concept",
            "function",
            "owner",
            json!({"unrelated": [3, 2, 1]}),
        )
        .await;
        seed(
            &rt,
            &token,
            module,
            "concept",
            "module",
            "crate",
            json!({"unrelated": [3, 2, 1]}),
        )
        .await;
        let mut refs: Vec<_> = (0..k)
            .map(|i| pending(reference(&format!("missing_{i}"), "call"), "calls.rs"))
            .collect();
        refs.push(refs[0].clone());
        refs.push(pending(
            reference("missing_0", "type_reference"),
            "types.rs",
        ));
        let expected_refs: Vec<_> = (0..k)
            .map(|i| reference(&format!("missing_{i}"), "call"))
            .chain([reference("missing_0", "type_reference")])
            .collect();
        let impls: Vec<_> = (0..k)
            .map(|i| {
                pending(
                    L2PendingImpl {
                        type_path: vec![format!("Type{i}")],
                        trait_path: vec![format!("Trait{i}")],
                    },
                    "impl.rs",
                )
            })
            .collect();
        for (id, key) in [
            (symbol, "l2_unresolved_references"),
            (module, "l2_pending_impls"),
        ] {
            let (report, work) = if id == symbol {
                owner_batch(&rt, &token, id, key, &refs).await
            } else {
                owner_batch(&rt, &token, id, key, &impls).await
            };
            assert_eq!(report.fts_indexed, 1);
            {
                let actual = work.lock().expect("work");
                assert_eq!(
                    actual.reads[&id], 2,
                    "one stage read plus one fresh CAS read"
                );
                assert_eq!(
                    actual.writes[&id], 1,
                    "counts only successful guarded row writes"
                );
                assert_eq!(actual.fts[&id], 1, "counts completed real index calls");
            }
            let (again, work) = if id == symbol {
                owner_batch(&rt, &token, id, key, &refs).await
            } else {
                owner_batch(&rt, &token, id, key, &impls).await
            };
            assert_eq!(again.fts_indexed, 0);
            let actual = work.lock().expect("work");
            assert_eq!(actual.reads[&id], 1);
            assert!(actual.writes.is_empty() && actual.fts.is_empty());
        }
        let symbol = stored(&rt, &token, symbol).await;
        assert_eq!(
            read_l2_unresolved(symbol.properties.as_ref().expect("properties")),
            expected_refs
        );
        assert_eq!(
            symbol.properties.as_ref().expect("properties")["unrelated"],
            json!([3, 2, 1])
        );
        let module = stored(&rt, &token, module).await;
        assert_eq!(
            read_l2_pending_impls(module.properties.as_ref().expect("properties")),
            impls
                .iter()
                .map(|item| item.value.clone())
                .collect::<Vec<_>>()
        );
    }
}

#[tokio::test]
async fn l2_persist_calls_then_types_and_impls_preserve_phase_owner_order() {
    let dir = TempDir::new().expect("directory");
    let (rt, token) = runtime(&dir.path().join("phase.db"), true);
    let module = module_uuid("fixture", "rust", "crate");
    seed(
        &rt,
        &token,
        module,
        "concept",
        "module",
        "crate",
        json!({"unrelated": true}),
    )
    .await;
    let mut first = declaration("first", DeclKind::Function);
    first.calls = vec![
        CallRef {
            segments: vec!["z".into()],
        },
        CallRef {
            segments: vec!["a".into()],
        },
        CallRef {
            segments: vec!["z".into()],
        },
        CallRef {
            segments: vec!["helper".into()],
        },
    ];
    first.type_refs = vec![
        TypeRef {
            segments: vec!["z".into()],
        },
        TypeRef {
            segments: vec!["b".into()],
        },
        TypeRef {
            segments: vec!["first".into()],
        },
    ];
    let parsed = ExtractedFile {
        declarations: vec![first, declaration("helper", DeclKind::Function)],
        impls: vec![ExtractedImpl {
            type_path: vec!["MissingType".into()],
            trait_path: vec!["MissingTrait".into()],
            module_segments: vec!["inline".into()],
        }],
    };
    let mut state = L2SweepState::default();
    let mut report = CodeSourceIngestReport {
        l2: Some(CodeSourceIngestL2Report::default()),
        ..Default::default()
    };
    persist_l2_file(
        &rt,
        &token,
        "fixture",
        "rust",
        module,
        "crate",
        "src/lib.rs",
        "unversioned",
        "hash",
        Ok(&parsed),
        time(2),
        "src/lib.rs",
        &mut state,
        &mut report,
    )
    .await
    .expect("persist");
    let first = stored(
        &rt,
        &token,
        symbol_uuid("fixture", "rust", "crate", "first", "function"),
    )
    .await;
    assert_eq!(
        read_l2_unresolved(first.properties.as_ref().expect("properties")),
        vec![
            reference("z", "call"),
            reference("a", "call"),
            reference("z", "type_reference"),
            reference("b", "type_reference"),
            reference("first", "type_reference")
        ]
    );
    let module = stored(&rt, &token, module).await;
    assert_eq!(
        module.properties.as_ref().expect("properties")["l2_pending_impls"],
        json!([{"type_path":["MissingType"],"trait_path":["MissingTrait"]}])
    );
    assert_eq!(
        module.properties.as_ref().expect("properties")["l2_content_hash"],
        "hash"
    );
    let edge = rt
        .graph(&token)
        .expect("graph")
        .get_edge(LinkId::from(edge_uuid(
            EdgeRelation::DependsOn,
            first.id,
            symbol_uuid("fixture", "rust", "crate", "helper", "function"),
        )))
        .await
        .expect("edge")
        .expect("positive edge");
    assert_eq!(
        edge.metadata.expect("metadata")["l2_evidence"],
        json!(["call"])
    );
}

fn strip_fields(mut value: Value, fields: &[&str]) -> Vec<u8> {
    let object = value.as_object_mut().expect("serialized row object");
    for field in fields {
        assert!(
            object.remove(*field).is_some(),
            "deny-list field must exist: {field}"
        );
    }
    serde_json::to_vec(&value).expect("serialize every remaining field")
}

type SerializedRows = BTreeMap<Uuid, Vec<u8>>;
type FileSnapshot = (SerializedRows, SerializedRows, SerializedRows);

async fn snapshot_file(rt: &KhiveRuntime, token: &NamespaceToken, ids: &[Uuid]) -> FileSnapshot {
    let mut entities = BTreeMap::new();
    let mut documents = BTreeMap::new();
    let mut edges = BTreeMap::new();
    for id in ids {
        let entity = stored(rt, token, *id).await;
        let doc = rt
            .text(token)
            .expect("text")
            .get_document(&entity.namespace, *id)
            .await
            .expect("actual FTS read")
            .expect("indexed row");
        assert_eq!(doc.updated_at.timestamp_micros(), entity.updated_at);
        entities.insert(
            *id,
            strip_fields(
                serde_json::to_value(&entity).expect("serialize entity"),
                &["updated_at", "version"],
            ),
        );
        documents.insert(
            *id,
            strip_fields(
                serde_json::to_value(doc).expect("serialize FTS"),
                &["updated_at"],
            ),
        );
    }
    let graph = rt
        .graph(token)
        .expect("graph")
        .query_edges(
            EdgeFilter::default(),
            Vec::new(),
            PageRequest {
                limit: 1000,
                offset: 0,
            },
        )
        .await
        .expect("edges");
    for edge in graph
        .items
        .into_iter()
        .filter(|edge| ids.contains(&edge.source_id) || ids.contains(&edge.target_id))
    {
        assert!(
            edge.updated_at >= edge.created_at,
            "graph_edges.updated_at history invariant"
        );
        edges.insert(
            Uuid::from(edge.id),
            strip_fields(
                serde_json::to_value(edge).expect("serialize edge"),
                &["created_at", "updated_at"],
            ),
        );
    }
    (entities, documents, edges)
}
async fn ingest_at(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    root: &Path,
    at: DateTime<Utc>,
) -> Result<CodeSourceIngestReport, CodeSourceIngestError> {
    run_code_ingest(
        rt,
        token,
        CodeSourceIngestOptions {
            path: root,
            languages: ["rust"].into_iter().collect(),
            sweep_time: at,
            enable_l1: false,
            enable_l1_5: false,
            enable_l2: true,
        },
    )
    .await
}

#[tokio::test]
async fn l2_failed_file_retry_denylists_only_five_history_fields_and_reindexes() {
    for wal in [true, false] {
        for fts_fault in [false, true] {
            let dir = TempDir::new().expect("directory");
            let root = dir.path().join("source");
            fs::create_dir_all(root.join("src")).expect("source directory");
            fs::write(
                root.join("Cargo.toml"),
                "[package]\nname=\"fixture\"\nversion=\"0.1.0\"\n",
            )
            .expect("manifest");
            fs::write(root.join("src/lib.rs"), "fn owner() { missing_a(); helper(); missing_b(); }\nfn helper() {}\nimpl MissingTrait for MissingType {}\n").expect("source");
            let parsed =
                parse_rust_file(&fs::read_to_string(root.join("src/lib.rs")).expect("source"))
                    .expect("parse");
            let module = module_uuid("fixture", "rust", "crate");
            let ids: Vec<_> = std::iter::once(module)
                .chain(parsed.declarations.iter().map(|decl| {
                    symbol_uuid(
                        "fixture",
                        "rust",
                        "crate",
                        &decl.name,
                        decl.kind.code_token(),
                    )
                }))
                .collect();
            let (retry, token) = runtime(&dir.path().join("retry.db"), wal);
            let (clean, clean_token) = runtime(&dir.path().join("clean.db"), wal);
            for (rt, token) in [(&retry, &token), (&clean, &clean_token)] {
                seed(
                    rt,
                    token,
                    project_uuid("fixture"),
                    "project",
                    "project",
                    "fixture",
                    json!({}),
                )
                .await;
                seed(
                    rt,
                    token,
                    module,
                    "concept",
                    "module",
                    "crate",
                    json!({"unrelated": [2, 1]}),
                )
                .await;
                for decl in &parsed.declarations {
                    seed(
                        rt,
                        token,
                        symbol_uuid(
                            "fixture",
                            "rust",
                            "crate",
                            &decl.name,
                            decl.kind.code_token(),
                        ),
                        "concept",
                        decl.kind.code_token(),
                        &decl.name,
                        json!({"unrelated": [2, 1]}),
                    )
                    .await;
                }
            }
            let owner = symbol_uuid("fixture", "rust", "crate", "owner", "function");
            let fault = if fts_fault {
                format!("CREATE TRIGGER l2_fault BEFORE INSERT ON fts_entities_rowids WHEN NEW.subject_id='{owner}' AND EXISTS (SELECT 1 FROM entities WHERE id=NEW.subject_id AND json_array_length(json_extract(properties,'$.l2_unresolved_references'))>0) BEGIN SELECT RAISE(ABORT,'l2_fts_fault'); END;")
            } else {
                format!("CREATE TRIGGER l2_fault BEFORE UPDATE ON entities WHEN NEW.id='{module}' AND json_array_length(json_extract(NEW.properties,'$.l2_pending_impls'))>0 AND COALESCE(json_array_length(json_extract(OLD.properties,'$.l2_pending_impls')),0)=0 BEGIN SELECT RAISE(ABORT,'l2_impl_fault'); END;")
            };
            script(&retry, fault).await;
            let expected_error = if fts_fault {
                let before = stored(&retry, &token, owner).await;
                let mut probe = before.clone();
                probe.properties.as_mut().expect("properties")["l2_unresolved_references"] =
                    json!([reference("probe", "call")]);
                retry
                    .entities(&token)
                    .expect("entities")
                    .upsert_entity(probe.clone())
                    .await
                    .expect("probe committed base row");
                let cause = retry
                    .text(&token)
                    .expect("text")
                    .upsert_document(entity_fts_document(&probe))
                    .await
                    .expect_err("real FTS trigger refusal");
                retry
                    .entities(&token)
                    .expect("entities")
                    .upsert_entity(before)
                    .await
                    .expect("restore semantic seed before ingest");
                CodeSourceIngestError::Storage(format!("entity FTS indexing: {cause}")).to_string()
            } else {
                let before = stored(&retry, &token, module).await;
                let mut probe = before.clone();
                probe.properties.as_mut().expect("properties")["l2_pending_impls"] =
                    json!([{"type_path":["ProbeType"],"trait_path":["ProbeTrait"]}]);
                probe.updated_at += 1;
                let cause = retry
                    .entities(&token)
                    .expect("entities")
                    .replace_entity_if_unchanged(probe, before.updated_at, before.deleted_at)
                    .await
                    .expect_err("real row trigger refusal");
                CodeSourceIngestError::Storage(cause.to_string()).to_string()
            };
            let error = ingest_at(&retry, &token, &root, time(2))
                .await
                .expect_err("real storage failure");
            assert!(matches!(error, CodeSourceIngestError::Storage(_)));
            assert_eq!(
                error.to_string(),
                expected_error,
                "C6 retains exact old primitive error text without batch prefix"
            );
            assert!(error.to_string().contains(if fts_fault {
                "l2_fts_fault"
            } else {
                "l2_impl_fault"
            }));
            assert_eq!(
                error
                    .to_string()
                    .matches(if fts_fault {
                        "l2_fts_fault"
                    } else {
                        "l2_impl_fault"
                    })
                    .count(),
                1
            );
            let failed = ids.clone();
            let mut failed_revisions = BTreeMap::new();
            for id in &failed {
                let row = stored(&retry, &token, *id).await;
                failed_revisions.insert(*id, (row.updated_at, row.version));
            }
            let failed_module = stored(&retry, &token, module).await;
            let failed_stamp_absent = failed_module
                .properties
                .as_ref()
                .expect("properties")
                .get("l2_content_hash")
                .is_none()
                && failed_module
                    .properties
                    .as_ref()
                    .expect("properties")
                    .get("declaration_ids")
                    .is_none();
            script(&retry, "DROP TRIGGER l2_fault;".into()).await;
            ingest_at(&retry, &token, &root, time(3))
                .await
                .expect("retry");
            ingest_at(&clean, &clean_token, &root, time(3))
                .await
                .expect("clean");
            let retried = snapshot_file(&retry, &token, &ids).await;
            let clean_rows = snapshot_file(&clean, &clean_token, &ids).await;
            assert_eq!(retried, clean_rows, "every serialized field outside five exact deny-list fields (future fields included)");
            assert!(
                failed_stamp_absent,
                "C1 no completion stamp at failed flush"
            );
            for id in &ids {
                let actual = stored(&retry, &token, *id).await;
                assert!(actual.updated_at > failed_revisions[id].0);
                assert!(actual.version > failed_revisions[id].1);
                assert_eq!(
                    actual.created_at,
                    time(1).timestamp_micros(),
                    "identical preseeding keeps entity.created_at compared"
                );
            }
            let module_row = stored(&retry, &token, module).await;
            assert_eq!(
                module_row.properties.as_ref().expect("properties")["l2_pending_impls"],
                json!([{"type_path":["MissingType"],"trait_path":["MissingTrait"]}]),
                "post-stamp impl-flush control must miss this retry entry"
            );
            assert!(module_row
                .properties
                .as_ref()
                .expect("properties")
                .get("declaration_ids")
                .is_some());
        }
    }
}

#[tokio::test]
async fn l2_concurrent_batches_fresh_cas_preserve_both_in_wal_and_delete() {
    for wal in [true, false] {
        let dir = TempDir::new().expect("directory");
        let path = dir.path().join("race.db");
        let (a, ta) = runtime(&path, wal);
        let id = Uuid::from_u128(11);
        seed(
            &a,
            &ta,
            id,
            "concept",
            "function",
            "owner",
            json!({"unrelated": true}),
        )
        .await;
        let (b, tb) = runtime(&path, wal);
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let pa = Arc::new(race_seam::OneShotPause::new(Arc::clone(&barrier)));
        let pb = Arc::new(race_seam::OneShotPause::new(barrier));
        let aa = [pending(reference("a", "call"), "a.rs")];
        let bb = [pending(reference("b", "call"), "b.rs")];
        let mut ra = CodeSourceIngestReport::default();
        let mut rb = CodeSourceIngestReport::default();
        let wa = Arc::new(Mutex::new(Work::default()));
        let wb = Arc::new(Mutex::new(Work::default()));
        let (ar, br) = tokio::join!(
            WORK.scope(
                Arc::clone(&wa),
                race_seam::AFTER_ROW_READ.scope(
                    pa,
                    record_l2_pending_batch(&a, &ta, id, "l2_unresolved_references", &aa, &mut ra)
                )
            ),
            WORK.scope(
                Arc::clone(&wb),
                race_seam::AFTER_ROW_READ.scope(
                    pb,
                    record_l2_pending_batch(&b, &tb, id, "l2_unresolved_references", &bb, &mut rb)
                )
            )
        );
        ar.expect("writer A");
        br.expect("writer B");
        let row = stored(&a, &ta, id).await;
        let refs = read_l2_unresolved(row.properties.as_ref().expect("properties"));
        assert_eq!(refs.len(), 2);
        assert!(refs.contains(&reference("a", "call")) && refs.contains(&reference("b", "call")));
        assert_eq!(
            row.properties.as_ref().expect("properties")["unrelated"],
            true
        );
        assert!(row.version >= 3);
        assert_eq!(ra.fts_indexed + rb.fts_indexed, 2);
        assert_eq!(
            wa.lock().expect("work").writes[&id] + wb.lock().expect("work").writes[&id],
            2
        );
        assert_eq!(
            wa.lock().expect("work").reads[&id] + wb.lock().expect("work").reads[&id],
            5,
            "two stage reads and three CAS reads, one losing CAS"
        );
    }
}

#[tokio::test]
async fn l2_concurrent_refusal_reports_each_new_occurrence_and_no_commit() {
    for wal in [true, false] {
        let dir = TempDir::new().expect("directory");
        let path = dir.path().join("refusal.db");
        let (rt, token) = runtime(&path, wal);
        let (other, other_token) = runtime(&path, wal);
        let id = Uuid::from_u128(12);
        seed(
            &rt,
            &token,
            id,
            "concept",
            "function",
            "owner",
            json!({"l2_unresolved_references":[reference("existing", "call")]}),
        )
        .await;
        let entries = [
            pending(reference("existing", "call"), "old.rs"),
            pending(reference("new", "call"), "first.rs"),
            pending(reference("new", "call"), "second.rs"),
            pending(reference("new", "type_reference"), "type.rs"),
        ];
        let pause = Pause::new();
        let work = Arc::new(Mutex::new(Work::default()));
        let mut report = CodeSourceIngestReport::default();
        let (result, concurrent) = tokio::join!(
            WORK.scope(
                Arc::clone(&work),
                BEFORE_BATCH.scope(
                    Arc::clone(&pause),
                    record_l2_pending_batch(
                        &rt,
                        &token,
                        id,
                        "l2_unresolved_references",
                        &entries,
                        &mut report
                    )
                )
            ),
            async {
                pause.entered.wait().await;
                let mut row = stored(&other, &other_token, id).await;
                row.description = Some(fake_secret());
                other
                    .entities(&other_token)
                    .expect("entities")
                    .upsert_entity(row.clone())
                    .await
                    .expect("concurrent write");
                let expected = match gate_check(&row).expect_err("real secret detector") {
                    RuntimeError::SecretDetected(secret) => {
                        (secret.detector.to_string(), secret.masked)
                    }
                    other => panic!("wrong refusal {other}"),
                };
                let concurrent = stored(&other, &other_token, id).await;
                pause.release.wait().await;
                (expected, concurrent)
            }
        );
        assert!(!result.expect("nonfatal refusal"));
        assert_eq!(report.blocked_count, 3);
        assert_eq!(
            report
                .blocked
                .iter()
                .map(|item| item.file.as_str())
                .collect::<Vec<_>>(),
            ["first.rs", "second.rs", "type.rs"]
        );
        for item in &report.blocked {
            assert_eq!(
                (&item.detector, &item.masked_excerpt),
                (&concurrent.0 .0, &concurrent.0 .1)
            );
        }
        assert_eq!(
            serde_json::to_value(stored(&rt, &token, id).await).expect("row"),
            serde_json::to_value(concurrent.1).expect("concurrent row")
        );
        assert_eq!(report.fts_indexed, 0);
        assert!(work.lock().expect("work").writes.is_empty());
        assert!(work.lock().expect("work").fts.is_empty());
        assert!(report
            .blocked
            .iter()
            .all(|item| !item.masked_excerpt.contains(fake_secret().as_str())));
    }
}

#[tokio::test]
async fn l2_missing_empty_duplicate_reserved_and_revision_exhaustion_companions() {
    let dir = TempDir::new().expect("directory");
    let (rt, token) = runtime(&dir.path().join("companions.db"), true);
    let id = Uuid::from_u128(13);
    let values = [pending(reference("new", "call"), "source.rs")];
    let mut report = CodeSourceIngestReport::default();
    assert!(!record_l2_pending_batch(
        &rt,
        &token,
        id,
        "l2_unresolved_references",
        &values,
        &mut report
    )
    .await
    .expect("missing"));
    seed(&rt, &token, id, "concept", "function", "owner", json!({"l2_unresolved_references":[reference("new", "call")], "khive:secret_gate":"reserved"})).await;
    assert!(!record_l2_pending_batch(
        &rt,
        &token,
        id,
        "l2_unresolved_references",
        &values,
        &mut report
    )
    .await
    .expect("existing is no-op before screening"));
    let new = [pending(reference("other", "call"), "source.rs")];
    let error = record_l2_pending_batch(
        &rt,
        &token,
        id,
        "l2_unresolved_references",
        &new,
        &mut report,
    )
    .await
    .expect_err("reserved property refused");
    let expected = gate_check(&stored(&rt, &token, id).await).expect_err("existing refusal");
    assert_eq!(
        error.to_string(),
        CodeSourceIngestError::Runtime(expected).to_string(),
        "C6 no batch prefix"
    );
    assert!(!record_l2_pending_batch::<L2UnresolvedRef>(
        &rt,
        &token,
        id,
        "l2_unresolved_references",
        &[],
        &mut report
    )
    .await
    .expect("empty"));
    script(
        &rt,
        format!(
            "UPDATE entities SET properties='{{}}',updated_at={},version=version+1 WHERE id='{id}';",
            i64::MAX
        ),
    )
    .await;
    let error = record_l2_pending_batch(
        &rt,
        &token,
        id,
        "l2_unresolved_references",
        &new,
        &mut report,
    )
    .await
    .expect_err("revision exhausts");
    assert_eq!(
        error.to_string(),
        format!(
            "storage error: entity revision {} cannot advance past i64::MAX",
            i64::MAX
        )
    );
    let mut exhausted = stored(&rt, &token, id).await;
    exhausted.description = Some(fake_secret());
    rt.entities(&token)
        .expect("entities")
        .upsert_entity(exhausted)
        .await
        .expect("direct exhausted unsafe owner seed");
    let error = record_l2_pending_batch(
        &rt,
        &token,
        id,
        "l2_unresolved_references",
        &new,
        &mut report,
    )
    .await
    .expect_err("revision refusal precedes new-entry screening");
    assert_eq!(
        error.to_string(),
        format!(
            "storage error: entity revision {} cannot advance past i64::MAX",
            i64::MAX
        )
    );
    assert_eq!(report.fts_indexed, 0);
}

type MembershipCounters = (Arc<AtomicUsize>, Arc<AtomicUsize>);
std::thread_local! {
    static MEMBERSHIP_COUNTERS: std::cell::RefCell<Option<MembershipCounters>> = const {
        std::cell::RefCell::new(None)
    };
}
struct MembershipCounterScope(Option<MembershipCounters>);
impl MembershipCounterScope {
    fn new(counters: MembershipCounters) -> Self {
        Self(MEMBERSHIP_COUNTERS.with(|slot| slot.replace(Some(counters))))
    }
}
impl Drop for MembershipCounterScope {
    fn drop(&mut self) {
        MEMBERSHIP_COUNTERS.with(|slot| {
            slot.replace(self.0.take());
        });
    }
}
#[derive(Clone)]
struct Counted<T> {
    value: T,
}
impl<T: Eq> PartialEq for Counted<T> {
    fn eq(&self, other: &Self) -> bool {
        MEMBERSHIP_COUNTERS.with(|slot| {
            slot.borrow()
                .as_ref()
                .expect("synchronous membership measurement")
                .0
                .fetch_add(1, Ordering::Relaxed);
        });
        self.value == other.value
    }
}
impl<T: Eq> Eq for Counted<T> {}
impl<T: Hash> Hash for Counted<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        MEMBERSHIP_COUNTERS.with(|slot| {
            slot.borrow()
                .as_ref()
                .expect("synchronous membership measurement")
                .1
                .fetch_add(1, Ordering::Relaxed);
        });
        self.value.hash(state);
    }
}
fn membership_work<T: Clone + Eq + Hash>(values: Vec<T>) {
    let comparisons = Arc::new(AtomicUsize::new(0));
    let hashes = Arc::new(AtomicUsize::new(0));
    let _counters = MembershipCounterScope::new((Arc::clone(&comparisons), Arc::clone(&hashes)));
    let input: Vec<_> = values
        .iter()
        .cloned()
        .map(|value| Counted { value })
        .collect();
    let mut actual = Vec::new();
    append_l2_pending(&mut actual, &input);
    append_l2_pending(&mut actual, &input);
    let expected: Vec<_> = values.into_iter().fold(Vec::new(), |mut list, value| {
        if !list.contains(&value) {
            list.push(value);
        }
        list
    });
    assert!(actual.iter().map(|item| &item.value).eq(expected.iter()));
    assert!(
        comparisons.load(Ordering::Relaxed) < 20 * input.len(),
        "actual Eq calls reveal quadratic Vec::contains control"
    );
    assert!(hashes.load(Ordering::Relaxed) >= input.len());
    let original: HashSet<_> = input.iter().take(input.len() / 2).cloned().collect();
    let remaining = input[input.len() / 4..input.len() / 2].to_vec();
    let mut current = actual.clone();
    current.reverse();
    let mut oracle: Vec<_> = current
        .iter()
        .map(|item| item.value.clone())
        .filter(|value| !original.iter().any(|old| old.value == *value))
        .collect();
    for item in &remaining {
        if !oracle.contains(&item.value) {
            oracle.push(item.value.clone());
        }
    }
    comparisons.store(0, Ordering::Relaxed);
    hashes.store(0, Ordering::Relaxed);
    rebase_l2_pending(&mut current, &original, &remaining);
    assert!(current.iter().map(|item| &item.value).eq(oracle.iter()));
    assert!(comparisons.load(Ordering::Relaxed) < 20 * input.len());
}
#[test]
fn l2_membership_real_eq_hash_work_for_reference_and_impl_keys() {
    for k in [64, 512, 2048] {
        membership_work(
            (0..k)
                .map(|i| {
                    reference(
                        &format!("name_{i}"),
                        if i % 2 == 0 { "call" } else { "type_reference" },
                    )
                })
                .collect(),
        );
        membership_work(
            (0..k)
                .map(|i| L2PendingImpl {
                    type_path: vec![format!("T{i}")],
                    trait_path: vec![format!("Trait{i}")],
                })
                .collect(),
        );
    }
}

#[tokio::test]
async fn l2_manifest_range_counts_actual_visits_and_preserves_collision_scope() {
    let mut scopes = ManifestScopeIndex::new();
    for i in 0..2048 {
        scopes.insert(
            (format!("unrelated_{i:04}"), "rust".into(), "target".into()),
            ["normal".to_owned()].into_iter().collect(),
        );
    }
    for (project, language, target, scope) in [
        ("selected", "rust", "", "normal"),
        ("selected", "rust", "foo-bar", "dev"),
        ("selected", "rust", "foo_bar", "build"),
        ("selected", "rust", "λ", "normal"),
        ("selected", "rust-extra", "foo-bar", "normal"),
        ("selected-next", "rust", "foo-bar", "normal"),
        ("selected", "python", "foo_bar", "build"),
    ] {
        scopes.insert(
            (project.into(), language.into(), target.into()),
            [scope.to_owned()].into_iter().collect(),
        );
    }
    let work = Arc::new(Mutex::new(Work::default()));
    let found = WORK
        .scope(Arc::clone(&work), async {
            declared_project_import_target_and_scope(&scopes, "selected", "rust", "foo_bar")
                .expect("normalized")
        })
        .await;
    assert_eq!(found.target, "foo-bar");
    assert_eq!(found.scope, "dev");
    assert_eq!(found.normalization_matches, ["foo-bar", "foo_bar"]);
    assert_eq!(
        work.lock().expect("work").visits,
        5,
        "four in range and one stopping tuple"
    );
    assert_eq!(work.lock().expect("work").normalizations, 4);
    for (project, language, target, expected) in [
        ("selected", "rust", "", Some("")),
        ("selected", "rust", "λ", Some("λ")),
        ("selected", "rust", "missing", None),
        ("selected", "python", "foo_bar", Some("foo_bar")),
        ("selected-prefix", "rust", "foo_bar", None),
        ("selected", "rust-absent", "foo_bar", None),
    ] {
        let work = Arc::new(Mutex::new(Work::default()));
        let result = WORK
            .scope(Arc::clone(&work), async {
                declared_project_import_target_and_scope(&scopes, project, language, target)
            })
            .await;
        assert_eq!(result.as_ref().map(|item| item.target.as_str()), expected);
        assert!(work.lock().expect("work").visits <= 5);
    }
    let mut aliases = ProjectRenames::new();
    aliases.insert(
        ("selected".into(), "rust".into(), "alias".into()),
        "foo_bar".into(),
    );
    let aliased = project_import_target_and_scope(&scopes, &aliases, "selected", "rust", "alias");
    assert_eq!(aliased.target, "foo-bar");
    assert_eq!(aliased.scope, "dev");
}

#[tokio::test]
async fn l2_scanner_to_persist_observes_actual_phase_batches_not_outer_counts() {
    for k in [8, 64] {
        let dir = TempDir::new().expect("directory");
        let (rt, token) = runtime(&dir.path().join("reachable.db"), true);
        let module = module_uuid("fixture", "rust", "crate");
        seed(&rt, &token, module, "concept", "module", "crate", json!({})).await;
        let arguments = (0..k)
            .map(|i| format!("_x{i}: missing_{i}"))
            .collect::<Vec<_>>()
            .join(",");
        let calls = (0..k)
            .map(|i| format!("missing_{i}();"))
            .collect::<String>();
        let impls = (0..k)
            .map(|i| format!("impl Trait{i} for Type{i} {{}}\n"))
            .collect::<String>();
        let parsed = parse_rust_file(&format!(
            "fn owner({arguments}) {{{calls} missing_0(); helper();}}\nfn helper() {{}}\n{impls}"
        ))
        .expect("real scanner");
        let owner = symbol_uuid("fixture", "rust", "crate", "owner", "function");
        let mut report = CodeSourceIngestReport {
            l2: Some(CodeSourceIngestL2Report::default()),
            ..Default::default()
        };
        let mut state = L2SweepState::default();
        let work = Arc::new(Mutex::new(Work::default()));
        WORK.scope(
            Arc::clone(&work),
            persist_l2_file(
                &rt,
                &token,
                "fixture",
                "rust",
                module,
                "crate",
                "src/lib.rs",
                "unversioned",
                "hash",
                Ok(&parsed),
                time(2),
                "src/lib.rs",
                &mut state,
                &mut report,
            ),
        )
        .await
        .expect("persist real extraction");
        {
            let actual = work.lock().expect("work");
            assert_eq!(
                actual.reads[&owner], 3,
                "Phase A and one stage+CAS pending batch"
            );
            assert_eq!(actual.writes[&owner], 2);
            assert_eq!(actual.fts[&owner], 2);
            assert_eq!(
                actual.reads[&module], 3,
                "stage+CAS impl batch and completion stamp"
            );
            assert_eq!(actual.writes[&module], 2);
            assert_eq!(actual.fts[&module], 2);
        }
        let row = stored(&rt, &token, owner).await;
        let refs = read_l2_unresolved(row.properties.as_ref().expect("properties"));
        assert_eq!(refs.len(), 2 * k);
        assert!(refs[..k].iter().all(|item| item.evidence == "call"));
        assert!(refs[k..]
            .iter()
            .all(|item| item.evidence == "type_reference"));
        let work = Arc::new(Mutex::new(Work::default()));
        WORK.scope(
            Arc::clone(&work),
            persist_l2_file(
                &rt,
                &token,
                "fixture",
                "rust",
                module,
                "crate",
                "src/lib.rs",
                "unversioned",
                "hash",
                Ok(&parsed),
                time(3),
                "src/lib.rs",
                &mut state,
                &mut report,
            ),
        )
        .await
        .expect("repeat");
        let actual = work.lock().expect("work");
        assert_eq!(actual.writes[&owner], 1, "only declaration refresh");
        assert_eq!(actual.fts[&owner], 1);
        assert_eq!(actual.writes[&module], 1, "only stamp");
        assert_eq!(actual.fts[&module], 1);
    }
}

#[tokio::test]
async fn l2_replay_rebases_concurrent_reference_and_impl_additions_in_both_modes() {
    for wal in [true, false] {
        for impl_path in [false, true] {
            let dir = TempDir::new().expect("directory");
            let path = dir.path().join("replay.db");
            let (rt, token) = runtime(&path, wal);
            let (other, other_token) = runtime(&path, wal);
            let module = module_uuid("fixture", "rust", "crate");
            let symbol = symbol_uuid("fixture", "rust", "crate", "owner", "function");
            let target = symbol_uuid(
                "fixture",
                "rust",
                "crate",
                "resolved",
                if impl_path { "datatype" } else { "function" },
            );
            let trait_id = symbol_uuid("fixture", "rust", "crate", "Trait", "interface");
            let resolved_impl = L2PendingImpl {
                type_path: vec!["resolved".into()],
                trait_path: vec!["Trait".into()],
            };
            let remaining_impl = L2PendingImpl {
                type_path: vec!["missing".into()],
                trait_path: vec!["Trait".into()],
            };
            let concurrent_impl = L2PendingImpl {
                type_path: vec!["concurrent".into()],
                trait_path: vec!["Trait".into()],
            };
            let owner = if impl_path { module } else { symbol };
            let key = if impl_path {
                "l2_pending_impls"
            } else {
                "l2_unresolved_references"
            };
            let initial = if impl_path {
                json!([resolved_impl, remaining_impl])
            } else {
                json!([reference("resolved", "call"), reference("missing", "call")])
            };
            seed(&rt, &token, owner, "concept", if impl_path { "module" } else { "function" }, "owner", json!({"source_project":"fixture","language":"rust","module_path":"crate",(key):initial,"unrelated":[2,1]})).await;
            seed(
                &rt,
                &token,
                target,
                "concept",
                if impl_path { "datatype" } else { "function" },
                "resolved",
                json!({}),
            )
            .await;
            if impl_path {
                seed(
                    &rt,
                    &token,
                    trait_id,
                    "concept",
                    "interface",
                    "Trait",
                    json!({}),
                )
                .await;
            }
            let mut state = L2SweepState::default();
            state.mark_current_module(module, "fixture", "rust");
            state.mark_current_declarations(&[symbol, target, trait_id], "fixture", "rust");
            let mut report = CodeSourceIngestReport {
                l2: Some(CodeSourceIngestL2Report::default()),
                ..Default::default()
            };
            let pause = Pause::new();
            let (result, ()) = tokio::join!(
                BEFORE_REBASE.scope(
                    Arc::clone(&pause),
                    l2_reresolve_pass(&rt, &token, time(3), &mut state, &mut report)
                ),
                async {
                    pause.entered.wait().await;
                    let mut row = stored(&other, &other_token, owner).await;
                    let props = row.properties.as_mut().expect("properties");
                    props[key]
                        .as_array_mut()
                        .expect("array")
                        .push(if impl_path {
                            serde_json::to_value(&concurrent_impl).expect("impl")
                        } else {
                            serde_json::to_value(reference("concurrent", "call")).expect("ref")
                        });
                    props["concurrent_property"] = json!([4, 3]);
                    other
                        .entities(&other_token)
                        .expect("entities")
                        .upsert_entity(row)
                        .await
                        .expect("concurrent append");
                    pause.release.wait().await;
                }
            );
            result.expect("actual production replay");
            let row = stored(&rt, &token, owner).await;
            let props = row.properties.expect("properties");
            let expected = if impl_path {
                json!([concurrent_impl, remaining_impl])
            } else {
                json!([
                    reference("concurrent", "call"),
                    reference("missing", "call")
                ])
            };
            assert_eq!(
                props[key], expected,
                "fresh rebase keeps concurrent entries first and remaining originals in order"
            );
            assert_eq!(props["concurrent_property"], json!([4, 3]));
            assert_eq!(props["unrelated"], json!([2, 1]));
            assert_eq!(
                report.l2.expect("L2").symbol_dependencies_unresolved,
                1,
                "observed original unresolved semantics"
            );
        }
    }
}

#[tokio::test]
async fn l2_unsafe_repeated_candidates_owner_refusal_and_soft_delete_history() {
    let dir = TempDir::new().expect("directory");
    let path = dir.path().join("screen.db");
    let (rt, token) = runtime(&path, true);
    let id = Uuid::from_u128(14);
    seed(&rt, &token, id, "concept", "function", "owner", json!({})).await;
    let bad = reference(&fake_secret(), "call");
    let safe = reference("safe", "call");
    let items = [
        pending(bad.clone(), "first.rs"),
        pending(safe.clone(), "safe.rs"),
        pending(bad, "second.rs"),
    ];
    let (report, _) = owner_batch(&rt, &token, id, "l2_unresolved_references", &items).await;
    assert_eq!(report.blocked_count, 2);
    assert_eq!(
        report
            .blocked
            .iter()
            .map(|item| item.file.as_str())
            .collect::<Vec<_>>(),
        ["first.rs", "second.rs"]
    );
    assert_eq!(report.fts_indexed, 1);
    let row = stored(&rt, &token, id).await;
    assert_eq!(
        read_l2_unresolved(row.properties.as_ref().expect("properties")),
        [safe]
    );
    let mut row = row;
    row.description = Some(fake_secret());
    rt.entities(&token)
        .expect("entities")
        .upsert_entity(row)
        .await
        .expect("unsafe owner seeded");
    let items = [
        pending(reference("safe", "call"), "existing.rs"),
        pending(reference("other", "call"), "new1.rs"),
        pending(reference("other", "call"), "new2.rs"),
    ];
    let (report, _) = owner_batch(&rt, &token, id, "l2_unresolved_references", &items).await;
    assert_eq!(report.blocked_count, 2);
    assert_eq!(report.fts_indexed, 0);
    assert_eq!(
        report
            .blocked
            .iter()
            .map(|item| item.file.as_str())
            .collect::<Vec<_>>(),
        ["new1.rs", "new2.rs"]
    );
    let mut row = stored(&rt, &token, id).await;
    row.description = None;
    row.deleted_at = Some(time(2).timestamp_micros());
    rt.entities(&token)
        .expect("entities")
        .upsert_entity(row)
        .await
        .expect("tombstone");
    let items = [pending(reference("restored", "call"), "restore.rs")];
    let (report, _) = owner_batch(&rt, &token, id, "l2_unresolved_references", &items).await;
    assert_eq!(report.fts_indexed, 1);
    let row = stored(&rt, &token, id).await;
    assert!(row.deleted_at.is_none());
    assert_eq!(row.created_at, time(1).timestamp_micros());
}
