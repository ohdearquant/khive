use super::*;
use khive_runtime::{Namespace, RuntimeConfig};
use serde_json::json;

struct CapturingBackfillEmbedder(Arc<std::sync::Mutex<Vec<String>>>);

#[async_trait::async_trait]
impl lattice_embed::EmbeddingService for CapturingBackfillEmbedder {
    async fn embed(
        &self,
        texts: &[String],
        _model: lattice_embed::EmbeddingModel,
    ) -> std::result::Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
        self.0.lock().unwrap().extend_from_slice(texts);
        Ok(texts.iter().map(|_| vec![1.0; 4]).collect())
    }

    fn supports_model(&self, _model: lattice_embed::EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "backfill-capture"
    }
}

#[async_trait::async_trait]
impl khive_runtime::EmbedderProvider for CapturingBackfillEmbedder {
    fn name(&self) -> &str {
        "backfill-capture"
    }

    fn dimensions(&self) -> usize {
        4
    }

    async fn build(
        &self,
    ) -> khive_runtime::RuntimeResult<Arc<dyn lattice_embed::EmbeddingService>> {
        Ok(Arc::new(Self(Arc::clone(&self.0))))
    }
}

// Control: restore the legacy guarded call in scan (and discard its report).
// The first row commits then returns embedding_input_truncated, so scanned
// and promoted stay below two and the report disclosure assertions fail.
#[tokio::test]
async fn apply_scan_retains_truncation_report_and_continues_to_the_second_entity() {
    let args = EntityTypeBackfillArgs::parse_from(["entity-type-backfill", "--apply"]);
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        packs: vec!["kg".into(), "git".into()],
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let (_registry, types) = compose_registry(&runtime).unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let description = "d".repeat(lattice_embed::MAX_TEXT_BYTES + 1);
    let first = Uuid::from_u128(1);
    let second = Uuid::from_u128(2);
    {
        let writer = runtime.backend().pool().writer().unwrap();
        writer.autocommit(|conn| {
        for (id, name, description) in [
            (first, "First", description.as_str()),
            (second, "Second", "short description"),
        ] {
            conn
                    .execute(
                        "INSERT INTO entities \
                         (id, namespace, kind, name, description, properties, created_at, updated_at) \
                         VALUES (?1, 'local', 'document', ?2, ?3, '{\"type\":\"paper\"}', 1, 1)",
                        (id.to_string(), name, description),
                    )
                    .unwrap();
        }
        Ok(())
        }).unwrap();
    }
    // Only this injected-provider fixture enables the latent embedding
    // branch. Production target resolution remains model-free.
    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    runtime.register_embedder(CapturingBackfillEmbedder(Arc::clone(&captured)));
    let mut report = EntityTypeBackfillReport {
        mode: "apply",
        namespace: "local".into(),
        source_revision: khive_runtime::BUILD_INFO.source_revision,
        loaded_packs: vec!["kg".into(), "git".into()],
        registry_types: BTreeMap::new(),
        target: PathBuf::from(":memory:"),
        backend: "main".into(),
        count_scope: "private injected-provider scan fixture",
        effective_limit: MAX_SCAN,
        scanned: 0,
        eligible: 0,
        promote: 0,
        echo: 0,
        untouched: 0,
        wrong_kind: 0,
        nullable_before: 2,
        nullable_after: None,
        projected_nullable_after: 0,
        after_count_basis: "not yet observed",
        promoted: 0,
        echo_removed: 0,
        embedding_truncation_report: EmbeddingTruncationReport::default(),
        complete: false,
        failures: Vec::new(),
    };
    scan(
        &runtime,
        &token,
        &types,
        args.apply.then_some(&runtime),
        &mut report,
        1,
    )
    .await
    .unwrap();
    assert_eq!(report.scanned, 2);
    assert_eq!(report.promoted, 2);
    assert!(report.complete);
    assert!(report.failures.is_empty());
    let first_text = format!("First {description}");
    assert_eq!(report.embedding_truncation_report.truncated, 1);
    assert_eq!(
        report.embedding_truncation_report.discarded_bytes,
        (first_text.len() - lattice_embed::MAX_TEXT_BYTES) as u64
    );
    let encoded = serde_json::to_value(&report).unwrap();
    assert_eq!(
        encoded["embedding_truncation_report"],
        json!({"truncated": 1, "discarded_bytes": 7})
    );
    assert_eq!(
        captured.lock().unwrap().as_slice(),
        &[
            first_text[..lattice_embed::MAX_TEXT_BYTES].to_string(),
            "Second short description".to_string()
        ]
    );
    for id in [first, second] {
        assert_eq!(
            runtime
                .get_entity(&token, id)
                .await
                .unwrap()
                .entity_type
                .as_deref(),
            Some("paper")
        );
    }
    assert_eq!(
        runtime
            .get_entity(&token, first)
            .await
            .unwrap()
            .description
            .as_deref(),
        Some(description.as_str())
    );
}

#[test]
fn classifier_uses_composed_types_and_preserves_raw_promotion_for_validation() {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: Vec::new(),
        packs: vec!["kg".into(), "git".into()],
        ..RuntimeConfig::default()
    })
    .unwrap();
    let (_registry, types) = compose_registry(&runtime).unwrap();
    for (kind, raw, canonical) in [
        ("document", "paper", "paper"),
        ("concept", " ALGO ", "algorithm"),
        ("document", " Architecture--Decision__Record ", "adr"),
    ] {
        let entity = Entity::new("local", kind, "fixture").with_properties(json!({"type": raw}));
        assert!(
            matches!(classify(&entity, &types), Classification::Promote(value) if value == raw)
        );
        assert_eq!(
            types
                .resolve(kind.parse().unwrap(), Some(raw))
                .unwrap()
                .entity_type
                .as_deref(),
            Some(canonical)
        );
    }
    let echo =
        Entity::new("local", "concept", "echo").with_properties(json!({"type": " __CoNcEpT-- "}));
    assert!(matches!(classify(&echo, &types), Classification::Echo));
    let wrong =
        Entity::new("local", "concept", "wrong").with_properties(json!({"type": "Article"}));
    assert!(matches!(
        classify(&wrong, &types),
        Classification::Untouched { wrong_kind: true }
    ));
    let unknown = Entity::new("local", "document", "unknown")
        .with_properties(json!({"type": "unregistered-backfill-test"}));
    assert!(matches!(
        classify(&unknown, &types),
        Classification::Untouched { wrong_kind: false }
    ));
    for properties in [
        None,
        Some(json!({"type": null})),
        Some(json!({"type": 7})),
        Some(json!({"type": ["paper"]})),
    ] {
        let mut entity = Entity::new("local", "document", "ineligible");
        entity.properties = properties;
        assert!(matches!(
            classify(&entity, &types),
            Classification::Ineligible
        ));
    }
    let typed = Entity::new("local", "document", "typed")
        .with_entity_type(Some("report"))
        .with_properties(json!({"type": "paper"}));
    assert!(matches!(
        classify(&typed, &types),
        Classification::Ineligible
    ));
    let mut deleted =
        Entity::new("local", "document", "deleted").with_properties(json!({"type": "paper"}));
    deleted.deleted_at = Some(1);
    assert!(matches!(
        classify(&deleted, &types),
        Classification::Ineligible
    ));
}

/// #2943: `compose_registry` must install the same entity-kind hooks the
/// MCP boot paths install, because `scan`'s `--apply` mode writes through
/// this exact composed registry's runtime (`compose_registry(&runtime)`
/// in `backfill_resolved`, then `runtime.update_entity_if_unchanged`
/// inside `scan`, sharing the same `Arc`-backed runtime instance). Proves
/// the install by exercising a real guarded update end to end rather than
/// inspecting the aggregate alone — a hook set that is computed but never
/// installed onto `runtime` would pass a weaker, aggregate-only check
/// silently.
///
/// Mutation prediction: deleting the
/// `runtime.install_entity_kind_hooks(registry.entity_kind_hooks());`
/// line from `compose_registry` makes the refusal arm below fail — the
/// workspace entity's missing `schema_version` update succeeds instead
/// of being refused, and `expect_err` panics.
#[tokio::test]
async fn compose_registry_installs_entity_kind_hooks_for_backfill_writes() {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: Vec::new(),
        packs: vec![
            "kg".into(),
            "git".into(),
            "gtd".into(),
            "session".into(),
            "workspace".into(),
        ],
        ..RuntimeConfig::default()
    })
    .unwrap();
    let (_registry, _types) = compose_registry(&runtime).unwrap();
    // NamespaceToken::local() is pub(crate) to khive-runtime by design: its own doc
    // says external callers mint through KhiveRuntime::authorize. Going through
    // authorize is also the stronger arm -- it exercises the gate check and actor
    // resolution this backfill actually runs under. Matches lines 178 and 273 above.
    let tok = runtime.authorize(Namespace::local()).unwrap();

    let invalid = runtime
        .create_entity_with_embedding_report(
            &tok,
            "workspace",
            None,
            "Backfill Workspace",
            None,
            None,
            vec![],
        )
        .await
        .map(|(row, _report)| row)
        .unwrap();
    let error = runtime
        .update_entity_with_embedding_report(
            &tok,
            invalid.id,
            EntityPatch {
                name: Some("Renamed Backfill Workspace".to_string()),
                ..Default::default()
            },
        )
        .await
        .map(|(row, _report)| row)
        .expect_err(
            "compose_registry must install the workspace KindHook onto this runtime, \
                 refusing an update that leaves properties.schema_version missing",
        );
    assert!(
        matches!(error, khive_runtime::RuntimeError::InvalidInput(ref msg) if msg.contains("schema_version")),
        "unexpected error: {error:?}"
    );

    let valid = runtime
        .create_entity_with_embedding_report(
            &tok,
            "workspace",
            None,
            "Valid Backfill Workspace",
            None,
            Some(json!({"schema_version": 1})),
            vec![],
        )
        .await
        .map(|(row, _report)| row)
        .unwrap();
    let updated = runtime
        .update_entity_with_embedding_report(
            &tok,
            valid.id,
            EntityPatch {
                name: Some("Renamed Valid Workspace".to_string()),
                ..Default::default()
            },
        )
        .await
        .map(|(row, _report)| row)
        .expect("a workspace entity carrying a valid schema_version must update freely");
    assert_eq!(updated.name, "Renamed Valid Workspace");
}

#[tokio::test]
async fn nullable_census_counts_only_live_null_types_in_the_exact_namespace() {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    runtime.backend().entities().unwrap();
    {
        let writer = runtime.backend().pool().writer().unwrap();
        writer
            .autocommit(|conn| {
                for (index, namespace, entity_type, deleted_at) in [
                    (1, "census", None, None),
                    (2, "census", None, None),
                    (3, "census", Some("paper"), None),
                    (4, "census", Some("NULL"), None),
                    (5, "other", None, None),
                    (6, "census", None, Some(2_i64)),
                ] {
                    conn.execute(
                        "INSERT INTO entities \
                     (id, namespace, kind, entity_type, name, created_at, updated_at, deleted_at) \
                     VALUES (?1, ?2, 'document', ?3, 'census fixture', 1, 1, ?4)",
                        (
                            Uuid::from_u128(index).to_string(),
                            namespace,
                            entity_type,
                            deleted_at,
                        ),
                    )
                    .unwrap();
                }
                Ok(())
            })
            .unwrap();
    }

    assert_eq!(nullable_count(&runtime, "census").await.unwrap(), 2);
    assert_eq!(nullable_count(&runtime, "other").await.unwrap(), 1);
    assert_eq!(nullable_count(&runtime, "missing").await.unwrap(), 0);

    runtime
        .backend()
        .pool()
        .writer()
        .unwrap()
        .autocommit(|conn| {
            Ok(conn.execute(
                "UPDATE entities SET entity_type = 'paper', version = version + 1 \
             WHERE namespace = 'census' AND entity_type IS NULL AND deleted_at IS NULL",
                [],
            )?)
        })
        .unwrap();
    assert_eq!(nullable_count(&runtime, "census").await.unwrap(), 0);
}

#[test]
fn nullable_census_distinguishes_unavailable_reporting_from_an_absent_null_group() {
    let error = nullable_count_from_groups(None).unwrap_err();
    assert_eq!(
        error.to_string(),
        "entity-type-backfill entity type census is unavailable"
    );
    assert_eq!(nullable_count_from_groups(Some(Vec::new())).unwrap(), 0);
    assert_eq!(
        nullable_count_from_groups(Some(vec![(Some("NULL".into()), 7)])).unwrap(),
        0
    );
}

#[tokio::test]
async fn nullable_census_propagates_storage_failure_instead_of_reporting_zero() {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    assert_eq!(nullable_count(&runtime, "local").await.unwrap(), 0);
    runtime
        .backend()
        .pool()
        .writer()
        .unwrap()
        .autocommit(|conn| Ok(conn.execute("DROP TABLE entities", [])?))
        .unwrap();

    let error = nullable_count(&runtime, "local").await.unwrap_err();
    assert!(error
        .downcast_ref::<khive_storage::StorageError>()
        .is_some());
}
