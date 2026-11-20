use super::*;
use crate::{Namespace, NamespaceToken};

fn specs(token: &NamespaceToken) -> [AtomicNoteSpec<'_>; 2] {
    [0, 1].map(|_| AtomicNoteSpec {
        token,
        id: Some(Uuid::new_v4()),
        kind: "message",
        name: None,
        content: "ordinary message body",
        properties: None,
    })
}

#[tokio::test]
async fn keyed_message_claim_refuses_its_physical_key_before_unkeyed_preparation() {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let seed = Note::new("local", "observation", "retained comm fixture");
    runtime
        .raw_notes(&token)
        .unwrap()
        .upsert_note(seed.clone())
        .await
        .unwrap();
    let ordinary = r#"comm-v1:["local","lambda:fixture","review-pass"]"#;
    let created = create_keyed_message_pair(&runtime, specs(&token), ordinary)
        .await
        .unwrap();
    let KeyedMessageWrite::Created { notes, .. } = created else {
        panic!("new pair must be created")
    };
    assert_eq!(notes.len(), 2);
    assert_eq!(notes[0].key.as_deref(), Some(ordinary));
    assert_eq!(notes[1].key, None);
    let before = runtime.list_notes(&token, None, 100, 0).await.unwrap();
    let key = format!("ghp_{}", "A".repeat(40));
    let physical = format!(
        "comm-v1:{}",
        serde_json::json!(["local", "lambda:fixture", key])
    );
    let result = create_keyed_message_pair_with_extra_statements(
        &runtime,
        specs(&token),
        &physical,
        &[],
        vec![],
    )
    .await;
    assert!(
        matches!(&result, Err(RuntimeError::SecretDetected(matched))
        if matched.detector == "github-token" && matched.location.as_deref() == Some("note.key")),
        "comm physical key must be refused before its unkeyed pair and direct UPDATE claim"
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
