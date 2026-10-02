use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use khive_pack_blob::BlobPack;
use khive_pack_kg::KgPack;
use khive_pack_tool::ToolPack;
use khive_runtime::engine_config::ExecSectionConfig;
use khive_runtime::{KhiveRuntime, RuntimeConfig, VerbRegistry, VerbRegistryBuilder};
use khive_storage::{BlobStore, ContentRef, StorageCapability, StorageError, StorageResult};
use serde_json::{json, Value};

use crate::membership::{MembershipWork, SuppliedRefs, WorkObserver};
use crate::tree::DeclaredCoverage;

#[derive(Clone, Debug)]
struct ProbeFault {
    reference: String,
    observation: usize,
    error: bool,
}

#[derive(Debug)]
struct ProbeStore {
    inner: Arc<dyn BlobStore>,
    probes: Mutex<Vec<String>>,
    puts: AtomicUsize,
    fault: Mutex<Option<ProbeFault>>,
}

#[async_trait::async_trait]
impl BlobStore for ProbeStore {
    async fn put(&self, bytes: Vec<u8>) -> StorageResult<ContentRef> {
        self.puts.fetch_add(1, Ordering::Relaxed);
        self.inner.put(bytes).await
    }

    async fn get_bounded_verified(
        &self,
        reference: &ContentRef,
        max_bytes: u64,
    ) -> StorageResult<Vec<u8>> {
        self.inner.get_bounded_verified(reference, max_bytes).await
    }

    async fn exists(&self, reference: &ContentRef) -> StorageResult<bool> {
        let observation = {
            let mut probes = self.probes.lock().unwrap();
            probes.push(reference.as_str().to_string());
            probes
                .iter()
                .filter(|value| *value == reference.as_str())
                .count()
        };
        let fault = self.fault.lock().unwrap().clone();
        if let Some(fault) = fault {
            if fault.reference == reference.as_str() && fault.observation == observation {
                return if fault.error {
                    Err(StorageError::InvalidInput {
                        capability: StorageCapability::Blob,
                        operation: "exists".into(),
                        message: "second observation failed".into(),
                    })
                } else {
                    Ok(false)
                };
            }
        }
        self.inner.exists(reference).await
    }

    async fn size(&self, reference: &ContentRef) -> StorageResult<Option<u64>> {
        self.inner.size(reference).await
    }

    async fn delete(&self, reference: &ContentRef) -> StorageResult<bool> {
        self.inner.delete(reference).await
    }
}

struct Fixture {
    registry: VerbRegistry,
    store: Arc<ProbeStore>,
    root: PathBuf,
    blobs: PathBuf,
    _dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let parent = std::fs::canonicalize(dir.path()).unwrap();
        let root = parent.join("exec-root");
        let blobs = parent.join("blobs");
        let rt = KhiveRuntime::new(RuntimeConfig {
            db_path: Some(parent.join("khive.db")),
            exec: ExecSectionConfig {
                root: Some(root.to_string_lossy().into_owned()),
                read_roots: vec!["/bin".into(), "/usr/bin".into()],
                ..Default::default()
            },
            ..RuntimeConfig::no_embeddings()
        })
        .unwrap();
        let store = Arc::new(ProbeStore {
            inner: Arc::new(khive_db::stores::blob::FsBlobStore::new(blobs.clone(), 0).unwrap()),
            probes: Mutex::new(Vec::new()),
            puts: AtomicUsize::new(0),
            fault: Mutex::new(None),
        });
        let installed: Arc<dyn BlobStore> = store.clone();
        rt.install_blob_store(installed).unwrap();
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(BlobPack::new(rt.clone()));
        builder.register(ToolPack::new(rt.clone()));
        builder.register(crate::ExecPack::new(rt.clone()));
        let registry = builder.build().unwrap();
        registry.apply_schema_plans(rt.backend());
        rt.install_edge_rules(registry.all_edge_rules());
        Self {
            registry,
            store,
            root,
            blobs,
            _dir: dir,
        }
    }

    async fn call(&self, verb: &str, params: Value) -> Value {
        self.registry
            .dispatch(verb, params)
            .await
            .unwrap_or_else(|e| panic!("{verb}: {e}"))
    }

    async fn error(&self, verb: &str, params: Value) -> String {
        self.registry
            .dispatch(verb, params)
            .await
            .expect_err(verb)
            .to_string()
    }

    async fn put(&self, bytes: &[u8]) -> String {
        self.store
            .put(bytes.to_vec())
            .await
            .unwrap()
            .as_str()
            .to_string()
    }

    async fn tree(&self, entries: Vec<Value>) -> String {
        self.call("exec.tree", json!({"entries":entries})).await["tree"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn reset(&self) {
        self.store.probes.lock().unwrap().clear();
        self.store.puts.store(0, Ordering::Relaxed);
    }

    fn object_count(&self) -> usize {
        fn count(path: &std::path::Path) -> usize {
            std::fs::read_dir(path)
                .map(|entries| {
                    entries
                        .map(|e| {
                            let path = e.unwrap().path();
                            if path.is_dir() {
                                count(&path)
                            } else {
                                1
                            }
                        })
                        .sum()
                })
                .unwrap_or(0)
        }
        count(&self.blobs)
    }

    #[cfg(target_os = "macos")]
    async fn register_shell(&self) {
        self.call("tool.register", json!({"name":"sh","kind":"tool","description":"shell","source":"exec:/bin/sh","side_effect":"write","trust":"first_party"})).await;
        self.call(
            "tool.policy",
            json!({"actor":"*","tool":"sh","decision":"allow"}),
        )
        .await;
    }
}

#[tokio::test]
async fn supplied_ref_work_is_observed_on_public_tree_put() {
    let f = Fixture::new();
    let old = f.put(b"old body").await;
    let n = 1024usize;
    let m = 256usize;
    let base = f
        .tree(
            (0..n)
                .map(|i| json!({"path":format!("base/{i:04}"),"ref":old,"mode":644}))
                .collect(),
        )
        .await;
    let mut edits = Vec::new();
    for i in 0..m {
        let reference = f.put(format!("new body {i}").as_bytes()).await;
        edits.push(json!({"path":format!("new/{i:04}"),"ref":reference}));
    }
    f.reset();
    let observed = WorkObserver::new(&f.root);
    let result = f
        .call("exec.tree_put", json!({"tree":base,"edits":edits}))
        .await;
    let work = observed.work.snapshot();
    assert_eq!(result["entries"], n + m);
    assert_eq!(result["changed"].as_array().unwrap().len(), m);
    assert!(
        work.ref_equalities < (16 * (n + 2 * m)) as u64,
        "REF_EQ_BUDGET: {work:?}"
    );
    assert!(
        work.ref_hashes >= (n + m) as u64,
        "public selection did not hash its queries: {work:?}"
    );
    assert!(
        work.ref_hashes < (8 * (n + 2 * m)) as u64,
        "REF_HASH_BUDGET: {work:?}"
    );
    assert_eq!(work.ref_hash_bytes, work.ref_hashes * 64);
    assert_eq!(
        f.store.probes.lock().unwrap().len(),
        m + 1,
        "one manifest observation plus every selected entry"
    );
}

#[tokio::test]
async fn shared_refs_keep_each_observation_and_first_later_failure() {
    for error in [false, true] {
        let f = Fixture::new();
        let reference = f.put(b"shared bytes").await;
        let base = f
            .tree(vec![
                json!({"path":"a","ref":reference,"mode":755}),
                json!({"path":"b","ref":reference,"mode":644}),
            ])
            .await;
        let before = f.object_count();
        assert!(before > 0);
        f.reset();
        *f.store.fault.lock().unwrap() = Some(ProbeFault {
            reference: reference.clone(),
            observation: 2,
            error,
        });
        let refusal = f.error("exec.tree_put", json!({"tree":base,"edits":[{"path":"z","ref":reference},{"path":"c","content":"new unpublished bytes"}]})).await;
        if error {
            assert!(refusal.contains("second observation failed"), "{refusal}");
        } else {
            assert!(refusal.contains("entry \"b\""), "{refusal}");
        }
        assert_eq!(f.store.puts.load(Ordering::Relaxed), 0);
        assert_eq!(f.object_count(), before);
        assert_eq!(
            *f.store.probes.lock().unwrap(),
            vec![base, reference.clone(), reference]
        );
    }
}

#[tokio::test]
async fn tree_put_missing_refs_refuse_in_candidate_order_before_publication() {
    let f = Fixture::new();
    let old = f.put(b"old").await;
    let base = f.tree(vec![json!({"path":"a","ref":old,"mode":644})]).await;
    let before = f.object_count();
    f.reset();
    let refusal = f
        .error(
            "exec.tree_put",
            json!({"tree":base,"edits":[
                {"path":"z","ref":"1".repeat(64)},
                {"path":"good","content":"new unpublished body"},
                {"path":"b","ref":"0".repeat(64)}
            ]}),
        )
        .await;
    assert!(refusal.contains("entry \"b\""), "{refusal}");
    assert_eq!(f.object_count(), before);
    assert_eq!(f.store.puts.load(Ordering::Relaxed), 0);
    let result = f
        .call(
            "exec.tree_put",
            json!({"tree":base,"edits":[{"path":"good","content":"new unpublished body"}]}),
        )
        .await;
    assert!(result["tree"].is_string());
    assert!(
        f.object_count() > before,
        "same backing-store instrument must see publication"
    );
}

#[test]
fn coverage_matches_legacy_and_does_not_broaden_manifest_parents() {
    let families: Vec<Vec<&str>> = vec![
        vec![],
        vec!["a"],
        vec!["a/b", "a", "a/b"],
        vec!["東京/é", "東京/é", "東京"],
        vec!["ab", "a.bak", "aa/z"],
    ];
    let queries = [
        "",
        ".",
        "/",
        "a",
        "a/",
        "a/b",
        "a/b/c",
        "a.bak",
        "aa",
        "aa/z",
        "東京",
        "東京/é/x",
        "東京/é/x",
        "東京.bak/x",
    ];
    for terms in families {
        let mut paths: Vec<String> = terms.into_iter().map(str::to_string).collect();
        for _ in 0..2 {
            let index = DeclaredCoverage::new(&paths, MembershipWork::default());
            assert_eq!(index.declarations, paths);
            for query in queries {
                let oracle_work = MembershipWork::default();
                let legacy = paths.iter().any(|d| {
                    oracle_work.declaration_attempt();
                    d == query || query.starts_with(&format!("{d}/"))
                });
                assert_eq!(index.covers(query), legacy, "{paths:?} / {query:?}");
                if !paths.is_empty() {
                    assert!(oracle_work.snapshot().declaration_attempts > 0);
                }
            }
            paths.reverse();
        }
    }
    let reference = "a".repeat(64);
    assert!(
        crate::tree::parse_entries(&json!([{ "path":"a","ref":reference,"mode":644 }])).is_ok()
    );
    for entries in [
        json!([{"path":"a","ref":reference,"mode":644},{"path":"a","ref":reference,"mode":644}]),
        json!([{"path":"a","ref":reference,"mode":644},{"path":"a/b","ref":reference,"mode":644}]),
    ] {
        assert!(crate::tree::parse_entries(&entries).is_err());
    }
    let refs = vec!["a".repeat(64), "a".repeat(64), "b".repeat(64)];
    let supplied = SuppliedRefs::new(&refs, MembershipWork::default());
    assert!(supplied.contains(&refs[0]));
    assert!(!supplied.contains(&"c".repeat(64)));
}

#[test]
fn coverage_shared_prefix_build_and_long_deletion_queries_are_bounded() {
    let long = format!("{}z", "a/".repeat(2048));
    let mut paths: Vec<String> = (1..=512).map(|i| format!("{}b", "a/".repeat(i))).collect();
    paths.push(long.clone());
    paths.reverse();
    let bytes: usize = paths.iter().map(String::len).sum();
    let work = MembershipWork::default();
    let index = DeclaredCoverage::new(&paths, work.clone());
    assert!(index.covers(&long));
    assert!(!index.covers(&format!("{long}.bak")));
    let state = work.snapshot();
    assert!(state.build_bytes > 0 && state.build_lookups > 0 && state.build_copied > 0);
    assert!(
        state.build_copied <= (2 * bytes) as u64,
        "LABEL_COPY_BUDGET: {state:?}"
    );
    assert!(
        state.query_bytes <= (4 * long.len()) as u64,
        "QUERY_BYTE_BUDGET: {state:?}"
    );
    assert!(
        state.query_lookups > 0 && state.query_lookups <= state.query_bytes,
        "QUERY_LOOKUP_BUDGET: {state:?}"
    );
    assert_eq!(state.queries, 2);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn declared_work_is_observed_on_public_capture_and_deletions() {
    let f = Fixture::new();
    f.register_shell().await;
    let old = f.put(b"old body").await;
    let n = 64usize;
    let base = f
        .tree(
            (0..n)
                .map(|i| json!({"path":format!("gone-{i:03}"),"ref":old,"mode":644}))
                .chain(std::iter::once(json!({"path":"keep","ref":old,"mode":644})))
                .collect(),
        )
        .await;
    let declarations: Vec<String> = (0..1024).map(|i| format!("gone-x/{i:04}")).collect();
    let observed = WorkObserver::new(&f.root);
    let result = f.call("exec.run", json!({"tree":base,"tool":"sh","actor":"local","args":["-c","rm gone-*; printf new > added"],"declared_write_paths":declarations})).await;
    let receipt = &result["receipt"];
    assert_eq!(receipt["exit_code"], 0);
    assert_eq!(receipt["success"], false);
    assert_eq!(
        receipt["undeclared_changes"].as_array().unwrap().len(),
        n + 1
    );
    assert_eq!(receipt["tree_out"], base);
    assert_eq!(receipt["changed"], json!([]));
    let work = observed.work.snapshot();
    assert_eq!(
        work.queries,
        (n + 1) as u64,
        "unchanged keep must bypass coverage"
    );
    assert!(
        work.declaration_attempts + work.query_lookups + work.query_bytes < (32 * (n + 1)) as u64,
        "DECLARATION_WORK_BUDGET: {work:?}"
    );
    assert!(
        work.query_lookups > 0 && work.query_bytes > 0,
        "public capture did not reach radix comparisons: {work:?}"
    );
    assert!(work.build_bytes > 0 && work.build_lookups > 0);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn public_coverage_preserves_none_empty_unicode_and_old_content() {
    for declared in [
        None,
        Some(Value::Null),
        Some(json!([])),
        Some(json!(["東京", "gone", "東京/子", "東京"])),
    ] {
        let f = Fixture::new();
        f.register_shell().await;
        let old = f.put(b"old body").await;
        let base = f
            .tree(vec![
                json!({"path":"東京/子","ref":old,"mode":644}),
                json!({"path":"東京.bak","ref":old,"mode":755}),
                json!({"path":"gone","ref":old,"mode":644}),
            ])
            .await;
        let mut params = json!({"tree":base,"tool":"sh","actor":"local","cwd":"","args":["-c","printf allowed > '東京/子'; printf created > '東京/new'; printf forbidden > '東京.bak'; rm gone"]});
        if let Some(declared) = &declared {
            params["declared_write_paths"] = declared.clone();
        }
        let result = f.call("exec.run", params).await;
        let receipt = &result["receipt"];
        assert_eq!(receipt["exit_code"], 0);
        let out = f
            .call("exec.tree_get", json!({"tree":receipt["tree_out"]}))
            .await;
        let entries = out["entries"].as_array().unwrap();
        let unrestricted = declared.as_ref().is_none_or(Value::is_null);
        if unrestricted {
            assert_eq!(receipt["success"], true);
            assert_eq!(receipt["undeclared_changes"], json!([]));
        } else if declared.as_ref().unwrap().as_array().unwrap().is_empty() {
            assert_eq!(receipt["tree_out"], base);
            assert_eq!(receipt["changed"], json!([]));
        } else {
            assert_eq!(receipt["undeclared_changes"], json!(["東京.bak"]));
            assert_eq!(
                receipt["changed"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v["path"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                vec!["gone", "東京/new", "東京/子"]
            );
        }
        if !unrestricted {
            let entry = entries.iter().find(|e| e["path"] == "東京.bak").unwrap();
            assert_eq!(entry["ref"], old);
            assert_eq!(entry["mode"], 755);
            let forbidden = ContentRef::from_hex(crate::tree::digest_hex(b"forbidden")).unwrap();
            assert!(!f.store.inner.exists(&forbidden).await.unwrap());
        }
    }
}

#[tokio::test]
async fn invalid_declarations_keep_input_error_order_and_do_not_materialize() {
    let f = Fixture::new();
    let base = f.tree(vec![]).await;
    for invalid in ["", ".", "/", "a/", "a//b", "a/../b"] {
        let err = f.error("exec.run",json!({"tree":base,"tool":"unused","actor":"local","declared_write_paths":["valid",invalid,"/second"]})).await;
        assert!(err.contains("declared_write_paths"), "{err}");
        if invalid.is_empty() {
            assert!(err.contains("empty"), "{err}");
        }
        assert!(
            !err.contains("/second"),
            "first invalid declaration must win: {err}"
        );
        assert!(!f.root.exists());
    }
}
