#![cfg(feature = "fault-injection")]

use khive_runtime::{
    arm_prefix_resolve_fail_scoped, KhiveRuntime, Namespace, NamespaceToken, RuntimeConfig,
    RuntimeError,
};
use khive_storage::{entity::Entity, StorageError};
use uuid::Uuid;

fn runtime() -> KhiveRuntime {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: Vec::new(),
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: Default::default(),
        wal_ceiling_env_raw: None,
        disk_guard_environment: Default::default(),
        disk_guard_config: None,
        volume_lock_dir: None,
        credentials: Vec::new(),
        visibility_receipts: None,
        mounts: Vec::new(),
        events_split: None,
        packs: Vec::new(),
        actor_id: None,
        brain_profile: None,
        brain: Default::default(),
        blob: Default::default(),
        visible_namespaces: Vec::new(),
        allowed_outbound_namespaces: Vec::new(),
        ..RuntimeConfig::no_embeddings()
    })
    .expect("private memory runtime");
    assert!(runtime.backend().pool().canonical_path().is_none());
    assert!(runtime.backend_data_dir().is_none());
    assert!(runtime.backend_ann_root().is_none());
    assert!(runtime.registered_embedding_model_names().is_empty());
    runtime
}

fn assert_invalid(error: RuntimeError, expected: String) {
    match error {
        RuntimeError::InvalidInput(message) => assert_eq!(message, expected),
        other => panic!("expected InvalidInput, got {other:?}"),
    }
}

fn assert_prefix_timeout(error: RuntimeError) {
    assert!(
        matches!(
            error,
            RuntimeError::Storage(StorageError::Timeout { ref operation })
                if operation == "resolve_prefix"
        ),
        "storage failure must retain its variant and operation: {error:?}"
    );
}

async fn seed(runtime: &KhiveRuntime, token: &NamespaceToken, id: Uuid, deleted: bool) {
    let mut entity = Entity::new(token.namespace().as_str(), "concept", "prefix fixture");
    entity.id = id;
    entity.deleted_at = deleted.then_some(1);
    runtime
        .entities(token)
        .unwrap()
        .upsert_entity(entity)
        .await
        .unwrap();
}

#[tokio::test]
async fn full_uuid_bypasses_lookup_and_short_input_preserves_the_fault_arm() {
    let runtime = runtime();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let id = Uuid::new_v4();
    // No row holds this UUID. The arm proves parseable input does not reach
    // prefix resolution; the direct call afterward proves the arm was active.
    for input in [
        id.to_string(),
        id.simple().to_string(),
        id.to_string().to_uppercase(),
    ] {
        let _arm = arm_prefix_resolve_fail_scoped(&input);
        assert_eq!(
            runtime
                .resolve_uuid_or_prefix(&token, &input)
                .await
                .unwrap(),
            id
        );
        assert_prefix_timeout(runtime.resolve_prefix(&token, &input).await.unwrap_err());
    }
    for input in [format!("{{{id}}}"), format!("urn:uuid:{id}")] {
        assert_eq!(
            runtime
                .resolve_uuid_or_prefix(&token, &input)
                .await
                .unwrap(),
            id
        );
    }

    let short = Uuid::new_v4().simple().to_string()[..7].to_owned();
    let _arm = arm_prefix_resolve_fail_scoped(&short);
    assert_invalid(
        runtime
            .resolve_uuid_or_prefix(&token, &short)
            .await
            .unwrap_err(),
        format!("invalid UUID (expected full UUID or 8+ hex prefix): {short:?}"),
    );
    assert_prefix_timeout(runtime.resolve_prefix(&token, &short).await.unwrap_err());
}

#[tokio::test]
async fn prefixes_keep_primary_namespace_live_rows_and_ambiguity() {
    let runtime = runtime();
    let primary = Namespace::parse("uuid-primary").unwrap();
    let other = Namespace::parse("uuid-other").unwrap();
    let token = runtime
        .authorize_with_visibility(primary.clone(), vec![other.clone()])
        .unwrap();
    let foreign = runtime.authorize(other).unwrap();
    let first = Uuid::parse_str("aabbccdd-1111-4000-8000-000000000001").unwrap();
    let second = Uuid::parse_str("aabbccdd-2222-4000-8000-000000000002").unwrap();
    let foreign_collision = Uuid::parse_str("aabbccdd-3333-4000-8000-000000000005").unwrap();
    let hidden = Uuid::parse_str("bbccddee-1111-4000-8000-000000000003").unwrap();
    let deleted = Uuid::parse_str("ccddeeaa-1111-4000-8000-000000000004").unwrap();
    seed(&runtime, &token, first, false).await;
    seed(&runtime, &foreign, foreign_collision, false).await;
    seed(&runtime, &foreign, hidden, false).await;
    seed(&runtime, &token, deleted, true).await;

    let compact = first.simple().to_string();
    for length in [8, 9, 20, 31] {
        let input = compact[..length].to_uppercase();
        assert_eq!(
            runtime
                .resolve_uuid_or_prefix(&token, &input)
                .await
                .unwrap(),
            first
        );
    }
    for input in ["bbccddee", "ccddeeaa", "ddeeffaa"] {
        assert_invalid(
            runtime
                .resolve_uuid_or_prefix(&token, input)
                .await
                .unwrap_err(),
            format!("no record matches prefix: {input:?}"),
        );
    }
    // Full UUID is an identifier passthrough, not an existence/liveness gate.
    for id in [hidden, deleted] {
        assert_eq!(
            runtime
                .resolve_uuid_or_prefix(&token, &id.to_string())
                .await
                .unwrap(),
            id
        );
    }

    seed(&runtime, &token, second, false).await;
    match runtime
        .resolve_uuid_or_prefix(&token, "AABBCCDD")
        .await
        .unwrap_err()
    {
        RuntimeError::AmbiguousPrefix {
            prefix,
            mut matches,
        } => {
            assert_eq!(prefix, "AABBCCDD");
            matches.sort();
            assert_eq!(matches, vec![first, second]);
        }
        other => panic!("expected AmbiguousPrefix, got {other:?}"),
    }
}

#[tokio::test]
async fn invalid_shape_missing_prefix_and_storage_failure_stay_distinct() {
    let runtime = runtime();
    let token = runtime.authorize(Namespace::local()).unwrap();
    for input in [
        "",
        "1234567",
        "aabbccdd-1",
        "aabbccdd%",
        "aabbccdd_",
        " aabbccdd",
        "aabbccdd\n",
        "abcdefgh",
        "éabcdef0",
    ] {
        assert_invalid(
            runtime
                .resolve_uuid_or_prefix(&token, input)
                .await
                .unwrap_err(),
            format!("invalid UUID (expected full UUID or 8+ hex prefix): {input:?}"),
        );
    }
    // Compact all-hex input passes the wrapper, including the old overlong
    // case: existing prefix bounds decide that it has no match.
    let overlong = "a".repeat(33);
    assert_invalid(
        runtime
            .resolve_uuid_or_prefix(&token, &overlong)
            .await
            .unwrap_err(),
        format!("no record matches prefix: {overlong:?}"),
    );
    let prefix = Uuid::new_v4().simple().to_string()[..16].to_owned();
    let _arm = arm_prefix_resolve_fail_scoped(&prefix);
    assert_prefix_timeout(
        runtime
            .resolve_uuid_or_prefix(&token, &prefix)
            .await
            .unwrap_err(),
    );
    assert_invalid(
        runtime
            .resolve_uuid_or_prefix(&token, &prefix)
            .await
            .unwrap_err(),
        format!("no record matches prefix: {prefix:?}"),
    );
}

#[tokio::test]
async fn verb_context_preserves_passthrough_validation_and_storage_errors() {
    let runtime = runtime();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let id = Uuid::new_v4();
    for verb in ["read", "reply", "thread", "cancel"] {
        for input in [
            id.to_string(),
            id.simple().to_string(),
            id.to_string().to_uppercase(),
        ] {
            let _arm = arm_prefix_resolve_fail_scoped(&input);
            assert_eq!(
                runtime
                    .resolve_uuid_or_prefix_for_verb(&token, &input, verb)
                    .await
                    .unwrap(),
                id
            );
            assert_prefix_timeout(runtime.resolve_prefix(&token, &input).await.unwrap_err());
        }
        for input in [format!("{{{id}}}"), format!("urn:uuid:{id}")] {
            assert_eq!(
                runtime
                    .resolve_uuid_or_prefix_for_verb(&token, &input, verb)
                    .await
                    .unwrap(),
                id
            );
        }
        for input in [
            "",
            "1234567",
            "aabbccdd-1",
            "aabbccdd%",
            " aabbccdd",
            "éabcdef0",
        ] {
            assert_invalid(
                runtime
                    .resolve_uuid_or_prefix_for_verb(&token, input, verb)
                    .await
                    .unwrap_err(),
                format!("{verb}: invalid id {input:?}; expected full UUID or 8-char hex prefix"),
            );
        }
        let _short_arm = arm_prefix_resolve_fail_scoped("1234567");
        assert_invalid(
            runtime
                .resolve_uuid_or_prefix_for_verb(&token, "1234567", verb)
                .await
                .unwrap_err(),
            format!("{verb}: invalid id \"1234567\"; expected full UUID or 8-char hex prefix"),
        );
        assert_prefix_timeout(runtime.resolve_prefix(&token, "1234567").await.unwrap_err());
        let prefix = "aabbccdd";
        let _arm = arm_prefix_resolve_fail_scoped(prefix);
        assert_prefix_timeout(
            runtime
                .resolve_uuid_or_prefix_for_verb(&token, prefix, verb)
                .await
                .unwrap_err(),
        );
        assert_invalid(
            runtime
                .resolve_uuid_or_prefix_for_verb(&token, prefix, verb)
                .await
                .unwrap_err(),
            format!("{verb}: no record matches prefix: {prefix:?}"),
        );
    }
}

#[tokio::test]
async fn verb_context_keeps_primary_namespace_liveness_and_ambiguity() {
    let runtime = runtime();
    let primary = Namespace::parse("verb-primary").unwrap();
    let other = Namespace::parse("verb-other").unwrap();
    let token = runtime
        .authorize_with_visibility(primary, vec![other.clone()])
        .unwrap();
    let foreign = runtime.authorize(other).unwrap();
    let first = Uuid::parse_str("aabbccdd-1111-4000-8000-000000000001").unwrap();
    let second = Uuid::parse_str("aabbccdd-2222-4000-8000-000000000002").unwrap();
    let foreign_collision = Uuid::parse_str("aabbccdd-3333-4000-8000-000000000005").unwrap();
    let hidden = Uuid::parse_str("bbccddee-1111-4000-8000-000000000003").unwrap();
    let deleted = Uuid::parse_str("ccddeeaa-1111-4000-8000-000000000004").unwrap();
    seed(&runtime, &token, first, false).await;
    seed(&runtime, &foreign, foreign_collision, false).await;
    seed(&runtime, &foreign, hidden, false).await;
    seed(&runtime, &token, deleted, true).await;
    for verb in ["read", "reply", "thread", "cancel"] {
        assert_eq!(
            runtime
                .resolve_uuid_or_prefix_for_verb(&token, "AABBCCDD", verb)
                .await
                .unwrap(),
            first
        );
        for id in [hidden, deleted] {
            let compact = id.simple().to_string();
            let prefix = &compact[..8];
            assert_invalid(
                runtime
                    .resolve_uuid_or_prefix_for_verb(&token, prefix, verb)
                    .await
                    .unwrap_err(),
                format!("{verb}: no record matches prefix: {prefix:?}"),
            );
            assert_eq!(
                runtime
                    .resolve_uuid_or_prefix_for_verb(&token, &id.to_string(), verb)
                    .await
                    .unwrap(),
                id
            );
        }
    }
    seed(&runtime, &token, second, false).await;
    for verb in ["read", "reply", "thread", "cancel"] {
        match runtime
            .resolve_uuid_or_prefix_for_verb(&token, "AABBCCDD", verb)
            .await
            .unwrap_err()
        {
            RuntimeError::AmbiguousPrefix {
                prefix,
                mut matches,
            } => {
                assert_eq!(prefix, "AABBCCDD");
                matches.sort();
                assert_eq!(matches, vec![first, second]);
            }
            other => panic!("expected AmbiguousPrefix, got {other:?}"),
        }
    }
}
