use super::*;
use crate::Namespace;
use serde_json::json;

fn rt() -> KhiveRuntime {
    KhiveRuntime::memory().unwrap()
}

fn tok(namespace: &str) -> NamespaceToken {
    NamespaceToken::for_namespace(Namespace::parse(namespace).unwrap())
}

async fn keyed(
    rt: &KhiveRuntime,
    tok: &NamespaceToken,
    kind: &str,
    key: &str,
    name: &str,
    properties: Option<serde_json::Value>,
    tags: Vec<&str>,
) -> RuntimeResult<KeyedEntityCreate> {
    rt.create_keyed_entity_with_post_commit_report(
        tok,
        kind,
        key,
        None,
        name,
        None,
        properties,
        tags.into_iter().map(str::to_owned).collect(),
    )
    .await
}

fn created(outcome: KeyedEntityCreate) -> Entity {
    match outcome {
        KeyedEntityCreate::Created { entity, .. } => entity,
        other => panic!("expected a fresh keyed create, got {other:?}"),
    }
}

async fn holder_id(rt: &KhiveRuntime, tok: &NamespaceToken, kind: &str, key: &str) -> Option<Uuid> {
    rt.get_entity_by_key(tok, kind, key)
        .await
        .expect("key lookup")
        .map(|entity| entity.id)
}

async fn live_count(rt: &KhiveRuntime, tok: &NamespaceToken, kind: &str) -> usize {
    rt.list_entities(tok, Some(kind), None, 100, 0)
        .await
        .expect("list entities")
        .len()
}

fn reason(error: &RuntimeError) -> Option<&str> {
    match error {
        RuntimeError::Khive(error) => error.details().and_then(|details| details.get("reason")),
        _ => None,
    }
}

fn taken_keys(error: &RuntimeError) -> serde_json::Value {
    let RuntimeError::Khive(khive) = error else {
        panic!("restore_key_conflict is a domain error: {error:?}");
    };
    serde_json::from_str(
        khive
            .details()
            .and_then(|details| details.get("taken_keys"))
            .expect("taken keys named"),
    )
    .unwrap()
}

/// Rows in `entity_keys` for entity `id`, released or held, read directly.
async fn key_rows(rt: &KhiveRuntime, id: Uuid) -> u64 {
    rt.sql()
        .reader()
        .await
        .unwrap()
        .count(khive_storage::SqlStatement::new(
            "SELECT COUNT(*) FROM entity_keys WHERE entity_id = ?1",
            vec![khive_storage::SqlValue::Text(id.to_string())],
        ))
        .await
        .unwrap()
}

fn secret_shaped_key() -> String {
    const ALPHANUMERIC: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let candidate: String = (0..48)
        .map(|index| char::from(ALPHANUMERIC[(index * 17 + 11) % ALPHANUMERIC.len()]))
        .collect();
    format!("secret value: {candidate}")
}

#[tokio::test]
async fn fresh_key_writes_the_entity_and_get_by_key_finds_it() {
    let rt = rt();
    let tok = tok("entity-keys-fresh");
    let entity = created(
        keyed(
            &rt,
            &tok,
            "concept",
            "paper:smith2024",
            "Smith 2024",
            None,
            vec![],
        )
        .await
        .unwrap(),
    );
    assert_eq!(
        holder_id(&rt, &tok, "concept", "paper:smith2024").await,
        Some(entity.id)
    );
    assert_eq!(holder_id(&rt, &tok, "concept", "paper:other").await, None);
    assert_eq!(live_count(&rt, &tok, "concept").await, 1);
}

#[tokio::test]
async fn identical_payload_replays_the_holder_and_writes_nothing() {
    let rt = rt();
    let tok = tok("entity-keys-replay");
    let first = created(
        keyed(
            &rt,
            &tok,
            "concept",
            "k",
            "Replay",
            Some(json!({"year": 2024, "venue": "x"})),
            vec!["a", "b"],
        )
        .await
        .unwrap(),
    );
    // Tags compare as a set and object members compare without order.
    let replay = keyed(
        &rt,
        &tok,
        "concept",
        "k",
        "Replay",
        Some(json!({"venue": "x", "year": 2024})),
        vec!["b", "a", "a"],
    )
    .await
    .unwrap();
    match replay {
        KeyedEntityCreate::Replayed { entity } => {
            assert_eq!(entity.id, first.id);
            assert_eq!(entity.version, first.version);
        }
        other => panic!("expected a replay, got {other:?}"),
    }
    assert_eq!(live_count(&rt, &tok, "concept").await, 1);
}

#[tokio::test]
async fn differing_payload_is_a_conflict_and_leaves_the_holder_unchanged() {
    let rt = rt();
    let tok = tok("entity-keys-conflict");
    let first = created(
        keyed(
            &rt,
            &tok,
            "concept",
            "k",
            "Original",
            Some(json!({"n": 1})),
            vec![],
        )
        .await
        .unwrap(),
    );
    for (name, properties) in [
        ("Renamed", Some(json!({"n": 1}))),
        // Typed equality: an integer and a float are different values.
        ("Original", Some(json!({"n": 1.0}))),
        ("Original", None),
    ] {
        let outcome = keyed(&rt, &tok, "concept", "k", name, properties, vec![])
            .await
            .unwrap();
        match outcome {
            KeyedEntityCreate::Conflict { existing_id } => assert_eq!(existing_id, first.id),
            other => panic!("expected a conflict for {name}, got {other:?}"),
        }
    }
    let holder = rt
        .get_entity_by_key(&tok, "concept", "k")
        .await
        .unwrap()
        .expect("holder stays live");
    assert_eq!(holder.name, "Original");
    assert_eq!(holder.version, first.version);
    assert_eq!(live_count(&rt, &tok, "concept").await, 1);
}

#[tokio::test]
async fn a_key_is_scoped_to_its_namespace_and_kind() {
    let rt = rt();
    let tok_a = tok("entity-keys-scope-a");
    let tok_b = tok("entity-keys-scope-b");
    let concept_a = created(
        keyed(&rt, &tok_a, "concept", "same", "A", None, vec![])
            .await
            .unwrap(),
    );
    let project_a = created(
        keyed(&rt, &tok_a, "project", "same", "A", None, vec![])
            .await
            .unwrap(),
    );
    let concept_b = created(
        keyed(&rt, &tok_b, "concept", "same", "A", None, vec![])
            .await
            .unwrap(),
    );
    assert_ne!(concept_a.id, project_a.id);
    assert_ne!(concept_a.id, concept_b.id);
    assert_eq!(
        holder_id(&rt, &tok_a, "concept", "same").await,
        Some(concept_a.id)
    );
    assert_eq!(
        holder_id(&rt, &tok_a, "project", "same").await,
        Some(project_a.id)
    );
    assert_eq!(
        holder_id(&rt, &tok_b, "concept", "same").await,
        Some(concept_b.id)
    );
}

#[tokio::test]
async fn soft_delete_releases_the_key_for_a_new_entity() {
    let rt = rt();
    let tok = tok("entity-keys-release");
    let first = created(
        keyed(&rt, &tok, "concept", "k", "First", None, vec![])
            .await
            .unwrap(),
    );
    assert!(rt.delete_entity(&tok, first.id, false).await.unwrap());
    assert_eq!(holder_id(&rt, &tok, "concept", "k").await, None);
    let second = created(
        keyed(&rt, &tok, "concept", "k", "Second", None, vec![])
            .await
            .unwrap(),
    );
    assert_ne!(second.id, first.id);
    assert_eq!(holder_id(&rt, &tok, "concept", "k").await, Some(second.id));
}

#[tokio::test]
async fn restore_takes_a_free_key_back() {
    let rt = rt();
    let tok = tok("entity-keys-restore");
    let entity = created(
        keyed(&rt, &tok, "concept", "k", "Back", None, vec![])
            .await
            .unwrap(),
    );
    assert!(rt.delete_entity(&tok, entity.id, false).await.unwrap());
    let (restored, changed) = rt
        .restore_entity(&tok, entity.id)
        .await
        .unwrap()
        .expect("tombstone found");
    assert!(changed);
    assert_eq!(restored.id, entity.id);
    assert_eq!(holder_id(&rt, &tok, "concept", "k").await, Some(entity.id));
    let outcome = keyed(&rt, &tok, "concept", "k", "Back", None, vec![])
        .await
        .unwrap();
    assert!(
        matches!(outcome, KeyedEntityCreate::Replayed { entity: holder } if holder.id == entity.id)
    );
}

#[tokio::test]
async fn restore_refuses_while_another_entity_holds_the_key() {
    let rt = rt();
    let tok = tok("entity-keys-restore-conflict");
    let first = created(
        keyed(&rt, &tok, "concept", "k", "First", None, vec![])
            .await
            .unwrap(),
    );
    assert!(rt.delete_entity(&tok, first.id, false).await.unwrap());
    let second = created(
        keyed(&rt, &tok, "concept", "k", "Second", None, vec![])
            .await
            .unwrap(),
    );

    // Without a disclosure rule the refusal names the kind and key only.
    let error = rt
        .restore_entity(&tok, first.id)
        .await
        .expect_err("a taken key refuses the restore");
    assert_eq!(reason(&error), Some("restore_key_conflict"), "{error:?}");
    assert_eq!(taken_keys(&error), json!([{"kind": "concept", "key": "k"}]));
    assert!(
        !error.to_string().contains(&second.id.to_string()),
        "{error}"
    );
    // A caller who may list the kind is told the holder.
    let error = rt
        .restore_entity_with_key_disclosure(&tok, first.id, &|kind: &str| kind == "concept")
        .await
        .expect_err("a taken key refuses the restore");
    assert_eq!(
        taken_keys(&error),
        json!([{"kind": "concept", "key": "k", "existing_id": second.id.to_string()}])
    );
    assert!(
        error.to_string().contains(&second.id.to_string()),
        "{error}"
    );

    let tombstone = rt
        .get_entity_including_deleted(&tok, first.id)
        .await
        .unwrap()
        .expect("tombstone kept");
    assert!(
        tombstone.deleted_at.is_some(),
        "the refused restore leaves the entity deleted"
    );
    assert_eq!(holder_id(&rt, &tok, "concept", "k").await, Some(second.id));

    // Once the holder is deleted the first entity can take its key back.
    assert!(rt.delete_entity(&tok, second.id, false).await.unwrap());
    rt.restore_entity(&tok, first.id)
        .await
        .unwrap()
        .expect("tombstone found");
    assert_eq!(holder_id(&rt, &tok, "concept", "k").await, Some(first.id));
    // The second entity's key is now taken in turn.
    let error = rt
        .restore_entity(&tok, second.id)
        .await
        .expect_err("taken in turn");
    assert_eq!(reason(&error), Some("restore_key_conflict"), "{error:?}");
}

#[tokio::test]
async fn hard_delete_removes_the_key_rows() {
    let rt = rt();
    let tok = tok("entity-keys-hard");
    let first = created(
        keyed(&rt, &tok, "concept", "k", "First", None, vec![])
            .await
            .unwrap(),
    );
    assert_eq!(key_rows(&rt, first.id).await, 1);
    assert!(rt.delete_entity(&tok, first.id, true).await.unwrap());
    assert_eq!(key_rows(&rt, first.id).await, 0, "live hard delete");
    assert_eq!(holder_id(&rt, &tok, "concept", "k").await, None);
    let second = created(
        keyed(&rt, &tok, "concept", "k", "Second", None, vec![])
            .await
            .unwrap(),
    );
    assert_ne!(second.id, first.id);

    // A soft delete keeps the released row for a restore; hard-deleting the
    // tombstone drops it.
    assert!(rt.delete_entity(&tok, second.id, false).await.unwrap());
    assert_eq!(key_rows(&rt, second.id).await, 1, "released row kept");
    assert!(rt.delete_entity(&tok, second.id, true).await.unwrap());
    assert_eq!(key_rows(&rt, second.id).await, 0, "tombstone hard delete");
}

#[tokio::test]
async fn a_stamped_holder_created_with_empty_properties_replays() {
    let rt = rt();
    let tok = tok("entity-keys-empty-properties");
    let first = created(
        keyed(&rt, &tok, "concept", "k", "Empty", Some(json!({})), vec![])
            .await
            .unwrap(),
    );
    // The runtime's own stamp, as the secret-gate finalizer leaves it.
    let mut writer = rt.sql().writer().await.unwrap();
    writer
        .execute_script(format!(
            "UPDATE entities SET properties = '{{\"khive:secret_gate\":\"exempted:content-sha256-manifest-v1\"}}', \
             version = version + 1 WHERE id = '{}'",
            first.id
        ))
        .await
        .unwrap();
    drop(writer);
    for properties in [Some(json!({})), None] {
        let outcome = keyed(
            &rt,
            &tok,
            "concept",
            "k",
            "Empty",
            properties.clone(),
            vec![],
        )
        .await
        .unwrap();
        assert!(
            matches!(&outcome, KeyedEntityCreate::Replayed { entity } if entity.id == first.id),
            "{properties:?}: {outcome:?}"
        );
    }
    assert_eq!(live_count(&rt, &tok, "concept").await, 1);
}

#[tokio::test]
async fn malformed_and_secret_shaped_keys_are_refused_without_writes() {
    let rt = rt();
    let tok = tok("entity-keys-refused");
    let long = "k".repeat(513);
    for key in ["nul\0byte", long.as_str()] {
        let error = keyed(&rt, &tok, "concept", key, "Refused", None, vec![])
            .await
            .expect_err("malformed key");
        assert!(
            matches!(&error, RuntimeError::InvalidInput(message) if message == ENTITY_KEY_REFUSAL),
            "{error:?}"
        );
    }
    let at_limit = "k".repeat(512);
    created(
        keyed(&rt, &tok, "concept", &at_limit, "At limit", None, vec![])
            .await
            .unwrap(),
    );

    let secret = secret_shaped_key();
    assert!(
        matches!(
            crate::secret_gate::check(&secret),
            Err(RuntimeError::SecretDetected(_))
        ),
        "fixture key must trip the detector on its own"
    );
    let error = keyed(&rt, &tok, "concept", &secret, "Refused", None, vec![])
        .await
        .expect_err("secret-shaped key");
    assert!(
        matches!(error, RuntimeError::SecretDetected(_)),
        "{error:?}"
    );
    assert!(matches!(
        rt.get_entity_by_key(&tok, "concept", &secret).await,
        Err(RuntimeError::SecretDetected(_))
    ));
    assert_eq!(live_count(&rt, &tok, "concept").await, 1);
}

#[tokio::test]
async fn a_failed_index_write_frees_the_key() {
    let rt = rt();
    let ns = "entity-keys-fts-fail";
    let tok = tok(ns);
    {
        let _arm = crate::arm_fts_fail_scoped(ns);
        let error = keyed(&rt, &tok, "concept", "k", "First", None, vec![])
            .await
            .expect_err("injected FTS failure");
        assert!(
            error.to_string().contains("injected FTS failure"),
            "{error}"
        );
    }
    assert_eq!(holder_id(&rt, &tok, "concept", "k").await, None);
    assert_eq!(live_count(&rt, &tok, "concept").await, 0);
    created(
        keyed(&rt, &tok, "concept", "k", "First", None, vec![])
            .await
            .unwrap(),
    );
}

#[tokio::test]
async fn a_claim_on_a_held_key_writes_nothing() {
    // The write itself, past the holder read: a concurrent create that lost
    // the race reaches the claim with the key already held.
    let rt = rt();
    let tok = tok("entity-keys-claim-guard");
    let first = created(
        keyed(&rt, &tok, "concept", "k", "First", None, vec![])
            .await
            .unwrap(),
    );
    let write = rt
        .create_entity_with_embedding_report_inner(
            &tok,
            "concept",
            None,
            "Loser",
            None,
            None,
            Vec::new(),
            Vec::new(),
            Some("k"),
        )
        .await
        .unwrap();
    assert!(matches!(write, EntityCreateWrite::KeyHeld));
    assert_eq!(live_count(&rt, &tok, "concept").await, 1);
    assert_eq!(holder_id(&rt, &tok, "concept", "k").await, Some(first.id));
}

#[test]
fn holder_payload_ignores_runtime_owned_properties() {
    let mut holder = Entity::new("ns", "concept", "Name");
    holder.properties = Some(json!({"khive:secret_gate": "exempted:content-sha256-manifest-v1"}));
    assert!(same_entity_payload(&holder, None, "Name", None, None, &[]));
    holder.properties = Some(json!({"a": 1, "khive:secret_gate": "exempted"}));
    assert!(same_entity_payload(
        &holder,
        None,
        "Name",
        None,
        Some(&json!({"a": 1})),
        &[]
    ));
    assert!(!same_entity_payload(&holder, None, "Name", None, None, &[]));
    // Absent and `{}` compare equal on both sides.
    holder.properties = Some(json!({}));
    assert!(same_entity_payload(&holder, None, "Name", None, None, &[]));
    assert!(same_entity_payload(
        &holder,
        None,
        "Name",
        None,
        Some(&json!({})),
        &[]
    ));
    holder.properties = None;
    assert!(same_entity_payload(
        &holder,
        None,
        "Name",
        None,
        Some(&json!({})),
        &[]
    ));
    assert!(!same_entity_payload(
        &holder,
        None,
        "Name",
        None,
        Some(&json!({"a": 1})),
        &[]
    ));
}
