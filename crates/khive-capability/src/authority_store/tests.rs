use super::current::{right_types, Current};
use super::*;
use crate::durable::*;
use khive_db::{
    authority::{self, AuthorityChange, AuthorityEpoch, AuthorityRecord, ChangeKind, RecordKind},
    ConnectionPool, SqlBridge, StorageBackend,
};
use khive_storage::{event::Event, StorageError, WriterTaskRequestState};
use khive_types::{EventKind, SubstrateKind, Timestamp};
use std::{
    any::Any,
    path::Path,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use uuid::Uuid;

struct Clock {
    now: AtomicU64,
    step: AtomicU64,
    reads: AtomicUsize,
}
impl Clock {
    fn new(now: u64) -> Self {
        Self {
            now: AtomicU64::new(now),
            step: AtomicU64::new(0),
            reads: AtomicUsize::new(0),
        }
    }
}
impl AuthorityClock for Clock {
    fn verification_time(&self) -> Result<Timestamp, AuthorityRefusal> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(Timestamp::from_micros(self.now.fetch_add(
            self.step.load(Ordering::SeqCst),
            Ordering::SeqCst,
        )))
    }
}
struct Context {
    caller: Principal,
    service: Uuid,
    denied: bool,
}
impl AdmissionContext for Context {
    fn as_any(&self) -> &dyn Any {
        self
    }
}
struct Admission;
impl AdmissionVerifier for Admission {
    fn revalidate(
        &self,
        context: &dyn AdmissionContext,
        binding: &AdmissionBinding,
    ) -> Result<AdmittedCaller, AuthorityRefusal> {
        let context = context
            .as_any()
            .downcast_ref::<Context>()
            .ok_or(AuthorityRefusal::Unavailable("typed admission absent"))?;
        if context.denied
            || context.service != binding.service_id
            || binding.store_id != Uuid::from_u128(101)
            || binding.epoch_id != Uuid::from_u128(102)
        {
            return Err(AuthorityRefusal::Denied(
                "current outer policy or binding refused",
            ));
        }
        Ok(AdmittedCaller {
            principal: context.caller.clone(),
            assurance: IssuanceAssurance::ConfiguredLocal,
            gate_implementation: "test-policy",
            readable_namespaces: vec!["research".into(), "authority".into()],
        })
    }
}
fn principal(id: &str) -> Principal {
    Principal {
        kind: "actor".into(),
        id: id.into(),
        leg: None,
    }
}
fn context(id: &str) -> Arc<dyn AdmissionContext> {
    Arc::new(Context {
        caller: principal(id),
        service: Uuid::from_u128(100),
        denied: false,
    })
}
fn contract(name: &str, byte: u8) -> ContractRef {
    ContractRef {
        name: name.into(),
        version: 1,
        digest: ContractDigest([byte; 32]),
    }
}
fn key_ref() -> KeyRef {
    KeyRef {
        id: "test-key".into(),
        generation: 1,
    }
}
fn keys() -> Arc<SuppliedKeys> {
    Arc::new(
        SuppliedKeys::provision(vec![
            ProvisionedKey::new(key_ref(), [1; 32], [2; 32]).unwrap()
        ])
        .unwrap(),
    )
}
fn catalog() -> Arc<ClosedCatalog> {
    Arc::new(
        ClosedCatalog::provision(
            vec![SubjectContract {
                identity: contract("subject", 1),
                reference_format: ReferenceFormat::Opaque,
                repository_independent: true,
                repositories: vec![],
                membership: vec![],
            }],
            vec![SourceContract {
                identity: contract("source", 2),
                reference_format: ReferenceFormat::Opaque,
                identities: vec![ExactBytes(b"sensor".to_vec())],
            }],
            vec![EntryContract {
                identity: contract("instrument", 3),
                class: ProducerClass::Instrument,
                subject_kind: contract("subject", 1),
                source_contract: Some(contract("source", 2)),
                response_contract: None,
                authority_arguments: vec![ArgumentSpec {
                    name: "document".into(),
                    kind: ArgumentType::String,
                }],
            }],
            vec![],
            vec![],
        )
        .unwrap(),
    )
}
fn scope(document: &str) -> ObservationScope {
    ObservationScope {
        permissions: vec![ObservationPermission {
            policy_domain: "research".into(),
            entry: contract("instrument", 3),
            producer_class: ProducerClass::Instrument,
            channel: SubmissionChannel::Observe,
            subject: SubjectScope {
                kind: contract("subject", 1),
                selector: SubjectSelector::AllSubjectsOfKind,
            },
            binding: SubmissionBinding::Instrument(SourceBinding {
                contract: contract("source", 2),
                identities: vec![ExactBytes(b"sensor".to_vec())],
                arguments: vec![ExactArguments(vec![ArgumentBinding {
                    name: "document".into(),
                    value: ArgumentValue::String(document.into()),
                }])],
            }),
        }],
    }
}
fn mixed(document: &str) -> Vec<RightSpec> {
    let scope = scope(document);
    vec![
        RightSpec::RecordObservation(scope.clone()),
        RightSpec::GrantAdmin(AdminScope {
            namespace: "research".into(),
            target: AdminTarget::RecordObservation(scope),
        }),
    ]
}
fn configuration(
    clock: Arc<Clock>,
    rights: Vec<RightSpec>,
    expires_at: Option<u64>,
) -> AuthorityConfiguration {
    let mut manifest = b"explicit test envelope digest:".to_vec();
    manifest.extend_from_slice(blake3::hash(&serde_json::to_vec(&rights).unwrap()).as_bytes());
    let digest = *blake3::hash(&manifest).as_bytes();
    let epoch = AuthorityEpoch {
        epoch_id: Uuid::from_u128(102),
        marker_id: Uuid::from_u128(104),
        store_id: Uuid::from_u128(101),
        manifest_digest: digest,
        manifest,
        namespace: "authority".into(),
        created_at: 10,
    };
    let anchor = AuthorityRecord {
        id: Uuid::from_u128(103),
        epoch_id: epoch.epoch_id,
        kind: RecordKind::Anchor,
        principal: canonical_principal(&principal("root")).unwrap(),
        namespace: "authority".into(),
        right_types: vec![],
        parent_id: None,
        bootstrap_slot: None,
        root_anchor_id: None,
        created_at: 10,
        body: b"immutable rights-free test anchor".to_vec(),
    };
    let source = BootstrapSource {
        store_id: epoch.store_id,
        epoch_id: epoch.epoch_id,
        root_anchor_id: anchor.id,
        owner: principal("root"),
        namespace: "research".into(),
        slot: "source".into(),
        manifest_digest: ContractDigest(digest),
        rights,
        validity: Validity {
            not_before: Timestamp::from_micros(10),
            expires_at: expires_at.map(Timestamp::from_micros),
        },
    };
    AuthorityConfiguration {
        service_id: Uuid::from_u128(100),
        epoch,
        anchor,
        boot_principal: principal("root"),
        bootstrap_sources: vec![source],
        issuance_key: key_ref(),
        keys: keys(),
        catalog: catalog(),
        clock,
        admission: Arc::new(Admission),
    }
}
fn clone_configuration(c: &AuthorityConfiguration) -> AuthorityConfiguration {
    AuthorityConfiguration {
        service_id: c.service_id,
        epoch: c.epoch.clone(),
        anchor: c.anchor.clone(),
        boot_principal: c.boot_principal.clone(),
        bootstrap_sources: c.bootstrap_sources.clone(),
        issuance_key: c.issuance_key.clone(),
        keys: Arc::clone(&c.keys),
        catalog: Arc::clone(&c.catalog),
        clock: Arc::clone(&c.clock),
        admission: Arc::clone(&c.admission),
    }
}
struct Fixture {
    store: AuthorityStore,
    backend: StorageBackend,
    clock: Arc<Clock>,
    directory: tempfile::TempDir,
}
impl Fixture {
    async fn new(expires_at: Option<u64>) -> Self {
        Self::with_rights(mixed(""), expires_at).await
    }
    async fn with_rights(rights: Vec<RightSpec>, expires_at: Option<u64>) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let backend = StorageBackend::sqlite_for_test_with_journal_mode_in(
            directory.path().join("authority.db"),
            true,
            Duration::from_secs(5),
            directory.path().join("locks"),
        )
        .unwrap();
        backend.prepare_core_schema().unwrap();
        let clock = Arc::new(Clock::new(100));
        let c = configuration(Arc::clone(&clock), rights, expires_at);
        let sql = Arc::new(SqlBridge::new(backend.pool_arc(), false));
        let epoch = c.epoch.clone();
        let anchor = c.anchor.clone();
        // Test-only initialized rows, not an alternate production initializer.
        let mut marker_event = Event::new(
            "authority",
            "initialize",
            EventKind::GrantChange,
            SubstrateKind::Event,
            "root",
        );
        marker_event.id = Uuid::from_u128(105);
        marker_event.created_at = 10;
        marker_event.payload =
            serde_json::json!({"action_id":epoch.marker_id,"epoch_id":epoch.epoch_id});
        let mut anchor_event = marker_event.clone();
        anchor_event.id = Uuid::from_u128(106);
        authority::transaction(&*sql, move |mut tx| {
            Box::pin(async move {
                tx.insert_epoch(&epoch).await?;
                tx.insert_record(&anchor).await?;
                for (record_id, event, kind) in [
                    (epoch.marker_id, marker_event, ChangeKind::Initialization),
                    (anchor.id, anchor_event, ChangeKind::Anchor),
                ] {
                    tx.append_change(
                        &AuthorityChange {
                            event_id: event.id,
                            epoch_id: epoch.epoch_id,
                            action_id: epoch.marker_id,
                            kind,
                            record_id,
                        },
                        &event,
                    )
                    .await?;
                }
                assert_eq!(tx.advance_revision(epoch.epoch_id, 0).await?, 1);
                Ok(())
            })
        })
        .await
        .unwrap();
        let store = AuthorityStore::from_initialized(sql, c).unwrap();
        Self {
            store,
            backend,
            clock,
            directory,
        }
    }
    fn pool(&self) -> Arc<ConnectionPool> {
        self.backend.pool_arc()
    }
    fn fresh(&self) -> AuthorityStore {
        AuthorityStore::from_initialized(
            Arc::new(SqlBridge::new(self.backend.pool_arc(), false)),
            clone_configuration(&self.store.configuration),
        )
        .unwrap()
    }
}
fn grant(id: &str, grantee: &str, rights: Vec<RightSpec>, parent: GrantParent) -> GrantRequest {
    GrantRequest {
        request_id: id.into(),
        rationale: "explicit test administration".into(),
        grants: vec![GrantItem {
            grantee: principal(grantee),
            namespace: "research".into(),
            parent,
            rights,
        }],
    }
}
fn revoke(id: &str, ids: Vec<Uuid>) -> RevokeRequest {
    RevokeRequest {
        request_id: id.into(),
        rationale: "terminal change".into(),
        grant_ids: ids,
    }
}
fn snapshot(pool: &ConnectionPool) -> Vec<i64> {
    let reader = pool.reader().unwrap();
    let mut values = vec![reader
        .query_row("SELECT revision FROM authority_revisions", [], |row| {
            row.get(0)
        })
        .unwrap()];
    for table in [
        "authority_records",
        "authority_record_right_types",
        "authority_transitions",
        "authority_replays",
        "authority_changes",
        "events",
    ] {
        values.push(
            reader
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap(),
        );
    }
    values
}
async fn effective(store: &AuthorityStore, id: Uuid) -> bool {
    let c = Arc::clone(&store.configuration);
    authority::transaction(&*store.sql, move |mut tx| {
        Box::pin(async move {
            let mut current = Current::open(
                &c,
                &mut tx,
                &*context("root"),
                AdministrationOperation::Grant,
                b"test consultation",
            )
            .await
            .map_err(|e| khive_storage::StorageError::Internal(e.to_string()))?;
            Ok(current.effective(id).await.is_ok())
        })
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn derived_interval_uses_one_writer_clock_and_never_admin_expiry() {
    let fixture = Fixture::new(Some(101)).await;
    fixture.clock.step.store(1, Ordering::SeqCst);
    let result = fixture
        .store
        .grant(
            context("root"),
            grant(
                "once",
                "alice",
                mixed(""),
                GrantParent::BootstrapSlot("source".into()),
            ),
        )
        .await
        .unwrap();
    assert_eq!(fixture.clock.reads.load(Ordering::SeqCst), 1);
    assert_eq!(
        result.targets[0].validity,
        Some(super::model::PublicValidity {
            not_before: 100,
            expires_at: Some(101)
        })
    );
    let failed = fixture
        .store
        .grant(
            context("root"),
            grant(
                "expired",
                "alice",
                mixed(""),
                GrantParent::BootstrapSlot("source".into()),
            ),
        )
        .await;
    assert!(failed.is_err(), "expiry equality must refuse");
    fixture.clock.step.store(0, Ordering::SeqCst);
    fixture.clock.now.store(99, Ordering::SeqCst);
    assert!(
        !effective(&fixture.store, result.targets[0].record_id).await,
        "issued child must refuse at t-1 while its bootstrap source is live"
    );
    fixture.clock.now.store(100, Ordering::SeqCst);
    assert!(effective(&fixture.store, result.targets[0].record_id).await);
    fixture.clock.now.store(101, Ordering::SeqCst);
    assert!(
        !effective(&fixture.store, result.targets[0].record_id).await,
        "issued child and source must refuse at their exclusive expiry"
    );
    let standing = Fixture::new(None).await;
    let result = standing
        .store
        .grant(
            context("root"),
            grant(
                "standing",
                "alice",
                mixed(""),
                GrantParent::BootstrapSlot("source".into()),
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        result.targets[0].validity.as_ref().unwrap().expires_at,
        None
    );
    standing.clock.now.store(9, Ordering::SeqCst);
    assert!(standing
        .store
        .grant(
            context("root"),
            grant(
                "before",
                "alice",
                mixed(""),
                GrantParent::BootstrapSlot("source".into())
            )
        )
        .await
        .is_err());
}

#[tokio::test]
async fn complete_current_authorization_precedes_exact_and_conflicting_replay() {
    let fixture = Fixture::new(None).await;
    let source = fixture
        .store
        .grant(
            context("root"),
            grant(
                "source",
                "alice",
                mixed(""),
                GrantParent::BootstrapSlot("source".into()),
            ),
        )
        .await
        .unwrap()
        .targets[0]
        .record_id;
    let request = grant(
        "child",
        "bob",
        vec![mixed("").remove(0)],
        GrantParent::Grant(source),
    );
    let first = fixture
        .store
        .grant(context("alice"), request.clone())
        .await
        .unwrap();
    assert_eq!(
        fixture
            .store
            .grant(context("alice"), request.clone())
            .await
            .unwrap(),
        first
    );
    let mut different = request.clone();
    different.rationale.push('!');
    assert!(matches!(
        fixture
            .store
            .grant(context("alice"), different.clone())
            .await,
        Err(AuthorityError::Refused(AuthorityRefusal::RequestConflict))
    ));
    fixture
        .store
        .revoke(context("root"), revoke("kill", vec![source]))
        .await
        .unwrap();
    let before = snapshot(&fixture.pool());
    assert!(matches!(
        fixture.store.grant(context("alice"), request).await,
        Err(AuthorityError::Refused(AuthorityRefusal::NotEffective))
    ));
    assert!(matches!(
        fixture.store.grant(context("alice"), different).await,
        Err(AuthorityError::Refused(AuthorityRefusal::NotEffective))
    ));
    assert_eq!(snapshot(&fixture.pool()), before);
}

#[tokio::test]
async fn all_terminal_receipts_preserve_original_ids_and_revision() {
    let fixture = Fixture::new(None).await;
    let target = fixture
        .store
        .grant(
            context("root"),
            grant(
                "target",
                "alice",
                mixed(""),
                GrantParent::BootstrapSlot("source".into()),
            ),
        )
        .await
        .unwrap()
        .targets[0]
        .record_id;
    let first = fixture
        .store
        .revoke(context("root"), revoke("first", vec![target]))
        .await
        .unwrap();
    let request = revoke("all-terminal", vec![target]);
    let second = fixture
        .store
        .revoke(context("root"), request.clone())
        .await
        .unwrap();
    assert!(!second.changed);
    assert_eq!(second.action_id, None);
    assert_eq!(second.authority_revision, first.authority_revision);
    assert_eq!(second.targets[0].action_id, first.targets[0].action_id);
    assert_eq!(
        second.targets[0].change_event_id,
        first.targets[0].change_event_id
    );
    let before = snapshot(&fixture.pool());
    assert_eq!(
        fixture
            .store
            .revoke(context("root"), request.clone())
            .await
            .unwrap(),
        second
    );
    let mut conflict = request;
    conflict.rationale.push('!');
    assert!(matches!(
        fixture.store.revoke(context("root"), conflict).await,
        Err(AuthorityError::Refused(AuthorityRefusal::RequestConflict))
    ));
    assert_eq!(snapshot(&fixture.pool()), before);
}

#[tokio::test]
async fn mixed_terminal_history_names_both_rights_and_indirect_leaf_stays_immutable() {
    let fixture = Fixture::new(None).await;
    let mixed_id = fixture
        .store
        .grant(
            context("root"),
            grant(
                "mixed",
                "alice",
                mixed(""),
                GrantParent::BootstrapSlot("source".into()),
            ),
        )
        .await
        .unwrap()
        .targets[0]
        .record_id;
    let sibling = fixture
        .store
        .grant(
            context("root"),
            grant(
                "sibling",
                "alice",
                vec![mixed("").remove(1)],
                GrantParent::BootstrapSlot("source".into()),
            ),
        )
        .await
        .unwrap()
        .targets[0]
        .record_id;
    let leaf = fixture
        .store
        .grant(
            context("alice"),
            grant(
                "leaf",
                "bob",
                vec![mixed("").remove(0)],
                GrantParent::Grant(mixed_id),
            ),
        )
        .await
        .unwrap()
        .targets[0]
        .record_id;
    assert!(effective(&fixture.store, leaf).await);
    let before_body: Vec<u8> = fixture
        .pool()
        .reader()
        .unwrap()
        .query_row(
            "SELECT body FROM authority_records WHERE record_id=?1",
            [leaf.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    let result = fixture
        .store
        .revoke(context("root"), revoke("disable", vec![mixed_id]))
        .await
        .unwrap();
    assert!(!effective(&fixture.store, mixed_id).await);
    assert!(!effective(&fixture.store, leaf).await);
    assert!(effective(&fixture.store, sibling).await);
    let history = fixture
        .store
        .read_change(
            context("root"),
            result.action_id.unwrap(),
            result.targets[0].change_event_id,
        )
        .await
        .unwrap();
    assert_eq!(history.terminal_state, Some(RecordedState::Disabled));
    assert_eq!(history.former_rights, mixed(""));
    assert!(history.event.payload.get("rights").is_none());
    assert!(history.event.payload["authorization"]
        .get("checked_rights")
        .is_none());
    assert_eq!(
        fixture
            .pool()
            .reader()
            .unwrap()
            .query_row::<Vec<u8>, _, _>(
                "SELECT body FROM authority_records WHERE record_id=?1",
                [leaf.to_string()],
                |row| row.get(0)
            )
            .unwrap(),
        before_body
    );
    assert_eq!(
        fixture
            .pool()
            .reader()
            .unwrap()
            .query_row::<i64, _, _>(
                "SELECT COUNT(*) FROM authority_transitions WHERE record_id=?1",
                [leaf.to_string()],
                |row| row.get(0)
            )
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn retired_anchor_retry_uses_boot_identity_without_live_root() {
    let fixture = Fixture::new(None).await;
    let target = fixture
        .store
        .grant(
            context("root"),
            grant(
                "target",
                "alice",
                mixed(""),
                GrantParent::BootstrapSlot("source".into()),
            ),
        )
        .await
        .unwrap()
        .targets[0]
        .record_id;
    let request = RetireAnchorRequest {
        request_id: "retire".into(),
        rationale: "root retirement".into(),
        root_anchor_id: Uuid::from_u128(103),
    };
    assert!(fixture
        .store
        .retire_anchor(context("alice"), request.clone())
        .await
        .is_err());
    let result = fixture
        .store
        .retire_anchor(context("root"), request.clone())
        .await
        .unwrap();
    assert!(!effective(&fixture.store, target).await);
    let history = fixture
        .store
        .read_change(
            context("root"),
            result.action_id.unwrap(),
            result.targets[0].change_event_id,
        )
        .await
        .unwrap();
    assert_eq!(history.terminal_state, Some(RecordedState::Retired));
    assert!(history.former_rights.is_empty());
    assert_eq!(
        history.event.payload["root_anchor_id"],
        serde_json::json!(Uuid::from_u128(103))
    );
    assert_eq!(
        fixture
            .store
            .retire_anchor(context("root"), request)
            .await
            .unwrap(),
        result
    );
    let next = fixture
        .store
        .retire_anchor(
            context("root"),
            RetireAnchorRequest {
                request_id: "new-retire".into(),
                rationale: "repeat".into(),
                root_anchor_id: Uuid::from_u128(103),
            },
        )
        .await
        .unwrap();
    assert!(!next.changed);
    assert_eq!(next.authority_revision, result.authority_revision);
    assert_eq!(
        next.targets[0].change_event_id,
        result.targets[0].change_event_id
    );
}

#[tokio::test]
async fn history_failure_after_mint_rolls_back_then_fresh_service_can_issue() {
    let fixture = Fixture::new(None).await;
    let mut request = grant(
        "two",
        "alice",
        mixed(""),
        GrantParent::BootstrapSlot("source".into()),
    );
    request.grants.push(GrantItem {
        grantee: principal("bob"),
        namespace: "research".into(),
        parent: GrantParent::BootstrapSlot("source".into()),
        rights: mixed(""),
    });
    let before = snapshot(&fixture.pool());
    fixture.pool().writer().unwrap().transaction(|conn| { conn.execute_batch("CREATE TRIGGER reject_second_authority_event BEFORE INSERT ON events WHEN NEW.verb='grant' AND (SELECT COUNT(*) FROM events WHERE verb='grant')>=1 BEGIN SELECT RAISE(ABORT,'second history failure'); END;")?; Ok(()) }).unwrap();
    let error = fixture
        .store
        .grant(context("root"), request.clone())
        .await
        .unwrap_err();
    assert_storage_rollback(&error);
    assert!(error.to_string().contains("second history failure"));
    assert_eq!(
        snapshot(&fixture.pool()),
        before,
        "a failed history write must leave all state unchanged"
    );
    fixture
        .pool()
        .writer()
        .unwrap()
        .transaction(|conn| {
            conn.execute_batch("DROP TRIGGER reject_second_authority_event;")?;
            Ok(())
        })
        .unwrap();
    let fresh = fixture.fresh();
    let result = fresh.grant(context("root"), request).await.unwrap();
    assert_eq!(result.targets.len(), 2);
    for target in result.targets {
        assert!(effective(&fresh, target.record_id).await);
        fresh
            .read_change(context("root"), target.action_id, target.change_event_id)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn receipt_failure_rolls_back_all_preceding_records_and_events() {
    let fixture = Fixture::new(None).await;
    let before = snapshot(&fixture.pool());
    fixture.pool().writer().unwrap().transaction(|conn| { conn.execute_batch("CREATE TRIGGER reject_authority_receipt BEFORE INSERT ON authority_replays BEGIN SELECT RAISE(ABORT,'receipt failure'); END;")?; Ok(()) }).unwrap();
    let error = fixture
        .store
        .grant(
            context("root"),
            grant(
                "receipt",
                "alice",
                mixed(""),
                GrantParent::BootstrapSlot("source".into()),
            ),
        )
        .await
        .unwrap_err();
    assert_storage_rollback(&error);
    assert!(error.to_string().contains("receipt failure"));
    assert_eq!(snapshot(&fixture.pool()), before);
}

#[tokio::test]
async fn source_only_admin_only_subactor_and_outer_denial_cannot_issue() {
    let fixture = Fixture::new(None).await;
    for (name, index) in [("source-only", 0), ("admin-only", 1)] {
        let parent = fixture
            .store
            .grant(
                context("root"),
                grant(
                    name,
                    name,
                    vec![mixed("").remove(index)],
                    GrantParent::BootstrapSlot("source".into()),
                ),
            )
            .await
            .unwrap()
            .targets[0]
            .record_id;
        assert!(fixture
            .store
            .grant(
                context(name),
                grant(
                    "attempt",
                    "bob",
                    vec![mixed("").remove(0)],
                    GrantParent::Grant(parent)
                )
            )
            .await
            .is_err());
    }
    let mut leg = principal("root");
    leg.leg = Some("worker".into());
    assert!(fixture
        .store
        .grant(
            Arc::new(Context {
                caller: leg,
                service: Uuid::from_u128(100),
                denied: false
            }),
            grant(
                "leg",
                "bob",
                mixed(""),
                GrantParent::BootstrapSlot("source".into())
            )
        )
        .await
        .is_err());
    let before = snapshot(&fixture.pool());
    assert!(fixture
        .store
        .grant(
            Arc::new(Context {
                caller: principal("root"),
                service: Uuid::from_u128(100),
                denied: true
            }),
            grant(
                "outer",
                "bob",
                mixed(""),
                GrantParent::BootstrapSlot("source".into())
            )
        )
        .await
        .is_err());
    assert_eq!(snapshot(&fixture.pool()), before);
}

#[tokio::test]
async fn cold_process_reads_committed_grant_and_delegates_over_same_file() {
    const EXACT: &str =
        "authority_store::tests::cold_process_reads_committed_grant_and_delegates_over_same_file";
    if let Ok(path) = std::env::var("KHIVE_AUTHORITY_TEST_CHILD_DB") {
        let path = Path::new(&path);
        let directory = path.parent().unwrap();
        let backend = StorageBackend::sqlite_for_test_with_journal_mode_in(
            path,
            true,
            Duration::from_secs(5),
            directory.join("locks"),
        )
        .unwrap();
        backend.prepare_core_schema().unwrap();
        let store = AuthorityStore::from_initialized(
            Arc::new(SqlBridge::new(backend.pool_arc(), false)),
            configuration(Arc::new(Clock::new(100)), mixed(""), None),
        )
        .unwrap();
        let grant_id =
            Uuid::parse_str(&std::env::var("KHIVE_AUTHORITY_TEST_GRANT").unwrap()).unwrap();
        assert!(effective(&store, grant_id).await);
        let result = store
            .grant(
                context("alice"),
                grant(
                    "child-process",
                    "bob",
                    vec![mixed("").remove(0)],
                    GrantParent::Grant(grant_id),
                ),
            )
            .await
            .unwrap();
        assert!(effective(&store, result.targets[0].record_id).await);
        std::fs::write(
            directory.join("child-verified"),
            b"verified durable lineage",
        )
        .unwrap();
        return;
    }
    let fixture = Fixture::new(None).await;
    let id = fixture
        .store
        .grant(
            context("root"),
            grant(
                "cold-parent",
                "alice",
                mixed(""),
                GrantParent::BootstrapSlot("source".into()),
            ),
        )
        .await
        .unwrap()
        .targets[0]
        .record_id;
    let join = fixture.pool().take_writer_task_join();
    let Fixture {
        store,
        backend,
        clock,
        directory,
    } = fixture;
    drop(store);
    drop(backend);
    drop(clock);
    if let Some(join) = join {
        join.await.unwrap();
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", EXACT, "--nocapture"])
        .env(
            "KHIVE_AUTHORITY_TEST_CHILD_DB",
            directory.path().join("authority.db"),
        )
        .env("KHIVE_AUTHORITY_TEST_GRANT", id.to_string())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    assert_eq!(
        std::fs::read(directory.path().join("child-verified")).unwrap(),
        b"verified durable lineage"
    );
}

fn draft(
    c: &AuthorityConfiguration,
    id: u128,
    grantee: &str,
    rights: Vec<RightSpec>,
    expiry: Option<u64>,
) -> EnvelopeDraft {
    EnvelopeDraft {
        id: Uuid::from_u128(id),
        store_id: c.epoch.store_id,
        epoch_id: c.epoch.epoch_id,
        root_anchor_id: c.anchor.id,
        issuer: principal("root"),
        grantee: principal(grantee),
        namespace: "research".into(),
        rights,
        validity: Validity {
            not_before: Timestamp::from_micros(100),
            expires_at: expiry.map(Timestamp::from_micros),
        },
        created_at: Timestamp::from_micros(100),
        assurance: IssuanceAssurance::ConfiguredLocal,
        operation: CreationOperation::Grant,
        action_id: Uuid::from_u128(id + 1000),
        configuration_source: None,
        issuance_id: Uuid::from_u128(id + 2000),
        key: key_ref(),
    }
}
async fn insert_authenticated_fixture(store: &AuthorityStore, envelope: AuthenticatedEnvelope) {
    let grant = &envelope.body().grant;
    let (parent_id, bootstrap_slot) = match &envelope.body().parent {
        ParentBinding::Grant { grant_id, .. } => (Some(*grant_id), None),
        ParentBinding::BootstrapSlot { slot, .. } => (None, Some(slot.clone())),
    };
    let record = AuthorityRecord {
        id: grant.id,
        epoch_id: grant.epoch_id,
        kind: RecordKind::Grant,
        principal: canonical_principal(&grant.grantee).unwrap(),
        namespace: grant.namespace.clone(),
        right_types: right_types(&grant.rights),
        parent_id,
        bootstrap_slot,
        root_anchor_id: Some(grant.root_anchor_id),
        created_at: 100,
        body: envelope.as_bytes().to_vec(),
    };
    let action_id = grant.action_id;
    let event_id = Uuid::new_v4();
    let c = Arc::clone(&store.configuration);
    authority::transaction(&*store.sql, move |mut tx| {
        Box::pin(async move {
            let mut current = Current::open(
                &c,
                &mut tx,
                &*context("root"),
                AdministrationOperation::Grant,
                b"privileged authenticated test fixture",
            )
            .await
            .map_err(|e| khive_storage::StorageError::Internal(e.to_string()))?;
            let proof = super::authorize::administration(
                &mut current,
                &record.namespace,
                &envelope.body().grant.rights,
            )
            .await
            .map_err(|e| khive_storage::StorageError::Internal(e.to_string()))?;
            let plan = super::mutation::PlannedChange {
                association: AuthorityChange {
                    event_id,
                    epoch_id: record.epoch_id,
                    action_id,
                    kind: ChangeKind::Grant,
                    record_id: record.id,
                },
                target: RecordRef::of(&record),
                namespace: record.namespace.clone(),
                record: Some(record),
                transition: None,
                proof: Some(proof),
            };
            let event = super::history::change_event(
                &c,
                &plan,
                &current.caller,
                AdministrationOperation::Grant,
                "authenticated bound fixture",
                current.now,
            )?;
            let revision = current.revision;
            tx.insert_record(plan.record.as_ref().unwrap()).await?;
            tx.append_change(&plan.association, &event).await?;
            tx.advance_revision(c.epoch.epoch_id, revision).await?;
            Ok(())
        })
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn exact_two_mib_mixed_record_revokes_with_bounded_receipt_and_full_typed_history() {
    let clock = Arc::new(Clock::new(100));
    let c = configuration(clock, mixed(""), None);
    let mut grant = draft(&c, 500, "alice", mixed(""), None);
    let issuer = EnvelopeIssuer::new(&c.keys, &c.catalog);
    let small = issuer
        .issue_from_bootstrap(&c.bootstrap_sources[0], grant.clone())
        .unwrap();
    let text_length = MAX_RECORD_BYTES - small.as_bytes().len();
    let RightSpec::RecordObservation(scope) = &mut grant.rights[0] else {
        panic!("RO fixture")
    };
    let SubmissionBinding::Instrument(binding) = &mut scope.permissions[0].binding else {
        panic!("instrument fixture")
    };
    binding.arguments[0].0[0].value = ArgumentValue::String("x".repeat(text_length));
    let large_scope = match &grant.rights[0] {
        RightSpec::RecordObservation(scope) => scope.clone(),
        _ => unreachable!(),
    };
    let mut source_rights = grant.rights.clone();
    source_rights.push(RightSpec::GrantAdmin(AdminScope {
        namespace: "research".into(),
        target: AdminTarget::RecordObservation(large_scope),
    }));
    source_rights
        .sort_by_cached_key(|right| canonical_rights(std::slice::from_ref(right)).unwrap());
    let fixture = Fixture::with_rights(source_rights, None).await;
    let c = &fixture.store.configuration;
    let issuer = EnvelopeIssuer::new(&c.keys, &c.catalog);
    let exact = issuer
        .issue_from_bootstrap(&c.bootstrap_sources[0], grant.clone())
        .unwrap();
    assert_eq!(exact.as_bytes().len(), MAX_RECORD_BYTES);
    // This privileged fixture reaches an accepted body that a 1MiB request may
    // not produce. It uses a real authority-relevant argument, never padding.
    insert_authenticated_fixture(&fixture.store, exact).await;
    assert!(effective(&fixture.store, grant.id).await);
    let result = fixture
        .store
        .revoke(context("root"), revoke("bound-revoke", vec![grant.id]))
        .await
        .unwrap();
    let receipt_bytes = serde_json::to_vec(&result).unwrap().len();
    assert!(receipt_bytes <= authority::MAX_AUTHORITY_REPLAY_BYTES);
    let history = fixture
        .store
        .read_change(
            context("root"),
            result.action_id.unwrap(),
            result.targets[0].change_event_id,
        )
        .await
        .unwrap();
    assert_eq!(history.former_rights, grant.rights);
    assert_eq!(history.former_rights.len(), 2);
    assert_eq!(history.terminal_state, Some(RecordedState::Disabled));
    assert!(history.event.payload.get("rights").is_none());
    let transition_bytes: i64 = fixture
        .pool()
        .reader()
        .unwrap()
        .query_row(
            "SELECT length(body) FROM authority_transitions WHERE record_id=?1",
            [grant.id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(transition_bytes, 130);
    println!("record_bytes={} transition_bytes={transition_bytes} replay_bytes={receipt_bytes} event_payload_bytes={}",MAX_RECORD_BYTES,serde_json::to_vec(&history.event.payload).unwrap().len());
    let RightSpec::RecordObservation(scope) = &mut grant.rights[0] else {
        unreachable!()
    };
    let SubmissionBinding::Instrument(binding) = &mut scope.permissions[0].binding else {
        unreachable!()
    };
    let ArgumentValue::String(document) = &mut binding.arguments[0].0[0].value else {
        unreachable!()
    };
    document.push('x');
    let mut over_source = c.bootstrap_sources[0].clone();
    over_source.rights = grant.rights.clone();
    assert_eq!(
        issuer.issue_from_bootstrap(&over_source, grant),
        Err(EnvelopeError::OverLimit {
            field: "canonical record",
            measured: MAX_RECORD_BYTES + 1,
            bound: MAX_RECORD_BYTES
        })
    );
}

#[tokio::test]
async fn independent_administration_expiry_does_not_shorten_standing_source() {
    let fixture = Fixture::new(None).await;
    let source = fixture
        .store
        .grant(
            context("root"),
            grant(
                "ordinary",
                "alice",
                vec![mixed("").remove(0)],
                GrantParent::BootstrapSlot("source".into()),
            ),
        )
        .await
        .unwrap()
        .targets[0]
        .record_id;
    let c = &fixture.store.configuration;
    let admin = EnvelopeIssuer::new(&c.keys, &c.catalog)
        .issue_from_bootstrap(
            &c.bootstrap_sources[0],
            draft(c, 700, "alice", vec![mixed("").remove(1)], Some(101)),
        )
        .unwrap();
    insert_authenticated_fixture(&fixture.store, admin).await;
    fixture.clock.reads.store(0, Ordering::SeqCst);
    fixture.clock.step.store(1, Ordering::SeqCst);
    let result = fixture
        .store
        .grant(
            context("alice"),
            grant(
                "standing-child",
                "bob",
                vec![mixed("").remove(0)],
                GrantParent::Grant(source),
            ),
        )
        .await
        .unwrap();
    assert_eq!(fixture.clock.reads.load(Ordering::SeqCst), 1);
    assert_eq!(result.targets[0].validity.as_ref().unwrap().not_before, 100);
    assert_eq!(
        result.targets[0].validity.as_ref().unwrap().expires_at,
        None
    );
    let before = snapshot(&fixture.pool());
    let retry = fixture
        .store
        .grant(
            context("alice"),
            grant(
                "standing-child",
                "bob",
                vec![mixed("").remove(0)],
                GrantParent::Grant(source),
            ),
        )
        .await;
    assert!(matches!(
        retry,
        Err(AuthorityError::Refused(AuthorityRefusal::Denied(_)))
    ));
    assert_eq!(snapshot(&fixture.pool()), before);
}

fn assert_storage_rollback(error: &AuthorityError) {
    assert!(
        matches!(
            error,
            AuthorityError::Storage(StorageError::WriterTaskRequestFailed {
                request_state: WriterTaskRequestState::TransactionRolledBack,
                ..
            })
        ),
        "expected preserved file-backed rollback wrapper, got {error}"
    );
}

#[tokio::test]
async fn one_verification_clock_read_defines_child_time_with_advancing_clock() {
    let fixture = Fixture::new(None).await;
    fixture.clock.step.store(1, Ordering::SeqCst);
    let result = fixture
        .store
        .grant(
            context("root"),
            grant(
                "one-clock",
                "alice",
                mixed(""),
                GrantParent::BootstrapSlot("source".into()),
            ),
        )
        .await
        .unwrap();
    // A standing source keeps the mutant's second instant valid. The oracle is
    // specifically the wrong child time, not an incidental expiry refusal.
    assert_eq!(result.targets[0].validity.as_ref().unwrap().not_before, 100);
    assert_eq!(fixture.clock.reads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unknown_target_and_unsupported_profile_refuse_without_partial_state() {
    let fixture = Fixture::new(None).await;
    let id = fixture
        .store
        .grant(
            context("root"),
            grant(
                "known",
                "alice",
                mixed(""),
                GrantParent::BootstrapSlot("source".into()),
            ),
        )
        .await
        .unwrap()
        .targets[0]
        .record_id;
    let before = snapshot(&fixture.pool());
    assert!(fixture
        .store
        .revoke(
            context("root"),
            revoke("unknown", vec![id, Uuid::from_u128(999)])
        )
        .await
        .is_err());
    assert!(effective(&fixture.store, id).await);
    let unsupported = fixture
        .store
        .grant(
            context("root"),
            grant(
                "ordinary-right",
                "alice",
                vec![RightSpec::Read],
                GrantParent::BootstrapSlot("source".into()),
            ),
        )
        .await;
    assert!(matches!(
        unsupported,
        Err(AuthorityError::Refused(AuthorityRefusal::Envelope(
            EnvelopeError::RightNotSupported(UnsupportedRight::OrdinaryNamespace)
        )))
    ));
    let mut claim_scope = scope("");
    claim_scope.permissions[0].binding = SubmissionBinding::ClaimTimePlatformBundle;
    let claim = fixture
        .store
        .grant(
            context("root"),
            grant(
                "claim-profile",
                "alice",
                vec![RightSpec::RecordObservation(claim_scope)],
                GrantParent::BootstrapSlot("source".into()),
            ),
        )
        .await;
    assert!(matches!(
        claim,
        Err(AuthorityError::Refused(AuthorityRefusal::Envelope(
            EnvelopeError::RightNotSupported(UnsupportedRight::ClaimTimePlatformBundleInM1)
        )))
    ));
    assert_eq!(snapshot(&fixture.pool()), before);
}
