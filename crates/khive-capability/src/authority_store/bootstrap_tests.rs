use super::*;
use crate::durable::*;
use khive_db::{authority, ConnectionPool, SqlBridge, StorageBackend};
use khive_storage::{StorageError, WriterTaskRequestState};
use khive_types::Timestamp;
use std::any::Any;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

fn principal(id: &str) -> Principal {
    Principal {
        kind: "actor".into(),
        id: id.into(),
        leg: None,
    }
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
        id: "bootstrap-test-key".into(),
        generation: 1,
    }
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
                authority_arguments: vec![],
            }],
            vec![],
            vec![],
        )
        .unwrap(),
    )
}
fn scope() -> ObservationScope {
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
                arguments: vec![ExactArguments(vec![])],
            }),
        }],
    }
}
fn mixed() -> Vec<RightSpec> {
    vec![
        RightSpec::RecordObservation(scope()),
        RightSpec::GrantAdmin(AdminScope {
            namespace: "research".into(),
            target: AdminTarget::RecordObservation(scope()),
        }),
    ]
}
fn seed(grantee: &str) -> GrantItem {
    GrantItem {
        grantee: principal(grantee),
        namespace: "research".into(),
        parent: GrantParent::BootstrapSlot("source".into()),
        rights: mixed(),
    }
}
fn input(seeds: Vec<GrantItem>) -> BootstrapManifestInput {
    BootstrapManifestInput {
        store_id: Uuid::from_u128(101),
        administrator: principal("root"),
        marker_namespace: "authority".into(),
        anchor_namespace: "authority".into(),
        identity_profile: IssuanceAssurance::ConfiguredLocal,
        issuance_key: key_ref(),
        verification_key_refs: vec![key_ref()],
        configuration_source: "bootstrap-test".into(),
        slots: vec![BootstrapSlotInput {
            slot: "source".into(),
            namespace: "research".into(),
            rights: mixed(),
            validity: Validity {
                not_before: Timestamp::from_micros(10),
                expires_at: None,
            },
        }],
        seeds,
    }
}
fn manifest(seeds: Vec<GrantItem>) -> ValidatedBootstrapManifest {
    ValidatedBootstrapManifest::validate(input(seeds), &catalog()).unwrap()
}

#[test]
fn manifest_normalizes_seed_intents_and_preserves_exact_multiplicity() {
    let a = manifest(vec![seed("alice"), seed("bob"), seed("alice")]);
    let b = manifest(vec![seed("bob"), seed("alice"), seed("alice")]);
    let c = manifest(vec![seed("alice"), seed("bob")]);
    assert_eq!(a.canonical_bytes(), b.canonical_bytes());
    assert_eq!(a.digest(), b.digest());
    assert_ne!(a.canonical_bytes(), c.canonical_bytes());
    assert_eq!(*blake3::hash(a.canonical_bytes()).as_bytes(), a.digest().0);
}

#[test]
fn manifest_requires_explicit_complete_administration_and_exact_slot_parent() {
    let mut missing = input(vec![]);
    missing.slots[0].rights.remove(1);
    assert!(ValidatedBootstrapManifest::validate(missing, &catalog()).is_err());
    let mut partial = input(vec![]);
    let RightSpec::GrantAdmin(admin) = &mut partial.slots[0].rights[1] else {
        panic!("admin fixture")
    };
    let AdminTarget::RecordObservation(admin_scope) = &mut admin.target else {
        panic!("observation fixture")
    };
    admin_scope.permissions[0].subject.selector = SubjectSelector::Exact(vec![
        ExactBytes(b"one".to_vec()),
        ExactBytes(b"two".to_vec()),
    ]);
    assert!(
        ValidatedBootstrapManifest::validate(partial, &catalog()).is_err(),
        "narrow scope cannot administer complete source"
    );
    let mut stitched = input(vec![]);
    let mut complete = scope();
    complete.permissions[0].subject.selector = SubjectSelector::Exact(vec![
        ExactBytes(b"one".to_vec()),
        ExactBytes(b"two".to_vec()),
    ]);
    stitched.slots[0].rights = vec![RightSpec::RecordObservation(complete.clone())];
    for (slot, subject) in [("admin-one", "one"), ("admin-two", "two")] {
        let mut partial = complete.clone();
        partial.permissions[0].subject.selector =
            SubjectSelector::Exact(vec![ExactBytes(subject.as_bytes().to_vec())]);
        stitched.slots.push(BootstrapSlotInput {
            slot: slot.into(),
            namespace: "research".into(),
            rights: vec![RightSpec::GrantAdmin(AdminScope {
                namespace: "research".into(),
                target: AdminTarget::RecordObservation(partial),
            })],
            validity: Validity {
                not_before: Timestamp::from_micros(10),
                expires_at: None,
            },
        });
    }
    assert!(
        ValidatedBootstrapManifest::validate(stitched, &catalog()).is_err(),
        "two partial administration permissions cannot be stitched into one complete proof"
    );
    let mut wrong = input(vec![seed("alice")]);
    wrong.seeds[0].parent = GrantParent::Grant(Uuid::from_u128(5));
    assert!(ValidatedBootstrapManifest::validate(wrong, &catalog()).is_err());
}

#[test]
fn manifest_bound_reports_complete_canonical_size_without_truncation() {
    let mut exact = input(vec![]);
    let before = ValidatedBootstrapManifest::validate(exact.clone(), &catalog()).unwrap();
    let bound = khive_db::authority::MAX_AUTHORITY_MANIFEST_BYTES;
    exact.configuration_source =
        "x".repeat(exact.configuration_source.len() + bound - before.canonical_bytes().len());
    assert_eq!(
        ValidatedBootstrapManifest::validate(exact.clone(), &catalog())
            .unwrap()
            .canonical_bytes()
            .len(),
        bound
    );
    exact.configuration_source.push('x');
    assert!(
        matches!(ValidatedBootstrapManifest::validate(exact, &catalog()),
        Err(AuthorityError::Refused(AuthorityRefusal::Envelope(EnvelopeError::OverLimit { field: "bootstrap manifest", measured, bound: actual }))) if measured == bound + 1 && actual == bound)
    );
}

struct Clock {
    now: AtomicU64,
    step: AtomicU64,
    reads: AtomicUsize,
}
impl Clock {
    fn new() -> Self {
        Self {
            now: AtomicU64::new(100),
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
struct Context(Principal, Vec<String>);
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
            .ok_or(AuthorityRefusal::Denied("typed caller missing"))?;
        if binding.service_id != Uuid::from_u128(100) || binding.store_id != Uuid::from_u128(101) {
            return Err(AuthorityRefusal::Denied("request binding mismatch"));
        }
        Ok(AdmittedCaller {
            principal: context.0.clone(),
            assurance: IssuanceAssurance::ConfiguredLocal,
            gate_implementation: "bootstrap-test-policy",
            readable_namespaces: context.1.clone(),
        })
    }
}
fn context(id: &str) -> Arc<dyn AdmissionContext> {
    Arc::new(Context(
        principal(id),
        vec!["research".into(), "authority".into()],
    ))
}
fn keys() -> Arc<SuppliedKeys> {
    Arc::new(
        SuppliedKeys::provision(vec![
            ProvisionedKey::new(key_ref(), [1; 32], [2; 32]).unwrap()
        ])
        .unwrap(),
    )
}
fn runtime(clock: Arc<Clock>) -> BootstrapRuntime {
    BootstrapRuntime {
        service_id: Uuid::from_u128(100),
        keys: keys(),
        catalog: catalog(),
        clock,
        admission: Arc::new(Admission),
        construction_implementation: "bootstrap-test-construction",
    }
}
struct Fixture {
    directory: tempfile::TempDir,
    backend: StorageBackend,
    sql: Arc<SqlBridge>,
    clock: Arc<Clock>,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let backend = StorageBackend::sqlite_for_test_with_journal_mode_in(
            directory.path().join("authority.db"),
            true,
            Duration::from_secs(5),
            directory.path().join("locks"),
        )
        .unwrap();
        backend.prepare_core_schema().unwrap();
        let sql = Arc::new(SqlBridge::new(backend.pool_arc(), false));
        Self {
            directory,
            backend,
            sql,
            clock: Arc::new(Clock::new()),
        }
    }
    fn pool(&self) -> Arc<ConnectionPool> {
        self.backend.pool_arc()
    }
    fn runtime(&self) -> BootstrapRuntime {
        runtime(self.clock.clone())
    }
    async fn initialize(
        &self,
        manifest: ValidatedBootstrapManifest,
    ) -> AuthorityResult<InitializationResult> {
        AuthorityStore::initialize(self.sql.clone(), manifest, self.runtime()).await
    }
    async fn load(&self, manifest: ValidatedBootstrapManifest) -> AuthorityResult<LoadedAuthority> {
        AuthorityStore::load_active(self.sql.clone(), manifest, self.runtime()).await
    }
}
fn snapshot(pool: &ConnectionPool) -> Vec<i64> {
    let reader = pool.reader().unwrap();
    let mut result = vec![reader
        .query_row(
            "SELECT COALESCE(MAX(revision),0) FROM authority_revisions",
            [],
            |row| row.get(0),
        )
        .unwrap()];
    for table in [
        "authority_epochs",
        "authority_revisions",
        "authority_records",
        "authority_record_right_types",
        "authority_transitions",
        "authority_replays",
        "authority_changes",
        "events",
    ] {
        result.push(
            reader
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap(),
        );
    }
    result
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
struct StoredTable {
    table: String,
    columns: Vec<String>,
    rows: Vec<Vec<String>>,
}

// Capture every current column, preserving its SQLite storage type and complete
// value bytes. Hex avoids text quoting/NUL ambiguity; row ordering is stable.
fn stored_rows(pool: &ConnectionPool) -> Vec<StoredTable> {
    let reader = pool.reader().unwrap();
    let mut tables = Vec::new();
    for table in [
        "authority_epochs",
        "authority_revisions",
        "authority_records",
        "authority_record_right_types",
        "authority_transitions",
        "authority_replays",
        "authority_changes",
        "events",
    ] {
        let columns_json: String = reader.query_row(
            "SELECT json_group_array(name) FROM (SELECT name FROM pragma_table_info(?1) ORDER BY cid)",
            [table], |row| row.get(0),
        ).unwrap();
        let columns: Vec<String> = serde_json::from_str(&columns_json).unwrap();
        assert!(
            !columns.is_empty(),
            "complete stored-row capture requires table {table}"
        );
        let expressions = columns
            .iter()
            .flat_map(|column| {
                let quoted = format!("\"{}\"", column.replace('"', "\"\""));
                [format!("typeof({quoted})"), format!("hex({quoted})")]
            })
            .collect::<Vec<_>>();
        let projection = expressions.join(",");
        let rows_json: String = reader.query_row(
            &format!("SELECT json_group_array(json(cells)) FROM (SELECT json_array({projection}) AS cells FROM \"{table}\" ORDER BY {projection})"),
            [], |row| row.get(0),
        ).unwrap();
        let rows: Vec<Vec<String>> = serde_json::from_str(&rows_json).unwrap();
        tables.push(StoredTable {
            table: table.into(),
            columns,
            rows,
        });
    }
    tables
}

fn assert_immutable_rows_preserved(before: &[StoredTable], after: &[StoredTable]) {
    for old in before
        .iter()
        .filter(|table| table.table != "authority_revisions")
    {
        let new = after.iter().find(|table| table.table == old.table).unwrap();
        assert_eq!(old.columns, new.columns);
        for row in &old.rows {
            assert!(
                new.rows.contains(row),
                "existing immutable row changed in {}",
                old.table
            );
        }
    }
}

fn assert_rollback(error: &AuthorityError) {
    assert!(
        matches!(
            error,
            AuthorityError::Storage(StorageError::WriterTaskRequestFailed {
                request_state: WriterTaskRequestState::TransactionRolledBack,
                ..
            })
        ),
        "{error}"
    );
}
fn grant_request(id: &str, grantee: &str, parent: GrantParent) -> GrantRequest {
    GrantRequest {
        request_id: id.into(),
        rationale: "explicit registration".into(),
        grants: vec![GrantItem {
            grantee: principal(grantee),
            namespace: "research".into(),
            parent,
            rights: mixed(),
        }],
    }
}
async fn effective(store: &AuthorityStore, id: Uuid) -> bool {
    let configuration = store.configuration.clone();
    authority::transaction(&*store.sql, move |mut tx| {
        Box::pin(async move {
            let mut current = super::current::Current::open(
                &configuration,
                &mut tx,
                &*context("root"),
                AdministrationOperation::Grant,
                b"bootstrap test consultation",
            )
            .await
            .map_err(|error| StorageError::Internal(error.to_string()))?;
            Ok(current.effective(id).await.is_ok())
        })
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn empty_initialize_records_both_events_and_identical_retry_writes_nothing() {
    let fixture = Fixture::new();
    let manifest = manifest(vec![]);
    let first = fixture.initialize(manifest.clone()).await.unwrap();
    assert!(first.changed);
    assert_eq!(first.initialization.initial_revision, 1);
    assert!(first.initialization.seeds.is_empty());
    assert_eq!(snapshot(&fixture.pool()), vec![1, 1, 1, 1, 0, 0, 0, 2, 2]);
    let before = snapshot(&fixture.pool());
    let before_rows = stored_rows(&fixture.pool());
    let reads = fixture.clock.reads.load(Ordering::SeqCst);
    fixture.clock.now.store(999, Ordering::SeqCst);
    let retry = fixture.initialize(manifest.clone()).await.unwrap();
    assert!(!retry.changed);
    assert_eq!(retry.initialization, first.initialization);
    assert_eq!(fixture.clock.reads.load(Ordering::SeqCst), reads);
    assert_eq!(snapshot(&fixture.pool()), before);
    assert_eq!(stored_rows(&fixture.pool()), before_rows);
    let loaded = fixture.load(manifest).await.unwrap();
    assert_eq!(loaded.current_revision, 1);
    for event in [
        first.initialization.marker_event_id,
        first.initialization.anchor_event_id,
    ] {
        let history = loaded
            .store
            .read_initialization_change(context("root"), first.initialization.marker_id, event)
            .await
            .unwrap();
        assert!(matches!(
            history,
            BootstrapHistoricalChange::MarkerInitialize { .. }
                | BootstrapHistoricalChange::AnchorInitialize { .. }
        ));
    }
    assert_eq!(snapshot(&fixture.pool()), before);
    assert_eq!(stored_rows(&fixture.pool()), before_rows);
}

#[tokio::test]
async fn divergent_initializer_conflicts_without_union_or_revision_change() {
    let fixture = Fixture::new();
    let first = fixture
        .initialize(manifest(vec![seed("alice")]))
        .await
        .unwrap();
    let before = snapshot(&fixture.pool());
    let before_rows = stored_rows(&fixture.pool());
    let mut different = input(vec![seed("bob")]);
    different.configuration_source = "different-source".into();
    assert!(matches!(
        fixture
            .initialize(ValidatedBootstrapManifest::validate(different, &catalog()).unwrap())
            .await,
        Err(AuthorityError::Refused(
            AuthorityRefusal::InitializationConflict
        ))
    ));
    assert_eq!(snapshot(&fixture.pool()), before);
    assert_eq!(stored_rows(&fixture.pool()), before_rows);
    assert_eq!(first.initialization.seeds.len(), 1);
}

#[tokio::test]
async fn concurrent_identical_initializers_share_original_result_once() {
    let fixture = Fixture::new();
    let manifest = manifest(vec![seed("alice")]);
    let (a, b) = tokio::join!(
        fixture.initialize(manifest.clone()),
        fixture.initialize(manifest)
    );
    let a = a.unwrap();
    let b = b.unwrap();
    assert_ne!(a.changed, b.changed);
    assert_eq!(a.initialization, b.initialization);
    assert_eq!(snapshot(&fixture.pool()), vec![1, 1, 1, 2, 2, 0, 0, 3, 3]);
}

#[tokio::test]
async fn concurrent_divergent_initializers_commit_only_winning_manifest() {
    let fixture = Fixture::new();
    let a_manifest = manifest(vec![seed("alice")]);
    let b_manifest = manifest(vec![seed("bob")]);
    let (a, b) = tokio::join!(
        fixture.initialize(a_manifest.clone()),
        fixture.initialize(b_manifest.clone())
    );
    let winner = match (a, b) {
        (Ok(result), Err(AuthorityError::Refused(AuthorityRefusal::InitializationConflict))) => {
            (result, a_manifest)
        }
        (Err(AuthorityError::Refused(AuthorityRefusal::InitializationConflict)), Ok(result)) => {
            (result, b_manifest)
        }
        _ => panic!("exactly one divergent initializer must win"),
    };
    assert!(winner.0.changed);
    assert_eq!(
        fixture.load(winner.1).await.unwrap().initialization,
        winner.0.initialization
    );
    assert_eq!(snapshot(&fixture.pool()), vec![1, 1, 1, 2, 2, 0, 0, 3, 3]);
}

#[tokio::test]
async fn unused_slot_difference_is_a_manifest_conflict() {
    let fixture = Fixture::new();
    fixture
        .initialize(manifest(vec![seed("alice")]))
        .await
        .unwrap();
    let before = snapshot(&fixture.pool());
    let before_rows = stored_rows(&fixture.pool());
    let mut different = input(vec![seed("alice")]);
    let mut unused = different.slots[0].clone();
    unused.slot = "unused".into();
    different.slots.push(unused);
    assert!(matches!(
        fixture
            .initialize(ValidatedBootstrapManifest::validate(different, &catalog()).unwrap())
            .await,
        Err(AuthorityError::Refused(
            AuthorityRefusal::InitializationConflict
        ))
    ));
    assert_eq!(snapshot(&fixture.pool()), before);
    assert_eq!(stored_rows(&fixture.pool()), before_rows);
}

#[tokio::test]
async fn seed_interval_is_derived_once_and_remains_original_on_retry_and_history() {
    let fixture = Fixture::new();
    let mut configured = input(vec![seed("alice")]);
    configured.slots[0].validity.expires_at = Some(Timestamp::from_micros(101));
    let manifest = ValidatedBootstrapManifest::validate(configured, &catalog()).unwrap();
    fixture.clock.step.store(1, Ordering::SeqCst);
    let first = fixture.initialize(manifest.clone()).await.unwrap();
    let initial_rows = stored_rows(&fixture.pool());
    assert_eq!(fixture.clock.reads.load(Ordering::SeqCst), 1);
    let seed = &first.initialization.seeds[0];
    assert_eq!(
        seed.validity,
        PublicValidity {
            not_before: 100,
            expires_at: Some(101)
        }
    );
    let loaded = fixture.load(manifest.clone()).await.unwrap();
    assert_eq!(
        fixture.clock.reads.load(Ordering::SeqCst),
        1,
        "restart never rederives validity"
    );
    fixture.clock.step.store(0, Ordering::SeqCst);
    fixture.clock.now.store(99, Ordering::SeqCst);
    assert!(!effective(&loaded.store, seed.record_id).await);
    fixture.clock.now.store(100, Ordering::SeqCst);
    assert!(effective(&loaded.store, seed.record_id).await);
    fixture.clock.now.store(101, Ordering::SeqCst);
    assert!(!effective(&loaded.store, seed.record_id).await);
    fixture.clock.now.store(1000, Ordering::SeqCst);
    let reads = fixture.clock.reads.load(Ordering::SeqCst);
    let before = snapshot(&fixture.pool());
    let before_rows = stored_rows(&fixture.pool());
    let retry = fixture.initialize(manifest).await.unwrap();
    assert_eq!(retry.initialization, first.initialization);
    assert!(!retry.changed);
    assert_eq!(fixture.clock.reads.load(Ordering::SeqCst), reads);
    let history = loaded
        .store
        .read_initialization_change(
            context("root"),
            first.initialization.marker_id,
            seed.event_id,
        )
        .await
        .unwrap();
    let BootstrapHistoricalChange::ImportedSeed {
        validity,
        rights,
        event,
        ..
    } = history
    else {
        panic!("import history")
    };
    assert_eq!(validity, seed.validity);
    assert_eq!(rights.len(), 2);
    assert_eq!(
        event.payload["authorization"]["derived_validity"],
        serde_json::json!(seed.validity)
    );
    assert_eq!(event.payload["authorization"]["source_slot"], "source");
    assert_eq!(event.payload_schema_version, 1);
    assert!(event.payload.get("payload_schema_version").is_none());
    assert_eq!(snapshot(&fixture.pool()), before);
    assert_eq!(stored_rows(&fixture.pool()), before_rows);
    assert_eq!(stored_rows(&fixture.pool()), initial_rows);
}

#[tokio::test]
async fn standing_seed_uses_first_writer_instant_and_independent_admin_never_shortens_it() {
    let fixture = Fixture::new();
    let mut configured = input(vec![GrantItem {
        rights: vec![mixed().remove(0)],
        ..seed("alice")
    }]);
    configured.slots[0].rights = vec![mixed().remove(0)];
    configured.slots.push(BootstrapSlotInput {
        slot: "administrator".into(),
        namespace: "research".into(),
        rights: vec![mixed().remove(1)],
        validity: Validity {
            not_before: Timestamp::from_micros(10),
            expires_at: Some(Timestamp::from_micros(101)),
        },
    });
    let manifest = ValidatedBootstrapManifest::validate(configured.clone(), &catalog()).unwrap();
    fixture.clock.step.store(1, Ordering::SeqCst);
    let result = fixture.initialize(manifest.clone()).await.unwrap();
    let initial_rows = stored_rows(&fixture.pool());
    assert_eq!(fixture.clock.reads.load(Ordering::SeqCst), 1);
    assert_eq!(
        result.initialization.seeds[0].validity,
        PublicValidity {
            not_before: 100,
            expires_at: None
        }
    );
    fixture.clock.step.store(0, Ordering::SeqCst);
    fixture.clock.now.store(102, Ordering::SeqCst);
    let loaded = fixture.load(manifest).await.unwrap();
    assert!(effective(&loaded.store, result.initialization.seeds[0].record_id).await);
    let before_start = Fixture::new();
    before_start.clock.now.store(9, Ordering::SeqCst);
    assert!(matches!(
        before_start
            .initialize(
                ValidatedBootstrapManifest::validate(input(vec![seed("alice")]), &catalog())
                    .unwrap()
            )
            .await,
        Err(AuthorityError::Refused(AuthorityRefusal::NotEffective))
    ));
    assert_eq!(snapshot(&before_start.pool()), vec![0; 9]);
    for administrator_interval in [
        Validity {
            not_before: Timestamp::from_micros(10),
            expires_at: Some(Timestamp::from_micros(100)),
        },
        Validity {
            not_before: Timestamp::from_micros(101),
            expires_at: None,
        },
    ] {
        let negative = Fixture::new();
        let before = stored_rows(&negative.pool());
        let mut expected = configured.clone();
        expected.slots[1].validity = administrator_interval;
        assert!(
            expected.slots[0]
                .validity
                .contains(Timestamp::from_micros(100)),
            "ordinary source remains standing and live"
        );
        let expected = ValidatedBootstrapManifest::validate(expected, &catalog()).unwrap();
        assert!(
            matches!(
                negative.initialize(expected).await,
                Err(AuthorityError::Refused(AuthorityRefusal::Denied(_)))
            ),
            "only the independent administrator interval must deny first initialization"
        );
        assert_eq!(negative.clock.reads.load(Ordering::SeqCst), 1);
        assert_eq!(snapshot(&negative.pool()), vec![0; 9]);
        assert_eq!(stored_rows(&negative.pool()), before);
    }
    assert_eq!(stored_rows(&fixture.pool()), initial_rows);
}

#[tokio::test]
async fn identical_retry_preserves_current_revision_and_retirement_without_reactivation() {
    let fixture = Fixture::new();
    let manifest = manifest(vec![seed("alice")]);
    let first = fixture.initialize(manifest.clone()).await.unwrap();
    let loaded = fixture.load(manifest.clone()).await.unwrap();
    let grant = loaded
        .store
        .grant(
            context("alice"),
            grant_request(
                "runtime-child",
                "bob",
                GrantParent::Grant(first.initialization.seeds[0].record_id),
            ),
        )
        .await
        .unwrap();
    assert_eq!(grant.authority_revision, 2);
    let before = snapshot(&fixture.pool());
    let before_rows = stored_rows(&fixture.pool());
    let retry = fixture.initialize(manifest.clone()).await.unwrap();
    assert!(!retry.changed);
    assert_eq!(retry.initialization, first.initialization);
    assert_eq!(snapshot(&fixture.pool()), before);
    assert_eq!(stored_rows(&fixture.pool()), before_rows);
    assert_eq!(
        fixture
            .load(manifest.clone())
            .await
            .unwrap()
            .current_revision,
        2
    );
    let retired = loaded
        .store
        .retire_anchor(
            context("root"),
            RetireAnchorRequest {
                request_id: "retire-root".into(),
                rationale: "retire configured authority".into(),
                root_anchor_id: first.initialization.anchor_id,
            },
        )
        .await
        .unwrap();
    assert_eq!(retired.authority_revision, 3);
    let before = snapshot(&fixture.pool());
    let before_rows = stored_rows(&fixture.pool());
    let reads = fixture.clock.reads.load(Ordering::SeqCst);
    let retry = fixture.initialize(manifest.clone()).await.unwrap();
    assert!(!retry.changed);
    assert_eq!(retry.initialization, first.initialization);
    assert_eq!(fixture.clock.reads.load(Ordering::SeqCst), reads);
    assert_eq!(snapshot(&fixture.pool()), before);
    assert_eq!(stored_rows(&fixture.pool()), before_rows);
    assert!(matches!(
        fixture.load(manifest.clone()).await,
        Err(AuthorityError::Refused(AuthorityRefusal::NotEffective))
    ));
    let history = AuthorityStore::load_history(fixture.sql.clone(), manifest, fixture.runtime())
        .await
        .unwrap();
    assert_eq!(history.current_revision(), 3);
    assert_eq!(history.initialization().initial_revision, 1);
    let change = history
        .read_change(
            context("root"),
            retired.action_id.unwrap(),
            retired.targets[0].change_event_id,
        )
        .await
        .unwrap();
    assert_eq!(change.terminal_state, Some(RecordedState::Retired));
    assert!(matches!(
        history
            .read_initialization_change(
                context("root"),
                first.initialization.marker_id,
                first.initialization.marker_event_id
            )
            .await
            .unwrap(),
        BootstrapHistoricalChange::MarkerInitialize { .. }
    ));
    assert_eq!(
        loaded
            .store
            .retire_anchor(
                context("root"),
                RetireAnchorRequest {
                    request_id: "retire-root".into(),
                    rationale: "retire configured authority".into(),
                    root_anchor_id: first.initialization.anchor_id
                }
            )
            .await
            .unwrap(),
        retired,
        "existing-service retirement retry remains available"
    );
    assert_eq!(stored_rows(&fixture.pool()), before_rows);
}

#[tokio::test]
async fn all_256_seeds_commit_with_258_events_and_restart_pages_every_association() {
    let fixture = Fixture::new();
    let manifest = manifest((0..256).map(|n| seed(&format!("actor-{n}"))).collect());
    let first = fixture.initialize(manifest.clone()).await.unwrap();
    let initial_rows = stored_rows(&fixture.pool());
    assert_eq!(first.initialization.seeds.len(), 256);
    assert_eq!(
        snapshot(&fixture.pool()),
        vec![1, 1, 1, 257, 512, 0, 0, 258, 258]
    );
    let loaded = fixture.load(manifest.clone()).await.unwrap();
    assert_eq!(loaded.initialization, first.initialization);
    assert_eq!(loaded.current_revision, 1);
    assert!(!fixture.initialize(manifest).await.unwrap().changed);
    let last = first.initialization.seeds.last().unwrap();
    let change = loaded
        .store
        .read_initialization_change(
            context("root"),
            first.initialization.marker_id,
            last.event_id,
        )
        .await
        .unwrap();
    assert!(
        matches!(change, BootstrapHistoricalChange::ImportedSeed { record, rights, .. } if record.id == last.record_id && rights.len() == 2)
    );
    assert_eq!(stored_rows(&fixture.pool()), initial_rows);
}

#[tokio::test]
async fn initialization_history_failure_after_real_mints_rolls_back_every_row() {
    let fixture = Fixture::new();
    let manifest = manifest(vec![seed("alice"), seed("bob")]);
    let before = snapshot(&fixture.pool());
    let before_rows = stored_rows(&fixture.pool());
    fixture.pool().writer().unwrap().transaction(|conn| {
        conn.execute_batch("CREATE TRIGGER reject_second_import_event BEFORE INSERT ON events WHEN NEW.verb='import' AND (SELECT COUNT(*) FROM events WHERE verb='import')>=1 BEGIN SELECT RAISE(ABORT,'second import history failure'); END;")?;
        Ok(())
    }).unwrap();
    let error = fixture.initialize(manifest.clone()).await.unwrap_err();
    assert_rollback(&error);
    assert_eq!(
        fixture.clock.reads.load(Ordering::SeqCst),
        1,
        "preparation reached both real import mints at one instant"
    );
    assert_eq!(
        snapshot(&fixture.pool()),
        before,
        "event failure must roll back marker, anchor, grants, tags, associations and revision"
    );
    assert_eq!(stored_rows(&fixture.pool()), before_rows);
    fixture
        .pool()
        .writer()
        .unwrap()
        .transaction(|conn| {
            conn.execute_batch("DROP TRIGGER reject_second_import_event;")?;
            Ok(())
        })
        .unwrap();
    let retry = fixture.initialize(manifest.clone()).await.unwrap();
    assert!(retry.changed);
    let loaded = fixture.load(manifest).await.unwrap();
    for seed in &retry.initialization.seeds {
        assert!(effective(&loaded.store, seed.record_id).await);
    }
    assert_eq!(snapshot(&fixture.pool()), vec![1, 1, 1, 3, 4, 0, 0, 4, 4]);
}

#[tokio::test]
async fn restart_distinguishes_missing_keys_from_wrong_usable_keys() {
    let fixture = Fixture::new();
    let manifest = manifest(vec![seed("alice")]);
    fixture.initialize(manifest.clone()).await.unwrap();
    let before = snapshot(&fixture.pool());
    let before_rows = stored_rows(&fixture.pool());
    let mut missing = fixture.runtime();
    missing.keys = Arc::new(
        SuppliedKeys::provision(vec![ProvisionedKey::new(
            KeyRef {
                id: "other-key".into(),
                generation: 1,
            },
            [1; 32],
            [2; 32],
        )
        .unwrap()])
        .unwrap(),
    );
    assert!(matches!(
        AuthorityStore::load_active(fixture.sql.clone(), manifest.clone(), missing).await,
        Err(AuthorityError::Refused(AuthorityRefusal::Envelope(
            EnvelopeError::Unavailable(_)
        )))
    ));
    let mut wrong = fixture.runtime();
    wrong.keys = Arc::new(
        SuppliedKeys::provision(vec![
            ProvisionedKey::new(key_ref(), [3; 32], [4; 32]).unwrap()
        ])
        .unwrap(),
    );
    assert!(matches!(
        AuthorityStore::load_active(fixture.sql.clone(), manifest, wrong).await,
        Err(AuthorityError::Refused(AuthorityRefusal::Envelope(
            EnvelopeError::Reject(_)
        )))
    ));
    assert_eq!(snapshot(&fixture.pool()), before);
    assert_eq!(stored_rows(&fixture.pool()), before_rows);
}

#[tokio::test]
async fn every_write_boundary_fault_rolls_back_initialization_and_revision_zero() {
    for trigger in [
        "CREATE TRIGGER fail_initialization BEFORE INSERT ON authority_records BEGIN SELECT RAISE(ABORT,'anchor record failure'); END;",
        "CREATE TRIGGER fail_initialization BEFORE INSERT ON authority_records WHEN NEW.record_kind='grant' BEGIN SELECT RAISE(ABORT,'seed grant record failure'); END;",
        "CREATE TRIGGER fail_initialization BEFORE INSERT ON authority_record_right_types BEGIN SELECT RAISE(ABORT,'tag failure'); END;",
        "CREATE TRIGGER fail_initialization BEFORE INSERT ON events WHEN NEW.verb='initialize' BEGIN SELECT RAISE(ABORT,'marker initialize Event failure'); END;",
        "CREATE TRIGGER fail_initialization BEFORE INSERT ON events WHEN NEW.verb='initialize' AND (SELECT COUNT(*) FROM events WHERE verb='initialize')=1 BEGIN SELECT RAISE(ABORT,'anchor initialize Event failure'); END;",
        "CREATE TRIGGER fail_initialization BEFORE INSERT ON authority_changes BEGIN SELECT RAISE(ABORT,'association failure'); END;",
        "CREATE TRIGGER fail_initialization BEFORE UPDATE ON authority_revisions BEGIN SELECT RAISE(ABORT,'revision advance failure'); END;",
    ] {
        let fixture = Fixture::new();
        fixture.pool().writer().unwrap().transaction(|conn| { conn.execute_batch(trigger)?; Ok(()) }).unwrap();
        let error = fixture.initialize(manifest(vec![seed("alice")])).await.unwrap_err();
        assert_rollback(&error);
        assert_eq!(snapshot(&fixture.pool()), vec![0; 9], "{trigger}");
    }
}

#[tokio::test]
async fn restart_compares_all_immutable_manifest_fields_after_seed_disable() {
    let fixture = Fixture::new();
    let original = input(vec![seed("alice")]);
    let manifest = ValidatedBootstrapManifest::validate(original.clone(), &catalog()).unwrap();
    let first = fixture.initialize(manifest.clone()).await.unwrap();
    let loaded = fixture.load(manifest.clone()).await.unwrap();
    loaded
        .store
        .revoke(
            context("root"),
            RevokeRequest {
                request_id: "disable-seed".into(),
                rationale: "disable configured seed".into(),
                grant_ids: vec![first.initialization.seeds[0].record_id],
            },
        )
        .await
        .unwrap();
    let before = snapshot(&fixture.pool());
    let before_rows = stored_rows(&fixture.pool());
    let mut alternatives = Vec::new();
    let mut changed = original.clone();
    changed.administrator = principal("different-root");
    alternatives.push(changed);
    let mut changed = original.clone();
    changed.configuration_source.push_str("-different");
    alternatives.push(changed);
    let mut changed = original.clone();
    changed.marker_namespace.push_str("-different");
    alternatives.push(changed);
    let mut changed = original.clone();
    changed.anchor_namespace.push_str("-different");
    alternatives.push(changed);
    let mut changed = original.clone();
    changed.seeds[0].grantee = principal("bob");
    alternatives.push(changed);
    let mut changed = original.clone();
    changed.slots[0].validity.expires_at = Some(Timestamp::from_micros(200));
    alternatives.push(changed);
    let mut changed = original.clone();
    changed.slots[0].slot = "renamed-source".into();
    changed.seeds[0].parent = GrantParent::BootstrapSlot("renamed-source".into());
    alternatives.push(changed);
    let mut changed = original.clone();
    let mut narrowed = scope();
    narrowed.permissions[0].subject.selector =
        SubjectSelector::Exact(vec![ExactBytes(b"one".to_vec())]);
    let narrowed_rights = vec![
        RightSpec::RecordObservation(narrowed.clone()),
        RightSpec::GrantAdmin(AdminScope {
            namespace: "research".into(),
            target: AdminTarget::RecordObservation(narrowed),
        }),
    ];
    changed.slots[0].rights = narrowed_rights.clone();
    changed.seeds[0].rights = narrowed_rights;
    alternatives.push(changed);
    let mut changed = original.clone();
    changed.verification_key_refs.push(KeyRef {
        id: "old-key".into(),
        generation: 2,
    });
    alternatives.push(changed);
    for changed in alternatives {
        let changed = ValidatedBootstrapManifest::validate(changed, &catalog()).unwrap();
        let mut configured = fixture.runtime();
        configured.keys = Arc::new(
            SuppliedKeys::provision(vec![
                ProvisionedKey::new(key_ref(), [1; 32], [2; 32]).unwrap(),
                ProvisionedKey::new(
                    KeyRef {
                        id: "old-key".into(),
                        generation: 2,
                    },
                    [3; 32],
                    [4; 32],
                )
                .unwrap(),
            ])
            .unwrap(),
        );
        assert!(matches!(
            AuthorityStore::load_active(fixture.sql.clone(), changed, configured).await,
            Err(AuthorityError::Refused(
                AuthorityRefusal::InitializationConflict
            ))
        ));
        assert_eq!(snapshot(&fixture.pool()), before);
        assert_eq!(stored_rows(&fixture.pool()), before_rows);
    }
    let restarted = fixture.load(manifest.clone()).await.unwrap();
    assert_eq!(restarted.current_revision, 2);
    assert!(!effective(&restarted.store, first.initialization.seeds[0].record_id).await);
    assert_eq!(
        fixture.initialize(manifest).await.unwrap().initialization,
        first.initialization
    );
    assert_eq!(snapshot(&fixture.pool()), before);
    assert_eq!(stored_rows(&fixture.pool()), before_rows);
}

#[tokio::test]
async fn initialization_history_rechecks_current_admission_and_namespace_scope() {
    let fixture = Fixture::new();
    let manifest = manifest(vec![seed("alice")]);
    let first = fixture.initialize(manifest.clone()).await.unwrap();
    let before_rows = stored_rows(&fixture.pool());
    let history = AuthorityStore::load_history(fixture.sql.clone(), manifest, fixture.runtime())
        .await
        .unwrap();
    struct RejectedContext;
    impl AdmissionContext for RejectedContext {
        fn as_any(&self) -> &dyn Any {
            self
        }
    }
    assert!(matches!(
        history
            .read_initialization_change(
                Arc::new(RejectedContext),
                first.initialization.marker_id,
                first.initialization.marker_event_id
            )
            .await,
        Err(AuthorityError::Refused(AuthorityRefusal::Denied(_)))
    ));
    let outside: Arc<dyn AdmissionContext> =
        Arc::new(Context(principal("root"), vec!["other".into()]));
    assert!(matches!(
        history
            .read_initialization_change(
                outside,
                first.initialization.marker_id,
                first.initialization.marker_event_id
            )
            .await,
        Err(AuthorityError::Refused(AuthorityRefusal::Denied(_)))
    ));
    assert!(matches!(
        history
            .read_initialization_change(
                context("root"),
                first.initialization.marker_id,
                first.initialization.marker_event_id
            )
            .await
            .unwrap(),
        BootstrapHistoricalChange::MarkerInitialize { .. }
    ));
    assert!(history
        .read_initialization_change(
            context("root"),
            Uuid::from_u128(999),
            first.initialization.seeds[0].event_id
        )
        .await
        .is_err());
    assert_eq!(stored_rows(&fixture.pool()), before_rows);
}

fn summary_json(summary: &InitializationSummary) -> serde_json::Value {
    serde_json::json!({
        "store":summary.store_id, "epoch":summary.epoch_id, "marker":summary.marker_id,
        "anchor":summary.anchor_id, "marker_event":summary.marker_event_id,
        "anchor_event":summary.anchor_event_id, "initial_revision":summary.initial_revision,
        "seeds":summary.seeds.iter().map(|seed| serde_json::json!({"id":seed.record_id,"event":seed.event_id,"validity":seed.validity})).collect::<Vec<_>>()
    })
}
fn cold_manifest(phase: &str) -> ValidatedBootstrapManifest {
    let mut configured = input(match phase {
        "empty" => vec![],
        "rollback" => vec![seed("alice"), seed("bob")],
        _ => vec![seed("alice")],
    });
    if phase == "race-other" {
        configured.configuration_source = "race-other-configuration".into();
    }
    ValidatedBootstrapManifest::validate(configured, &catalog()).unwrap()
}

#[tokio::test]
async fn cold_process_restart_preserves_empty_disabled_retired_and_rolled_back_authority() {
    const EXACT: &str = "authority_store::bootstrap_tests::cold_process_restart_preserves_empty_disabled_retired_and_rolled_back_authority";
    if let Ok(path) = std::env::var("KHIVE_BOOTSTRAP_TEST_CHILD_DB") {
        let phase = std::env::var("KHIVE_BOOTSTRAP_TEST_PHASE").unwrap();
        let expected: serde_json::Value =
            serde_json::from_str(&std::env::var("KHIVE_BOOTSTRAP_TEST_EXPECTED").unwrap()).unwrap();
        let path = std::path::Path::new(&path);
        let directory = path.parent().unwrap();
        let backend = StorageBackend::sqlite_for_test_with_journal_mode_in(
            path,
            true,
            Duration::from_secs(5),
            directory.join("locks"),
        )
        .unwrap();
        backend.prepare_core_schema().unwrap();
        let sql = Arc::new(SqlBridge::new(backend.pool_arc(), false));
        let clock = Arc::new(Clock::new());
        let before = snapshot(&backend.pool_arc());
        let before_rows = stored_rows(&backend.pool_arc());
        assert_eq!(serde_json::json!(before), expected["counts"]);
        assert_eq!(
            serde_json::json!(before_rows),
            expected["rows"],
            "cold child must observe every exact parent row"
        );
        let initialized =
            AuthorityStore::initialize(sql.clone(), cold_manifest(&phase), runtime(clock.clone()))
                .await
                .unwrap();
        assert_eq!(initialized.changed, phase == "rollback");
        if phase != "rollback" {
            assert_eq!(
                summary_json(&initialized.initialization),
                expected["initialization"]
            );
            assert_eq!(
                clock.reads.load(Ordering::SeqCst),
                0,
                "equal cold retry must not consult clock or rederive seeds"
            );
            assert_eq!(snapshot(&backend.pool_arc()), before);
            assert_eq!(stored_rows(&backend.pool_arc()), before_rows);
        }
        let summary = &initialized.initialization;
        if phase == "retired" {
            assert!(matches!(
                AuthorityStore::load_active(
                    sql.clone(),
                    cold_manifest(&phase),
                    runtime(clock.clone())
                )
                .await,
                Err(AuthorityError::Refused(AuthorityRefusal::NotEffective))
            ));
        } else {
            let loaded = AuthorityStore::load_active(
                sql.clone(),
                cold_manifest(&phase),
                runtime(clock.clone()),
            )
            .await
            .unwrap();
            assert_eq!(loaded.initialization, *summary);
            if phase == "empty" {
                assert!(summary.seeds.is_empty());
                assert_eq!(loaded.current_revision, 1);
            } else if phase == "disabled" {
                assert!(!effective(&loaded.store, summary.seeds[0].record_id).await);
                assert!(loaded
                    .store
                    .grant(
                        context("alice"),
                        grant_request(
                            "disabled-cold-child",
                            "other",
                            GrantParent::Grant(summary.seeds[0].record_id)
                        )
                    )
                    .await
                    .is_err());
                assert_eq!(snapshot(&backend.pool_arc()), before);
                assert_eq!(stored_rows(&backend.pool_arc()), before_rows);
                let replacement = loaded
                    .store
                    .grant(
                        context("root"),
                        grant_request(
                            "configured-new-producer",
                            "replacement",
                            GrantParent::BootstrapSlot("source".into()),
                        ),
                    )
                    .await
                    .unwrap();
                assert!(effective(&loaded.store, replacement.targets[0].record_id).await);
                assert!(
                    !effective(&loaded.store, summary.seeds[0].record_id).await,
                    "configured new issuance never restores the old seed"
                );
            } else {
                let parent = summary
                    .seeds
                    .iter()
                    .find(|seed| {
                        let reader = backend.pool_arc();
                        let reader = reader.reader().unwrap();
                        let bytes: Vec<u8> = reader
                            .query_row(
                                "SELECT principal FROM authority_records WHERE record_id=?1",
                                [seed.record_id.to_string()],
                                |row| row.get(0),
                            )
                            .unwrap();
                        bytes == canonical_principal(&principal("alice")).unwrap()
                    })
                    .unwrap();
                assert!(effective(&loaded.store, parent.record_id).await);
                let child = loaded
                    .store
                    .grant(
                        context("alice"),
                        grant_request("cold-child", "next", GrantParent::Grant(parent.record_id)),
                    )
                    .await
                    .unwrap();
                assert!(effective(&loaded.store, child.targets[0].record_id).await);
                let history = loaded
                    .store
                    .read_change(
                        context("root"),
                        child.action_id.unwrap(),
                        child.targets[0].change_event_id,
                    )
                    .await
                    .unwrap();
                assert_eq!(history.former_rights.len(), 2);
            }
        }
        let history =
            AuthorityStore::load_history(sql.clone(), cold_manifest(&phase), runtime(clock))
                .await
                .unwrap();
        assert_eq!(history.initialization(), summary);
        assert_eq!(history.initialization().initial_revision, 1);
        assert!(matches!(
            history
                .read_initialization_change(
                    context("root"),
                    summary.marker_id,
                    summary.marker_event_id
                )
                .await
                .unwrap(),
            BootstrapHistoricalChange::MarkerInitialize { .. }
        ));
        assert!(matches!(
            history
                .read_initialization_change(
                    context("root"),
                    summary.marker_id,
                    summary.anchor_event_id
                )
                .await
                .unwrap(),
            BootstrapHistoricalChange::AnchorInitialize { .. }
        ));
        for seed in &summary.seeds {
            let change = history
                .read_initialization_change(context("root"), summary.marker_id, seed.event_id)
                .await
                .unwrap();
            let BootstrapHistoricalChange::ImportedSeed {
                record,
                rights,
                terminal_state,
                validity,
                ..
            } = change
            else {
                panic!("cold imported history")
            };
            assert_eq!(record.id, seed.record_id);
            assert_eq!(rights.len(), 2);
            assert_eq!(validity, seed.validity);
            if phase == "disabled" {
                assert_eq!(terminal_state, Some(RecordedState::Disabled));
            }
        }
        if !expected["terminal"].is_null() {
            let terminal = &expected["terminal"];
            let action = Uuid::parse_str(terminal["action"].as_str().unwrap()).unwrap();
            let event = Uuid::parse_str(terminal["event"].as_str().unwrap()).unwrap();
            let change = history
                .read_change(context("root"), action, event)
                .await
                .unwrap();
            if phase == "retired" {
                assert_eq!(change.terminal_state, Some(RecordedState::Retired));
                assert!(change.former_rights.is_empty());
                assert_eq!(history.current_revision(), 2);
                assert_eq!(
                    snapshot(&backend.pool_arc()),
                    before,
                    "read-only cold history must not reactivate or write"
                );
                assert_eq!(stored_rows(&backend.pool_arc()), before_rows);
            } else {
                assert_eq!(change.terminal_state, Some(RecordedState::Disabled));
                assert_eq!(change.former_rights.len(), 2);
                assert_eq!(history.current_revision(), 3);
                assert_eq!(
                    snapshot(&backend.pool_arc())[5],
                    before[5],
                    "disabled seed terminal transition remains intact after new issuance"
                );
            }
        }
        assert_immutable_rows_preserved(&before_rows, &stored_rows(&backend.pool_arc()));
        std::fs::write(
            directory.join("bootstrap-child-verified"),
            serde_json::to_vec(&serde_json::json!({"phase":phase,"pid":std::process::id()}))
                .unwrap(),
        )
        .unwrap();
        return;
    }
    for phase in [
        "healthy", "race", "empty", "disabled", "retired", "rollback",
    ] {
        let fixture = Fixture::new();
        let mut child_phase = phase;
        let mut expected = serde_json::json!({"initialization":null,"terminal":null});
        if phase == "rollback" {
            fixture.pool().writer().unwrap().transaction(|conn| {
                conn.execute_batch("CREATE TRIGGER reject_cold_second_import BEFORE INSERT ON events WHEN NEW.verb='import' AND (SELECT COUNT(*) FROM events WHERE verb='import')>=1 BEGIN SELECT RAISE(ABORT,'cold second import failure'); END;")?; Ok(())
            }).unwrap();
            let error = fixture.initialize(cold_manifest(phase)).await.unwrap_err();
            assert_rollback(&error);
            assert_eq!(snapshot(&fixture.pool()), vec![0; 9]);
            fixture
                .pool()
                .writer()
                .unwrap()
                .transaction(|conn| {
                    conn.execute_batch("DROP TRIGGER reject_cold_second_import;")?;
                    Ok(())
                })
                .unwrap();
        } else {
            let first = if phase == "race" {
                let (a, b) = tokio::join!(
                    fixture.initialize(cold_manifest("race")),
                    fixture.initialize(cold_manifest("race-other"))
                );
                match (a, b) {
                    (
                        Ok(winner),
                        Err(AuthorityError::Refused(AuthorityRefusal::InitializationConflict)),
                    ) => winner,
                    (
                        Err(AuthorityError::Refused(AuthorityRefusal::InitializationConflict)),
                        Ok(winner),
                    ) => {
                        child_phase = "race-other";
                        winner
                    }
                    _ => panic!("one divergent race winner must survive cold restart"),
                }
            } else {
                fixture.initialize(cold_manifest(phase)).await.unwrap()
            };
            expected["initialization"] = summary_json(&first.initialization);
            if phase == "disabled" || phase == "retired" {
                let loaded = fixture.load(cold_manifest(phase)).await.unwrap();
                let terminal = if phase == "disabled" {
                    loaded
                        .store
                        .revoke(
                            context("root"),
                            RevokeRequest {
                                request_id: "cold-disable".into(),
                                rationale: "disable seed before restart".into(),
                                grant_ids: vec![first.initialization.seeds[0].record_id],
                            },
                        )
                        .await
                        .unwrap()
                } else {
                    loaded
                        .store
                        .retire_anchor(
                            context("root"),
                            RetireAnchorRequest {
                                request_id: "cold-retire".into(),
                                rationale: "retire root before restart".into(),
                                root_anchor_id: first.initialization.anchor_id,
                            },
                        )
                        .await
                        .unwrap()
                };
                expected["terminal"] = serde_json::json!({"action":terminal.action_id.unwrap(),"event":terminal.targets[0].change_event_id});
            }
        }
        expected["counts"] = serde_json::json!(snapshot(&fixture.pool()));
        expected["rows"] = serde_json::json!(stored_rows(&fixture.pool()));
        let join = fixture.pool().take_writer_task_join();
        let Fixture {
            directory,
            backend,
            sql,
            clock,
        } = fixture;
        drop(sql);
        drop(backend);
        drop(clock);
        if let Some(join) = join {
            join.await.unwrap();
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", EXACT, "--nocapture"])
            .env(
                "KHIVE_BOOTSTRAP_TEST_CHILD_DB",
                directory.path().join("authority.db"),
            )
            .env("KHIVE_BOOTSTRAP_TEST_PHASE", child_phase)
            .env("KHIVE_BOOTSTRAP_TEST_EXPECTED", expected.to_string())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{phase}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        let verified: serde_json::Value = serde_json::from_slice(
            &std::fs::read(directory.path().join("bootstrap-child-verified")).unwrap(),
        )
        .unwrap();
        assert_eq!(verified["phase"], child_phase);
        assert_ne!(
            verified["pid"].as_u64().unwrap(),
            u64::from(std::process::id()),
            "cold proof must execute a fresh OS process"
        );
    }
}

#[tokio::test]
async fn empty_seed_configuration_issues_first_producer_without_implicit_downstream_bootstrap() {
    let fixture = Fixture::new();
    let manifest = manifest(vec![]);
    fixture.initialize(manifest.clone()).await.unwrap();
    let loaded = fixture.load(manifest).await.unwrap();
    let first = loaded
        .store
        .grant(
            context("root"),
            grant_request(
                "first-producer",
                "alice",
                GrantParent::BootstrapSlot("source".into()),
            ),
        )
        .await
        .unwrap();
    assert!(effective(&loaded.store, first.targets[0].record_id).await);
    let mut admin_request = grant_request(
        "admin-only",
        "administrator",
        GrantParent::BootstrapSlot("source".into()),
    );
    admin_request.grants[0].rights = vec![mixed().remove(1)];
    let admin = loaded
        .store
        .grant(context("root"), admin_request)
        .await
        .unwrap();
    let before = snapshot(&fixture.pool());
    let before_rows = stored_rows(&fixture.pool());
    for parent in [
        GrantParent::BootstrapSlot("source".into()),
        GrantParent::Grant(admin.targets[0].record_id),
    ] {
        assert!(loaded
            .store
            .grant(
                context("administrator"),
                grant_request("no-implicit-source", "bob", parent)
            )
            .await
            .is_err());
    }
    assert_eq!(snapshot(&fixture.pool()), before);
    assert_eq!(stored_rows(&fixture.pool()), before_rows);
}

#[tokio::test]
async fn duplicate_seed_intents_roundtrip_as_distinct_stable_records_and_events() {
    let fixture = Fixture::new();
    let manifest = manifest(vec![seed("alice"), seed("alice")]);
    let first = fixture.initialize(manifest.clone()).await.unwrap();
    let seeds = &first.initialization.seeds;
    assert_eq!(seeds.len(), 2);
    assert_ne!(seeds[0].record_id, seeds[1].record_id);
    assert_ne!(seeds[0].event_id, seeds[1].event_id);
    let before = stored_rows(&fixture.pool());
    assert_eq!(snapshot(&fixture.pool()), vec![1, 1, 1, 3, 4, 0, 0, 4, 4]);
    let retry = fixture.initialize(manifest.clone()).await.unwrap();
    assert!(!retry.changed);
    assert_eq!(retry.initialization, first.initialization);
    let loaded = fixture.load(manifest).await.unwrap();
    assert_eq!(loaded.initialization, first.initialization);
    for seed in seeds {
        let change = loaded
            .store
            .read_initialization_change(
                context("root"),
                first.initialization.marker_id,
                seed.event_id,
            )
            .await
            .unwrap();
        assert!(
            matches!(change, BootstrapHistoricalChange::ImportedSeed { record, grantee, .. } if record.id == seed.record_id && grantee == principal("alice"))
        );
    }
    assert_eq!(stored_rows(&fixture.pool()), before);
}

#[tokio::test]
async fn source_scope_only_difference_conflicts_with_identical_narrow_seed_intent() {
    let fixture = Fixture::new();
    let mut narrow = scope();
    narrow.permissions[0].subject.selector =
        SubjectSelector::Exact(vec![ExactBytes(b"one".to_vec())]);
    let narrow_rights = vec![
        RightSpec::RecordObservation(narrow.clone()),
        RightSpec::GrantAdmin(AdminScope {
            namespace: "research".into(),
            target: AdminTarget::RecordObservation(narrow),
        }),
    ];
    let mut original = input(vec![seed("alice")]);
    original.seeds[0].rights = narrow_rights.clone();
    let original_manifest =
        ValidatedBootstrapManifest::validate(original.clone(), &catalog()).unwrap();
    let first = fixture.initialize(original_manifest.clone()).await.unwrap();
    let loaded = fixture.load(original_manifest).await.unwrap();
    loaded
        .store
        .revoke(
            context("root"),
            RevokeRequest {
                request_id: "disable-narrow-seed".into(),
                rationale: "disable original seed before source comparison".into(),
                grant_ids: vec![first.initialization.seeds[0].record_id],
            },
        )
        .await
        .unwrap();
    let before_counts = snapshot(&fixture.pool());
    let before_rows = stored_rows(&fixture.pool());
    let mut expected = original;
    // Only the source slot changes. The already-narrow seed intent, source name,
    // key history, identities, validity and provenance are identical.
    expected.slots[0].rights = narrow_rights;
    let expected = ValidatedBootstrapManifest::validate(expected, &catalog()).unwrap();
    assert!(matches!(
        fixture.initialize(expected).await,
        Err(AuthorityError::Refused(
            AuthorityRefusal::InitializationConflict
        ))
    ));
    assert_eq!(snapshot(&fixture.pool()), before_counts);
    assert_eq!(stored_rows(&fixture.pool()), before_rows);
}
