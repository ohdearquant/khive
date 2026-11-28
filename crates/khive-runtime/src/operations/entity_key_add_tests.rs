use super::*;
use crate::curation::{ContentMergeStrategy, EntityDedupMergePolicy};
use crate::Namespace;

fn rt() -> KhiveRuntime {
    KhiveRuntime::memory().unwrap()
}

fn tok(namespace: &str) -> NamespaceToken {
    NamespaceToken::for_namespace(Namespace::parse(namespace).unwrap())
}

async fn keyed(rt: &KhiveRuntime, tok: &NamespaceToken, kind: &str, key: &str) -> Entity {
    match rt
        .create_keyed_entity_with_post_commit_report(
            tok,
            kind,
            key,
            None,
            key,
            None,
            None,
            Vec::new(),
            &EntityAliasChange::default(),
        )
        .await
        .unwrap()
    {
        KeyedEntityCreate::Created { entity, .. } => entity,
        other => panic!("expected a fresh keyed create, got {other:?}"),
    }
}

async fn plain(rt: &KhiveRuntime, tok: &NamespaceToken, name: &str) -> Entity {
    rt.create_entity(tok, "concept", None, name, None, None, Vec::new())
        .await
        .unwrap()
}

async fn holder_id(rt: &KhiveRuntime, tok: &NamespaceToken, kind: &str, key: &str) -> Option<Uuid> {
    rt.get_entity_by_key(tok, kind, key)
        .await
        .expect("key lookup")
        .map(|entity| entity.id)
}

/// Rows in `entity_keys` for entity `id`, released or held, read directly.
async fn key_rows(rt: &KhiveRuntime, id: Uuid) -> u64 {
    rt.sql()
        .reader()
        .await
        .unwrap()
        .count(SqlStatement::new(
            "SELECT COUNT(*) FROM entity_keys WHERE entity_id = ?1",
            vec![SqlValue::Text(id.to_string())],
        ))
        .await
        .unwrap()
}

fn reason(error: &RuntimeError) -> Option<&str> {
    match error {
        RuntimeError::Khive(error) => error.details().and_then(|details| details.get("reason")),
        _ => None,
    }
}

fn added(outcome: EntityKeyAdd) -> Entity {
    match outcome {
        EntityKeyAdd::Added { entity } => entity,
        other => panic!("expected the key to be added, got {other:?}"),
    }
}

fn rolled_back_at(outcome: AtomicRunOutcome) -> Option<String> {
    match outcome {
        AtomicRunOutcome::RolledBack {
            failure:
                AtomicOpFailure::GuardFailed {
                    statement_label, ..
                },
            ..
        } => statement_label,
        other => panic!("expected a guard to roll the unit back, got {other:?}"),
    }
}

fn secret_shaped_key() -> String {
    const ALPHANUMERIC: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let candidate: String = (0..48)
        .map(|index| char::from(ALPHANUMERIC[(index * 17 + 11) % ALPHANUMERIC.len()]))
        .collect();
    format!("secret value: {candidate}")
}

#[tokio::test]
async fn a_free_key_is_added_beside_the_key_the_entity_was_created_with() {
    let rt = rt();
    let tok = tok("entity-key-add-free");
    let entity = keyed(&rt, &tok, "concept", "first").await;
    let after = added(rt.add_entity_key(&tok, entity.id, "second").await.unwrap());
    assert_eq!(after.id, entity.id);
    assert_eq!(
        holder_id(&rt, &tok, "concept", "second").await,
        Some(entity.id)
    );
    assert_eq!(
        holder_id(&rt, &tok, "concept", "first").await,
        Some(entity.id)
    );
    assert_eq!(key_rows(&rt, entity.id).await, 2);

    let unkeyed = plain(&rt, &tok, "Unkeyed").await;
    added(rt.add_entity_key(&tok, unkeyed.id, "third").await.unwrap());
    assert_eq!(
        holder_id(&rt, &tok, "concept", "third").await,
        Some(unkeyed.id)
    );
    assert_eq!(key_rows(&rt, unkeyed.id).await, 1);
}

#[tokio::test]
async fn a_key_the_entity_holds_is_added_again_as_nothing_written() {
    let rt = rt();
    let tok = tok("entity-key-add-held");
    let entity = keyed(&rt, &tok, "concept", "first").await;
    let outcome = rt.add_entity_key(&tok, entity.id, "first").await.unwrap();
    assert!(
        matches!(&outcome, EntityKeyAdd::Held { entity: held } if held.id == entity.id),
        "{outcome:?}"
    );
    assert_eq!(key_rows(&rt, entity.id).await, 1);

    added(rt.add_entity_key(&tok, entity.id, "second").await.unwrap());
    let again = rt.add_entity_key(&tok, entity.id, "second").await.unwrap();
    assert!(matches!(again, EntityKeyAdd::Held { .. }), "{again:?}");
    assert_eq!(key_rows(&rt, entity.id).await, 2);

    // The plans alone, committed over a key the entity already holds, pass
    // and write no second row.
    let plans = entity_key_add_plans(
        tok.namespace().as_str(),
        entity.id,
        "concept",
        "second",
        Utc::now().timestamp_micros(),
    )
    .unwrap();
    assert!(matches!(
        run_atomic_unit(rt.sql().as_ref(), plans).await.unwrap(),
        AtomicRunOutcome::Committed { .. }
    ));
    assert_eq!(key_rows(&rt, entity.id).await, 2);
}

#[tokio::test]
async fn a_key_another_live_entity_holds_is_a_conflict_and_writes_nothing() {
    let rt = rt();
    let tok = tok("entity-key-add-conflict");
    let entity = keyed(&rt, &tok, "concept", "mine").await;
    let other = keyed(&rt, &tok, "concept", "taken").await;
    let outcome = rt.add_entity_key(&tok, entity.id, "taken").await.unwrap();
    assert!(
        matches!(outcome, EntityKeyAdd::Conflict { existing_id } if existing_id == other.id),
        "{outcome:?}"
    );
    assert_eq!(
        holder_id(&rt, &tok, "concept", "taken").await,
        Some(other.id)
    );
    assert_eq!(key_rows(&rt, entity.id).await, 1);
}

#[tokio::test]
async fn a_key_taken_between_read_and_commit_rolls_the_unit_back() {
    let rt = rt();
    let tok = tok("entity-key-add-race");
    let entity = keyed(&rt, &tok, "concept", "mine").await;
    let plans = entity_key_add_plans(
        tok.namespace().as_str(),
        entity.id,
        "concept",
        "race",
        Utc::now().timestamp_micros(),
    )
    .unwrap();
    let winner = keyed(&rt, &tok, "concept", "race").await;
    let outcome = run_atomic_unit(rt.sql().as_ref(), plans).await.unwrap();
    assert_eq!(
        rolled_back_at(outcome).as_deref(),
        Some(ENTITY_KEY_ADD_HELD_LABEL)
    );
    assert_eq!(
        holder_id(&rt, &tok, "concept", "race").await,
        Some(winner.id)
    );
    assert_eq!(key_rows(&rt, entity.id).await, 1);
}

#[tokio::test]
async fn a_key_another_entity_released_is_free_and_that_entity_stays_deleted_on_restore() {
    let rt = rt();
    let tok = tok("entity-key-add-released");
    let entity = keyed(&rt, &tok, "concept", "mine").await;
    let released = keyed(&rt, &tok, "concept", "released").await;
    assert!(rt.delete_entity(&tok, released.id, false).await.unwrap());
    assert_eq!(holder_id(&rt, &tok, "concept", "released").await, None);

    added(
        rt.add_entity_key(&tok, entity.id, "released")
            .await
            .unwrap(),
    );
    assert_eq!(
        holder_id(&rt, &tok, "concept", "released").await,
        Some(entity.id)
    );

    let error = rt
        .restore_entity(&tok, released.id)
        .await
        .expect_err("the key it released is held again");
    assert_eq!(reason(&error), Some("restore_key_conflict"), "{error:?}");
    let RuntimeError::Khive(khive) = &error else {
        panic!("restore_key_conflict is a domain error: {error:?}");
    };
    let taken: serde_json::Value = serde_json::from_str(
        khive
            .details()
            .and_then(|details| details.get("taken_keys"))
            .expect("taken keys named"),
    )
    .unwrap();
    assert_eq!(taken[0]["key"], "released", "{taken}");
    let still = rt
        .entities(&tok)
        .unwrap()
        .get_entity_including_deleted(released.id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        still.deleted_at.is_some(),
        "the refused restore leaves it deleted"
    );
    assert_eq!(
        holder_id(&rt, &tok, "concept", "released").await,
        Some(entity.id)
    );
}

#[tokio::test]
async fn a_key_held_under_another_kind_does_not_clash() {
    let rt = rt();
    let tok = tok("entity-key-add-kinds");
    let entity = keyed(&rt, &tok, "concept", "mine").await;
    let document = keyed(&rt, &tok, "document", "shared").await;
    added(rt.add_entity_key(&tok, entity.id, "shared").await.unwrap());
    assert_eq!(
        holder_id(&rt, &tok, "concept", "shared").await,
        Some(entity.id)
    );
    assert_eq!(
        holder_id(&rt, &tok, "document", "shared").await,
        Some(document.id)
    );
}

#[tokio::test]
async fn an_added_key_holds_its_entity_to_the_kind_it_was_added_under() {
    let rt = rt();
    let tok = tok("entity-key-add-kind-guard");
    let unkeyed = plain(&rt, &tok, "Control").await;
    let entity = plain(&rt, &tok, "Guarded").await;
    added(rt.add_entity_key(&tok, entity.id, "bound").await.unwrap());
    let mut writer = rt.sql().writer().await.unwrap();
    writer
        .execute_script(format!(
            "UPDATE entities SET kind = 'document', version = version + 1 WHERE id = '{}'",
            unkeyed.id
        ))
        .await
        .expect("an entity holding no key may change kind");
    let error = writer
        .execute_script(format!(
            "UPDATE entities SET kind = 'document', version = version + 1 WHERE id = '{}'",
            entity.id
        ))
        .await
        .expect_err("an entity holding an added key may not change kind");
    assert!(
        format!("{error:?}").contains("entity kind differs from the kind its keys are held under"),
        "{error:?}"
    );
}

#[tokio::test]
async fn plans_for_an_entity_of_another_kind_or_deleted_fail_the_live_guard() {
    let rt = rt();
    let tok = tok("entity-key-add-live-guard");
    let namespace = tok.namespace().as_str().to_owned();
    let entity = plain(&rt, &tok, "Live").await;
    let plans = entity_key_add_plans(
        &namespace,
        entity.id,
        "document",
        "wrong-kind",
        Utc::now().timestamp_micros(),
    )
    .unwrap();
    let outcome = run_atomic_unit(rt.sql().as_ref(), plans).await.unwrap();
    assert_eq!(
        rolled_back_at(outcome).as_deref(),
        Some(ENTITY_KEY_ADD_LIVE_LABEL)
    );

    let plans = entity_key_add_plans(
        &namespace,
        entity.id,
        "concept",
        "after-delete",
        Utc::now().timestamp_micros(),
    )
    .unwrap();
    assert!(rt.delete_entity(&tok, entity.id, false).await.unwrap());
    let outcome = run_atomic_unit(rt.sql().as_ref(), plans).await.unwrap();
    assert_eq!(
        rolled_back_at(outcome).as_deref(),
        Some(ENTITY_KEY_ADD_LIVE_LABEL)
    );
    assert_eq!(key_rows(&rt, entity.id).await, 0);
}

#[tokio::test]
async fn an_add_to_a_deleted_merged_away_or_missing_entity_is_refused() {
    let rt = rt();
    let tok = tok("entity-key-add-refused");
    let deleted = keyed(&rt, &tok, "concept", "deleted").await;
    assert!(rt.delete_entity(&tok, deleted.id, false).await.unwrap());
    let error = rt
        .add_entity_key(&tok, deleted.id, "late")
        .await
        .expect_err("deleted");
    assert_eq!(reason(&error), Some("entity_deleted"), "{error:?}");
    assert_eq!(key_rows(&rt, deleted.id).await, 1);
    assert_eq!(holder_id(&rt, &tok, "concept", "late").await, None);

    let kept = keyed(&rt, &tok, "concept", "kept").await;
    let merged = keyed(&rt, &tok, "concept", "merged").await;
    rt.merge_entity(
        &tok,
        kept.id,
        merged.id,
        EntityDedupMergePolicy::PreferInto,
        ContentMergeStrategy::PreferInto,
        false,
    )
    .await
    .unwrap();
    let error = rt
        .add_entity_key(&tok, merged.id, "late")
        .await
        .expect_err("merged away");
    assert_eq!(reason(&error), Some("entity_merged"), "{error:?}");
    let RuntimeError::Khive(khive) = &error else {
        panic!("entity_merged is a domain error: {error:?}");
    };
    assert_eq!(
        khive
            .details()
            .and_then(|details| details.get("merged_into")),
        Some(kept.id.to_string().as_str())
    );
    assert_eq!(holder_id(&rt, &tok, "concept", "late").await, None);

    let error = rt
        .add_entity_key(&tok, Uuid::new_v4(), "late")
        .await
        .expect_err("missing");
    assert!(matches!(error, RuntimeError::NotFound(_)), "{error:?}");
}

#[tokio::test]
async fn malformed_and_secret_shaped_keys_are_refused_before_any_read() {
    let rt = rt();
    let tok = tok("entity-key-add-scan");
    let entity = plain(&rt, &tok, "Scanned").await;
    let long = "k".repeat(513);
    for key in ["nul\0byte", long.as_str()] {
        // A missing entity would answer NotFound: the key is refused first.
        let error = rt
            .add_entity_key(&tok, Uuid::new_v4(), key)
            .await
            .expect_err("malformed key");
        assert!(
            matches!(&error, RuntimeError::InvalidInput(message)
                if message == super::super::entity_keys::ENTITY_KEY_REFUSAL),
            "{error:?}"
        );
    }
    let secret = secret_shaped_key();
    assert!(
        matches!(
            crate::secret_gate::check(&secret),
            Err(RuntimeError::SecretDetected(_))
        ),
        "fixture key must trip the detector on its own"
    );
    let error = rt
        .add_entity_key(&tok, Uuid::new_v4(), &secret)
        .await
        .expect_err("secret-shaped key");
    assert!(
        matches!(error, RuntimeError::SecretDetected(_)),
        "{error:?}"
    );
    assert!(matches!(
        entity_key_add_plans(
            tok.namespace().as_str(),
            entity.id,
            "concept",
            &secret,
            Utc::now().timestamp_micros()
        ),
        Err(RuntimeError::SecretDetected(_))
    ));
    let at_limit = "k".repeat(512);
    added(rt.add_entity_key(&tok, entity.id, &at_limit).await.unwrap());
    assert_eq!(key_rows(&rt, entity.id).await, 1);
}
