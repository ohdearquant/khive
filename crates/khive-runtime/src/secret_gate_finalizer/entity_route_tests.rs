//! Real entity routes for the test-only, empty-production manifest mechanism.

use khive_gate::ActorRef;
use khive_storage::{Entity, Event, EventFilter, PageRequest, SqlStatement, SqlValue};
use khive_types::{EventKind, Namespace, SubstrateKind};
use serde_json::{json, Value};
use uuid::Uuid;

use super::entity_admission::fixture;
use super::faults;
use super::manifest::fixture::TestOnlyManifestFixture;
use super::manifest::{digest_to_hex, scoped_digest, RuntimeFieldScope};
use super::matrix::{generated_wired_acceptance_matrix, MatrixCaseKind};
use super::outcome::{FailureClass, FinalizerOutcome, ManifestFault};
use crate::curation::EntityPatch;
use crate::operations::EntityClaimSpec;
use crate::{
    EntityCandidateAdmission, EntityCandidateContext, EntityCandidateMutation,
    EntityCandidateOrigin, EntityCreateSpec, KhiveRuntime, NamespaceToken, RuntimeConfig,
    RuntimeResult,
};

const STAMP: &str = "exempted:content-sha256-manifest-v1";

struct Fixture {
    runtime: KhiveRuntime,
    token: NamespaceToken,
    value: String,
}

impl Fixture {
    fn new() -> Self {
        let namespace = Namespace::parse(&format!("manifest-{}", Uuid::new_v4())).unwrap();
        let runtime = KhiveRuntime::new_for_test(RuntimeConfig {
            db_path: None,
            default_namespace: namespace.clone(),
            brain_profile: None,
            actor_id: None,
            credentials: vec![],
            mounts: vec![],
            events_split: None,
            wal_ceiling_bytes: 0,
            wal_ceiling_configured_bytes: 0,
            wal_ceiling_source: crate::config::WalCeilingSource::Default,
            wal_ceiling_env_raw: None,
            disk_guard_environment: khive_db::DiskGuardEnvironment::default(),
            disk_guard_config: None,
            volume_lock_dir: None,
            ..RuntimeConfig::no_embeddings()
        })
        .unwrap();
        assert!(runtime.registered_embedding_model_names().is_empty());
        let token = NamespaceToken::mint_authorized(namespace, ActorRef::new("agent", "fixture"));
        let value = TestOnlyManifestFixture::new().exact_value().to_owned();
        assert!(crate::secret_gate::check(&value).is_err());
        Self {
            runtime,
            token,
            value,
        }
    }

    fn ns(&self) -> &str {
        self.token.namespace().as_str()
    }

    fn install(&self, scope: RuntimeFieldScope) -> impl Drop {
        fixture::install(
            self.ns(),
            TestOnlyManifestFixture::for_scope(scope).snapshot(),
        )
    }

    fn spec(&self, name: &str, description: Option<&str>) -> EntityCreateSpec {
        EntityCreateSpec {
            kind: "concept".into(),
            entity_type: None,
            name: name.into(),
            description: description.map(str::to_owned),
            properties: Some(json!({"purpose": "fixture"})),
            tags: vec![],
        }
    }

    async fn create(
        &self,
        name: &str,
        description: Option<&str>,
        properties: Option<Value>,
    ) -> RuntimeResult<Entity> {
        self.runtime
            .create_entity_with_post_commit_report(
                &self.token,
                "concept",
                None,
                name,
                description,
                properties,
                vec![],
            )
            .await
            .map(|(entity, _, _)| entity)
    }

    async fn count(&self, table: &str) -> u64 {
        self.runtime
            .sql()
            .reader()
            .await
            .unwrap()
            .count(SqlStatement::new(
                format!("SELECT COUNT(*) FROM {table} WHERE namespace = ?1"),
                vec![SqlValue::Text(self.ns().into())],
            ))
            .await
            .unwrap()
    }

    async fn exemptions(&self) -> Vec<Event> {
        self.runtime
            .events(&self.token)
            .unwrap()
            .query_events(
                EventFilter {
                    kinds: vec![EventKind::Audit],
                    ..Default::default()
                }
                .payload_eq(
                    "$.mechanism",
                    SqlValue::Text("content-sha256-manifest-v1".into()),
                )
                .payload_eq("$.outcome", SqlValue::Text("exempted".into())),
                PageRequest {
                    offset: 0,
                    limit: 100,
                },
            )
            .await
            .unwrap()
            .items
    }

    async fn assert_empty(&self) {
        assert_eq!(self.count("entities").await, 0);
        assert_eq!(self.count("fts_entities").await, 0);
        assert_eq!(self.count("fts_entities_rowids").await, 0);
        assert!(self.exemptions().await.is_empty());
    }

    async fn assert_admitted(
        &self,
        entity: &Entity,
        verb: &str,
        scope: RuntimeFieldScope,
        family: &str,
    ) {
        let stored = self
            .runtime
            .get_entity(&self.token, entity.id)
            .await
            .unwrap();
        assert_eq!(stored.description, entity.description);
        assert_eq!(
            stored.properties.as_ref().unwrap()["khive:secret_gate"],
            STAMP
        );
        assert_eq!(stored.properties.as_ref().unwrap()["purpose"], "fixture");
        let fts_count = self
            .runtime
            .sql()
            .reader()
            .await
            .unwrap()
            .count(SqlStatement::new(
                "SELECT COUNT(*) FROM fts_entities WHERE namespace = ?1 AND subject_id = ?2",
                vec![
                    SqlValue::Text(self.ns().into()),
                    SqlValue::Text(entity.id.to_string()),
                ],
            ))
            .await
            .unwrap();
        assert_eq!(fts_count, 1);
        let events: Vec<_> = self
            .exemptions()
            .await
            .into_iter()
            .filter(|event| event.target_id == Some(entity.id))
            .collect();
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.namespace, self.ns());
        assert_eq!(event.actor, "agent:fixture");
        assert_eq!(event.verb, verb);
        assert_eq!(event.substrate, SubstrateKind::Entity);
        let field_scope = match scope {
            RuntimeFieldScope::NameDescription => "name-description",
            RuntimeFieldScope::JsonProperties => "json-properties",
            RuntimeFieldScope::Tags => "tags",
            _ => panic!("entity fixture scope"),
        };
        assert_eq!(
            event.payload,
            json!({
                "mechanism": "content-sha256-manifest-v1",
                "digest_sha256": digest_to_hex(&scoped_digest(scope, &self.value)),
                "field_scope": field_scope,
                "manifest_id": TestOnlyManifestFixture::for_scope(scope).snapshot().manifest_id(),
                "canonical_verb": verb,
                "actor": "agent:fixture",
                "namespace": self.ns(),
                "overridden_detector": "aws-access-key-id",
                "outcome": "exempted",
                "record_id": entity.id,
                "entry_point": family,
            })
        );
        assert!(!event.payload.to_string().contains(&self.value));
    }
}

#[tokio::test]
async fn declared_entity_routes_persist_stamp_and_one_exact_audit() {
    let rows = generated_wired_acceptance_matrix();
    let families: Vec<_> = rows
        .iter()
        .filter(|row| row.case == MatrixCaseKind::FixtureMatch)
        .map(|row| row.entry_point.id)
        .collect();
    assert_eq!(
        families,
        vec!["entity.create", "entity.update", "entity.bulk"]
    );
    for family in families {
        let f = Fixture::new();
        let _manifest = f.install(RuntimeFieldScope::NameDescription);
        let (entity, verb) = match family {
            "entity.create" => (
                f.create(
                    "created",
                    Some(&f.value),
                    Some(json!({"purpose": "fixture"})),
                )
                .await
                .unwrap(),
                "create",
            ),
            "entity.update" => {
                let original = f
                    .create(
                        "updated",
                        Some("clean"),
                        Some(json!({"purpose": "fixture"})),
                    )
                    .await
                    .unwrap();
                let (entity, _) = f
                    .runtime
                    .update_entity_with_expected_version_and_embedding_report(
                        &f.token,
                        original.id,
                        EntityPatch {
                            description: Some(Some(f.value.clone())),
                            ..Default::default()
                        },
                        Some(original.version),
                    )
                    .await
                    .unwrap();
                assert_eq!(entity.version, original.version + 1);
                (entity, "update")
            }
            "entity.bulk" => {
                let mut entities = f
                    .runtime
                    .create_many(
                        &f.token,
                        vec![f.spec("bulk", Some(&f.value)), f.spec("ordinary", None)],
                    )
                    .await
                    .unwrap();
                assert_eq!(entities.len(), 2);
                assert!(entities[1]
                    .properties
                    .as_ref()
                    .unwrap()
                    .get("khive:secret_gate")
                    .is_none());
                (entities.remove(0), "create")
            }
            unexpected => panic!("new entity family needs a real route: {unexpected}"),
        };
        assert_eq!(entity.description.as_deref(), Some(f.value.as_str()));
        f.assert_admitted(&entity, verb, RuntimeFieldScope::NameDescription, family)
            .await;
        assert!(fixture::outcomes(f.ns()).iter().any(|outcome| matches!(outcome, FinalizerOutcome::Exempted(commit) if commit.record_id == entity.id && commit.entry_point == family)));
    }
}

#[tokio::test]
async fn claim_route_admits_once_and_preserves_a_competing_identity() {
    let f = Fixture::new();
    let _manifest = f.install(RuntimeFieldScope::NameDescription);
    let id = Uuid::new_v4();
    let spec = || EntityClaimSpec {
        id,
        kind: "concept".into(),
        entity_type: None,
        name: "claimed".into(),
        description: Some(f.value.clone()),
        properties: Some(json!({"purpose": "fixture"})),
        tags: vec!["identity".into()],
        identity_tag: "identity".into(),
    };
    let (first, inserted) = f
        .runtime
        .claim_entity_if_absent(&f.token, spec())
        .await
        .unwrap();
    assert!(inserted);
    f.assert_admitted(
        &first,
        "create",
        RuntimeFieldScope::NameDescription,
        "entity.create",
    )
    .await;
    let (again, inserted) = f
        .runtime
        .claim_entity_if_absent(&f.token, spec())
        .await
        .unwrap();
    assert!(!inserted);
    assert_eq!(
        serde_json::to_value(again).unwrap(),
        serde_json::to_value(&first).unwrap()
    );
    assert_eq!(f.exemptions().await.len(), 1);
    let mut other = spec();
    other.name = "another identity".into();
    assert!(f
        .runtime
        .claim_entity_if_absent(&f.token, other)
        .await
        .is_err());
    assert_eq!(f.exemptions().await.len(), 1);
    assert_eq!(f.count("entities").await, 1);
}

#[tokio::test]
async fn empty_manifest_keeps_clean_writes_and_legacy_refusals() {
    let f = Fixture::new();
    assert!(f.create("blocked", Some(&f.value), None).await.is_err());
    f.assert_empty().await;
    let ordinary = f
        .create(
            "ordinary",
            Some("clean"),
            Some(json!({"purpose": "fixture"})),
        )
        .await
        .unwrap();
    assert!(ordinary
        .properties
        .as_ref()
        .unwrap()
        .get("khive:secret_gate")
        .is_none());
    assert!(f.exemptions().await.is_empty());
    assert!(f
        .runtime
        .create_many(&f.token, vec![])
        .await
        .unwrap()
        .is_empty());
    assert_eq!(f.count("entities").await, 1);
    assert!(f
        .runtime
        .create_many(&f.token, vec![f.spec("legacy refused", Some(&f.value))])
        .await
        .is_err());
    assert_eq!(f.count("entities").await, 1);
    let clean_bulk = f
        .runtime
        .create_many(&f.token, vec![f.spec("legacy bulk", None)])
        .await
        .unwrap();
    assert_eq!(clean_bulk.len(), 1);
    assert!(clean_bulk[0]
        .properties
        .as_ref()
        .unwrap()
        .get("khive:secret_gate")
        .is_none());
}

#[tokio::test]
async fn one_byte_wrong_scope_and_second_secret_still_refuse() {
    for case in ["one byte", "wrong scope", "second secret"] {
        let f = Fixture::new();
        let _manifest = f.install(RuntimeFieldScope::NameDescription);
        let mut mutated = f.value.clone();
        mutated.pop();
        mutated.push('1');
        assert!(crate::secret_gate::check(&mutated).is_err());
        let (description, properties) = match case {
            "one byte" => (Some(mutated.clone()), None),
            "wrong scope" => (None, Some(json!({"credential": f.value}))),
            _ => (Some(f.value.clone()), Some(json!({"credential": mutated}))),
        };
        assert!(
            f.create("refused", description.as_deref(), properties)
                .await
                .is_err(),
            "{case}"
        );
        f.assert_empty().await;
    }
}

#[tokio::test]
async fn caller_stamps_are_refused_before_matching_content() {
    for value in [Value::Null, json!(STAMP), json!("forged"), json!({})] {
        let f = Fixture::new();
        let _manifest = f.install(RuntimeFieldScope::NameDescription);
        let error = f
            .create(
                "forged",
                Some(&f.value),
                Some(json!({"khive:secret_gate": value})),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("khive:secret_gate"));
        f.assert_empty().await;
    }
}

#[tokio::test]
async fn real_record_stamp_and_audit_failures_leave_no_state() {
    for class in [
        FailureClass::RecordWrite,
        FailureClass::Stamp,
        FailureClass::SuccessAudit,
    ] {
        for failure_audit_fails in [false, true] {
            let f = Fixture::new();
            let _manifest = f.install(RuntimeFieldScope::NameDescription);
            let _primary = match class {
                FailureClass::RecordWrite => faults::arm_record_write_fail(f.ns()),
                FailureClass::Stamp => faults::arm_stamp_fail(f.ns()),
                FailureClass::SuccessAudit => faults::arm_success_audit_fail(f.ns()),
            };
            let _secondary = failure_audit_fails.then(|| faults::arm_failure_audit_fail(f.ns()));
            assert!(f
                .create(
                    "rolled back",
                    Some(&f.value),
                    Some(json!({"purpose": "fixture"}))
                )
                .await
                .is_err());
            f.assert_empty().await;
            let outcomes = fixture::outcomes(f.ns());
            assert_eq!(outcomes.len(), 1);
            let diagnostic = match (&outcomes[0], class) {
                (FinalizerOutcome::RecordWriteFailed(d), FailureClass::RecordWrite)
                | (FinalizerOutcome::StampFailed(d), FailureClass::Stamp)
                | (FinalizerOutcome::AuditFailed(d), FailureClass::SuccessAudit) => d,
                other => panic!("wrong typed failure: {other:?}"),
            };
            assert_eq!(diagnostic.namespace, f.ns());
            assert_eq!(diagnostic.entry_point, "entity.create");
            let gaps = fixture::audit_gaps(f.ns());
            assert_eq!(gaps.len(), usize::from(failure_audit_fails));
            if let Some(gap) = gaps.first() {
                assert_eq!(gap.failure_class, class);
                assert_eq!(gap.diagnostic_id, diagnostic.diagnostic_id);
                assert_eq!(gap.record_id, diagnostic.record_id);
                assert!(!format!("{gap:?}").contains(&f.value));
            }
            // The one-shot fault is consumed; the same actual route now commits.
            let entity = f
                .create(
                    "retried",
                    Some(&f.value),
                    Some(json!({"purpose": "fixture"})),
                )
                .await
                .unwrap();
            f.assert_admitted(
                &entity,
                "create",
                RuntimeFieldScope::NameDescription,
                "entity.create",
            )
            .await;
        }
    }
}

#[tokio::test]
async fn manifest_invalid_is_typed_and_does_not_write() {
    let f = Fixture::new();
    let _manifest = f.install(RuntimeFieldScope::NameDescription);
    let _fault = faults::arm_manifest_invalid(f.ns(), ManifestFault::UnknownSchemaVersion);
    assert!(f
        .create("invalid manifest", Some(&f.value), None)
        .await
        .is_err());
    f.assert_empty().await;
    assert!(matches!(
        fixture::outcomes(f.ns()).as_slice(),
        [FinalizerOutcome::ManifestInvalid(
            ManifestFault::UnknownSchemaVersion
        )]
    ));
}

#[tokio::test]
async fn later_bulk_failure_rolls_back_the_preceding_clean_entity() {
    let f = Fixture::new();
    let _manifest = f.install(RuntimeFieldScope::NameDescription);
    let _fault = faults::arm_success_audit_fail(f.ns());
    let error = f
        .runtime
        .create_many(
            &f.token,
            vec![
                f.spec("first clean", None),
                f.spec("second matched", Some(&f.value)),
            ],
        )
        .await;
    assert!(error.is_err());
    f.assert_empty().await;
    assert!(fixture::outcomes(f.ns()).iter().any(|outcome| matches!(outcome, FinalizerOutcome::AuditFailed(d) if d.entry_point == "entity.bulk")));
}

#[tokio::test]
async fn update_audit_failure_and_stale_version_preserve_the_old_row() {
    let f = Fixture::new();
    let _manifest = f.install(RuntimeFieldScope::NameDescription);
    let original = f
        .create(
            "original",
            Some("old body"),
            Some(json!({"purpose": "fixture"})),
        )
        .await
        .unwrap();
    let before = serde_json::to_value(&original).unwrap();
    let patch = || EntityPatch {
        description: Some(Some(f.value.clone())),
        ..Default::default()
    };
    let _fault = faults::arm_success_audit_fail(f.ns());
    assert!(f
        .runtime
        .update_entity_with_expected_version_and_embedding_report(
            &f.token,
            original.id,
            patch(),
            Some(original.version)
        )
        .await
        .is_err());
    assert_eq!(
        serde_json::to_value(f.runtime.get_entity(&f.token, original.id).await.unwrap()).unwrap(),
        before
    );
    assert!(f.exemptions().await.is_empty());
    assert_eq!(f.count("fts_entities").await, 1);
    assert!(f
        .runtime
        .update_entity_with_expected_version_and_embedding_report(
            &f.token,
            original.id,
            patch(),
            Some(original.version + 1)
        )
        .await
        .is_err());
    assert_eq!(
        serde_json::to_value(f.runtime.get_entity(&f.token, original.id).await.unwrap()).unwrap(),
        before
    );
    assert!(f.exemptions().await.is_empty());
    let (updated, _) = f
        .runtime
        .update_entity_with_expected_version_and_embedding_report(
            &f.token,
            original.id,
            patch(),
            Some(original.version),
        )
        .await
        .unwrap();
    assert_eq!(updated.version, original.version + 1);
    f.assert_admitted(
        &updated,
        "update",
        RuntimeFieldScope::NameDescription,
        "entity.update",
    )
    .await;
}

#[tokio::test]
async fn excluded_atomic_prepare_and_admin_update_keep_legacy_refusal() {
    let f = Fixture::new();
    let _manifest = f.install(RuntimeFieldScope::NameDescription);
    let ordinary = f
        .create("existing", None, Some(json!({"purpose": "fixture"})))
        .await
        .unwrap();
    let before = serde_json::to_value(&ordinary).unwrap();
    assert!(crate::atomic_prepare::prepare_update_entity_plan(
        &f.runtime,
        &f.token,
        ordinary.id,
        EntityPatch {
            description: Some(Some(f.value.clone())),
            ..Default::default()
        }
    )
    .await
    .is_err());
    assert!(f
        .runtime
        .update_entity_if_unchanged(
            &f.token,
            &ordinary,
            EntityPatch {
                description: Some(Some(f.value.clone())),
                ..Default::default()
            },
            &[]
        )
        .await
        .is_err());
    assert_eq!(
        serde_json::to_value(f.runtime.get_entity(&f.token, ordinary.id).await.unwrap()).unwrap(),
        before
    );
    assert!(f.exemptions().await.is_empty());
}

#[tokio::test]
async fn structured_properties_and_tags_use_their_exact_field_scope() {
    for scope in [RuntimeFieldScope::JsonProperties, RuntimeFieldScope::Tags] {
        let f = Fixture::new();
        let _manifest = f.install(scope);
        let properties = if scope == RuntimeFieldScope::JsonProperties {
            json!({"purpose": "fixture", "nested": {"credential": f.value}})
        } else {
            json!({"purpose": "fixture"})
        };
        let tags = if scope == RuntimeFieldScope::Tags {
            vec![f.value.clone()]
        } else {
            vec![]
        };
        let (entity, _, _) = f
            .runtime
            .create_entity_with_post_commit_report(
                &f.token,
                "concept",
                None,
                "scoped",
                None,
                Some(properties),
                tags,
            )
            .await
            .unwrap();
        f.assert_admitted(&entity, "create", scope, "entity.create")
            .await;
    }
}

fn direct_candidate(f: &Fixture, name: &str) -> Entity {
    Entity::new(f.ns(), "concept", name)
        .with_description(&f.value)
        .with_properties(json!({"purpose": "fixture"}))
}

#[tokio::test]
async fn direct_ingest_origins_commit_through_the_shared_constructor() {
    for (origin, verb) in [
        (EntityCandidateOrigin::CodeIngest, "code.ingest"),
        (
            EntityCandidateOrigin::CodeFindingsIngest,
            "code.findings_ingest",
        ),
    ] {
        let f = Fixture::new();
        let _manifest = f.install(RuntimeFieldScope::NameDescription);
        let candidate = direct_candidate(&f, "direct");
        let id = candidate.id;
        let prepared = EntityCandidateContext::new(f.ns(), origin)
            .prepare(candidate)
            .unwrap();
        let outcome = f
            .runtime
            .try_commit_manifest_entity_candidate(
                &f.token,
                prepared,
                EntityCandidateMutation::CreateIfAbsent,
            )
            .await
            .unwrap();
        let EntityCandidateAdmission::Committed(entity) = outcome else {
            panic!("matching direct candidate did not commit")
        };
        assert_eq!(entity.id, id);
        f.assert_admitted(
            &entity,
            verb,
            RuntimeFieldScope::NameDescription,
            "entity.create",
        )
        .await;

        let duplicate = EntityCandidateContext::new(f.ns(), origin)
            .prepare(direct_candidate(&f, "unused"))
            .unwrap();
        let wrong_token = f
            .token
            .with_namespace(Namespace::parse("unrelated").unwrap());
        assert!(f
            .runtime
            .try_commit_manifest_entity_candidate(
                &wrong_token,
                duplicate,
                EntityCandidateMutation::CreateIfAbsent
            )
            .await
            .is_err());
        assert_eq!(f.count("entities").await, 1);
        assert_eq!(f.exemptions().await.len(), 1);
    }
}

#[tokio::test]
async fn direct_conditional_insert_and_replacement_preserve_rival_rows() {
    let f = Fixture::new();
    let _manifest = f.install(RuntimeFieldScope::NameDescription);
    let old = f
        .create("rival", Some("old"), Some(json!({"purpose": "fixture"})))
        .await
        .unwrap();
    let mut candidate = old.clone();
    candidate.description = Some(f.value.clone());
    candidate.updated_at += 1;
    let context = EntityCandidateContext::new(f.ns(), EntityCandidateOrigin::CodeIngest);
    let prepared = context.clone().prepare(candidate.clone()).unwrap();
    assert!(matches!(
        f.runtime
            .try_commit_manifest_entity_candidate(
                &f.token,
                prepared,
                EntityCandidateMutation::CreateIfAbsent,
            )
            .await
            .unwrap(),
        EntityCandidateAdmission::Conflict
    ));
    assert!(f.exemptions().await.is_empty());

    let prepared = context.clone().prepare(candidate).unwrap();
    let (rival, _) = f
        .runtime
        .update_entity_with_embedding_report(
            &f.token,
            old.id,
            EntityPatch {
                description: Some(Some("rival body".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        f.runtime
            .try_commit_manifest_entity_candidate(
                &f.token,
                prepared,
                EntityCandidateMutation::ReplaceIfUnchanged {
                    expected: old,
                    expected_version: None
                },
            )
            .await
            .unwrap(),
        EntityCandidateAdmission::Conflict
    ));
    assert_eq!(
        serde_json::to_value(f.runtime.get_entity(&f.token, rival.id).await.unwrap()).unwrap(),
        serde_json::to_value(&rival).unwrap()
    );
    assert!(f.exemptions().await.is_empty());

    let mut candidate = rival.clone();
    candidate.description = Some(f.value.clone());
    candidate.updated_at += 1;
    let prepared = context.prepare(candidate).unwrap();
    let outcome = f
        .runtime
        .try_commit_manifest_entity_candidate(
            &f.token,
            prepared,
            EntityCandidateMutation::ReplaceIfUnchanged {
                expected: rival.clone(),
                expected_version: Some(rival.version),
            },
        )
        .await
        .unwrap();
    let EntityCandidateAdmission::Committed(entity) = outcome else {
        panic!("rebased candidate did not commit")
    };
    assert_eq!(entity.version, rival.version + 1);
    f.assert_admitted(
        &entity,
        "code.ingest",
        RuntimeFieldScope::NameDescription,
        "entity.update",
    )
    .await;
}

#[tokio::test]
async fn direct_preflight_snapshot_survives_refresh_and_next_context_sees_empty() {
    let f = Fixture::new();
    let manifest = f.install(RuntimeFieldScope::NameDescription);
    let context = EntityCandidateContext::new(f.ns(), EntityCandidateOrigin::CodeIngest);
    context.check_name_description(&f.value).unwrap();
    drop(manifest);
    let _empty = fixture::install(f.ns(), super::manifest::ManifestSnapshot::empty());
    let prepared = context
        .prepare(direct_candidate(&f, "captured snapshot"))
        .unwrap();
    let outcome = f
        .runtime
        .try_commit_manifest_entity_candidate(
            &f.token,
            prepared,
            EntityCandidateMutation::CreateIfAbsent,
        )
        .await
        .unwrap();
    let EntityCandidateAdmission::Committed(entity) = outcome else {
        panic!("captured snapshot was replaced")
    };
    f.assert_admitted(
        &entity,
        "code.ingest",
        RuntimeFieldScope::NameDescription,
        "entity.create",
    )
    .await;
    assert!(
        EntityCandidateContext::new(f.ns(), EntityCandidateOrigin::CodeIngest)
            .prepare(direct_candidate(&f, "new snapshot"))
            .is_err()
    );

    let clean = Entity::new(f.ns(), "concept", "legacy direct");
    let prepared = EntityCandidateContext::new(f.ns(), EntityCandidateOrigin::CodeIngest)
        .prepare(clean.clone())
        .unwrap();
    let outcome = f
        .runtime
        .try_commit_manifest_entity_candidate(
            &f.token,
            prepared,
            EntityCandidateMutation::CreateIfAbsent,
        )
        .await
        .unwrap();
    assert!(
        matches!(outcome, EntityCandidateAdmission::Legacy(ref entity) if entity.id == clean.id)
    );
    assert!(f
        .runtime
        .entities(&f.token)
        .unwrap()
        .get_entity(clean.id)
        .await
        .unwrap()
        .is_none());
    assert_eq!(f.exemptions().await.len(), 1);
}

#[tokio::test]
async fn later_sql_audit_failure_rolls_back_two_prepared_admissions() {
    let f = Fixture::new();
    let _manifest = f.install(RuntimeFieldScope::NameDescription);
    let (first, first_plan) = f
        .runtime
        .prepare_bulk_entity_plan(&f.token, f.spec("first", Some(&f.value)))
        .await
        .unwrap();
    let (second, second_plan) = f
        .runtime
        .prepare_bulk_entity_plan(&f.token, f.spec("second", Some(&f.value)))
        .await
        .unwrap();
    let mut writer = f.runtime.sql().writer().await.unwrap();
    writer
        .execute_script(format!(
            "CREATE TRIGGER refuse_second_exemption BEFORE INSERT ON events \
         WHEN json_extract(NEW.payload, '$.outcome') = 'exempted' \
         AND json_extract(NEW.payload, '$.record_id') = '{}' \
         BEGIN SELECT RAISE(ABORT, 'refused fixture audit'); END;",
            second.id
        ))
        .await
        .unwrap();
    drop(writer);
    let result =
        crate::run_atomic_unit(f.runtime.sql().as_ref(), vec![first_plan, second_plan]).await;
    assert!(!matches!(
        result,
        Ok(crate::AtomicRunOutcome::Committed { .. })
    ));
    f.assert_empty().await;
    let outcomes = fixture::outcomes(f.ns());
    assert!(!outcomes
        .iter()
        .any(|outcome| matches!(outcome, FinalizerOutcome::Exempted(_))));
    assert!(outcomes.iter().any(
        |outcome| matches!(outcome, FinalizerOutcome::AuditFailed(d) if d.record_id == second.id)
    ));
    assert!(f
        .runtime
        .entities(&f.token)
        .unwrap()
        .get_entity(first.id)
        .await
        .unwrap()
        .is_none());
}

#[derive(Clone, Copy)]
enum OuterFault {
    Rollback,
    LostAcknowledgement,
}

struct FaultedAccess {
    inner: std::sync::Arc<dyn khive_storage::SqlAccess>,
    fault: OuterFault,
}

#[async_trait::async_trait]
impl khive_storage::SqlAccess for FaultedAccess {
    async fn reader(&self) -> khive_storage::StorageResult<Box<dyn khive_storage::SqlReader>> {
        self.inner.reader().await
    }

    async fn writer(&self) -> khive_storage::StorageResult<Box<dyn khive_storage::SqlWriter>> {
        self.inner.writer().await
    }

    async fn atomic_unit(
        &self,
        op: khive_storage::AtomicUnitOp,
    ) -> khive_storage::StorageResult<Box<dyn std::any::Any + Send>> {
        match self.fault {
            OuterFault::Rollback => {
                self.inner
                    .atomic_unit(Box::new(move |writer| {
                        Box::pin(async move {
                            let _completed_body = op(writer).await?;
                            Err(khive_storage::StorageError::Internal(
                                "fixture outer rollback".into(),
                            ))
                        })
                    }))
                    .await
            }
            OuterFault::LostAcknowledgement => {
                let _committed_body = self.inner.atomic_unit(op).await?;
                Err(khive_storage::StorageError::writer_task_terminated(
                    khive_storage::WriterTaskRequestState::SideEffectsUnknown,
                ))
            }
        }
    }
}

#[tokio::test]
async fn outer_rollback_and_lost_acknowledgement_never_claim_exemption() {
    for fault in [OuterFault::Rollback, OuterFault::LostAcknowledgement] {
        let f = Fixture::new();
        let _manifest = f.install(RuntimeFieldScope::NameDescription);
        let (entity, plan) = f
            .runtime
            .prepare_bulk_entity_plan(&f.token, f.spec("outer unit", Some(&f.value)))
            .await
            .unwrap();
        let access = FaultedAccess {
            inner: f.runtime.sql(),
            fault,
        };
        let error = crate::run_atomic_unit(&access, vec![plan])
            .await
            .unwrap_err();
        assert!(!fixture::outcomes(f.ns())
            .iter()
            .any(|outcome| matches!(outcome, FinalizerOutcome::Exempted(_))));
        match fault {
            OuterFault::Rollback => {
                assert!(matches!(
                    error.0,
                    khive_storage::StorageError::WriterTaskRequestFailed {
                        request_state: khive_storage::WriterTaskRequestState::TransactionRolledBack,
                        ..
                    }
                ));
                f.assert_empty().await;
                assert!(
                    matches!(fixture::outcomes(f.ns()).as_slice(), [FinalizerOutcome::RecordWriteFailed(d)] if d.record_id == entity.id)
                );
            }
            OuterFault::LostAcknowledgement => {
                assert!(matches!(
                    error.0,
                    khive_storage::StorageError::WriterTaskTerminated {
                        request_state: khive_storage::WriterTaskRequestState::SideEffectsUnknown,
                        ..
                    }
                ));
                // The real unit committed before acknowledgement was lost. It is
                // equally incorrect to claim rollback or confirmed exemption.
                f.assert_admitted(
                    &entity,
                    "create",
                    RuntimeFieldScope::NameDescription,
                    "entity.bulk",
                )
                .await;
                assert!(fixture::outcomes(f.ns()).is_empty());
                assert!(fixture::audit_gaps(f.ns()).is_empty());
            }
        }
    }
}

struct FixtureProvider;
struct FixtureEmbedding;

#[async_trait::async_trait]
impl crate::EmbedderProvider for FixtureProvider {
    fn name(&self) -> &str {
        "manifestfixture"
    }
    fn dimensions(&self) -> usize {
        4
    }
    async fn build(&self) -> RuntimeResult<std::sync::Arc<dyn lattice_embed::EmbeddingService>> {
        Ok(std::sync::Arc::new(FixtureEmbedding))
    }
}

#[async_trait::async_trait]
impl lattice_embed::EmbeddingService for FixtureEmbedding {
    async fn embed(
        &self,
        texts: &[String],
        _model: lattice_embed::EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
        Ok(texts.iter().map(|_| vec![0.25; 4]).collect())
    }
    fn supports_model(&self, _model: lattice_embed::EmbeddingModel) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "manifestfixture"
    }
}

#[tokio::test]
async fn success_audit_failure_rolls_back_attachments_vectors_and_provenance() {
    use khive_storage::BlobStore as _;
    let f = Fixture::new();
    let _manifest = f.install(RuntimeFieldScope::NameDescription);
    f.runtime.register_embedder(FixtureProvider);
    let directory = tempfile::tempdir().unwrap();
    let blob = std::sync::Arc::new(
        khive_db::stores::blob::FsBlobStore::new(directory.path().to_path_buf(), 0).unwrap(),
    );
    let content_ref = blob.put(b"fixture attachment".to_vec()).await.unwrap();
    f.runtime.install_blob_store(blob).unwrap();
    let attachment = khive_storage::NewAttachment {
        role: "content".into(),
        content_ref: content_ref.clone(),
        media_type: Some("text/plain".into()),
        size_bytes: Some(18),
    };
    let create = || {
        f.runtime.create_entity_with_attachments_and_report(
            &f.token,
            "concept",
            None,
            "attached",
            Some(&f.value),
            Some(json!({"purpose": "fixture"})),
            vec![],
            vec![attachment.clone()],
        )
    };
    let _fault = faults::arm_success_audit_fail(f.ns());
    assert!(create().await.is_err());
    f.assert_empty().await;
    for table in ["vec_manifestfixture", "vector_provenance", "ann_write_log"] {
        assert_eq!(f.count(table).await, 0, "rollback retained {table}");
    }
    let count = f
        .runtime
        .sql()
        .reader()
        .await
        .unwrap()
        .count(SqlStatement::new(
            "SELECT COUNT(*) FROM attachments",
            vec![],
        ))
        .await
        .unwrap();
    assert_eq!(count, 0);
    let (entity, _) = create().await.unwrap();
    f.assert_admitted(
        &entity,
        "create",
        RuntimeFieldScope::NameDescription,
        "entity.create",
    )
    .await;
    for table in ["vec_manifestfixture", "ann_write_log"] {
        assert_eq!(f.count(table).await, 1, "positive control omitted {table}");
    }
    // Raw atomic vector publication intentionally invalidates source-text provenance.
    assert_eq!(f.count("vector_provenance").await, 0);
    let attachments = f
        .runtime
        .attachments()
        .unwrap()
        .list_attachments(entity.id)
        .await
        .unwrap();
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0].content_ref, content_ref);
    assert_eq!(entity.content_ref.as_deref(), Some(content_ref.as_str()));
}

#[tokio::test]
async fn carried_reserved_property_cannot_be_laundered_by_an_admitted_update() {
    let f = Fixture::new();
    let _manifest = f.install(RuntimeFieldScope::NameDescription);
    let mut existing = Entity::new(f.ns(), "concept", "preexisting")
        .with_properties(json!({"purpose": "fixture", "khive:secret_gate": STAMP}));
    f.runtime
        .entities(&f.token)
        .unwrap()
        .upsert_entity(existing.clone())
        .await
        .unwrap();
    for properties in [None, Some(json!({"other": true}))] {
        assert!(f
            .runtime
            .update_entity_with_embedding_report(
                &f.token,
                existing.id,
                EntityPatch {
                    description: Some(Some(f.value.clone())),
                    properties,
                    ..Default::default()
                }
            )
            .await
            .is_err());
        assert_eq!(
            serde_json::to_value(f.runtime.get_entity(&f.token, existing.id).await.unwrap())
                .unwrap(),
            serde_json::to_value(&existing).unwrap()
        );
    }
    assert!(f.exemptions().await.is_empty());
    existing.properties = Some(json!({"purpose": "fixture"}));
    f.runtime
        .entities(&f.token)
        .unwrap()
        .upsert_entity(existing.clone())
        .await
        .unwrap();
    let (updated, _) = f
        .runtime
        .update_entity_with_embedding_report(
            &f.token,
            existing.id,
            EntityPatch {
                description: Some(Some(f.value.clone())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    f.assert_admitted(
        &updated,
        "update",
        RuntimeFieldScope::NameDescription,
        "entity.update",
    )
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn events_split_reads_the_success_audit_committed_on_the_main_unit() {
    let directory = tempfile::tempdir().unwrap();
    let _registry = crate::events_split::TestRegistryGuard::new(directory.path());
    let namespace = Namespace::parse(&format!("manifest-{}", Uuid::new_v4())).unwrap();
    let runtime = KhiveRuntime::new_for_test(RuntimeConfig {
        db_path: Some(directory.path().join("main.db")),
        default_namespace: namespace.clone(),
        brain_profile: None,
        actor_id: None,
        credentials: vec![],
        mounts: vec![],
        events_split: Some(crate::events_split::EventsSplitConfig {
            db_path: directory.path().join("events.db"),
            socket_path: None,
        }),
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: crate::config::WalCeilingSource::Default,
        wal_ceiling_env_raw: None,
        disk_guard_environment: khive_db::DiskGuardEnvironment::default(),
        disk_guard_config: None,
        volume_lock_dir: None,
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let f = Fixture {
        runtime,
        token: NamespaceToken::mint_authorized(namespace, ActorRef::new("agent", "fixture")),
        value: TestOnlyManifestFixture::new().exact_value().into(),
    };
    let _manifest = f.install(RuntimeFieldScope::NameDescription);
    let entity = f
        .create(
            "split event",
            Some(&f.value),
            Some(json!({"purpose": "fixture"})),
        )
        .await
        .unwrap();
    f.assert_admitted(
        &entity,
        "create",
        RuntimeFieldScope::NameDescription,
        "entity.create",
    )
    .await;
    let in_main = f.runtime.sql().reader().await.unwrap().count(SqlStatement::new(
        "SELECT COUNT(*) FROM events WHERE namespace = ?1 AND json_extract(payload, '$.outcome') = 'exempted' AND json_extract(payload, '$.record_id') = ?2",
        vec![SqlValue::Text(f.ns().into()), SqlValue::Text(entity.id.to_string())],
    )).await.unwrap();
    assert_eq!(in_main, 1);
}

#[tokio::test]
async fn empty_manifest_update_preserves_patch_scanning_and_nonempty_scans_final_state() {
    let f = Fixture::new();
    let existing = Entity::new(f.ns(), "concept", "legacy")
        .with_description(&f.value)
        .with_properties(json!({"purpose": "fixture"}));
    f.runtime
        .entities(&f.token)
        .unwrap()
        .upsert_entity(existing.clone())
        .await
        .unwrap();
    let (renamed, _) = f
        .runtime
        .update_entity_with_embedding_report(
            &f.token,
            existing.id,
            EntityPatch {
                name: Some("renamed without touching legacy description".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(renamed.description, existing.description);
    assert!(renamed
        .properties
        .as_ref()
        .unwrap()
        .get("khive:secret_gate")
        .is_none());
    assert!(f
        .runtime
        .update_entity_with_embedding_report(
            &f.token,
            renamed.id,
            EntityPatch {
                description: Some(Some(f.value.clone())),
                ..Default::default()
            }
        )
        .await
        .is_err());
    let error = f
        .runtime
        .update_entity_with_embedding_report(
            &f.token,
            Uuid::new_v4(),
            EntityPatch {
                description: Some(Some(f.value.clone())),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(error, crate::RuntimeError::SecretDetected(_)));
    let _manifest = f.install(RuntimeFieldScope::JsonProperties);
    // The description is not allowlisted by this snapshot. A harmless patch
    // must not convert an unmatched carried secret into an admitted record.
    assert!(f
        .runtime
        .update_entity_with_embedding_report(
            &f.token,
            renamed.id,
            EntityPatch {
                properties: Some(json!({"credential": f.value})),
                ..Default::default()
            }
        )
        .await
        .is_err());
    assert_eq!(
        serde_json::to_value(f.runtime.get_entity(&f.token, renamed.id).await.unwrap()).unwrap(),
        serde_json::to_value(renamed).unwrap()
    );
    assert!(f.exemptions().await.is_empty());
}

#[tokio::test]
async fn direct_cas_cannot_move_a_foreign_row_or_overflow_revision() {
    let f = Fixture::new();
    let _manifest = f.install(RuntimeFieldScope::NameDescription);
    let foreign_token = f
        .token
        .with_namespace(Namespace::parse(&format!("foreign-{}", Uuid::new_v4())).unwrap());
    let foreign = Entity::new(foreign_token.namespace().as_str(), "concept", "foreign")
        .with_properties(json!({"purpose": "fixture"}));
    f.runtime
        .entities(&foreign_token)
        .unwrap()
        .upsert_entity(foreign.clone())
        .await
        .unwrap();
    let mut forged = foreign.clone();
    forged.namespace = f.ns().into();
    let mut candidate = forged.clone();
    candidate.description = Some(f.value.clone());
    candidate.updated_at = foreign.updated_at.checked_add(1).unwrap();
    let prepared = EntityCandidateContext::new(f.ns(), EntityCandidateOrigin::CodeIngest)
        .prepare(candidate)
        .unwrap();
    let result = f
        .runtime
        .try_commit_manifest_entity_candidate(
            &f.token,
            prepared,
            EntityCandidateMutation::ReplaceIfUnchanged {
                expected: forged,
                expected_version: None,
            },
        )
        .await
        .unwrap();
    assert!(matches!(result, EntityCandidateAdmission::Conflict));
    assert_eq!(
        serde_json::to_value(
            f.runtime
                .get_entity(&foreign_token, foreign.id)
                .await
                .unwrap()
        )
        .unwrap(),
        serde_json::to_value(&foreign).unwrap()
    );
    f.assert_empty().await;

    let mut exhausted = direct_candidate(&f, "revision exhausted");
    exhausted.version = i64::MAX;
    let expected = exhausted.clone();
    let prepared = EntityCandidateContext::new(f.ns(), EntityCandidateOrigin::CodeIngest)
        .prepare(exhausted)
        .unwrap();
    let error = f
        .runtime
        .try_commit_manifest_entity_candidate(
            &f.token,
            prepared,
            EntityCandidateMutation::ReplaceIfUnchanged {
                expected,
                expected_version: None,
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, crate::RuntimeError::InvalidInput(ref message) if message == "entity version exhausted")
    );
    f.assert_empty().await;
}

async fn invoke_declared_route(
    f: &Fixture,
    family: &str,
    before: Option<&Entity>,
    spec: EntityCreateSpec,
) -> RuntimeResult<Entity> {
    match family {
        "entity.create" => f
            .runtime
            .create_entity_with_post_commit_report(
                &f.token,
                &spec.kind,
                spec.entity_type.as_deref(),
                &spec.name,
                spec.description.as_deref(),
                spec.properties,
                spec.tags,
            )
            .await
            .map(|(entity, _, _)| entity),
        "entity.update" => f
            .runtime
            .update_entity_with_expected_version_and_embedding_report(
                &f.token,
                before.unwrap().id,
                EntityPatch {
                    name: Some(spec.name),
                    description: Some(spec.description),
                    properties: spec.properties,
                    tags: Some(spec.tags),
                    ..Default::default()
                },
                Some(before.unwrap().version),
            )
            .await
            .map(|(entity, _)| entity),
        "entity.bulk" => f
            .runtime
            .create_many(&f.token, vec![spec])
            .await
            .map(|mut entities| entities.remove(0)),
        _ => panic!("new family requires an actual route"),
    }
}

async fn route_baseline(f: &Fixture, family: &str) -> Option<Entity> {
    if family == "entity.update" {
        Some(
            f.create("before", Some("clean"), Some(json!({"purpose": "fixture"})))
                .await
                .unwrap(),
        )
    } else {
        None
    }
}

async fn assert_route_unchanged(f: &Fixture, before: Option<&Entity>) {
    if let Some(before) = before {
        assert_eq!(
            serde_json::to_value(f.runtime.get_entity(&f.token, before.id).await.unwrap()).unwrap(),
            serde_json::to_value(before).unwrap()
        );
        for table in ["entities", "fts_entities", "fts_entities_rowids"] {
            assert_eq!(f.count(table).await, 1);
        }
        assert!(f.exemptions().await.is_empty());
    } else {
        f.assert_empty().await;
    }
}

#[tokio::test]
async fn generated_negative_candidates_refuse_on_every_wired_route() {
    for row in generated_wired_acceptance_matrix()
        .into_iter()
        .filter(|row| {
            matches!(
                row.case,
                MatrixCaseKind::OneByteMiss
                    | MatrixCaseKind::WrongScopeMiss
                    | MatrixCaseKind::ReservedKeyMutation
            )
        })
    {
        let f = Fixture::new();
        let _manifest = f.install(RuntimeFieldScope::NameDescription);
        let before = route_baseline(&f, row.entry_point.id).await;
        let mut spec = f.spec("attempted", Some(&f.value));
        match row.case {
            MatrixCaseKind::OneByteMiss => {
                let mut changed = f.value.clone();
                changed.pop();
                changed.push('1');
                assert!(crate::secret_gate::check(&changed).is_err());
                spec.description = Some(changed);
            }
            MatrixCaseKind::WrongScopeMiss => {
                spec.description = None;
                spec.properties = Some(json!({"purpose": "fixture", "credential": f.value}));
            }
            MatrixCaseKind::ReservedKeyMutation => {
                spec.properties = Some(json!({"purpose": "fixture", "khive:secret_gate": STAMP}));
            }
            _ => unreachable!(),
        }
        assert!(
            invoke_declared_route(&f, row.entry_point.id, before.as_ref(), spec)
                .await
                .is_err(),
            "{row:?}"
        );
        assert_route_unchanged(&f, before.as_ref()).await;
        let admitted = invoke_declared_route(
            &f,
            row.entry_point.id,
            before.as_ref(),
            f.spec("positive control", Some(&f.value)),
        )
        .await
        .unwrap();
        let verb = if row.entry_point.id == "entity.update" {
            "update"
        } else {
            "create"
        };
        f.assert_admitted(
            &admitted,
            verb,
            RuntimeFieldScope::NameDescription,
            row.entry_point.id,
        )
        .await;
    }
}

#[tokio::test]
async fn generated_faults_preserve_each_route_and_classify_failure_audit_loss() {
    let families: Vec<_> = generated_wired_acceptance_matrix()
        .into_iter()
        .filter(|row| row.case == MatrixCaseKind::FixtureMatch)
        .map(|row| row.entry_point.id)
        .collect();
    for family in families {
        for class in [
            FailureClass::RecordWrite,
            FailureClass::Stamp,
            FailureClass::SuccessAudit,
        ] {
            for secondary in [false, true] {
                let f = Fixture::new();
                let _manifest = f.install(RuntimeFieldScope::NameDescription);
                let before = route_baseline(&f, family).await;
                let _fault = match class {
                    FailureClass::RecordWrite => faults::arm_record_write_fail(f.ns()),
                    FailureClass::Stamp => faults::arm_stamp_fail(f.ns()),
                    FailureClass::SuccessAudit => faults::arm_success_audit_fail(f.ns()),
                };
                let _secondary = secondary.then(|| faults::arm_failure_audit_fail(f.ns()));
                assert!(invoke_declared_route(
                    &f,
                    family,
                    before.as_ref(),
                    f.spec("failed", Some(&f.value))
                )
                .await
                .is_err());
                assert_route_unchanged(&f, before.as_ref()).await;
                let outcomes = fixture::outcomes(f.ns());
                assert_eq!(outcomes.len(), 1);
                let diagnostic = match (&outcomes[0], class) {
                    (FinalizerOutcome::RecordWriteFailed(d), FailureClass::RecordWrite)
                    | (FinalizerOutcome::StampFailed(d), FailureClass::Stamp)
                    | (FinalizerOutcome::AuditFailed(d), FailureClass::SuccessAudit) => d,
                    other => panic!("wrong failure outcome: {other:?}"),
                };
                assert_eq!(diagnostic.entry_point, family);
                let gaps = fixture::audit_gaps(f.ns());
                assert_eq!(gaps.len(), usize::from(secondary));
                if secondary {
                    assert_eq!(gaps[0].diagnostic_id, diagnostic.diagnostic_id);
                    assert_eq!(gaps[0].failure_class, class);
                }
                let audit = f
                    .runtime
                    .events(&f.token)
                    .unwrap()
                    .query_events(
                        EventFilter {
                            kinds: vec![EventKind::Audit],
                            ..Default::default()
                        }
                        .payload_eq(
                            "$.diagnostic_id",
                            SqlValue::Text(diagnostic.diagnostic_id.to_string()),
                        ),
                        PageRequest {
                            offset: 0,
                            limit: 100,
                        },
                    )
                    .await
                    .unwrap()
                    .items;
                assert_eq!(audit.len(), usize::from(!secondary));
                if let Some(event) = audit.first() {
                    assert_eq!(event.actor, "agent:fixture");
                    assert_eq!(event.namespace, f.ns());
                    assert_eq!(event.target_id, Some(diagnostic.record_id));
                    assert_eq!(event.outcome, khive_types::EventOutcome::Error);
                    assert_eq!(event.payload["entry_point"], family);
                    assert_eq!(
                        event.payload["outcome"],
                        match class {
                            FailureClass::RecordWrite => "record-write-failed",
                            FailureClass::Stamp => "stamp-failed",
                            FailureClass::SuccessAudit => "audit-failed",
                        }
                    );
                    assert!(!event.payload.to_string().contains(&f.value));
                }
                let admitted = invoke_declared_route(
                    &f,
                    family,
                    before.as_ref(),
                    f.spec("retry", Some(&f.value)),
                )
                .await
                .unwrap();
                f.assert_admitted(
                    &admitted,
                    if family == "entity.update" {
                        "update"
                    } else {
                        "create"
                    },
                    RuntimeFieldScope::NameDescription,
                    family,
                )
                .await;
            }
        }
    }
}

#[tokio::test]
async fn prepared_bulk_uses_one_snapshot_across_manifest_refresh() {
    let f = Fixture::new();
    let installed = f.install(RuntimeFieldScope::NameDescription);
    let (entity, plan) = f
        .runtime
        .prepare_bulk_entity_plan(&f.token, f.spec("prepared", Some(&f.value)))
        .await
        .unwrap();
    drop(installed);
    let _empty = fixture::install(f.ns(), super::manifest::ManifestSnapshot::empty());
    assert!(matches!(
        crate::run_atomic_unit(f.runtime.sql().as_ref(), vec![plan])
            .await
            .unwrap(),
        crate::AtomicRunOutcome::Committed { .. }
    ));
    f.assert_admitted(
        &entity,
        "create",
        RuntimeFieldScope::NameDescription,
        "entity.bulk",
    )
    .await;
    assert!(f
        .runtime
        .prepare_bulk_entity_plan(&f.token, f.spec("next snapshot", Some(&f.value)))
        .await
        .is_err());
}

#[tokio::test]
async fn ignored_success_audit_rolls_back_and_ignored_failure_audit_emits_gap() {
    for ignore_failure in [false, true] {
        let f = Fixture::new();
        let _manifest = f.install(RuntimeFieldScope::NameDescription);
        let outcome = if ignore_failure {
            "stamp-failed"
        } else {
            "exempted"
        };
        f.runtime.sql().writer().await.unwrap().execute_script(format!(
            "CREATE TRIGGER ignore_finalizer_audit BEFORE INSERT ON events WHEN json_extract(NEW.payload, '$.outcome') = '{outcome}' BEGIN SELECT RAISE(IGNORE); END;"
        )).await.unwrap();
        let _fault = ignore_failure.then(|| faults::arm_stamp_fail(f.ns()));
        assert!(f
            .create(
                "ignored audit",
                Some(&f.value),
                Some(json!({"purpose": "fixture"}))
            )
            .await
            .is_err());
        f.assert_empty().await;
        let observed = fixture::outcomes(f.ns());
        let diagnostic = match (ignore_failure, observed.as_slice()) {
            (false, [FinalizerOutcome::AuditFailed(d)])
            | (true, [FinalizerOutcome::StampFailed(d)]) => d,
            other => panic!("ignored INSERT lost failure classification: {other:?}"),
        };
        let gaps = fixture::audit_gaps(f.ns());
        assert_eq!(gaps.len(), usize::from(ignore_failure));
        if let Some(gap) = gaps.first() {
            assert_eq!(gap.diagnostic_id, diagnostic.diagnostic_id);
        }
        f.runtime
            .sql()
            .writer()
            .await
            .unwrap()
            .execute_script("DROP TRIGGER ignore_finalizer_audit;".into())
            .await
            .unwrap();
        let entity = f
            .create(
                "restored audit",
                Some(&f.value),
                Some(json!({"purpose": "fixture"})),
            )
            .await
            .unwrap();
        f.assert_admitted(
            &entity,
            "create",
            RuntimeFieldScope::NameDescription,
            "entity.create",
        )
        .await;
    }
}

struct RefreshSnapshot {
    namespace: String,
    original: std::sync::Mutex<Option<fixture::ManifestFixtureGuard>>,
    replacement: std::sync::Mutex<Option<fixture::ManifestFixtureGuard>>,
}

struct RefreshProvider(std::sync::Arc<RefreshSnapshot>);
struct RefreshEmbedding(std::sync::Arc<RefreshSnapshot>);

#[async_trait::async_trait]
impl crate::EmbedderProvider for RefreshProvider {
    fn name(&self) -> &str {
        "manifestrefresh"
    }
    fn dimensions(&self) -> usize {
        4
    }
    async fn build(&self) -> RuntimeResult<std::sync::Arc<dyn lattice_embed::EmbeddingService>> {
        Ok(std::sync::Arc::new(RefreshEmbedding(self.0.clone())))
    }
}

#[async_trait::async_trait]
impl lattice_embed::EmbeddingService for RefreshEmbedding {
    async fn embed(
        &self,
        texts: &[String],
        _model: lattice_embed::EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
        if let Some(old) = self.0.original.lock().unwrap().take() {
            drop(old);
            *self.0.replacement.lock().unwrap() = Some(fixture::install(
                &self.0.namespace,
                super::manifest::ManifestSnapshot::empty(),
            ));
        }
        Ok(texts.iter().map(|_| vec![0.25; 4]).collect())
    }
    fn supports_model(&self, _model: lattice_embed::EmbeddingModel) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "manifestrefresh"
    }
}

#[tokio::test]
async fn actual_update_keeps_its_snapshot_when_embedding_refreshes_the_manifest() {
    let f = Fixture::new();
    let original = f
        .create(
            "before refresh",
            Some("clean"),
            Some(json!({"purpose": "fixture"})),
        )
        .await
        .unwrap();
    let state = std::sync::Arc::new(RefreshSnapshot {
        namespace: f.ns().into(),
        original: std::sync::Mutex::new(Some(fixture::install(
            f.ns(),
            TestOnlyManifestFixture::for_scope(RuntimeFieldScope::NameDescription).snapshot(),
        ))),
        replacement: std::sync::Mutex::new(None),
    });
    f.runtime.register_embedder(RefreshProvider(state.clone()));
    let (entity, _) = f
        .runtime
        .update_entity_with_expected_version_and_embedding_report(
            &f.token,
            original.id,
            EntityPatch {
                description: Some(Some(f.value.clone())),
                ..Default::default()
            },
            Some(original.version),
        )
        .await
        .unwrap();
    assert!(state.original.lock().unwrap().is_none());
    assert!(state.replacement.lock().unwrap().is_some());
    f.assert_admitted(
        &entity,
        "update",
        RuntimeFieldScope::NameDescription,
        "entity.update",
    )
    .await;
    assert!(f
        .create("next snapshot", Some(&f.value), None)
        .await
        .is_err());
    assert_eq!(fixture::outcomes(f.ns()).len(), 1);
}

#[tokio::test]
async fn direct_replacement_cannot_rewrite_creation_identity() {
    let f = Fixture::new();
    let _manifest = f.install(RuntimeFieldScope::NameDescription);
    let original = f
        .create(
            "original identity",
            None,
            Some(json!({"purpose": "fixture"})),
        )
        .await
        .unwrap();
    let mut candidate = original.clone();
    candidate.description = Some(f.value.clone());
    candidate.created_at += 1;
    candidate.updated_at += 1;
    let prepared = EntityCandidateContext::new(f.ns(), EntityCandidateOrigin::CodeIngest)
        .prepare(candidate)
        .unwrap();
    let error = f
        .runtime
        .try_commit_manifest_entity_candidate(
            &f.token,
            prepared,
            EntityCandidateMutation::ReplaceIfUnchanged {
                expected: original.clone(),
                expected_version: Some(original.version),
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, crate::RuntimeError::InvalidInput(ref message) if message == "entity finalization CAS snapshot mismatch")
    );
    assert_eq!(
        serde_json::to_value(f.runtime.get_entity(&f.token, original.id).await.unwrap()).unwrap(),
        serde_json::to_value(original).unwrap()
    );
    assert!(f.exemptions().await.is_empty());
}

#[tokio::test]
async fn ignored_claim_event_or_fts_map_prevents_an_admitted_record() {
    for claim in [false, true] {
        let f = Fixture::new();
        let _manifest = f.install(RuntimeFieldScope::NameDescription);
        let sql = if claim {
            "CREATE TRIGGER ignore_required BEFORE INSERT ON events WHEN NEW.kind = 'entity_created' BEGIN SELECT RAISE(IGNORE); END;"
        } else {
            "CREATE TRIGGER ignore_required BEFORE INSERT ON fts_entities_rowids BEGIN SELECT RAISE(IGNORE); END;"
        };
        f.runtime
            .sql()
            .writer()
            .await
            .unwrap()
            .execute_script(sql.into())
            .await
            .unwrap();
        let result = if claim {
            f.runtime
                .claim_entity_if_absent(
                    &f.token,
                    EntityClaimSpec {
                        id: Uuid::new_v4(),
                        kind: "concept".into(),
                        entity_type: None,
                        name: "claimed".into(),
                        description: Some(f.value.clone()),
                        properties: Some(json!({"purpose": "fixture"})),
                        tags: vec!["identity".into()],
                        identity_tag: "identity".into(),
                    },
                )
                .await
                .map(|(entity, _)| entity)
        } else {
            f.create(
                "map ignored",
                Some(&f.value),
                Some(json!({"purpose": "fixture"})),
            )
            .await
        };
        assert!(result.is_err());
        f.assert_empty().await;
        assert!(!fixture::outcomes(f.ns())
            .iter()
            .any(|outcome| matches!(outcome, FinalizerOutcome::Exempted(_))));
    }
}

#[tokio::test]
async fn ignored_attachment_cannot_commit_a_partial_admitted_record() {
    use khive_storage::BlobStore as _;
    let f = Fixture::new();
    let _manifest = f.install(RuntimeFieldScope::NameDescription);
    let directory = tempfile::tempdir().unwrap();
    let blob = std::sync::Arc::new(
        khive_db::stores::blob::FsBlobStore::new(directory.path().into(), 0).unwrap(),
    );
    let content_ref = blob.put(b"attachment".to_vec()).await.unwrap();
    f.runtime.install_blob_store(blob).unwrap();
    f.runtime.sql().writer().await.unwrap().execute_script("CREATE TRIGGER ignore_attachment BEFORE INSERT ON attachments BEGIN SELECT RAISE(IGNORE); END;".into()).await.unwrap();
    assert!(f
        .runtime
        .create_entity_with_attachments_and_report(
            &f.token,
            "concept",
            None,
            "attached",
            Some(&f.value),
            Some(json!({"purpose": "fixture"})),
            vec![],
            vec![khive_storage::NewAttachment {
                role: "content".into(),
                content_ref,
                media_type: None,
                size_bytes: Some(10),
            }]
        )
        .await
        .is_err());
    f.assert_empty().await;
    assert_eq!(
        f.runtime
            .sql()
            .reader()
            .await
            .unwrap()
            .count(SqlStatement::new(
                "SELECT COUNT(*) FROM attachments",
                vec![]
            ))
            .await
            .unwrap(),
        0
    );
    assert!(!fixture::outcomes(f.ns())
        .iter()
        .any(|outcome| matches!(outcome, FinalizerOutcome::Exempted(_))));
}

#[tokio::test]
async fn admitted_update_rollback_preserves_existing_vector_provenance() {
    let f = Fixture::new();
    let _manifest = f.install(RuntimeFieldScope::NameDescription);
    f.runtime.register_embedder(FixtureProvider);
    let original = f
        .create(
            "original vector",
            Some("clean"),
            Some(json!({"purpose": "fixture"})),
        )
        .await
        .unwrap();
    f.runtime.sql().writer().await.unwrap().execute(SqlStatement::new(
        "INSERT OR REPLACE INTO vector_provenance (model_key, subject_id, namespace, embedding_digest, text_fingerprint, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        vec![SqlValue::Text("manifestfixture".into()), SqlValue::Text(original.id.to_string()), SqlValue::Text(f.ns().into()), SqlValue::Text("a".repeat(64)), SqlValue::Text("b".repeat(64)), SqlValue::Text("fixture-original".into())],
    )).await.unwrap();
    let log_count = f.count("ann_write_log").await;
    let _fault = faults::arm_success_audit_fail(f.ns());
    assert!(f
        .runtime
        .update_entity_with_expected_version_and_embedding_report(
            &f.token,
            original.id,
            EntityPatch {
                description: Some(Some(f.value.clone())),
                ..Default::default()
            },
            Some(original.version)
        )
        .await
        .is_err());
    assert_eq!(
        serde_json::to_value(f.runtime.get_entity(&f.token, original.id).await.unwrap()).unwrap(),
        serde_json::to_value(original).unwrap()
    );
    assert_eq!(f.count("vec_manifestfixture").await, 1);
    assert_eq!(f.count("ann_write_log").await, log_count);
    let fingerprint = f
        .runtime
        .sql()
        .reader()
        .await
        .unwrap()
        .query_scalar(SqlStatement::new(
            "SELECT text_fingerprint FROM vector_provenance WHERE namespace = ?1",
            vec![SqlValue::Text(f.ns().into())],
        ))
        .await
        .unwrap();
    match fingerprint {
        Some(SqlValue::Text(text)) => assert_eq!(text, "b".repeat(64)),
        other => panic!("expected a text fingerprint, got {other:?}"),
    }
    assert!(f.exemptions().await.is_empty());
}

#[tokio::test]
async fn direct_cas_checks_the_stored_creation_time_not_only_the_supplied_pair() {
    let f = Fixture::new();
    let _manifest = f.install(RuntimeFieldScope::NameDescription);
    let original = f
        .create("stored identity", None, Some(json!({"purpose": "fixture"})))
        .await
        .unwrap();
    let mut expected = original.clone();
    expected.created_at += 1;
    let mut candidate = expected.clone();
    candidate.description = Some(f.value.clone());
    candidate.updated_at += 1;
    let prepared = EntityCandidateContext::new(f.ns(), EntityCandidateOrigin::CodeIngest)
        .prepare(candidate)
        .unwrap();
    assert!(matches!(
        f.runtime
            .try_commit_manifest_entity_candidate(
                &f.token,
                prepared,
                EntityCandidateMutation::ReplaceIfUnchanged {
                    expected,
                    expected_version: None,
                }
            )
            .await
            .unwrap(),
        EntityCandidateAdmission::Conflict
    ));
    assert_eq!(
        serde_json::to_value(f.runtime.get_entity(&f.token, original.id).await.unwrap()).unwrap(),
        serde_json::to_value(original).unwrap()
    );
    assert!(f.exemptions().await.is_empty());
}

#[tokio::test]
async fn ignored_ann_upsert_rolls_back_the_admitted_vector() {
    let f = Fixture::new();
    let _manifest = f.install(RuntimeFieldScope::NameDescription);
    f.runtime.register_embedder(FixtureProvider);
    f.runtime.sql().writer().await.unwrap().execute_script("CREATE TRIGGER ignore_ann_upsert BEFORE INSERT ON ann_write_log WHEN NEW.op = 'upsert' BEGIN SELECT RAISE(IGNORE); END;".into()).await.unwrap();
    assert!(f
        .create(
            "ignored ANN",
            Some(&f.value),
            Some(json!({"purpose": "fixture"}))
        )
        .await
        .is_err());
    f.assert_empty().await;
    assert_eq!(f.count("vec_manifestfixture").await, 0);
    assert_eq!(f.count("ann_write_log").await, 0);
    f.runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute_script("DROP TRIGGER ignore_ann_upsert;".into())
        .await
        .unwrap();
    let entity = f
        .create(
            "ANN retry",
            Some(&f.value),
            Some(json!({"purpose": "fixture"})),
        )
        .await
        .unwrap();
    f.assert_admitted(
        &entity,
        "create",
        RuntimeFieldScope::NameDescription,
        "entity.create",
    )
    .await;
    assert_eq!(f.count("vec_manifestfixture").await, 1);
    assert_eq!(f.count("ann_write_log").await, 1);
}

#[tokio::test]
async fn direct_admission_refuses_attachment_state_but_preserves_clean_legacy_handoff() {
    let f = Fixture::new();
    let _manifest = f.install(RuntimeFieldScope::NameDescription);
    let mut candidate = direct_candidate(&f, "unverified attachment");
    candidate.content_ref = Some("a".repeat(64));
    assert!(
        EntityCandidateContext::new(f.ns(), EntityCandidateOrigin::CodeIngest)
            .prepare(candidate)
            .is_err()
    );
    f.assert_empty().await;
    let mut clean = Entity::new(f.ns(), "concept", "legacy attachment");
    clean.content_ref = Some("a".repeat(64));
    let prepared = EntityCandidateContext::new(f.ns(), EntityCandidateOrigin::CodeIngest)
        .prepare(clean.clone())
        .unwrap();
    let result = f
        .runtime
        .try_commit_manifest_entity_candidate(
            &f.token,
            prepared,
            EntityCandidateMutation::CreateIfAbsent,
        )
        .await
        .unwrap();
    let EntityCandidateAdmission::Legacy(entity) = result else {
        panic!("clean attachment handoff changed")
    };
    assert_eq!(
        serde_json::to_value(entity).unwrap(),
        serde_json::to_value(clean).unwrap()
    );
    f.assert_empty().await;
}
