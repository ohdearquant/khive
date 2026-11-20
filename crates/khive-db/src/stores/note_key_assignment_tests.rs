use super::*;
use crate::pool::PoolConfig;
use khive_storage::note::NoteKeyAssignOutcome;
use rusqlite::types::Value as SqliteValue;
use std::collections::BTreeMap;

fn fixture() -> (Arc<ConnectionPool>, SqlNoteStore) {
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: None,
            ..PoolConfig::default()
        })
        .unwrap(),
    );
    {
        let writer = pool.writer().unwrap();
        writer.conn().execute_batch(NOTES_DDL).unwrap();
    }
    let store = SqlNoteStore::new(Arc::clone(&pool), false);
    (pool, store)
}

fn stored_row(pool: &ConnectionPool, id: Uuid) -> BTreeMap<String, SqliteValue> {
    let writer = pool.writer().unwrap();
    let mut statement = writer
        .conn()
        .prepare("SELECT * FROM notes WHERE id = ?1")
        .unwrap();
    let names: Vec<String> = statement.column_names().map(str::to_owned).collect();
    statement
        .query_row([id.to_string()], |row| {
            names
                .iter()
                .enumerate()
                .map(|(index, name)| Ok((name.clone(), row.get(index)?)))
                .collect()
        })
        .unwrap()
}

async fn insert(store: &SqlNoteStore, namespace: &str, kind: &str, key: Option<&str>) -> Note {
    let mut note = Note::new(namespace, kind, "content with a persistent identity")
        .with_name("fixture")
        .with_properties(serde_json::json!({"preserve": [1, 2, 3]}));
    note.key = key.map(str::to_owned);
    store.upsert_note(note.clone()).await.unwrap();
    note
}

#[tokio::test]
async fn assignment_changes_only_key_and_one_version_and_is_findable() {
    let (pool, store) = fixture();
    let note = insert(&store, "local", "observation", None).await;
    let before = stored_row(&pool, note.id);

    assert_eq!(
        store
            .assign_note_key_if_unset(note.id, "locator", "local")
            .await
            .unwrap(),
        NoteKeyAssignOutcome::Assigned
    );

    let mut expected = before.clone();
    let old_version = match before.get("version").unwrap() {
        SqliteValue::Integer(value) => *value,
        value => panic!("unexpected version: {value:?}"),
    };
    expected.insert("key".into(), SqliteValue::Text("locator".into()));
    expected.insert("version".into(), SqliteValue::Integer(old_version + 1));
    assert_eq!(stored_row(&pool, note.id), expected);
    let found = store
        .get_live_notes_by_key("local", "locator", Some("observation"))
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].id, note.id);
}

#[tokio::test]
async fn already_keyed_returns_existing_key_without_any_write() {
    let (pool, store) = fixture();
    let note = insert(&store, "local", "observation", Some("original")).await;
    let before = stored_row(&pool, note.id);

    for proposed in ["original", "different"] {
        assert_eq!(
            store
                .assign_note_key_if_unset(note.id, proposed, "local")
                .await
                .unwrap(),
            NoteKeyAssignOutcome::AlreadyKeyed {
                existing: "original".into()
            }
        );
        assert_eq!(stored_row(&pool, note.id), before);
    }
}

#[tokio::test]
async fn unique_index_refusal_does_not_write_and_other_constraints_are_errors() {
    let (pool, store) = fixture();
    insert(&store, "local", "observation", Some("occupied")).await;
    let target = insert(&store, "local", "observation", None).await;
    let before = stored_row(&pool, target.id);
    assert_eq!(
        store
            .assign_note_key_if_unset(target.id, "occupied", "local")
            .await
            .unwrap(),
        NoteKeyAssignOutcome::KeyTaken
    );
    assert_eq!(stored_row(&pool, target.id), before);

    {
        let writer = pool.writer().unwrap();
        writer
            .conn()
            .execute_batch(
                "CREATE TRIGGER assignment_other_constraint BEFORE UPDATE OF key ON notes \
             BEGIN SELECT RAISE(ABORT, 'fixture refusal'); END;",
            )
            .unwrap();
    }
    assert!(store
        .assign_note_key_if_unset(target.id, "free", "local")
        .await
        .is_err());
    assert_eq!(stored_row(&pool, target.id), before);
}

#[tokio::test]
async fn unrelated_unique_index_failure_is_not_reported_as_key_taken() {
    let (pool, store) = fixture();
    insert(&store, "other", "observation", Some("shared")).await;
    let target = insert(&store, "local", "observation", None).await;
    let control = insert(&store, "local", "observation", None).await;
    {
        let writer = pool.writer().unwrap();
        writer
            .conn()
            .execute_batch(
                "CREATE UNIQUE INDEX fixture_global_note_key ON notes(key) WHERE key IS NOT NULL;",
            )
            .unwrap();
    }
    assert_eq!(
        store
            .assign_note_key_if_unset(control.id, "free", "local")
            .await
            .unwrap(),
        NoteKeyAssignOutcome::Assigned
    );
    let before = stored_row(&pool, target.id);
    let result = store
        .assign_note_key_if_unset(target.id, "shared", "local")
        .await;
    assert!(
        result.is_err(),
        "the independent unique constraint must be propagated: {result:?}"
    );
    assert_eq!(stored_row(&pool, target.id), before);
}

#[tokio::test]
async fn uniqueness_is_scoped_to_live_notes_in_one_namespace_and_kind() {
    let (_pool, store) = fixture();
    let holder = insert(&store, "local", "observation", Some("shared")).await;
    let other_kind = insert(&store, "local", "task", None).await;
    let other_namespace = insert(&store, "other", "observation", None).await;
    assert_eq!(
        store
            .assign_note_key_if_unset(other_kind.id, "shared", "local")
            .await
            .unwrap(),
        NoteKeyAssignOutcome::Assigned
    );
    assert_eq!(
        store
            .assign_note_key_if_unset(other_namespace.id, "shared", "other")
            .await
            .unwrap(),
        NoteKeyAssignOutcome::Assigned
    );

    assert!(store
        .delete_note(holder.id, DeleteMode::Soft)
        .await
        .unwrap());
    let replacement = insert(&store, "local", "observation", None).await;
    assert_eq!(
        store
            .assign_note_key_if_unset(replacement.id, "shared", "local")
            .await
            .unwrap(),
        NoteKeyAssignOutcome::Assigned
    );
}

#[tokio::test]
async fn absent_deleted_and_wrong_namespace_are_not_found_without_writes() {
    let (pool, store) = fixture();
    let live = insert(&store, "local", "observation", None).await;
    let deleted = insert(&store, "local", "observation", None).await;
    assert!(store
        .delete_note(deleted.id, DeleteMode::Soft)
        .await
        .unwrap());
    let live_before = stored_row(&pool, live.id);
    let deleted_before = stored_row(&pool, deleted.id);

    for (id, namespace) in [
        (Uuid::new_v4(), "local"),
        (deleted.id, "local"),
        (live.id, "other"),
    ] {
        assert_eq!(
            store
                .assign_note_key_if_unset(id, "locator", namespace)
                .await
                .unwrap(),
            NoteKeyAssignOutcome::NotFound
        );
    }
    assert_eq!(stored_row(&pool, live.id), live_before);
    assert_eq!(stored_row(&pool, deleted.id), deleted_before);
}

#[tokio::test]
async fn invalid_locator_bytes_are_refused_without_mutation() {
    let (pool, store) = fixture();
    let note = insert(&store, "local", "observation", None).await;
    let before = stored_row(&pool, note.id);
    for invalid in ["x".repeat(513), "prefix\0suffix".to_owned()] {
        assert!(store
            .assign_note_key_if_unset(note.id, &invalid, "local")
            .await
            .is_err());
        assert_eq!(stored_row(&pool, note.id), before);
    }
    let max_utf8 = "é".repeat(256);
    assert_eq!(
        store
            .assign_note_key_if_unset(note.id, &max_utf8, "local")
            .await
            .unwrap(),
        NoteKeyAssignOutcome::Assigned
    );
}

#[tokio::test]
async fn secret_key_assignment_refuses_before_lookup_and_preserves_complete_rows() {
    let (pool, store) = fixture();
    let plain = insert(&store, "local", "observation", None).await;
    let held = insert(&store, "local", "observation", Some("existing")).await;
    let before = [stored_row(&pool, plain.id), stored_row(&pool, held.id)];
    let key = format!("ghp_{}", "A".repeat(40));
    let escaped = format!(r#""\u0067hp_{}""#, "A".repeat(40));
    for proposed in [&key, &escaped] {
        for id in [plain.id, held.id, Uuid::nil()] {
            let result = store.assign_note_key_if_unset(id, proposed, "local").await;
            assert!(
                matches!(&result, Err(StorageError::SecretDetected { matched, .. })
                if matched.detector == "github-token" && matched.location.as_deref() == Some("note.key")),
                "direct SQL assignment must refuse credential keys before lookup: {result:?}"
            );
            assert!(!format!("{result:?}").contains(&key));
        }
    }
    assert_eq!(
        [stored_row(&pool, plain.id), stored_row(&pool, held.id)],
        before,
        "refusal must preserve every stored column, version and timestamp"
    );
}

#[tokio::test]
async fn generated_and_unlabelled_note_keys_are_admitted_without_rewriting() {
    let (pool, store) = fixture();
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let id = "11111111-2222-4333-8444-555555555555";
    let keys = [
        ("commit", format!("git.commit:{sha}")),
        ("memory", "review-batch:create:1".into()),
        ("memory", format!("review-batch:{id}:1")),
        (
            "message",
            r#"comm-v1:["local","lambda:fixture","review-pass"]"#.into(),
        ),
        (
            "observation",
            format!("outbound-email-external-id-unverifiable:{id}"),
        ),
        ("observation", "Xk9mZ2vQpLrT8nJwYuAeHfBsDcGiONvM1".into()),
    ];
    for (kind, key) in keys {
        let note = insert(&store, "local", kind, None).await;
        let before = stored_row(&pool, note.id);
        let result = store.assign_note_key_if_unset(note.id, &key, "local").await;
        assert!(
            matches!(result, Ok(NoteKeyAssignOutcome::Assigned)),
            "generated or unlabelled ordinary key must remain assignable: {result:?}"
        );
        let mut expected = before;
        expected.insert("key".into(), SqliteValue::Text(key.clone()));
        expected.insert("version".into(), SqliteValue::Integer(note.version + 1));
        assert_eq!(stored_row(&pool, note.id), expected);
        assert_eq!(
            store
                .get_live_notes_by_key("local", &key, Some(kind))
                .await
                .unwrap()
                .iter()
                .map(|n| n.id)
                .collect::<Vec<_>>(),
            vec![note.id]
        );
    }
}

#[tokio::test]
async fn legacy_credential_key_lookup_remains_readable_and_does_not_rewrite() {
    let (pool, store) = fixture();
    let key = format!("ghp_{}", "A".repeat(40));
    let legacy = insert(&store, "local", "observation", Some(&key)).await;
    let before = stored_row(&pool, legacy.id);
    let rows = store
        .get_live_notes_by_key("local", &key, Some("observation"))
        .await
        .unwrap();
    assert_eq!(rows, vec![legacy.clone()]);
    assert_eq!(store.get_note(legacy.id).await.unwrap(), Some(legacy));
    assert_eq!(stored_row(&pool, rows[0].id), before);
}
