//! Public cancellation regressions for the shared note revision guard.

use std::{sync::Arc, time::Duration};

use khive_runtime::{KhiveRuntime, Namespace, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_types::ErrorKind;
use serde_json::json;
use tokio::sync::Barrier;
use uuid::Uuid;

use super::cancel_seam::AFTER_READ;

async fn pending_event() -> (VerbRegistry, KhiveRuntime, Uuid) {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(khive_pack_comm::CommPack::new(runtime.clone()));
    builder.register(crate::SchedulePack::new(runtime.clone()));
    let registry = builder.build().expect("registry");
    let created = registry
        .dispatch(
            "schedule.remind",
            json!({"content": "retain this reminder", "at": "2099-06-01T09:00:00Z"}),
        )
        .await
        .expect("create reminder");
    let id = created["full_id"].as_str().unwrap().parse().unwrap();
    (registry, runtime, id)
}

#[tokio::test]
async fn public_cancel_conflicts_after_concurrent_edit_fire_or_deletion() {
    for change in ["content", "fire", "delete"] {
        let (registry, runtime, id) = pending_event().await;
        let token = runtime.authorize(Namespace::local()).unwrap();
        let store = runtime.notes(&token).unwrap();
        let before = store.get_note(id).await.unwrap().unwrap();
        let arrived = Arc::new(Barrier::new(2));
        let resume = Arc::new(Barrier::new(2));
        let task_arrived = Arc::clone(&arrived);
        let task_resume = Arc::clone(&resume);
        let cancel = tokio::spawn(async move {
            AFTER_READ
                .scope((task_arrived, task_resume), async move {
                    registry
                        .dispatch("schedule.cancel", json!({"id": id.to_string()}))
                        .await
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(10), arrived.wait())
            .await
            .expect("cancel reached snapshot");

        let mut newer = before.clone();
        match change {
            "content" => newer.content = "edited while still pending".into(),
            "fire" => {
                let properties = newer.properties.as_mut().unwrap();
                properties["status"] = json!("fired");
                properties["fired_at"] = json!("2026-01-01T00:00:00Z");
            }
            "delete" => newer.deleted_at = Some(before.updated_at),
            _ => unreachable!(),
        }
        if change == "delete" {
            // Legacy deletion can change deleted_at without advancing revision.
            let mut writer = runtime.sql().writer().await.unwrap();
            writer
                .execute(SqlStatement {
                    sql: "UPDATE notes SET deleted_at = ?1 WHERE id = ?2".into(),
                    params: vec![
                        SqlValue::Integer(before.updated_at),
                        SqlValue::Text(id.to_string()),
                    ],
                    label: None,
                })
                .await
                .unwrap();
        } else {
            newer.updated_at = before.updated_at.checked_add(1).unwrap();
            assert!(store
                .replace_note_if_unchanged(newer.clone(), before.updated_at, None)
                .await
                .unwrap());
        }
        newer = store.get_note_including_deleted(id).await.unwrap().unwrap();
        tokio::time::timeout(Duration::from_secs(10), resume.wait())
            .await
            .expect("resume cancellation");
        let error = cancel
            .await
            .unwrap()
            .expect_err("stale cancellation must fail");
        assert!(
            matches!(error.refusal_source(), RuntimeError::Khive(e) if e.kind() == ErrorKind::Conflict),
            "{change}: {error}"
        );
        let after = store.get_note_including_deleted(id).await.unwrap().unwrap();
        assert_eq!(
            after, newer,
            "{change}: newer snapshot must remain unchanged"
        );
    }
}

#[tokio::test]
async fn public_cancel_advances_future_revision_and_preserves_other_fields() {
    let (registry, runtime, id) = pending_event().await;
    let token = runtime.authorize(Namespace::local()).unwrap();
    let store = runtime.notes(&token).unwrap();
    let mut before = store.get_note(id).await.unwrap().unwrap();
    before.updated_at = i64::MAX - 1;
    before.properties.as_mut().unwrap()["custom"] = json!({"keep": [1, 2]});
    store.upsert_note(before.clone()).await.unwrap();
    before = store.get_note(id).await.unwrap().unwrap();
    let result = registry
        .dispatch("schedule.cancel", json!({"id": id.to_string()}))
        .await
        .unwrap();
    let after = store.get_note(id).await.unwrap().unwrap();
    let mut expected = before;
    expected.updated_at = i64::MAX;
    expected.version = expected.version.checked_add(1).unwrap();
    let properties = expected.properties.as_mut().unwrap();
    properties["status"] = json!("cancelled");
    properties["cancelled_at"] = result["cancelled_at"].clone();
    assert_eq!(after, expected);
}

#[tokio::test]
async fn public_cancel_refuses_exhausted_revision_without_writing() {
    let (registry, runtime, id) = pending_event().await;
    let token = runtime.authorize(Namespace::local()).unwrap();
    let store = runtime.notes(&token).unwrap();
    let mut before = store.get_note(id).await.unwrap().unwrap();
    before.updated_at = i64::MAX;
    store.upsert_note(before.clone()).await.unwrap();
    before = store.get_note(id).await.unwrap().unwrap();
    let error = registry
        .dispatch("schedule.cancel", json!({"id": id.to_string()}))
        .await
        .unwrap_err();
    assert!(
        matches!(error.refusal_source(), RuntimeError::Internal(_)),
        "{error}"
    );
    assert!(error.to_string().contains("revision cannot advance"));
    assert_eq!(store.get_note(id).await.unwrap().unwrap(), before);
}
