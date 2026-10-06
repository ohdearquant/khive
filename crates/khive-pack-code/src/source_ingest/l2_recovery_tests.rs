use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tempfile::TempDir;

#[derive(Default)]
struct Work {
    parsed: Vec<PathBuf>,
    project_commits: usize,
    fts: u64,
}
tokio::task_local! {
    static WORK: Arc<Mutex<Work>>;
    static PAUSE: Arc<Pause>;
}
pub(super) fn observe_parse(file: &Path) {
    let _ = WORK.try_with(|work| work.lock().expect("work").parsed.push(file.to_path_buf()));
}
pub(super) fn observe_fts_write() {
    let _ = WORK.try_with(|work| work.lock().expect("work").fts += 1);
}

enum Point {
    AfterEntityRead(Uuid),
    Edge(Uuid),
    BeforeCompletion,
    BeforeRead(PathBuf),
}
struct Pause {
    point: Point,
    entered: tokio::sync::Barrier,
    release: tokio::sync::Barrier,
    used: AtomicBool,
}
impl Pause {
    fn new(point: Point) -> Arc<Self> {
        Arc::new(Self {
            point,
            entered: tokio::sync::Barrier::new(2),
            release: tokio::sync::Barrier::new(2),
            used: AtomicBool::new(false),
        })
    }
    async fn wait(&self) {
        if !self.used.swap(true, Ordering::AcqRel) {
            self.entered.wait().await;
            self.release.wait().await;
        }
    }
}
pub(super) async fn after_entity_read(id: Uuid) {
    let Ok(pause) = PAUSE.try_with(Arc::clone) else {
        return;
    };
    if matches!(pause.point, Point::AfterEntityRead(expected) if expected == id) {
        pause.wait().await;
    }
}
pub(super) fn after_entity_commit(entity: &Entity) {
    if entity.kind == "project" {
        let _ = WORK.try_with(|work| work.lock().expect("work").project_commits += 1);
    }
}
pub(super) async fn after_edge_commit(id: Uuid) {
    let Ok(pause) = PAUSE.try_with(Arc::clone) else {
        return;
    };
    if matches!(pause.point, Point::Edge(expected) if expected == id) {
        pause.wait().await;
    }
}
pub(super) async fn before_completion() {
    let Ok(pause) = PAUSE.try_with(Arc::clone) else {
        return;
    };
    if matches!(pause.point, Point::BeforeCompletion) {
        pause.wait().await;
    }
}
pub(super) async fn before_source_read(file: &Path) {
    let Ok(pause) = PAUSE.try_with(Arc::clone) else {
        return;
    };
    if matches!(&pause.point, Point::BeforeRead(expected) if expected == file) {
        pause.wait().await;
    }
}

fn runtime(path: &Path, wal: bool) -> (KhiveRuntime, NamespaceToken) {
    let mode = if wal {
        tests::TestJournalMode::Wal
    } else {
        tests::TestJournalMode::Delete
    };
    tests::runtime_on_with_mode(path, mode)
}
fn time(seconds: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(seconds, 0).expect("fixture time")
}
fn source(root: &Path, name: &str, content: &str) {
    fs::create_dir_all(root).expect("source directory");
    fs::write(root.join(name), content).expect("source file");
}
fn manifest(root: &Path, name: &str) {
    source(
        root,
        "Cargo.toml",
        &format!("[package]\nname=\"{name}\"\nversion=\"0.1.0\"\n"),
    );
}
fn two_files(root: &Path) {
    manifest(root, "fixture");
    source(root, "a.rs", "fn aa() { ah(); }\nfn ah() {}\n");
    source(root, "b.rs", "fn ba() { bh(); }\nfn bh() {}\n");
}
fn symbol(module: &str, name: &str) -> Uuid {
    symbol_uuid("fixture", "rust", module, name, "function")
}
fn natural(module: &str, from: &str, to: &str) -> Uuid {
    edge_uuid(
        EdgeRelation::DependsOn,
        symbol(module, from),
        symbol(module, to),
    )
}
async fn stored(rt: &KhiveRuntime, token: &NamespaceToken, id: Uuid) -> Entity {
    get_entity_opt(rt, token, id)
        .await
        .expect("read")
        .expect("entity")
}
async fn project(rt: &KhiveRuntime, token: &NamespaceToken, name: &str) -> Entity {
    stored(rt, token, project_uuid(name)).await
}
fn entry(row: &Entity) -> &Value {
    &row.properties.as_ref().expect("properties")["l2_sweep_runs"]["rust"]
}
fn is_completed(row: &Entity) -> bool {
    entry(row)["completed"].is_object() && entry(row)["attempted"] == entry(row)["completed"]
}
async fn edge(rt: &KhiveRuntime, token: &NamespaceToken, id: Uuid) -> Edge {
    rt.graph(token)
        .expect("graph")
        .get_edge_including_deleted(LinkId::from(id))
        .await
        .expect("edge read")
        .expect("edge")
}
fn stamp(edge: &Edge) -> &str {
    edge.metadata.as_ref().expect("metadata")["last_seen_at"]
        .as_str()
        .expect("stamp")
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
async fn ingest(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    root: &Path,
    seconds: i64,
    l1: bool,
    l1_5: bool,
    l2: bool,
) -> (Result<CodeSourceIngestReport, CodeSourceIngestError>, Work) {
    let work = Arc::new(Mutex::new(Work::default()));
    let result = WORK
        .scope(
            Arc::clone(&work),
            run_code_ingest(
                rt,
                token,
                CodeSourceIngestOptions {
                    path: root,
                    languages: BTreeSet::from(["rust"]),
                    enable_l1: l1,
                    enable_l1_5: l1_5,
                    enable_l2: l2,
                    sweep_time: time(seconds),
                },
            ),
        )
        .await;
    let work = Arc::try_unwrap(work)
        .ok()
        .expect("observer released")
        .into_inner()
        .expect("work");
    (result, work)
}
async fn l2(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    root: &Path,
    seconds: i64,
) -> (CodeSourceIngestReport, Work) {
    let (result, work) = ingest(rt, token, root, seconds, false, false, true).await;
    (result.expect("L2 ingest"), work)
}
async fn await_pause<F: std::future::Future>(future: &mut std::pin::Pin<Box<F>>, pause: &Pause) {
    tokio::select! {
        _ = pause.entered.wait() => {},
        _ = future.as_mut() => panic!("ingest completed before the real storage boundary"),
        _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => panic!("storage boundary not reached"),
    }
}

async fn resume_paused<F: std::future::Future>(
    future: &mut std::pin::Pin<Box<F>>,
    pause: &Pause,
) -> F::Output {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let (_, result) = tokio::join!(pause.release.wait(), future.as_mut());
        result
    })
    .await
    .expect("paused operation did not resume and complete")
}

#[tokio::test]
async fn paused_operation_resumes_in_both_entry_arrival_orders() {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    for operation_first in [true, false] {
        let pause = Pause::new(Point::BeforeCompletion);
        let value = std::sync::atomic::AtomicUsize::new(1);
        let completed = AtomicBool::new(false);
        let mut operation = Box::pin(async {
            pause.wait().await;
            completed.store(true, Ordering::Release);
            value.load(Ordering::Acquire)
        });
        let mut entered = Box::pin(pause.entered.wait());
        let mut cx = Context::from_waker(Waker::noop());

        if !operation_first {
            assert!(entered.as_mut().poll(&mut cx).is_pending());
        }
        assert!(operation.as_mut().poll(&mut cx).is_pending());
        let Poll::Ready(entry) = entered.as_mut().poll(&mut cx) else {
            panic!("both entry-barrier participants have arrived");
        };
        assert_eq!(entry.is_leader(), operation_first);
        assert!(!completed.load(Ordering::Acquire));

        value.store(2, Ordering::Release);
        assert_eq!(resume_paused(&mut operation, &pause).await, 2);
        assert!(completed.load(Ordering::Acquire));
    }
}

#[tokio::test]
async fn l2_recovery_whole_manifest_reuses_and_accounts_completion() {
    for wal in [true, false] {
        let dir = TempDir::new().expect("directory");
        let root = dir.path().join("source");
        two_files(&root);
        let (rt, token) = runtime(&dir.path().join("map.db"), wal);
        let (first, work) = l2(&rt, &token, &root, 10).await;
        assert_eq!(work.parsed.len(), 2);
        assert_eq!(work.project_commits, 2);
        assert_eq!(first.projects_created, 1);
        assert_eq!(
            first.projects_updated, 1,
            "completion is an additional mutation"
        );
        assert_eq!(first.fts_indexed, work.fts);
        let before = project(&rt, &token, "fixture").await;
        assert!(is_completed(&before));
        let first_id = entry(&before)["attempted"]["run_id"].clone();
        // Repeating and reversing caller times never repeats an invocation ID.
        for seconds in [10, 5] {
            let previous = project(&rt, &token, "fixture").await;
            let (again, work) = l2(&rt, &token, &root, seconds).await;
            let current = project(&rt, &token, "fixture").await;
            assert!(
                work.parsed.is_empty(),
                "completed owners retain real fast-path reuse"
            );
            assert_eq!(work.project_commits, 2);
            assert_eq!(again.projects_updated, 2);
            assert_eq!(again.fts_indexed, work.fts);
            assert_eq!(current.version, previous.version + 2);
            assert!(is_completed(&current));
            assert_ne!(entry(&current)["attempted"]["run_id"], first_id);
            assert_ne!(
                entry(&current)["attempted"]["run_id"],
                entry(&previous)["attempted"]["run_id"]
            );
            assert_eq!(
                stamp(&edge(&rt, &token, natural("a", "aa", "ah")).await),
                time(seconds).to_rfc3339()
            );
        }
    }
}

#[tokio::test]
async fn l2_recovery_mixed_final_refresh_reobserves_only_scanned_references() {
    for wal in [true, false] {
        for containment_fault in [false, true] {
            let dir = TempDir::new().expect("directory");
            let root = dir.path().join("source");
            two_files(&root);
            let db = dir.path().join("map.db");
            let (rt, token) = runtime(&db, wal);
            l2(&rt, &token, &root, 10).await;
            let ids = [natural("a", "aa", "ah"), natural("b", "ba", "bh")];
            let removed_source = symbol("a", "removed");
            for module in ["a", "b"] {
                let row = stored(&rt, &token, module_uuid("fixture", "rust", module)).await;
                let retained =
                    read_declaration_ids(&row.properties.expect("properties")["declaration_ids"])
                        .expect("current retained declarations");
                assert!(
                    !retained.contains(&removed_source),
                    "final-refresh history must have an unscanned source"
                );
            }
            // Seed removed-source, manual, foreign-owner and unvisited-source
            // history. Derived history lies outside retained source coverage,
            // so the completed predecessor genuinely reaches final refresh.
            let histories = [
                (removed_source, symbol("b", "bh"), true),
                (symbol("a", "ah"), symbol("a", "aa"), false),
                (
                    symbol_uuid("foreign", "rust", "a", "foreign", "function"),
                    symbol("a", "ah"),
                    true,
                ),
                (symbol("unvisited", "old"), symbol("b", "bh"), true),
            ];
            let mut historical = Vec::new();
            for (from, to, derived) in histories {
                if get_entity_opt(&rt, &token, from)
                    .await
                    .expect("lookup")
                    .is_none()
                {
                    let mut entity =
                        Entity::new(token.namespace().as_str(), "concept", "historical");
                    entity.id = from;
                    rt.entities(&token)
                        .expect("entities")
                        .upsert_entity(entity)
                        .await
                        .expect("seed endpoint");
                }
                let id = edge_uuid(EdgeRelation::DependsOn, from, to);
                let mut history = edge(&rt, &token, ids[0]).await;
                history.id = LinkId::from(id);
                history.source_id = from;
                history.target_id = to;
                history.metadata = Some(
                    json!({"l2_derived":derived,"language":"rust","last_seen_at":time(1).to_rfc3339()}),
                );
                rt.graph(&token)
                    .expect("graph")
                    .upsert_edge(history.clone())
                    .await
                    .expect("seed historical");
                historical.push((id, serde_json::to_value(history).expect("serialize")));
            }
            let trigger = if containment_fault {
                let contains = edge_uuid(
                    EdgeRelation::Contains,
                    module_uuid("fixture", "rust", "a"),
                    symbol("a", "aa"),
                );
                format!("CREATE TRIGGER recovery_fault BEFORE UPDATE ON graph_edges WHEN NEW.id='{contains}' BEGIN SELECT RAISE(ABORT,'partial_refresh'); END;")
            } else {
                format!("CREATE TRIGGER recovery_fault BEFORE UPDATE ON graph_edges WHEN NEW.id IN ('{}','{}') AND EXISTS (SELECT 1 FROM graph_edges WHERE id IN ('{}','{}') AND json_extract(metadata,'$.last_seen_at')='{}') BEGIN SELECT RAISE(ABORT,'partial_refresh'); END;", ids[0], ids[1], ids[0], ids[1], time(20).to_rfc3339())
            };
            script(&rt, trigger).await;
            let (failed, work) = ingest(&rt, &token, &root, 20, false, false, true).await;
            assert!(failed
                .expect_err("real edge fault")
                .to_string()
                .contains("partial_refresh"));
            assert!(
                work.parsed.is_empty(),
                "this failed invocation initially had a completed predecessor"
            );
            let actual = BTreeSet::from([
                stamp(&edge(&rt, &token, ids[0]).await).to_string(),
                stamp(&edge(&rt, &token, ids[1]).await).to_string(),
            ]);
            assert_eq!(
                actual,
                if containment_fault {
                    BTreeSet::from([time(20).to_rfc3339()])
                } else {
                    BTreeSet::from([time(10).to_rfc3339(), time(20).to_rfc3339()])
                },
                "real committed natural refresh prefix precedes the actual SQLite fault"
            );
            assert!(
                !is_completed(&project(&rt, &token, "fixture").await),
                "completion follows both refresh phases"
            );
            script(&rt, "DROP TRIGGER recovery_fault;".into()).await;
            drop(rt);
            let (retry, token) = runtime(&db, wal);
            let (_, work) = l2(&retry, &token, &root, 30).await;
            assert_eq!(
                work.parsed.len(),
                2,
                "incomplete owner must execute both real parsers"
            );
            for id in ids {
                assert_eq!(
                    stamp(&edge(&retry, &token, id).await),
                    time(30).to_rfc3339()
                );
            }
            for (id, before) in historical {
                assert_eq!(
                    serde_json::to_value(edge(&retry, &token, id).await).expect("serialize"),
                    before,
                    "history is not promoted"
                );
            }
            assert!(l2(&retry, &token, &root, 40).await.1.parsed.is_empty());
        }
    }
}

#[tokio::test]
async fn l2_recovery_attempt_precedes_changed_file_and_reresolve_failures() {
    for wal in [true, false] {
        for reresolve in [false, true] {
            let dir = TempDir::new().expect("directory");
            let root = dir.path().join("source");
            two_files(&root);
            let db = dir.path().join("map.db");
            let (rt, token) = runtime(&db, wal);
            l2(&rt, &token, &root, 10).await;
            let old_id =
                entry(&project(&rt, &token, "fixture").await)["attempted"]["run_id"].clone();
            source(
                &root,
                "a.rs",
                "fn aa() { crate::b::new_target(); }\nfn ah() {}\n",
            );
            source(
                &root,
                "b.rs",
                "fn ba() { bh(); }\nfn bh() {}\nfn new_target() {}\n",
            );
            let trigger = if reresolve {
                let label = root
                    .join("a.rs")
                    .canonicalize()
                    .unwrap()
                    .display()
                    .to_string();
                let path = format!(
                    "$.l2_file_pending.{}.references",
                    json!(file_pending::file_key(&label))
                )
                .replace('\'', "''");
                format!(
                    "CREATE TRIGGER recovery_fault BEFORE UPDATE ON entities WHEN NEW.id='{}' AND json_array_length(json_extract(OLD.properties,'{path}'))>0 AND json_array_length(json_extract(NEW.properties,'{path}'))=0 BEGIN SELECT RAISE(ABORT,'reresolve_prefix'); END;",
                    module_uuid("fixture", "rust", "a")
                )
            } else {
                format!(
                    "CREATE TRIGGER recovery_fault BEFORE UPDATE ON entities WHEN NEW.id='{}' BEGIN SELECT RAISE(ABORT,'changed_prefix'); END;",
                    module_uuid("fixture", "rust", "b")
                )
            };
            script(&rt, trigger).await;
            // Repeated time prevents clock equality from detecting the failed run.
            let (failed, work) = ingest(&rt, &token, &root, 10, false, false, true).await;
            assert!(failed
                .expect_err("actual storage fault")
                .to_string()
                .contains(if reresolve {
                    "reresolve_prefix"
                } else {
                    "changed_prefix"
                }));
            assert!(!work.parsed.is_empty(), "changed a.rs was really parsed");
            let current = project(&rt, &token, "fixture").await;
            assert_ne!(
                entry(&current)["attempted"]["run_id"],
                old_id,
                "attempt commits before destructive work"
            );
            assert!(
                !is_completed(&current),
                "pending flush and re-resolution must precede completion"
            );
            let a = stored(&rt, &token, module_uuid("fixture", "rust", "a")).await;
            assert!(
                a.properties
                    .expect("module props")
                    .get("declaration_ids")
                    .is_some(),
                "first file persistence committed"
            );
            if reresolve {
                let resolved = edge_uuid(
                    EdgeRelation::DependsOn,
                    symbol("a", "aa"),
                    symbol("b", "new_target"),
                );
                assert_eq!(
                    stamp(&edge(&rt, &token, resolved).await),
                    time(10).to_rfc3339(),
                    "re-resolution edge committed before its pending-row failure"
                );
            }
            script(&rt, "DROP TRIGGER recovery_fault;".into()).await;
            drop(rt);
            let (retry, token) = runtime(&db, wal);
            assert_eq!(l2(&retry, &token, &root, 5).await.1.parsed.len(), 2);
            assert!(is_completed(&project(&retry, &token, "fixture").await));
        }
    }
}

#[tokio::test]
async fn l2_recovery_cancel_after_committed_edge_reopens_and_reparses() {
    for wal in [true, false] {
        let dir = TempDir::new().expect("directory");
        let root = dir.path().join("source");
        two_files(&root);
        let db = dir.path().join("map.db");
        let (rt, token) = runtime(&db, wal);
        l2(&rt, &token, &root, 10).await;
        let id = natural("a", "aa", "ah");
        let pause = Pause::new(Point::Edge(id));
        let mut future = Box::pin(PAUSE.scope(
            Arc::clone(&pause),
            ingest(&rt, &token, &root, 20, false, false, true),
        ));
        await_pause(&mut future, &pause).await;
        assert_eq!(stamp(&edge(&rt, &token, id).await), time(20).to_rfc3339());
        drop(future); // Cancel a future after its real committed graph mutation.
        drop(rt);
        let (retry, token) = runtime(&db, wal);
        assert!(!is_completed(&project(&retry, &token, "fixture").await));
        assert_eq!(l2(&retry, &token, &root, 30).await.1.parsed.len(), 2);
    }
}

#[tokio::test]
async fn l2_recovery_completion_row_and_fts_faults_have_distinct_durability() {
    for wal in [true, false] {
        for fts in [false, true] {
            let dir = TempDir::new().expect("directory");
            let root = dir.path().join("source");
            two_files(&root);
            let db = dir.path().join("map.db");
            let (rt, token) = runtime(&db, wal);
            l2(&rt, &token, &root, 10).await;
            let id = project_uuid("fixture");
            let trigger = if fts {
                format!(
                    "CREATE TRIGGER recovery_fault BEFORE INSERT ON fts_entities_rowids WHEN NEW.subject_id='{id}' AND EXISTS (SELECT 1 FROM entities WHERE id=NEW.subject_id AND json_extract(properties,'$.l2_sweep_runs.rust.attempted.run_id')=json_extract(properties,'$.l2_sweep_runs.rust.completed.run_id') AND json_extract(properties,'$.l2_sweep_runs.rust.completed.sweep_time')='{}') BEGIN SELECT RAISE(ABORT,'completion_fts'); END;",
                    time(20).to_rfc3339()
                )
            } else {
                format!(
                    "CREATE TRIGGER recovery_fault BEFORE UPDATE ON entities WHEN NEW.id='{id}' AND json_extract(NEW.properties,'$.l2_sweep_runs.rust.attempted.run_id')=json_extract(NEW.properties,'$.l2_sweep_runs.rust.completed.run_id') BEGIN SELECT RAISE(ABORT,'completion_row'); END;"
                )
            };
            script(&rt, trigger).await;
            let (failed, _) = ingest(&rt, &token, &root, 20, false, false, true).await;
            assert!(failed
                .expect_err("actual completion fault")
                .to_string()
                .contains(if fts {
                    "completion_fts"
                } else {
                    "completion_row"
                }));
            assert_eq!(
                is_completed(&project(&rt, &token, "fixture").await),
                fts,
                "post-commit FTS failure does not roll back graph completion"
            );
            script(&rt, "DROP TRIGGER recovery_fault;".into()).await;
            drop(rt);
            let (retry, token) = runtime(&db, wal);
            assert_eq!(
                l2(&retry, &token, &root, 30).await.1.parsed.len(),
                if fts { 0 } else { 2 }
            );
        }
    }
}

#[tokio::test]
async fn l2_recovery_competing_attempt_cannot_be_copied_into_completion() {
    for wal in [true, false] {
        let dir = TempDir::new().expect("directory");
        let root = dir.path().join("source");
        two_files(&root);
        let db = dir.path().join("map.db");
        let (rt, token) = runtime(&db, wal);
        l2(&rt, &token, &root, 10).await;
        let predecessor = entry(&project(&rt, &token, "fixture").await)["completed"].clone();
        let pause = Pause::new(Point::BeforeCompletion);
        let mut future = Box::pin(PAUSE.scope(
            Arc::clone(&pause),
            ingest(&rt, &token, &root, 10, false, false, true),
        ));
        await_pause(&mut future, &pause).await;
        let mut competing = project(&rt, &token, "fixture").await;
        let competitor = Uuid::new_v4().to_string();
        competing.properties.as_mut().expect("props")["l2_sweep_runs"]["rust"]["attempted"]
            ["run_id"] = json!(competitor);
        rt.entities(&token)
            .expect("entities")
            .upsert_entity(competing)
            .await
            .expect("competing attempt commits");
        resume_paused(&mut future, &pause)
            .await
            .0
            .expect("superseded invocation returns normally");
        drop(future);
        let current = project(&rt, &token, "fixture").await;
        assert_eq!(entry(&current)["attempted"]["run_id"], competitor);
        assert_eq!(
            entry(&current)["completed"],
            predecessor,
            "superseded completion preserves the predecessor rather than copying any attempt"
        );
        assert!(
            !is_completed(&current),
            "equal times do not authorize another run's completion"
        );
        drop(rt);
        let (retry, token) = runtime(&db, wal);
        assert_eq!(l2(&retry, &token, &root, 10).await.1.parsed.len(), 2);
    }
}

#[tokio::test]
async fn l2_recovery_fallback_subtree_cannot_authorize_whole_owner() {
    for wal in [true, false] {
        let dir = TempDir::new().expect("directory");
        let root = dir.path().join("repo");
        manifest(&root, "src");
        source(
            &root.join("src"),
            "alpha.rs",
            "fn aa() { ah(); }\nfn ah() {}\n",
        );
        source(&root, "other.rs", "fn ba() { bh(); }\nfn bh() {}\n");
        let (rt, token) = runtime(&dir.path().join("map.db"), wal);
        assert_eq!(l2(&rt, &token, &root, 10).await.1.parsed.len(), 2);
        assert!(is_completed(&project(&rt, &token, "src").await));
        assert_eq!(
            l2(&rt, &token, &root.join("src"), 20).await.1.parsed.len(),
            1
        );
        assert!(
            !is_completed(&project(&rt, &token, "src").await),
            "fallback subtree never publishes completion"
        );
        let (_, work) = l2(&rt, &token, &root, 30).await;
        assert!(
            work.parsed.iter().any(|file| file.ends_with("other.rs")),
            "outside-subtree source is really parsed"
        );
    }
}

#[tokio::test]
async fn l2_recovery_manifestless_cost_and_reverse_fallback_witness() {
    for wal in [true, false] {
        let dir = TempDir::new().expect("directory");
        let root = dir.path().join("src");
        source(&root, "x.rs", "fn aa() { ah(); }\nfn ah() {}\n");
        let (rt, token) = runtime(&dir.path().join("map.db"), wal);
        for seconds in [10, 20] {
            assert_eq!(
                l2(&rt, &token, &root, seconds).await.1.parsed.len(),
                1,
                "manifestless owners pay the documented re-observation cost"
            );
            assert!(!is_completed(&project(&rt, &token, "src").await));
        }
        let sub = root.join("sub");
        manifest(&sub, "src");
        source(&sub, "y.rs", "fn ba() { bh(); }\nfn bh() {}\n");
        l2(&rt, &token, &sub, 30).await;
        assert!(is_completed(&project(&rt, &token, "src").await));
        let (_, work) = l2(&rt, &token, &root, 40).await;
        assert_eq!(
            work.parsed.len(),
            2,
            "one fallback file disqualifies earlier manifest-governed files too"
        );
        assert!(work.parsed.iter().any(|file| file.ends_with("x.rs")));
        assert!(!is_completed(&project(&rt, &token, "src").await));
    }
}

#[tokio::test]
async fn l2_recovery_malformed_markers_and_no_l2_clock_changes_force_parse() {
    for wal in [true, false] {
        let dir = TempDir::new().expect("directory");
        let root = dir.path().join("source");
        two_files(&root);
        let (rt, token) = runtime(&dir.path().join("map.db"), wal);
        l2(&rt, &token, &root, 10).await;
        let valid = entry(&project(&rt, &token, "fixture").await).clone();
        let mut malformed = vec![Value::Null];
        for (pointer, value) in [
            ("/version", json!(2)),
            ("/version", json!(1.0)),
            (
                "/attempted/run_id",
                json!("B024EB06-BD98-44C5-8542-74B49DF6E528"),
            ),
            ("/attempted/run_id", json!(Uuid::nil().to_string())),
            ("/attempted/sweep_time", json!(10)),
            ("/completed", Value::Null),
        ] {
            let mut entry = valid.clone();
            *entry.pointer_mut(pointer).expect("field") = value;
            malformed.push(entry);
        }
        let mut unknown_field = valid.clone();
        unknown_field["extra"] = json!(true);
        malformed.push(unknown_field);
        let mut missing = valid.clone();
        missing.as_object_mut().expect("entry").remove("completed");
        malformed.push(missing);
        for bad in malformed {
            let mut owner = project(&rt, &token, "fixture").await;
            owner.properties.as_mut().expect("props")["l2_sweep_runs"] =
                json!({"rust":bad,"python":{"retained":true}});
            owner.properties.as_mut().expect("props")["unrelated"] = json!([3, 1]);
            rt.entities(&token)
                .expect("entities")
                .upsert_entity(owner)
                .await
                .expect("marker fixture");
            assert_eq!(l2(&rt, &token, &root, 10).await.1.parsed.len(), 2);
            let current = project(&rt, &token, "fixture").await;
            assert_eq!(
                current.properties.as_ref().expect("props")["l2_sweep_runs"]["python"],
                json!({"retained":true})
            );
            assert_eq!(
                current.properties.as_ref().expect("props")["unrelated"],
                json!([3, 1])
            );
        }
        let before = entry(&project(&rt, &token, "fixture").await).clone();
        let (result, work) = ingest(&rt, &token, &root, 20, true, true, false).await;
        result.expect("no L2 invocation");
        assert!(work.parsed.is_empty());
        assert_eq!(
            entry(&project(&rt, &token, "fixture").await),
            &before,
            "no-L2 writes no attempt or completion"
        );
        let (result, work) = ingest(&rt, &token, &root, 30, true, true, true).await;
        result.expect("all selected tiers");
        assert_eq!(
            work.parsed.len(),
            2,
            "authority captured before earlier L1/L1.5 advances the visible clock"
        );
    }
}

#[test]
fn l2_recovery_marker_validation_is_strict_and_times_are_opaque() {
    let id = Uuid::new_v4().to_string();
    let marker = json!({"run_id":id,"sweep_time":"opaque legacy text"});
    let valid = json!({"version":1,"attempted":marker,"completed":marker});
    let props = json!({"sweep_clock":{"rust":"opaque legacy text"},"l2_sweep_runs":{"rust":valid}});
    assert_eq!(
        completed_l2_observation(&props, "rust"),
        Some(L2Observation {
            run_id: Uuid::parse_str(&id).unwrap(),
            sweep_time: "opaque legacy text".into(),
        })
    );
    for pointer in [
        "/l2_sweep_runs/rust/attempted",
        "/l2_sweep_runs/rust/completed",
    ] {
        let mut bad = props.clone();
        bad.pointer_mut(pointer).expect("marker")["extra"] = json!(1);
        assert_eq!(completed_l2_observation(&bad, "rust"), None);
    }
    for value in [json!([]), json!("malformed"), Value::Null] {
        let mut bad = props.clone();
        bad["l2_sweep_runs"] = value;
        assert_eq!(completed_l2_observation(&bad, "rust"), None);
    }
}

#[tokio::test]
async fn l2_recovery_project_and_language_state_are_independent() {
    for wal in [true, false] {
        let dir = TempDir::new().expect("directory");
        let root = dir.path().join("source");
        let first = root.join("first");
        let second = root.join("second");
        manifest(&first, "first");
        manifest(&second, "second");
        source(&first, "lib.rs", "// successful empty file\n");
        source(&second, "lib.rs", "// successful empty file\n");
        let (rt, token) = runtime(&dir.path().join("map.db"), wal);
        let (result, _) = ingest(&rt, &token, &root, 10, true, false, false).await;
        result.expect("L1 only");
        for name in ["first", "second"] {
            let mut row = project(&rt, &token, name).await;
            assert!(
                row.properties
                    .as_ref()
                    .expect("props")
                    .get("l2_sweep_runs")
                    .is_none(),
                "a fresh no-L2 invocation writes no markers"
            );
            let props = row.properties.as_mut().expect("props");
            props["sweep_clock"]["python"] = json!("independent opaque clock");
            props["l2_sweep_runs"] = json!({"python":{"independent":[2,1]}});
            props["unrelated"] = json!(name);
            rt.entities(&token)
                .expect("entities")
                .upsert_entity(row)
                .await
                .expect("other language seed");
        }
        let (_, work) = l2(&rt, &token, &root, 20).await;
        assert_eq!(work.parsed.len(), 2);
        let other_before = project(&rt, &token, "second").await;
        l2(&rt, &token, &first, 30).await;
        let other_after = project(&rt, &token, "second").await;
        assert_eq!(
            serde_json::to_value(other_before).expect("serialize"),
            serde_json::to_value(other_after).expect("serialize")
        );
        for name in ["first", "second"] {
            let row = project(&rt, &token, name).await;
            let props = row.properties.as_ref().expect("props");
            assert!(is_completed(&row));
            assert_eq!(
                props["l2_sweep_runs"]["python"],
                json!({"independent":[2,1]})
            );
            assert_eq!(props["sweep_clock"]["python"], "independent opaque clock");
            assert_eq!(props["unrelated"], name);
        }
    }
}

#[tokio::test]
async fn l2_recovery_empty_file_and_parse_skip_do_not_promote_history() {
    for wal in [true, false] {
        let dir = TempDir::new().expect("directory");
        let root = dir.path().join("source");
        two_files(&root);
        let (rt, token) = runtime(&dir.path().join("map.db"), wal);
        l2(&rt, &token, &root, 10).await;
        source(&root, "a.rs", "fn broken( {\n");
        source(&root, "b.rs", "// successful empty declaration set\n");
        let (report, _) = l2(&rt, &token, &root, 20).await;
        assert_eq!(report.l2.expect("L2 report").symbol_parse_failures, 1);
        assert!(
            is_completed(&project(&rt, &token, "fixture").await),
            "graph completion covers observed successes and skips"
        );
        assert_eq!(
            stamp(&edge(&rt, &token, natural("a", "aa", "ah")).await),
            time(10).to_rfc3339()
        );
        let empty = stored(&rt, &token, module_uuid("fixture", "rust", "b")).await;
        assert_eq!(
            empty.properties.expect("props")["declaration_ids"],
            json!([])
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn l2_recovery_restored_read_skip_reobserves_actual_references() {
    for wal in [true, false] {
        let dir = TempDir::new().expect("directory");
        let root = dir.path().join("source");
        two_files(&root);
        let (rt, token) = runtime(&dir.path().join("map.db"), wal);
        l2(&rt, &token, &root, 10).await;
        let path = root.join("a.rs").canonicalize().expect("canonical source");
        let original = fs::read_to_string(&path).expect("source");
        let outside = dir.path().join("outside.rs");
        fs::write(&outside, &original).expect("outside source");
        let pause = Pause::new(Point::BeforeRead(path.clone()));
        let mut future = Box::pin(PAUSE.scope(
            Arc::clone(&pause),
            ingest(&rt, &token, &root, 20, false, false, true),
        ));
        await_pause(&mut future, &pause).await;
        fs::remove_file(&path).expect("replace source");
        std::os::unix::fs::symlink(&outside, &path).expect("outside symlink");
        let (result, _) = resume_paused(&mut future, &pause).await;
        let report = result.expect("read skip does not abort sweep");
        assert!(
            report.source_files_refused == 1
                && report
                    .warnings
                    .iter()
                    .any(|warning| warning.starts_with("L2 refused source")
                        && warning.contains("a.rs")),
            "actual source read refusal is disclosed"
        );
        assert!(is_completed(&project(&rt, &token, "fixture").await));
        fs::remove_file(&path).expect("remove symlink");
        fs::write(&path, original).expect("restore unchanged disk");
        let (_, work) = l2(&rt, &token, &root, 30).await;
        assert_eq!(work.parsed, [path]);
        assert_eq!(
            stamp(&edge(&rt, &token, natural("a", "aa", "ah")).await),
            time(30).to_rfc3339(),
            "restored source is really re-observed before its natural edge is current"
        );
    }
}

#[path = "l2_removed_pending_tests.rs"]
mod pending_removed_tests;

#[path = "l2_file_pending_gate_tests.rs"]
mod file_pending_gate_tests;

#[path = "l2_owner_refresh_tests.rs"]
mod shared_owner_tests;

#[path = "l2_run_identity_tests.rs"]
mod run_identity_tests;

#[path = "l2_accepted_edge_tests.rs"]
mod accepted_edge_tests;
