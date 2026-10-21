//! Exercise config selection, CLI presence and the actual reindex batch calls.
use super::*;
use std::sync::{Arc, Mutex};

fn selected(dir: &std::path::Path, contents: &str) -> PathBuf {
    let path = dir.join("selected.toml");
    std::fs::write(&path, contents).unwrap();
    path
}

#[test]
fn selected_config_batch_and_explicit_cli_values_follow_precedence() {
    let dir = tempfile::tempdir().unwrap();
    for (contents, cli, expected) in [
        ("", None, 128),
        ("[embed]\n", None, 128),
        ("[embed]\nbatch_size = 64\n", None, 64),
        ("[embed]\nbatch_size = 64\n", Some(128), 128),
        ("[embed]\nbatch_size = 64\n", Some(0), 1),
        ("[embed]\nbatch_size = 900\n", None, 500),
        ("[embed]\nbatch_size = 64\n", Some(u32::MAX), 500),
        ("[embed]\nbatch_size = 9223372036854775807\n", None, 500),
    ] {
        let path = selected(dir.path(), contents);
        let (_, batch) = resolve_reindex_configuration(None, Some(&path), cli).unwrap();
        assert_eq!(batch, expected, "config={contents:?}, CLI={cli:?}");
    }
}

#[test]
fn record_repair_rejects_explicit_batch_even_when_it_equals_a_default() {
    let id = Uuid::new_v4().to_string();
    for value in ["128", "0", "64", "500"] {
        assert!(
            ReindexArgs::try_parse_from(["reindex", "--id", &id, "--batch-size", value]).is_err(),
            "CLI explicit batch {value} must conflict with --id"
        );
    }
    let mut args = ReindexArgs::try_parse_from(["reindex", "--id", &id]).unwrap();
    assert_eq!(args.batch_size, None);
    record_repair::validate_args(&args).unwrap();
    // Library callers cannot bypass the presence rule by constructing args.
    for value in [128, 0, 64, 500] {
        args.batch_size = Some(value);
        assert!(
            record_repair::validate_args(&args).is_err(),
            "library explicit batch {value} must conflict with --id"
        );
    }
}

#[test]
fn invalid_selected_batch_refuses_before_database_creation_even_with_cli_override() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("must-not-exist.db");
    for value in ["0", "-1", "1.5", "true", "\"64\"", "[]"] {
        let path = selected(dir.path(), &format!("[embed]\nbatch_size = {value}\n"));
        let error = resolve_reindex_configuration(db.to_str(), Some(&path), Some(2))
            .expect_err("a CLI override must not hide invalid selected-file input");
        assert!(format!("{error:#}").contains("embed.batch_size"));
        assert!(!db.exists());
    }
}

#[test]
fn discovered_batch_uses_the_database_anchor_and_explicit_file_wins() {
    if crate::test_process::run_in_child() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let database_dir = root.join("database");
    let caller = root.join("caller");
    let database_config_dir = database_dir.join(".khive");
    std::fs::create_dir_all(&database_config_dir).unwrap();
    std::fs::create_dir(&caller).unwrap();
    std::env::set_current_dir(&caller).unwrap();
    std::fs::write(
        database_config_dir.join("config.toml"),
        "[embed]\nbatch_size=64\n",
    )
    .unwrap();
    let db = database_dir.join("data.db");
    assert_eq!(
        resolve_reindex_configuration(db.to_str(), None, None)
            .unwrap()
            .1,
        64
    );
    let explicit = selected(root, "[embed]\nbatch_size=17\n");
    assert_eq!(
        resolve_reindex_configuration(db.to_str(), Some(&explicit), None)
            .unwrap()
            .1,
        17
    );
    std::fs::write(&explicit, "[embed]\nbatch_size=0\n").unwrap();
    assert!(resolve_reindex_configuration(db.to_str(), Some(&explicit), None).is_err());
    assert!(!db.exists(), "config selection cannot open a database");
}

type Calls = Arc<Mutex<Vec<Vec<String>>>>;
struct CaptureProvider(Calls);
struct CaptureService(Calls);
const MODEL: &str = "multilingual-e5-small";

#[async_trait::async_trait]
impl khive_runtime::EmbedderProvider for CaptureProvider {
    fn name(&self) -> &str {
        MODEL
    }
    fn dimensions(&self) -> usize {
        384
    }
    async fn build(
        &self,
    ) -> Result<Arc<dyn lattice_embed::EmbeddingService>, khive_runtime::RuntimeError> {
        Ok(Arc::new(CaptureService(self.0.clone())))
    }
}

#[async_trait::async_trait]
impl lattice_embed::EmbeddingService for CaptureService {
    async fn embed(
        &self,
        texts: &[String],
        _: lattice_embed::EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
        self.0.lock().unwrap().push(texts.to_vec());
        Ok(texts.iter().map(|_| vec![0.5; 384]).collect())
    }
    fn supports_model(&self, _: lattice_embed::EmbeddingModel) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "reindex-batch-probe"
    }
}

#[tokio::test]
async fn selected_batch_reaches_entity_note_and_knowledge_embedding_calls() {
    if crate::test_process::run_in_child() {
        return;
    }
    for (cli, expected) in [(None, vec![2, 2, 1]), (Some("3"), vec![3, 2])] {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("batch.db");
        let config = selected(
            dir.path(),
            &format!("[embed]\nbatch_size=2\n[[engines]]\nname={MODEL:?}\ndims=384\n"),
        );
        let cfg = resolve_runtime_config(RuntimeConfigInputs {
            db: db.to_str(),
            config: Some(&config),
            namespace: Namespace::local(),
            namespace_explicit: true,
            actor_explicit: false,
            no_embed: true,
            packs: None,
            brain_profile: None,
        })
        .unwrap();
        {
            let rt = KhiveRuntime::new(cfg.clone()).unwrap();
            let token = rt.authorize(Namespace::local()).unwrap();
            for index in 0..5 {
                let entity = Entity::new("local", "concept", format!("entitybatchprobe{index}"));
                rt.entities(&token)
                    .unwrap()
                    .upsert_entity(entity)
                    .await
                    .unwrap();
                rt.notes(&token)
                    .unwrap()
                    .upsert_note(Note::new(
                        "local",
                        "observation",
                        format!("notebatchprobe{index}"),
                    ))
                    .await
                    .unwrap();
                rt.sql().writer().await.unwrap().execute(khive_storage::SqlStatement {
                    sql: "INSERT INTO knowledge_atoms (id, namespace, slug, name, content, created_at, updated_at) VALUES (?1, 'local', ?2, ?2, ?2, 1, 1)".into(),
                    params: vec![
                        khive_storage::SqlValue::Text(Uuid::new_v4().to_string()),
                        khive_storage::SqlValue::Text(format!("atombatchprobe{index}")),
                    ],
                    label: Some("test.reindex_batch.seed".into()),
                }).await.unwrap();
            }
        }
        let mut argv = vec![
            "reindex",
            "--db",
            db.to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
            "--no-sections",
        ];
        if let Some(value) = cli {
            argv.extend(["--batch-size", value]);
        }
        let args = ReindexArgs::try_parse_from(argv).unwrap();
        let calls = Calls::default();
        let captured = calls.clone();
        run_reindex_with_setup(
            args,
            |cfg| cfg,
            move |rt| {
                rt.try_register_embedder(CaptureProvider(captured))?;
                Ok(())
            },
        )
        .await
        .unwrap();
        {
            let calls = calls.lock().unwrap();
            for marker in ["entitybatchprobe", "notebatchprobe", "atombatchprobe"] {
                let sizes: Vec<_> = calls
                    .iter()
                    .filter(|texts| texts.iter().any(|text| text.contains(marker)))
                    .map(|texts| {
                        assert!(texts.iter().all(|text| text.contains(marker)));
                        texts.len()
                    })
                    .collect();
                assert_eq!(sizes, expected, "{marker}: CLI={cli:?}, calls={calls:?}");
            }
        }
        // A configured batch does not turn omission into an explicit CLI flag.
        calls.lock().unwrap().clear();
        // Seed a new unindexed row after the bulk assertions. Healthy vectors
        // intentionally make single-record repair a no-op.
        let id = {
            let rt = KhiveRuntime::new(cfg).unwrap();
            let token = rt.authorize(Namespace::local()).unwrap();
            let entity = Entity::new("local", "concept", "entitybatchrepairprobe");
            let id = entity.id.to_string();
            rt.entities(&token)
                .unwrap()
                .upsert_entity(entity)
                .await
                .unwrap();
            id
        };
        let args = ReindexArgs::try_parse_from([
            "reindex",
            "--db",
            db.to_str().unwrap(),
            "--config",
            config.to_str().unwrap(),
            "--id",
            &id,
        ])
        .unwrap();
        let captured = calls.clone();
        run_reindex_with_setup(
            args,
            |cfg| cfg,
            move |rt| {
                rt.try_register_embedder(CaptureProvider(captured))?;
                Ok(())
            },
        )
        .await
        .unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "single-record repair: {calls:?}");
        assert_eq!(calls[0], ["passage: entitybatchrepairprobe"]);
    }
}
