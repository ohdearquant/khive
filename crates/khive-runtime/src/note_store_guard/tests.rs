use super::*;
use crate::{KhiveRuntime, NamespaceToken};
use serde_json::json;

async fn seed_health(runtime: &KhiveRuntime, deleted: bool) -> Note {
    let token = NamespaceToken::local();
    let mut note = Note::new("local", "channel_health", "heartbeat state");
    note.properties = Some(json!({
        "channel_kind": "email",
        "channel_slug": "original@example.com",
        "operator_note": "original",
    }));
    note.created_at = 100;
    note.updated_at = 100;
    note.deleted_at = deleted.then_some(101);
    let trusted = runtime.raw_notes(&token).unwrap();
    trusted.upsert_note(note.clone()).await.unwrap();
    trusted
        .get_note_including_deleted(note.id)
        .await
        .unwrap()
        .unwrap()
}

async fn write_full_row(
    store: &dyn NoteStore,
    before: &Note,
    replacement: Note,
    cas: bool,
) -> StorageResult<bool> {
    if cas {
        store
            .replace_note_if_unchanged(replacement, before.updated_at, before.deleted_at)
            .await
    } else {
        store.upsert_note(replacement).await.map(|()| true)
    }
}

async fn exercise_full_row_identity(cas: bool) {
    for deleted in [false, true] {
        let runtime = KhiveRuntime::memory().unwrap();
        let token = NamespaceToken::local();
        let store = runtime.notes(&token).unwrap();
        let before = seed_health(&runtime, deleted).await;
        for properties in [
            Some(json!({"channel_kind":"email", "channel_slug":"changed@example.com"})),
            Some(json!({"channel_kind":"telegram", "channel_slug":"original@example.com"})),
            Some(json!({"channel_kind":null, "channel_slug":"original@example.com"})),
            Some(json!({"channel_kind":"email", "channel_slug":null})),
            Some(json!({"channel_slug":"original@example.com"})),
            Some(json!({"channel_kind":"email"})),
            Some(json!({})),
            Some(json!(null)),
            Some(json!([])),
            None,
        ] {
            let mut replacement = before.clone();
            replacement.properties = properties;
            replacement.updated_at += 1;
            let error = write_full_row(store.as_ref(), &before, replacement, cas)
                .await
                .expect_err("a public full-row write must not alter health identity");
            assert!(
                matches!(error, StorageError::InvalidInput { ref message, .. }
                    if message.contains("channel_health") && message.contains("kind-owned")),
                "{error}"
            );
            assert_eq!(
                store.get_note_including_deleted(before.id).await.unwrap(),
                Some(before.clone()),
                "refusal must leave the complete row unchanged"
            );
        }

        // Dropping the owning kind would let a second call bypass its identity
        // policy, even when the first call leaves both coordinates untouched.
        let mut demoted = before.clone();
        demoted.kind = "observation".into();
        demoted.updated_at += 1;
        write_full_row(store.as_ref(), &before, demoted, cas)
            .await
            .expect_err("the owning kind cannot be removed to evade the next write");
        assert_eq!(
            store.get_note_including_deleted(before.id).await.unwrap(),
            Some(before.clone())
        );

        let mut metadata = before.clone();
        metadata.properties.as_mut().unwrap()["operator_note"] = json!("changed");
        metadata.updated_at += 1;
        assert!(write_full_row(store.as_ref(), &before, metadata, cas)
            .await
            .unwrap());
        let after = store
            .get_note_including_deleted(before.id)
            .await
            .unwrap()
            .unwrap();
        let properties = after.properties.as_ref().unwrap();
        assert_eq!(properties["channel_kind"], "email");
        assert_eq!(properties["channel_slug"], "original@example.com");
        assert_eq!(properties["operator_note"], "changed");
        assert_eq!(after.created_at, before.created_at);
        assert_eq!(after.deleted_at, before.deleted_at);
        assert_eq!(after.updated_at, before.updated_at + 1);
        assert!(after.version > before.version);
    }
}

// Must fail with only the existing-row identity guard reverted: the first
// replacement succeeds and changes channel_slug instead of returning refusal.
#[tokio::test]
async fn channel_health_identity_public_store_upsert_refuses_changes() {
    exercise_full_row_identity(false).await;
}

// Separate from the upsert arm so the reverted control must demonstrate that
// the same coordinate change is also refused by the public compare-and-swap.
#[tokio::test]
async fn channel_health_identity_public_store_cas_refuses_changes() {
    exercise_full_row_identity(true).await;
}

#[tokio::test]
async fn channel_health_identity_public_store_batch_validates_before_any_write() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = NamespaceToken::local();
    let store = runtime.notes(&token).unwrap();
    let before = seed_health(&runtime, false).await;
    let ordinary = Note::new("local", "observation", "unrelated sibling");
    let mut changed = before.clone();
    changed.properties.as_mut().unwrap()["channel_slug"] = json!("changed@example.com");
    changed.updated_at += 1;
    store
        .upsert_notes(vec![ordinary.clone(), changed])
        .await
        .expect_err("the complete batch must be admitted before its first write");
    assert_eq!(store.get_note(ordinary.id).await.unwrap(), None);
    assert_eq!(store.get_note(before.id).await.unwrap(), Some(before));

    let mut first = Note::new("local", "channel_health", "new health row");
    first.properties = Some(json!({"channel_kind":"email", "channel_slug":"first@example.com"}));
    let mut second = first.clone();
    second.properties.as_mut().unwrap()["channel_slug"] = json!("second@example.com");
    store
        .upsert_notes(vec![first.clone(), second])
        .await
        .expect_err("a duplicate ID cannot alter identity established earlier in its batch");
    assert_eq!(store.get_note(first.id).await.unwrap(), None);

    let mut metadata = first.clone();
    metadata.properties.as_mut().unwrap()["operator_note"] = json!("allowed");
    store
        .upsert_notes(vec![first.clone(), metadata])
        .await
        .unwrap();
    let stored = store.get_note(first.id).await.unwrap().unwrap();
    assert_eq!(
        stored.properties.as_ref().unwrap()["channel_slug"],
        "first@example.com"
    );
    assert_eq!(
        stored.properties.as_ref().unwrap()["operator_note"],
        "allowed"
    );
}

#[tokio::test]
async fn channel_health_identity_public_property_replacement_cannot_erase_coordinates() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = NamespaceToken::local();
    let store = runtime.notes(&token).unwrap();
    let before = seed_health(&runtime, false).await;
    for properties in [
        None,
        Some(json!(null)),
        Some(json!([])),
        Some(json!({"operator_note":"changed"})),
    ] {
        store
            .update_note_properties(before.id, properties, before.updated_at + 1)
            .await
            .expect_err("whole-property replacement cannot erase health coordinates");
        assert_eq!(
            store.get_note(before.id).await.unwrap(),
            Some(before.clone())
        );
    }
    assert!(store
        .set_note_property(
            before.id,
            "operator_note",
            json!("allowed"),
            before.updated_at + 1
        )
        .await
        .unwrap());
    let after = store.get_note(before.id).await.unwrap().unwrap();
    assert_eq!(
        after.properties.as_ref().unwrap()["channel_slug"],
        "original@example.com"
    );
    assert_eq!(
        after.properties.as_ref().unwrap()["operator_note"],
        "allowed"
    );
}

#[tokio::test]
async fn channel_health_identity_public_store_preserves_first_insert_and_ordinary_metadata() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = NamespaceToken::local();
    let store = runtime.notes(&token).unwrap();
    let mut first = Note::new("local", "channel_health", "first heartbeat");
    first.properties = Some(json!({"channel_kind":"email", "channel_slug":"new@example.com"}));
    assert!(store.insert_note_if_absent(first.clone()).await.unwrap());
    let mut collision = first.clone();
    collision.properties.as_mut().unwrap()["channel_slug"] = json!("other@example.com");
    assert!(!store.insert_note_if_absent(collision).await.unwrap());
    assert_eq!(store.get_note(first.id).await.unwrap(), Some(first));

    let mut ordinary = Note::new("local", "observation", "unowned metadata");
    ordinary.properties = Some(json!({"channel_kind":"one", "channel_slug":"before"}));
    store.upsert_note(ordinary.clone()).await.unwrap();
    for cas in [false, true] {
        let before = store.get_note(ordinary.id).await.unwrap().unwrap();
        let mut changed = before.clone();
        changed.properties =
            Some(json!({"channel_kind":cas, "channel_slug":format!("after-{cas}")}));
        changed.updated_at += 1;
        assert!(
            write_full_row(store.as_ref(), &before, changed.clone(), cas)
                .await
                .unwrap()
        );
        assert_eq!(
            store
                .get_note(ordinary.id)
                .await
                .unwrap()
                .unwrap()
                .properties,
            changed.properties
        );
    }
}
