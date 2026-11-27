//! Keys and aliases in the NDJSON snapshot (ADR-221 section 4).

use super::*;
use khive_storage::entity::EntityAliasOrigin;
use tempfile::TempDir;

const ID_A: &str = "00000000-0000-0000-0000-000000000010";
const ID_B: &str = "00000000-0000-0000-0000-000000000020";
const AT: &str = "2026-01-01T00:00:00+00:00";

/// One entity record as the snapshot writer emits it, with `names` placed
/// after `tags`.
fn line(id: &str, name: &str, names: &str) -> String {
    format!(
        r#"{{"id":"{id}","kind":"concept","name":"{name}","description":null,"properties":null,"tags":[]{names},"created_at":"{AT}","updated_at":"{AT}"}}"#
    )
}

fn snapshot_id(entities: &str) -> String {
    let records: Vec<NdjsonEntity> = read_ndjson(entities, "entity").unwrap();
    let archive = build_kg_archive("local", &records, &[]).unwrap();
    snapshot_id_for_archive(&archive).unwrap().to_string()
}

fn repo_with(entities: &str) -> TempDir {
    let repo = TempDir::new().unwrap();
    let kg = repo.path().join(".khive/kg");
    std::fs::create_dir_all(&kg).unwrap();
    std::fs::write(kg.join("entities.ndjson"), entities).unwrap();
    std::fs::write(kg.join("edges.ndjson"), "").unwrap();
    repo
}

fn open(db: &Path) -> (KhiveRuntime, khive_runtime::NamespaceToken) {
    let namespace = khive_types::Namespace::parse("local").unwrap();
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(db.to_path_buf()),
        default_namespace: namespace.clone(),
        embedding_model: None,
        ..RuntimeConfig::default()
    })
    .unwrap();
    let token = runtime.authorize(namespace).unwrap();
    (runtime, token)
}

#[test]
fn keys_and_aliases_enter_the_snapshot_id_and_a_record_without_them_keeps_its_id() {
    let plain = line(ID_A, "Alpha", "");
    // Computed before snapshots carried keys or aliases: a record holding
    // neither keeps the id it had.
    assert_eq!(
        snapshot_id(&plain),
        "sha256:a81b4a6d8a68a3e50383c3c50e58d678476f652cc753027ea5fed01fa8e00ef9"
    );
    let keyed = line(
        ID_A,
        "Alpha",
        r#","keys":["k1"],"aliases":[{"alias":"Alef","origin":"caller"}]"#,
    );
    let keyed_id = snapshot_id(&keyed);
    assert_ne!(keyed_id, snapshot_id(&plain));
    assert_ne!(
        keyed_id,
        snapshot_id(&line(ID_A, "Alpha", r#","keys":["k1"]"#))
    );
    // The origin is part of the hashed state.
    assert_ne!(
        keyed_id,
        snapshot_id(&line(
            ID_A,
            "Alpha",
            r#","keys":["k1"],"aliases":[{"alias":"Alef","origin":"rename"}]"#,
        ))
    );
    // The order they are listed in is not.
    let listed = line(
        ID_A,
        "Alpha",
        r#","keys":["k1","k2"],"aliases":[{"alias":"Alef","origin":"caller"},{"alias":"Bet","origin":"merge"}]"#,
    );
    let reversed = line(
        ID_A,
        "Alpha",
        r#","keys":["k2","k1"],"aliases":[{"alias":"Bet","origin":"merge"},{"alias":"Alef","origin":"caller"}]"#,
    );
    assert_eq!(snapshot_id(&listed), snapshot_id(&reversed));
}

#[test]
fn the_snapshot_writer_sorts_keys_and_aliases_after_tags() {
    let at = chrono::DateTime::parse_from_rfc3339(AT)
        .unwrap()
        .with_timezone(&Utc);
    let record = ExportedEntity {
        id: ID_A.parse().unwrap(),
        kind: "concept".to_owned(),
        entity_type: None,
        name: "Alpha".to_owned(),
        description: None,
        properties: None,
        tags: Vec::new(),
        keys: vec!["k2".to_owned(), "k1".to_owned()],
        aliases: vec![
            EntityAlias {
                alias: "Bet".to_owned(),
                origin: EntityAliasOrigin::Merge,
            },
            EntityAlias {
                alias: "Alef".to_owned(),
                origin: EntityAliasOrigin::Caller,
            },
        ],
        created_at: at,
        updated_at: at,
    };
    let mut written = Vec::new();
    write_entities_ndjson(&[record], &mut written).unwrap();
    let expected = line(
        ID_A,
        "Alpha",
        r#","keys":["k1","k2"],"aliases":[{"alias":"Alef","origin":"caller"},{"alias":"Bet","origin":"merge"}]"#,
    );
    assert_eq!(String::from_utf8(written).unwrap(), format!("{expected}\n"));
}

#[tokio::test]
async fn sync_rebuilds_keys_and_aliases_exactly_as_the_snapshot_holds_them() {
    let entities = format!(
        "{}\n{}\n",
        line(
            ID_A,
            "Alpha",
            r#","keys":["k1","k2"],"aliases":[{"alias":"Alef","origin":"caller"},{"alias":"Old Alpha","origin":"rename"}]"#,
        ),
        line(ID_B, "Beta", ""),
    );
    let repo = repo_with(&entities);
    let db = repo.path().join("names.db");
    run_sync(repo.path(), &db, "local").await.unwrap();
    let (runtime, token) = open(&db);
    let alpha: Uuid = ID_A.parse().unwrap();
    for key in ["k1", "k2"] {
        let holder = runtime
            .get_entity_by_key(&token, "concept", key)
            .await
            .unwrap()
            .map(|entity| entity.id);
        assert_eq!(holder, Some(alpha), "{key}");
    }
    // Exported again, the store writes the snapshot it was built from.
    let archive = runtime.export_kg(&token).await.unwrap();
    let mut written = Vec::new();
    write_entities_ndjson(&archive.entities, &mut written).unwrap();
    assert_eq!(String::from_utf8(written).unwrap(), entities);
}

#[tokio::test]
async fn sync_refuses_a_snapshot_whose_entities_share_a_key_before_any_write() {
    let entities = format!(
        "{}\n{}\n",
        line(ID_A, "Alpha", r#","keys":["k1"]"#),
        line(ID_B, "Beta", r#","keys":["k1"]"#),
    );
    let repo = repo_with(&entities);
    let db = repo.path().join("names.db");
    let error = run_sync(repo.path(), &db, "local").await.unwrap_err();
    assert!(format!("{error:#}").contains("both carry"), "{error:#}");
    assert!(!db.exists());
}

#[tokio::test]
async fn sync_refuses_an_alias_equal_to_its_entitys_name() {
    let entities = format!(
        "{}\n",
        line(
            ID_A,
            "Alpha",
            r#","aliases":[{"alias":"Alpha","origin":"caller"}]"#,
        )
    );
    let repo = repo_with(&entities);
    let db = repo.path().join("names.db");
    let error = run_sync(repo.path(), &db, "local").await.unwrap_err();
    assert!(format!("{error:#}").contains("own name"), "{error:#}");
    assert!(!db.exists());
}
