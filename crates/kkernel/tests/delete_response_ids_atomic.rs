//! The real atomic CLI reports the resolved UUID after the target is gone.

use std::time::Duration;

use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig, WalCeilingSource};
use khive_storage::{Edge, EdgeRelation, Entity, Note};
use serde_json::{json, Value};
use tokio::process::Command;
use uuid::Uuid;

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn atomic_delete_acknowledgements_identify_the_committed_records() {
    let home = tempfile::tempdir().expect("private CLI fixture");
    let db = home.path().join("records.db");
    let locks = home.path().join("volume-locks");
    std::fs::create_dir_all(&locks).unwrap();
    let config = home.path().join("config.toml");
    std::fs::write(
        &config,
        "[runtime]\npacks = ['kg']\n[packs.kg]\nbackend = 'main'\nno_embed = true\n",
    )
    .unwrap();
    let runtime_config = RuntimeConfig {
        db_path: Some(db.clone()),
        embedding_model: None,
        additional_embedding_models: Vec::new(),
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_env_raw: None,
        wal_ceiling_source: WalCeilingSource::Default,
        disk_guard_config: None,
        disk_guard_environment: Default::default(),
        volume_lock_dir: Some(locks.clone()),
        visibility_receipts: None,
        credentials: Vec::new(),
        actor_id: None,
        brain_profile: None,
        brain: Default::default(),
        events_split: None,
        mounts: Vec::new(),
        blob: Default::default(),
        packs: vec!["kg".into()],
        ..RuntimeConfig::no_embeddings()
    };
    assert_eq!(runtime_config.db_path.as_deref(), Some(db.as_path()));
    assert!(runtime_config.embedding_model.is_none());
    assert!(runtime_config.additional_embedding_models.is_empty());
    let runtime = KhiveRuntime::new(runtime_config).expect("private model-less seed runtime");
    let token = runtime.authorize(Namespace::local()).unwrap();
    let entities = runtime.entities(&token).unwrap();
    let notes = runtime.notes(&token).unwrap();
    let graph = runtime.graph(&token).unwrap();
    let mut source = Entity::new("local", "concept", "Edge source");
    source.id = Uuid::parse_str("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb").unwrap();
    let mut target = Entity::new("local", "concept", "Unrelated keeper");
    target.id = Uuid::parse_str("cccccccc-cccc-4ccc-8ccc-cccccccccccc").unwrap();
    let endpoints = (source.id, target.id);
    entities.upsert_entity(source).await.unwrap();
    entities.upsert_entity(target).await.unwrap();

    let mut operations = Vec::new();
    let mut expected = Vec::new();
    for kind in ["entity", "note", "edge"] {
        for spelling in ["canonical", "uppercase", "simple", "prefix", "name"] {
            if spelling == "name" && kind != "entity" {
                continue;
            }
            let id = Uuid::parse_str(&format!(
                "{:08x}-abcd-4abc-8abc-0123456789ab",
                0xabcdef00_u32 + expected.len() as u32
            ))
            .unwrap();
            let name = format!("Atomic delete {}", expected.len());
            match kind {
                "entity" => {
                    let mut row = Entity::new("local", "concept", &name);
                    row.id = id;
                    entities.upsert_entity(row).await.unwrap();
                }
                "note" => {
                    let mut row = Note::new("local", "observation", "Atomic delete fixture");
                    row.id = id;
                    notes.upsert_note(row).await.unwrap();
                }
                _ => {
                    // Distinct relations are not needed: each edge has a distinct target.
                    let mut endpoint = Entity::new("local", "concept", &name);
                    endpoint.id = Uuid::from_u128(
                        0xdededede_dede_4ded_8ded_000000000000 + expected.len() as u128,
                    );
                    let target_id = endpoint.id;
                    entities.upsert_entity(endpoint).await.unwrap();
                    let now = chrono::Utc::now();
                    graph
                        .upsert_edge(Edge {
                            id: id.into(),
                            namespace: "local".into(),
                            source_id: endpoints.0,
                            target_id,
                            relation: EdgeRelation::Extends,
                            weight: 1.0,
                            created_at: now,
                            updated_at: now,
                            deleted_at: None,
                            metadata: None,
                            target_backend: None,
                        })
                        .await
                        .unwrap();
                }
            }
            let reference = match spelling {
                "uppercase" => id.to_string().to_uppercase(),
                "simple" => id.simple().to_string(),
                "prefix" => id.simple().to_string()[..8].to_owned(),
                "name" => name,
                _ => id.to_string(),
            };
            let mut args = json!({"id": reference, "hard": true});
            if spelling != "name" {
                args["kind"] = json!(kind);
            }
            expected.push((id, kind, args.get("kind").cloned().unwrap_or(Value::Null)));
            operations.push(json!({"tool": "delete", "args": args}));
        }
    }
    let ops_file = home.path().join("deletes.jsonl");
    std::fs::write(
        &ops_file,
        operations
            .iter()
            .map(|op| format!("{op}\n"))
            .collect::<String>(),
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_kkernel"));
    command
        .args(["exec", "--atomic", "--strict", "--ops-file"])
        .arg(&ops_file)
        .arg("--config")
        .arg(&config)
        .arg("--db")
        .arg(&db)
        .args([
            "--actor",
            "test:delete-id",
            "--expect-actor",
            "test:delete-id",
            "--namespace",
            "local",
        ])
        .current_dir(home.path())
        .env_clear()
        .env("HOME", home.path())
        .env("TMPDIR", home.path())
        .env("KHIVE_VOLUME_LOCK_DIR", &locks)
        .env("KHIVE_NO_DAEMON", "1")
        .env("KHIVE_SOCKET", home.path().join("unused.sock"))
        .env("KHIVE_PACKS", "kg")
        .env("RUST_LOG", "error")
        .kill_on_drop(true);
    if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
        command.env("LLVM_PROFILE_FILE", profile);
    }
    let output = tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .expect("bounded atomic child")
        .expect("run actual kkernel binary");
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let envelope: Value = serde_json::from_slice(&output.stdout).expect("atomic JSON response");
    assert_eq!(envelope["atomic"]["committed"], true);
    assert_eq!(envelope["atomic"]["rolled_back"], false);
    assert_eq!(envelope["summary"]["succeeded"], expected.len());
    assert_eq!(envelope["summary"]["failed"], 0);
    let results = envelope["results"].as_array().unwrap();
    assert_eq!(results.len(), expected.len());
    for (index, (id, kind, kind_echo)) in expected.into_iter().enumerate() {
        assert_eq!(results[index]["ok"], true);
        assert_eq!(results[index]["tool"], "delete");
        assert_eq!(
            results[index]["result"],
            json!({"deleted": true, "id": id.to_string(), "kind": kind_echo})
        );
        match kind {
            "entity" => assert!(entities
                .get_entity_including_deleted(id)
                .await
                .unwrap()
                .is_none()),
            "note" => assert!(notes
                .get_note_including_deleted(id)
                .await
                .unwrap()
                .is_none()),
            _ => assert!(graph
                .get_edge_including_deleted(id.into())
                .await
                .unwrap()
                .is_none()),
        }
    }
    assert!(entities.get_entity(endpoints.0).await.unwrap().is_some());
    assert!(entities.get_entity(endpoints.1).await.unwrap().is_some());
}
