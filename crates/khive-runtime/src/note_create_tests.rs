use super::*;
use crate::atomic_runner::{run_atomic_unit, AtomicRunOutcome};
use crate::{Namespace, NamespaceToken};
use khive_storage::Note;

fn spec<'a>(token: &'a NamespaceToken, content: &'a str) -> AtomicNoteSpec<'a> {
    AtomicNoteSpec {
        token,
        id: None,
        kind: "observation",
        name: None,
        content,
        properties: None,
    }
}

#[tokio::test]
async fn deferred_key_publication_scans_before_taking_the_key() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let seed = Note::new("local", "observation", "persistent deferred fixture");
    runtime
        .raw_notes(&token)
        .unwrap()
        .upsert_note(seed.clone())
        .await
        .unwrap();
    let (prepared, _) = prepare_note_create(
        &runtime,
        spec(&token, "lawful deferred creation"),
        AtomicNoteOptions {
            key: Some("deferred-locator"),
            embed: Some(false),
            ..Default::default()
        },
        &[],
        KeyPublication::AfterDependents,
    )
    .await
    .unwrap();
    let created_id = prepared.notes[0].id;
    assert!(matches!(
        run_atomic_unit(runtime.sql().as_ref(), prepared.plans)
            .await
            .unwrap(),
        AtomicRunOutcome::Committed { .. }
    ));
    assert_eq!(
        runtime
            .raw_notes(&token)
            .unwrap()
            .get_note(created_id)
            .await
            .unwrap()
            .unwrap()
            .key
            .as_deref(),
        Some("deferred-locator")
    );
    let before = runtime.list_notes(&token, None, 100, 0).await.unwrap();
    let key = format!("ghp_{}", "A".repeat(40));
    let result = prepare_note_create(
        &runtime,
        spec(&token, "ordinary content"),
        AtomicNoteOptions {
            key: Some(&key),
            embed: Some(false),
            ..Default::default()
        },
        &[],
        KeyPublication::AfterDependents,
    )
    .await;
    assert!(
        matches!(&result, Err(RuntimeError::SecretDetected(matched))
        if matched.detector == "github-token" && matched.location.as_deref() == Some("note.key")),
        "deferred publication must refuse before key.take hides the incoming key"
    );
    assert_eq!(
        runtime.list_notes(&token, None, 100, 0).await.unwrap(),
        before
    );
    assert_eq!(
        runtime
            .raw_notes(&token)
            .unwrap()
            .get_note(seed.id)
            .await
            .unwrap(),
        Some(seed)
    );
}
