// ── ENTITY_TYPES pack composition (dead-letter fix) ─────────────────────────
//
// The git pack declares `adr` as a `Document` entity-type subtype
// (`vocab::GIT_ENTITY_TYPES`) — see `find_document_for_path`'s doc comment
// above for why `document` entities matched by git-tracked file path are the
// motivating case. These tests exercise the real `create` verb end-to-end
// (dispatch -> handle_create -> validate_entity_type -> the boot-time
// composed registry), not just the isolated unit-level composition covered
// in `khive-runtime::pack`'s tests.

/// The git-pack-declared `adr` Document subtype validates through the real
/// `create` handler once `GitPack` is loaded, and its alias normalises to
/// the canonical name.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn git_pack_adr_entity_type_validates_through_create() {
    let (_rt, _token, registry) = fixture().await;

    let resp = registry
        .dispatch(
            "create",
            json!({"kind": "document", "entity_type": "adr", "name": "ADR-001: example"}),
        )
        .await
        .expect("create must accept the git-pack-declared adr Document subtype");
    assert_eq!(resp["entity_type"], "adr", "{resp}");

    let resp_alias = registry
        .dispatch(
            "create",
            json!({
                "kind": "document",
                "entity_type": "architecture_decision_record",
                "name": "ADR-002: example",
            }),
        )
        .await
        .expect("create must accept the adr alias");
    assert_eq!(resp_alias["entity_type"], "adr", "{resp_alias}");

    // Builtin Document subtypes remain resolvable alongside the pack extension.
    let resp_builtin = registry
        .dispatch(
            "create",
            json!({"kind": "document", "entity_type": "paper", "name": "Some paper"}),
        )
        .await
        .expect("builtin paper subtype must remain resolvable when git pack adds adr");
    assert_eq!(resp_builtin["entity_type"], "paper", "{resp_builtin}");
}

/// Without the git pack loaded, `adr` is not a registered Document subtype —
/// proving the acceptance above is genuine pack composition, not a builtin.
#[tokio::test]
async fn adr_entity_type_rejected_without_git_pack_loaded() {
    let rt = rt();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");
    rt.install_edge_rules(registry.all_edge_rules());
    registry.apply_schema_plans(rt.backend());

    let err = registry
        .dispatch(
            "create",
            json!({"kind": "document", "entity_type": "adr", "name": "ADR-001: example"}),
        )
        .await
        .expect_err("adr must be rejected when the declaring pack is not loaded");
    assert!(
        err.to_string().contains("adr"),
        "error must name the rejected value: {err}"
    );
}

/// Pins the RUNTIME-layer validator, not just the handler-layer check above.
///
/// `fixture()` now runs the same `call_register_entity_type_validators` boot
/// step production runs in `serve.rs` before the first dispatch. This test
/// calls `KhiveRuntime::create_many` directly (the defense-in-depth path for
/// Rust callers that bypass the `create` verb handler) and would fail if the
/// boot-time composed aggregate were ever absent or builtin-only — the gap
/// PR #925 flagged: the handler-layer test above would stay
/// green even if that wiring were silently dropped, since it never exercises
/// `create_many` directly.
///
/// The positive `adr` assertion alone cannot prove the aggregate validator is
/// actually installed: `KhiveRuntime::validate_entity_type_for_kind` returns
/// its input unchanged when no validator is installed at all, so a
/// positive-only test also passes with the boot wiring deleted entirely
/// (PR #925). The negative case below — an unregistered
/// Document subtype rejected as `RuntimeError::InvalidInput` — is what
/// actually distinguishes "composed from pack `ENTITY_TYPES`" from
/// "builtin-only" from "absent".
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn git_pack_adr_entity_type_validates_through_runtime_create_many() {
    let (rt, token, _registry) = fixture().await;

    let created = rt
        .create_many(
            &token,
            vec![EntityCreateSpec {
                kind: "document".to_string(),
                entity_type: Some("adr".to_string()),
                name: "ADR-003: runtime layer".to_string(),
                description: None,
                properties: None,
                tags: vec![],
            }],
        )
        .await
        .expect("runtime-layer create_many must accept the git-pack-declared adr subtype");
    assert_eq!(created.len(), 1, "{created:?}");
    assert_eq!(
        created[0].entity_type.as_deref(),
        Some("adr"),
        "{created:?}"
    );

    // Negative companion (PR #925): the positive `adr`
    // assertion above passes whether the aggregate validator is wired up
    // OR entirely absent, since `KhiveRuntime::validate_entity_type_for_kind`
    // returns the input unchanged when no validator is installed. Only a
    // rejection of an UNREGISTERED Document subtype proves a validator is
    // actually composed and consulted — this call fails closed if the boot
    // wiring (`call_register_entity_type_validators`) were ever dropped.
    let err = rt
        .create_many(
            &token,
            vec![EntityCreateSpec {
                kind: "document".to_string(),
                entity_type: Some("not_a_registered_subtype".to_string()),
                name: "not an ADR".to_string(),
                description: None,
                properties: None,
                tags: vec![],
            }],
        )
        .await
        .expect_err(
            "runtime-layer create_many must reject an unregistered Document subtype when the \
             aggregate entity-type validator is composed",
        );
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "unregistered entity_type must fail as InvalidInput, got: {err:?}"
    );
    assert!(
        err.to_string().contains("not_a_registered_subtype"),
        "error must name the rejected value: {err}"
    );
}
