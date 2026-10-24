use super::{
    AdministrationOperation, AdmissionContext, AdmissionVerifier, AuthorityClock,
    AuthorityConfiguration, AuthorityError, AuthorityRefusal, AuthorityResult, AuthorityStore,
    BodyDigest, GrantItem, GrantParent, HistoricalChange, PublicValidity, RecordRef, RecordedState,
};
use crate::durable::{
    canonical_principal, canonical_rights, require_key_reference, AuthenticatedEnvelope,
    BootstrapSource, ClosedCatalog, ContractDigest, CreationOperation, EnvelopeDraft,
    EnvelopeError, EnvelopeIssuer, EnvelopeVerifier, IssuanceAssurance, KeyRef, Principal,
    RightSpec, SuppliedKeys, Validity, MAX_COLLECTION_ITEMS,
};
use khive_db::authority::{
    self, AuthorityChange, AuthorityEpoch, AuthorityRecord, AuthorityTransaction, ChangeKind,
    RecordKind, MAX_AUTHORITY_MANIFEST_BYTES,
};
use khive_storage::{event::Event, SqlAccess, StorageError, StorageResult};
use khive_types::{EventKind, EventOutcome, SubstrateKind, Timestamp};
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct BootstrapManifestInput {
    pub store_id: Uuid,
    pub administrator: Principal,
    pub marker_namespace: String,
    pub anchor_namespace: String,
    pub identity_profile: IssuanceAssurance,
    pub issuance_key: KeyRef,
    pub verification_key_refs: Vec<KeyRef>,
    pub configuration_source: String,
    pub slots: Vec<BootstrapSlotInput>,
    pub seeds: Vec<GrantItem>,
}

#[derive(Clone, Debug)]
pub struct BootstrapSlotInput {
    pub slot: String,
    pub namespace: String,
    pub rights: Vec<RightSpec>,
    pub validity: Validity,
}

#[derive(Clone, Debug)]
pub struct ValidatedBootstrapManifest {
    input: BootstrapManifestInput,
    canonical: Vec<u8>,
    digest: BodyDigest,
    seed_intents: Vec<Vec<u8>>,
}

#[derive(Clone)]
pub struct BootstrapRuntime {
    pub service_id: Uuid,
    pub keys: Arc<SuppliedKeys>,
    pub catalog: Arc<ClosedCatalog>,
    pub clock: Arc<dyn AuthorityClock>,
    pub admission: Arc<dyn AdmissionVerifier>,
    pub construction_implementation: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitializationSeed {
    pub record_id: Uuid,
    pub event_id: Uuid,
    pub validity: PublicValidity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitializationSummary {
    pub store_id: Uuid,
    pub epoch_id: Uuid,
    pub marker_id: Uuid,
    pub anchor_id: Uuid,
    pub marker_event_id: Uuid,
    pub anchor_event_id: Uuid,
    pub initial_revision: i64,
    pub seeds: Vec<InitializationSeed>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitializationResult {
    pub changed: bool,
    pub initialization: InitializationSummary,
}

pub struct LoadedAuthority {
    pub store: AuthorityStore,
    pub current_revision: i64,
    pub initialization: InitializationSummary,
}

/// Read-only access to authenticated authority history, including retired stores.
pub struct AuthorityHistory {
    store: AuthorityStore,
    current_revision: i64,
    initialization: InitializationSummary,
}
impl AuthorityHistory {
    /// The revision observed when this history handle was loaded.
    pub fn current_revision(&self) -> i64 {
        self.current_revision
    }
    pub fn initialization(&self) -> &InitializationSummary {
        &self.initialization
    }
    pub async fn read_change(
        &self,
        context: Arc<dyn AdmissionContext>,
        action_id: Uuid,
        event_id: Uuid,
    ) -> AuthorityResult<HistoricalChange> {
        self.store.read_change(context, action_id, event_id).await
    }
    pub async fn read_initialization_change(
        &self,
        context: Arc<dyn AdmissionContext>,
        marker_action_id: Uuid,
        event_id: Uuid,
    ) -> AuthorityResult<BootstrapHistoricalChange> {
        self.store
            .read_initialization_change(context, marker_action_id, event_id)
            .await
    }
}

#[derive(Clone, Debug)]
pub enum BootstrapHistoricalChange {
    MarkerInitialize {
        event: Event,
        epoch_id: Uuid,
        marker_id: Uuid,
        administrator: Principal,
        manifest_digest: BodyDigest,
    },
    AnchorInitialize {
        event: Event,
        anchor: RecordRef,
        administrator: Principal,
        manifest_digest: BodyDigest,
    },
    ImportedSeed {
        event: Event,
        record: RecordRef,
        grantee: Principal,
        rights: Vec<RightSpec>,
        validity: PublicValidity,
        terminal_state: Option<RecordedState>,
    },
}

fn invalid(message: &'static str) -> super::AuthorityError {
    AuthorityRefusal::InvalidInput(message).into()
}
fn bounded_collection(length: usize) -> AuthorityResult<()> {
    if length > MAX_COLLECTION_ITEMS {
        return Err(EnvelopeError::OverLimit {
            field: "bootstrap collection",
            measured: length,
            bound: MAX_COLLECTION_ITEMS,
        }
        .into());
    }
    Ok(())
}

impl ValidatedBootstrapManifest {
    pub fn validate(
        mut input: BootstrapManifestInput,
        catalog: &ClosedCatalog,
    ) -> AuthorityResult<Self> {
        input.administrator.validate()?;
        if input.administrator.leg.is_some()
            || input.marker_namespace.is_empty()
            || input.anchor_namespace.is_empty()
            || input.configuration_source.is_empty()
            || input.issuance_key.id.is_empty()
        {
            return Err(invalid("invalid bootstrap administrator or provenance"));
        }
        bounded_collection(input.slots.len())?;
        bounded_collection(input.seeds.len())?;
        bounded_collection(input.verification_key_refs.len())?;
        input.verification_key_refs.sort();
        if input
            .verification_key_refs
            .iter()
            .any(|key| key.id.is_empty())
            || input
                .verification_key_refs
                .windows(2)
                .any(|pair| pair[0] == pair[1])
            || !input.verification_key_refs.contains(&input.issuance_key)
        {
            return Err(invalid(
                "bootstrap key references must be unique and include issuance",
            ));
        }
        input.slots.sort_by(|a, b| a.slot.cmp(&b.slot));
        if input
            .slots
            .windows(2)
            .any(|pair| pair[0].slot == pair[1].slot)
        {
            return Err(invalid("duplicate bootstrap slot"));
        }
        for slot in &input.slots {
            if slot.slot.is_empty() || slot.namespace.is_empty() {
                return Err(invalid("empty bootstrap slot or namespace"));
            }
            slot.validity.validate()?;
            let _ = catalog.validate_source_rights(
                &slot.rights,
                &slot.namespace,
                &input.administrator,
            )?;
        }
        for slot in &input.slots {
            let ordinary = slot
                .rights
                .iter()
                .filter(|right| matches!(right, RightSpec::RecordObservation(_)));
            for right in ordinary {
                for permission in super::authorize::permission_scope(right)? {
                    if !input.slots.iter().any(|admin| {
                        admin.namespace == slot.namespace
                            && super::authorize::covers(
                                catalog,
                                &admin.rights,
                                &slot.namespace,
                                permission,
                            )
                            .is_some()
                    }) {
                        return Err(invalid(
                            "bootstrap ordinary scope needs explicit complete administration",
                        ));
                    }
                }
            }
        }
        let mut seed_intents = Vec::with_capacity(input.seeds.len());
        for seed in &input.seeds {
            seed.grantee.validate()?;
            if seed.namespace.is_empty()
                || (seed.grantee.leg.is_some() && seed.grantee.parent() != input.administrator)
            {
                return Err(invalid("invalid bootstrap seed identity"));
            }
            let _ = catalog.validate_rights(&seed.rights, &seed.namespace, &seed.grantee)?;
            let GrantParent::BootstrapSlot(name) = &seed.parent else {
                return Err(invalid("bootstrap seed requires one configured slot"));
            };
            let slot = input
                .slots
                .iter()
                .find(|slot| slot.slot == *name)
                .ok_or_else(|| invalid("bootstrap seed slot missing"))?;
            if slot.namespace != seed.namespace
                || !catalog.covers_rights(&slot.rights, &seed.rights)
            {
                return Err(invalid("bootstrap source does not cover complete seed"));
            }
            seed_intents.push(seed_intent(seed)?);
        }
        let mut seeds = input
            .seeds
            .into_iter()
            .zip(seed_intents)
            .collect::<Vec<_>>();
        seeds.sort_by(|a, b| a.1.cmp(&b.1));
        let (normalized_seeds, seed_intents): (Vec<_>, Vec<_>) = seeds.into_iter().unzip();
        input.seeds = normalized_seeds;
        let mut out = Canonical::new(Some(MAX_AUTHORITY_MANIFEST_BYTES));
        out.put(b"khive.authority.bootstrap-manifest\0\x00\x01")?;
        out.put(input.store_id.as_bytes())?;
        out.field(&canonical_principal(&input.administrator)?)?;
        out.field(input.marker_namespace.as_bytes())?;
        out.field(input.anchor_namespace.as_bytes())?;
        match input.identity_profile {
            IssuanceAssurance::ConfiguredLocal => out.put(&[0])?,
        }
        out.key(&input.issuance_key)?;
        out.count(input.verification_key_refs.len())?;
        for key in &input.verification_key_refs {
            out.key(key)?;
        }
        out.field(input.configuration_source.as_bytes())?;
        out.count(input.slots.len())?;
        for slot in &input.slots {
            out.field(slot.slot.as_bytes())?;
            out.field(slot.namespace.as_bytes())?;
            out.field(&canonical_rights(&slot.rights)?)?;
            out.validity(slot.validity)?;
        }
        out.count(seed_intents.len())?;
        for seed in &seed_intents {
            out.field(seed)?;
        }
        let canonical = out.finish()?;
        let digest = BodyDigest(*blake3::hash(&canonical).as_bytes());
        Ok(Self {
            input,
            canonical,
            digest,
            seed_intents,
        })
    }
    pub fn canonical_bytes(&self) -> &[u8] {
        &self.canonical
    }
    pub fn digest(&self) -> BodyDigest {
        self.digest.clone()
    }
}

fn seed_intent(seed: &GrantItem) -> AuthorityResult<Vec<u8>> {
    let GrantParent::BootstrapSlot(slot) = &seed.parent else {
        return Err(invalid("bootstrap seed requires one configured slot"));
    };
    let mut out = Canonical::new(None);
    out.field(&canonical_principal(&seed.grantee)?)?;
    out.field(seed.namespace.as_bytes())?;
    out.field(slot.as_bytes())?;
    out.field(&canonical_rights(&seed.rights)?)?;
    out.finish()
}

struct Canonical {
    bytes: Option<Vec<u8>>,
    measured: usize,
    bound: Option<usize>,
}
impl Canonical {
    fn new(bound: Option<usize>) -> Self {
        Self {
            bytes: Some(Vec::new()),
            measured: 0,
            bound,
        }
    }
    fn put(&mut self, bytes: &[u8]) -> AuthorityResult<()> {
        self.measured = self
            .measured
            .checked_add(bytes.len())
            .ok_or_else(|| invalid("bootstrap canonical length overflow"))?;
        if self.bound.is_some_and(|bound| self.measured > bound) {
            self.bytes = None;
        }
        if let Some(out) = &mut self.bytes {
            out.extend_from_slice(bytes);
        }
        Ok(())
    }
    fn field(&mut self, bytes: &[u8]) -> AuthorityResult<()> {
        let length =
            u32::try_from(bytes.len()).map_err(|_| invalid("bootstrap field length overflow"))?;
        self.put(&length.to_be_bytes())?;
        self.put(bytes)
    }
    fn count(&mut self, value: usize) -> AuthorityResult<()> {
        let count = u32::try_from(value).map_err(|_| invalid("bootstrap count overflow"))?;
        self.put(&count.to_be_bytes())
    }
    fn key(&mut self, key: &KeyRef) -> AuthorityResult<()> {
        self.field(key.id.as_bytes())?;
        self.put(&key.generation.to_be_bytes())
    }
    fn validity(&mut self, validity: Validity) -> AuthorityResult<()> {
        self.put(&validity.not_before.as_micros().to_be_bytes())?;
        match validity.expires_at {
            Some(expiry) => {
                self.put(&[1])?;
                self.put(&expiry.as_micros().to_be_bytes())
            }
            None => self.put(&[0]),
        }
    }
    fn finish(self) -> AuthorityResult<Vec<u8>> {
        self.bytes.ok_or_else(|| {
            EnvelopeError::OverLimit {
                field: "bootstrap manifest",
                measured: self.measured,
                bound: self.bound.unwrap_or(MAX_AUTHORITY_MANIFEST_BYTES),
            }
            .into()
        })
    }
}

fn reject(message: &'static str) -> AuthorityError {
    AuthorityRefusal::Reject(message).into()
}
fn storage(message: impl Into<String>) -> StorageError {
    StorageError::Internal(message.into())
}
fn serialized<T: Serialize + ?Sized>(value: &T) -> StorageResult<Value> {
    serde_json::to_value(value)
        .map_err(|error| storage(format!("bootstrap history serialization failed: {error}")))
}
fn public_validity(validity: Validity) -> PublicValidity {
    PublicValidity {
        not_before: validity.not_before.as_micros(),
        expires_at: validity.expires_at.map(|expiry| expiry.as_micros()),
    }
}
fn validate_runtime(
    manifest: &ValidatedBootstrapManifest,
    runtime: &BootstrapRuntime,
) -> AuthorityResult<()> {
    if runtime.construction_implementation.is_empty() {
        return Err(invalid("empty construction implementation"));
    }
    for key in &manifest.input.verification_key_refs {
        require_key_reference(&runtime.keys, key)?;
    }
    Ok(())
}
fn source_descriptors(
    manifest: &ValidatedBootstrapManifest,
    epoch: &AuthorityEpoch,
    anchor_id: Uuid,
) -> Vec<BootstrapSource> {
    manifest
        .input
        .slots
        .iter()
        .map(|slot| BootstrapSource {
            store_id: epoch.store_id,
            epoch_id: epoch.epoch_id,
            root_anchor_id: anchor_id,
            owner: manifest.input.administrator.clone(),
            namespace: slot.namespace.clone(),
            slot: slot.slot.clone(),
            manifest_digest: ContractDigest(epoch.manifest_digest),
            rights: slot.rights.clone(),
            validity: slot.validity,
        })
        .collect()
}
fn configuration(
    manifest: &ValidatedBootstrapManifest,
    runtime: &BootstrapRuntime,
    epoch: AuthorityEpoch,
    anchor: AuthorityRecord,
) -> AuthorityConfiguration {
    let bootstrap_sources = source_descriptors(manifest, &epoch, anchor.id);
    AuthorityConfiguration {
        service_id: runtime.service_id,
        epoch,
        anchor,
        boot_principal: manifest.input.administrator.clone(),
        bootstrap_sources,
        issuance_key: manifest.input.issuance_key.clone(),
        keys: runtime.keys.clone(),
        catalog: runtime.catalog.clone(),
        clock: runtime.clock.clone(),
        admission: runtime.admission.clone(),
    }
}
fn anchor_body(
    manifest: &ValidatedBootstrapManifest,
    epoch: &AuthorityEpoch,
    anchor_id: Uuid,
) -> AuthorityResult<Vec<u8>> {
    let mut out = Canonical::new(None);
    out.put(b"khive.authority.bootstrap-anchor\0\x00\x01")?;
    for id in [epoch.store_id, epoch.epoch_id, epoch.marker_id, anchor_id] {
        out.put(id.as_bytes())?;
    }
    out.field(&canonical_principal(&manifest.input.administrator)?)?;
    out.field(manifest.input.anchor_namespace.as_bytes())?;
    let micros =
        u64::try_from(epoch.created_at).map_err(|_| reject("negative initialization time"))?;
    out.put(&micros.to_be_bytes())?;
    out.put(&epoch.manifest_digest)?;
    out.finish()
}

struct SeedIds {
    record: Uuid,
    issuance: Uuid,
    event: Uuid,
}
struct InitializationIds {
    epoch: Uuid,
    marker: Uuid,
    anchor: Uuid,
    marker_event: Uuid,
    anchor_event: Uuid,
    seeds: Vec<SeedIds>,
}
impl InitializationIds {
    fn allocate(seeds: usize) -> Self {
        Self {
            epoch: Uuid::new_v4(),
            marker: Uuid::new_v4(),
            anchor: Uuid::new_v4(),
            marker_event: Uuid::new_v4(),
            anchor_event: Uuid::new_v4(),
            seeds: (0..seeds)
                .map(|_| SeedIds {
                    record: Uuid::new_v4(),
                    issuance: Uuid::new_v4(),
                    event: Uuid::new_v4(),
                })
                .collect(),
        }
    }
}
struct PreparedSeed {
    record: AuthorityRecord,
    grant: EnvelopeDraft,
    event_id: Uuid,
}
struct PreparedInitialization {
    epoch: AuthorityEpoch,
    anchor: AuthorityRecord,
    seeds: Vec<PreparedSeed>,
    summary: InitializationSummary,
}
enum InitializationPreparation {
    Existing(InitializationSummary),
    Create(Box<PreparedInitialization>),
}

fn prepare_initialization(
    sql: Arc<dyn SqlAccess>,
    manifest: &ValidatedBootstrapManifest,
    runtime: &BootstrapRuntime,
    ids: InitializationIds,
    now: Timestamp,
) -> AuthorityResult<PreparedInitialization> {
    let created_at = super::model::sql_time(now)?;
    let epoch = AuthorityEpoch {
        epoch_id: ids.epoch,
        marker_id: ids.marker,
        store_id: manifest.input.store_id,
        manifest_digest: manifest.digest.0,
        manifest: manifest.canonical.clone(),
        namespace: manifest.input.marker_namespace.clone(),
        created_at,
    };
    let anchor = AuthorityRecord {
        id: ids.anchor,
        epoch_id: ids.epoch,
        kind: RecordKind::Anchor,
        principal: canonical_principal(&manifest.input.administrator)?,
        namespace: manifest.input.anchor_namespace.clone(),
        right_types: vec![],
        parent_id: None,
        bootstrap_slot: None,
        root_anchor_id: None,
        created_at,
        body: anchor_body(manifest, &epoch, ids.anchor)?,
    };
    let checked = AuthorityStore::from_initialized(
        sql,
        configuration(manifest, runtime, epoch.clone(), anchor.clone()),
    )?;
    let sources = &checked.configuration.bootstrap_sources;
    let issuer = EnvelopeIssuer::new(&runtime.keys, &runtime.catalog);
    let mut seeds = Vec::with_capacity(ids.seeds.len());
    let mut results = Vec::with_capacity(ids.seeds.len());
    for (intent, ids) in manifest.input.seeds.iter().zip(ids.seeds) {
        let GrantParent::BootstrapSlot(slot) = &intent.parent else {
            return Err(invalid("bootstrap seed parent changed"));
        };
        let source = sources
            .iter()
            .find(|source| source.slot == *slot)
            .ok_or_else(|| reject("bootstrap source missing"))?;
        if !source.validity.contains(now) {
            return Err(AuthorityRefusal::NotEffective.into());
        }
        for right in &intent.rights {
            for wanted in super::authorize::permission_scope(right)? {
                if !sources.iter().any(|admin| {
                    admin.namespace == intent.namespace
                        && admin.validity.contains(now)
                        && super::authorize::covers(
                            &runtime.catalog,
                            &admin.rights,
                            &intent.namespace,
                            wanted,
                        )
                        .is_some()
                }) {
                    return Err(AuthorityRefusal::Denied(
                        "bootstrap seed requires current complete administration",
                    )
                    .into());
                }
            }
        }
        let grant = EnvelopeDraft {
            id: ids.record,
            store_id: epoch.store_id,
            epoch_id: epoch.epoch_id,
            root_anchor_id: anchor.id,
            issuer: manifest.input.administrator.clone(),
            grantee: intent.grantee.clone(),
            namespace: intent.namespace.clone(),
            rights: intent.rights.clone(),
            validity: Validity {
                not_before: now,
                expires_at: source.validity.expires_at,
            },
            created_at: now,
            assurance: manifest.input.identity_profile,
            operation: CreationOperation::Import,
            action_id: epoch.marker_id,
            configuration_source: Some(manifest.input.configuration_source.clone()),
            issuance_id: ids.issuance,
            key: manifest.input.issuance_key.clone(),
        };
        let envelope = issuer.issue_from_bootstrap(source, grant.clone())?;
        let _ = EnvelopeVerifier::new(&runtime.keys, &runtime.catalog).verify(
            envelope.as_bytes(),
            epoch.store_id,
            epoch.epoch_id,
        )?;
        let record = AuthorityRecord {
            id: grant.id,
            epoch_id: epoch.epoch_id,
            kind: RecordKind::Grant,
            principal: canonical_principal(&grant.grantee)?,
            namespace: grant.namespace.clone(),
            right_types: super::current::right_types(&grant.rights),
            parent_id: None,
            bootstrap_slot: Some(slot.clone()),
            root_anchor_id: Some(anchor.id),
            created_at,
            body: envelope.as_bytes().to_vec(),
        };
        results.push(InitializationSeed {
            record_id: record.id,
            event_id: ids.event,
            validity: public_validity(grant.validity),
        });
        seeds.push(PreparedSeed {
            record,
            grant,
            event_id: ids.event,
        });
    }
    results.sort_by_key(|seed| seed.record_id);
    let summary = InitializationSummary {
        store_id: epoch.store_id,
        epoch_id: epoch.epoch_id,
        marker_id: epoch.marker_id,
        anchor_id: anchor.id,
        marker_event_id: ids.marker_event,
        anchor_event_id: ids.anchor_event,
        initial_revision: 1,
        seeds: results,
    };
    Ok(PreparedInitialization {
        epoch,
        anchor,
        seeds,
        summary,
    })
}

fn event(
    id: Uuid,
    namespace: &str,
    verb: &str,
    target: Uuid,
    administrator: &Principal,
    created_at: i64,
    payload: Value,
) -> Event {
    Event {
        id,
        namespace: namespace.into(),
        verb: verb.into(),
        substrate: SubstrateKind::Event,
        actor: format!("{}:{}", administrator.kind, administrator.id),
        kind: EventKind::GrantChange,
        outcome: EventOutcome::Success,
        payload,
        payload_schema_version: 1,
        profile_state_version: None,
        duration_us: 0,
        target_id: Some(target),
        session_id: None,
        aggregate_kind: None,
        aggregate_id: None,
        created_at,
        op_index: None,
        ref_resolution: None,
    }
}
fn synthetic_authorization(
    manifest: &ValidatedBootstrapManifest,
    epoch: &AuthorityEpoch,
    implementation: &str,
) -> StorageResult<Value> {
    Ok(json!({"decision":"allow", "implementation":implementation,
        "construction":"validated_bootstrap_manifest", "boot_principal":serialized(&manifest.input.administrator)?,
        "epoch_marker_id":epoch.marker_id, "configuration_source":manifest.input.configuration_source,
        "manifest_digest":serialized(&BodyDigest(epoch.manifest_digest))?, "checked_at":epoch.created_at}))
}
fn metadata_payload(
    manifest: &ValidatedBootstrapManifest,
    epoch: &AuthorityEpoch,
    target: Uuid,
    kind: ChangeKind,
    implementation: &str,
) -> StorageResult<Value> {
    Ok(
        json!({"action_id":epoch.marker_id, "affected_record_id":target,
        "record_kind":if kind == ChangeKind::Initialization { "initialization" } else { "anchor" },
        "administrator":serialized(&manifest.input.administrator)?, "manifest_digest":serialized(&BodyDigest(epoch.manifest_digest))?,
        "initialized_epoch":epoch.epoch_id, "authorization":synthetic_authorization(manifest, epoch, implementation)?}),
    )
}
fn import_payload(
    manifest: &ValidatedBootstrapManifest,
    epoch: &AuthorityEpoch,
    grant: &EnvelopeDraft,
    slot: &str,
    implementation: &str,
) -> StorageResult<Value> {
    let mut authorization = synthetic_authorization(manifest, epoch, implementation)?;
    authorization["source_slot"] = json!(slot);
    authorization["derived_validity"] = serialized(&public_validity(grant.validity))?;
    Ok(
        json!({"action_id":epoch.marker_id,"grant_record_id":grant.id,"grantee":serialized(&grant.grantee)?,
        "rights":serialized(&grant.rights)?,"rationale":"configuration import at first boot","authorization":authorization}),
    )
}
async fn apply_initialization(
    tx: &mut AuthorityTransaction<'_>,
    manifest: &ValidatedBootstrapManifest,
    runtime: &BootstrapRuntime,
    prepared: PreparedInitialization,
) -> StorageResult<InitializationSummary> {
    tx.insert_epoch(&prepared.epoch).await?;
    tx.insert_record(&prepared.anchor).await?;
    for (id, target, kind, namespace) in [
        (
            prepared.summary.marker_event_id,
            prepared.epoch.marker_id,
            ChangeKind::Initialization,
            prepared.epoch.namespace.as_str(),
        ),
        (
            prepared.summary.anchor_event_id,
            prepared.anchor.id,
            ChangeKind::Anchor,
            prepared.anchor.namespace.as_str(),
        ),
    ] {
        let payload = metadata_payload(
            manifest,
            &prepared.epoch,
            target,
            kind,
            runtime.construction_implementation,
        )?;
        let history = event(
            id,
            namespace,
            "initialize",
            target,
            &manifest.input.administrator,
            prepared.epoch.created_at,
            payload,
        );
        tx.append_change(
            &AuthorityChange {
                event_id: id,
                epoch_id: prepared.epoch.epoch_id,
                action_id: prepared.epoch.marker_id,
                kind,
                record_id: target,
            },
            &history,
        )
        .await?;
    }
    for seed in prepared.seeds {
        tx.insert_record(&seed.record).await?;
        let slot = seed
            .record
            .bootstrap_slot
            .as_deref()
            .ok_or_else(|| storage("prepared import slot missing"))?;
        let payload = import_payload(
            manifest,
            &prepared.epoch,
            &seed.grant,
            slot,
            runtime.construction_implementation,
        )?;
        let history = event(
            seed.event_id,
            &seed.record.namespace,
            "import",
            seed.record.id,
            &manifest.input.administrator,
            prepared.epoch.created_at,
            payload,
        );
        tx.append_change(
            &AuthorityChange {
                event_id: seed.event_id,
                epoch_id: prepared.epoch.epoch_id,
                action_id: prepared.epoch.marker_id,
                kind: ChangeKind::Grant,
                record_id: seed.record.id,
            },
            &history,
        )
        .await?;
    }
    tx.advance_revision(prepared.epoch.epoch_id, 0).await?;
    Ok(prepared.summary)
}

impl AuthorityStore {
    pub async fn initialize(
        sql: Arc<dyn SqlAccess>,
        manifest: ValidatedBootstrapManifest,
        runtime: BootstrapRuntime,
    ) -> AuthorityResult<InitializationResult> {
        let manifest = ValidatedBootstrapManifest::validate(manifest.input, &runtime.catalog)?;
        validate_runtime(&manifest, &runtime)?;
        let ids = InitializationIds::allocate(manifest.input.seeds.len());
        let configuration_sql = sql.clone();
        let outcome = authority::transaction(&*sql, move |mut tx| {
            Box::pin(async move {
                let read: AuthorityResult<InitializationPreparation> = async {
                    if let Some(epoch) = tx.epoch().await? {
                        let facts = read_metadata(
                            &mut tx,
                            &manifest,
                            &runtime.keys,
                            &runtime.catalog,
                            epoch,
                        )
                        .await?;
                        return Ok(InitializationPreparation::Existing(facts.summary));
                    }
                    let now = runtime.clock.verification_time()?;
                    Ok(InitializationPreparation::Create(Box::new(
                        prepare_initialization(configuration_sql, &manifest, &runtime, ids, now)?,
                    )))
                }
                .await;
                let prepared = match read {
                    Ok(value) => value,
                    Err(AuthorityError::Refused(refusal)) => return Ok(Err(refusal)),
                    Err(AuthorityError::Storage(error)) => return Err(error),
                };
                match prepared {
                    InitializationPreparation::Existing(initialization) => {
                        Ok(Ok(InitializationResult {
                            changed: false,
                            initialization,
                        }))
                    }
                    InitializationPreparation::Create(prepared) => {
                        let initialization =
                            apply_initialization(&mut tx, &manifest, &runtime, *prepared).await?;
                        Ok(Ok(InitializationResult {
                            changed: true,
                            initialization,
                        }))
                    }
                }
            })
        })
        .await?;
        outcome.map_err(AuthorityError::Refused)
    }
}

struct InitializationFacts {
    epoch: AuthorityEpoch,
    anchor: AuthorityRecord,
    current_revision: i64,
    summary: InitializationSummary,
}
fn check_manifest(
    manifest: &ValidatedBootstrapManifest,
    epoch: &AuthorityEpoch,
) -> AuthorityResult<()> {
    if *blake3::hash(&epoch.manifest).as_bytes() != epoch.manifest_digest {
        return Err(reject("stored manifest digest mismatch"));
    }
    if epoch.manifest != manifest.canonical || epoch.manifest_digest != manifest.digest.0 {
        return Err(AuthorityRefusal::InitializationConflict.into());
    }
    if epoch.store_id != manifest.input.store_id
        || epoch.namespace != manifest.input.marker_namespace
        || epoch.created_at < 0
    {
        return Err(reject("initialized epoch projections disagree"));
    }
    Ok(())
}
async fn checked_anchor(
    tx: &mut AuthorityTransaction<'_>,
    manifest: &ValidatedBootstrapManifest,
    epoch: &AuthorityEpoch,
    anchor_id: Uuid,
) -> AuthorityResult<AuthorityRecord> {
    let anchor = tx
        .record(epoch.epoch_id, anchor_id)
        .await?
        .ok_or_else(|| reject("initialized anchor missing"))?;
    if anchor.id != anchor_id
        || anchor.epoch_id != epoch.epoch_id
        || anchor.kind != RecordKind::Anchor
        || anchor.principal != canonical_principal(&manifest.input.administrator)?
        || anchor.namespace != manifest.input.anchor_namespace
        || !anchor.right_types.is_empty()
        || anchor.parent_id.is_some()
        || anchor.bootstrap_slot.is_some()
        || anchor.root_anchor_id.is_some()
        || anchor.created_at != epoch.created_at
        || anchor.body != anchor_body(manifest, epoch, anchor_id)?
    {
        return Err(reject("initialized anchor projections disagree"));
    }
    if let Some(terminal) = tx.transition(epoch.epoch_id, anchor_id).await? {
        super::model::validate_terminal(&terminal, &anchor)?;
    }
    Ok(anchor)
}
fn historical_implementation(event: &Event) -> AuthorityResult<&str> {
    event
        .payload
        .get("authorization")
        .and_then(|value| value.get("implementation"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| reject("construction history implementation missing"))
}
fn check_event(
    event: &Event,
    association: &AuthorityChange,
    epoch: &AuthorityEpoch,
    administrator: &Principal,
    namespace: &str,
    verb: &str,
    payload: Value,
) -> AuthorityResult<()> {
    if event.id != association.event_id
        || association.epoch_id != epoch.epoch_id
        || association.action_id != epoch.marker_id
        || event.target_id != Some(association.record_id)
        || event.namespace != namespace
        || event.verb != verb
        || event.created_at != epoch.created_at
        || event.actor != format!("{}:{}", administrator.kind, administrator.id)
        || event.kind != EventKind::GrantChange
        || event.substrate != SubstrateKind::Event
        || event.outcome != EventOutcome::Success
        || event.payload_schema_version != 1
        || event.profile_state_version.is_some()
        || event.duration_us != 0
        || event.session_id.is_some()
        || event.aggregate_kind.is_some()
        || event.aggregate_id.is_some()
        || event.op_index.is_some()
        || event.ref_resolution.is_some()
        || event.payload != payload
    {
        return Err(reject(
            "construction Event differs from immutable initialization",
        ));
    }
    Ok(())
}
struct ImportedSeed {
    record: AuthorityRecord,
    envelope: AuthenticatedEnvelope,
    intent: Vec<u8>,
    terminal_state: Option<RecordedState>,
}
async fn authenticated_seed(
    tx: &mut AuthorityTransaction<'_>,
    manifest: &ValidatedBootstrapManifest,
    keys: &SuppliedKeys,
    catalog: &ClosedCatalog,
    epoch: &AuthorityEpoch,
    anchor_id: Uuid,
    id: Uuid,
) -> AuthorityResult<ImportedSeed> {
    let record = tx
        .record(epoch.epoch_id, id)
        .await?
        .ok_or_else(|| reject("original seed missing"))?;
    let envelope = EnvelopeVerifier::new(keys, catalog).verify(
        &record.body,
        epoch.store_id,
        epoch.epoch_id,
    )?;
    let grant = &envelope.body().grant;
    let crate::durable::ParentBinding::BootstrapSlot {
        slot,
        manifest_digest,
        ..
    } = &envelope.body().parent
    else {
        return Err(reject("import seed has a non-bootstrap parent"));
    };
    let source = manifest
        .input
        .slots
        .iter()
        .find(|source| source.slot == *slot)
        .ok_or_else(|| reject("original seed source missing from manifest"))?;
    let created_at =
        u64::try_from(epoch.created_at).map_err(|_| reject("negative initialization time"))?;
    if record.kind != RecordKind::Grant
        || record.id != grant.id
        || record.id != id
        || record.epoch_id != epoch.epoch_id
        || record.principal != canonical_principal(&grant.grantee)?
        || record.namespace != grant.namespace
        || record.parent_id.is_some()
        || record.bootstrap_slot.as_ref() != Some(slot)
        || record.root_anchor_id != Some(anchor_id)
        || record.created_at != epoch.created_at
        || record.right_types != super::current::right_types(&grant.rights)
        || grant.store_id != epoch.store_id
        || grant.epoch_id != epoch.epoch_id
        || grant.root_anchor_id != anchor_id
        || grant.issuer != manifest.input.administrator
        || grant.namespace != source.namespace
        || grant.created_at.as_micros() != created_at
        || grant.validity.not_before.as_micros() != created_at
        || grant.validity.expires_at != source.validity.expires_at
        || !source.validity.contains(grant.created_at)
        || !source.validity.covers(&grant.validity)
        || !catalog.covers_rights(&source.rights, &grant.rights)
        || manifest_digest.0 != epoch.manifest_digest
        || grant.operation != CreationOperation::Import
        || grant.action_id != epoch.marker_id
        || grant.configuration_source.as_deref()
            != Some(manifest.input.configuration_source.as_str())
        || grant.assurance != manifest.input.identity_profile
        || grant.key != manifest.input.issuance_key
    {
        return Err(reject(
            "authenticated seed differs from original initialization",
        ));
    }
    let intent = seed_intent(&GrantItem {
        grantee: grant.grantee.clone(),
        namespace: grant.namespace.clone(),
        parent: GrantParent::BootstrapSlot(slot.clone()),
        rights: grant.rights.clone(),
    })?;
    if manifest.seed_intents.binary_search(&intent).is_err() {
        return Err(reject("import seed not in exact initialized intent"));
    }
    let terminal_state = if let Some(terminal) = tx.transition(epoch.epoch_id, id).await? {
        super::model::validate_terminal(&terminal, &record)?;
        Some(RecordedState::from_database(terminal.state))
    } else {
        None
    };
    Ok(ImportedSeed {
        record,
        envelope,
        intent,
        terminal_state,
    })
}
async fn read_metadata(
    tx: &mut AuthorityTransaction<'_>,
    manifest: &ValidatedBootstrapManifest,
    keys: &SuppliedKeys,
    catalog: &ClosedCatalog,
    epoch: AuthorityEpoch,
) -> AuthorityResult<InitializationFacts> {
    check_manifest(manifest, &epoch)?;
    let current_revision = tx
        .revision(epoch.epoch_id)
        .await?
        .filter(|revision| *revision >= 1)
        .ok_or_else(|| reject("initialized revision missing or below one"))?;
    let expected_count = manifest.input.seeds.len() + 2;
    let mut associations = Vec::with_capacity(expected_count);
    let mut after = None;
    loop {
        let page = tx
            .changes_for_action(epoch.epoch_id, epoch.marker_id, after, 256)
            .await?;
        associations.extend(page.items);
        if associations.len() > expected_count {
            return Err(reject("initialization action has extra records"));
        }
        match page.next_id {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    if associations.len() != expected_count {
        return Err(reject("initialization action history incomplete"));
    }
    let markers = associations
        .iter()
        .filter(|change| change.kind == ChangeKind::Initialization)
        .collect::<Vec<_>>();
    let anchors = associations
        .iter()
        .filter(|change| change.kind == ChangeKind::Anchor)
        .collect::<Vec<_>>();
    if markers.len() != 1 || anchors.len() != 1 || markers[0].record_id != epoch.marker_id {
        return Err(reject(
            "initialization metadata identities missing or duplicated",
        ));
    }
    let anchor = checked_anchor(tx, manifest, &epoch, anchors[0].record_id).await?;
    let marker_event_id = markers[0].event_id;
    let anchor_event_id = anchors[0].event_id;
    let mut seeds = Vec::with_capacity(manifest.input.seeds.len());
    let mut actual_intents = Vec::with_capacity(manifest.input.seeds.len());
    for change in &associations {
        let (association, history) = tx
            .change_event(epoch.epoch_id, change.event_id)
            .await?
            .ok_or_else(|| reject("initialization full Event missing"))?;
        if &association != change {
            return Err(reject("initialization Event association changed"));
        }
        let implementation = historical_implementation(&history)?;
        match change.kind {
            ChangeKind::Initialization | ChangeKind::Anchor => {
                let namespace = if change.kind == ChangeKind::Initialization {
                    &epoch.namespace
                } else {
                    &anchor.namespace
                };
                let payload = metadata_payload(
                    manifest,
                    &epoch,
                    change.record_id,
                    change.kind,
                    implementation,
                )?;
                check_event(
                    &history,
                    change,
                    &epoch,
                    &manifest.input.administrator,
                    namespace,
                    "initialize",
                    payload,
                )?;
            }
            ChangeKind::Grant => {
                let seed = authenticated_seed(
                    tx,
                    manifest,
                    keys,
                    catalog,
                    &epoch,
                    anchor.id,
                    change.record_id,
                )
                .await?;
                let grant = &seed.envelope.body().grant;
                let slot = seed
                    .record
                    .bootstrap_slot
                    .as_deref()
                    .ok_or_else(|| reject("import slot missing"))?;
                let payload = import_payload(manifest, &epoch, grant, slot, implementation)?;
                check_event(
                    &history,
                    change,
                    &epoch,
                    &manifest.input.administrator,
                    &seed.record.namespace,
                    "import",
                    payload,
                )?;
                seeds.push(InitializationSeed {
                    record_id: seed.record.id,
                    event_id: change.event_id,
                    validity: public_validity(grant.validity),
                });
                actual_intents.push(seed.intent);
            }
        }
    }
    actual_intents.sort();
    if actual_intents != manifest.seed_intents {
        return Err(reject("original seed multiplicity differs from manifest"));
    }
    seeds.sort_by_key(|seed| seed.record_id);
    let summary = InitializationSummary {
        store_id: epoch.store_id,
        epoch_id: epoch.epoch_id,
        marker_id: epoch.marker_id,
        anchor_id: anchor.id,
        marker_event_id,
        anchor_event_id,
        initial_revision: 1,
        seeds,
    };
    Ok(InitializationFacts {
        epoch,
        anchor,
        current_revision,
        summary,
    })
}

enum LoadPurpose {
    Active,
    History,
}

impl AuthorityStore {
    pub async fn load_active(
        sql: Arc<dyn SqlAccess>,
        manifest: ValidatedBootstrapManifest,
        runtime: BootstrapRuntime,
    ) -> AuthorityResult<LoadedAuthority> {
        Self::load_verified(sql, manifest, runtime, LoadPurpose::Active).await
    }

    pub async fn load_history(
        sql: Arc<dyn SqlAccess>,
        manifest: ValidatedBootstrapManifest,
        runtime: BootstrapRuntime,
    ) -> AuthorityResult<AuthorityHistory> {
        let loaded = Self::load_verified(sql, manifest, runtime, LoadPurpose::History).await?;
        Ok(AuthorityHistory {
            store: loaded.store,
            current_revision: loaded.current_revision,
            initialization: loaded.initialization,
        })
    }

    async fn load_verified(
        sql: Arc<dyn SqlAccess>,
        manifest: ValidatedBootstrapManifest,
        runtime: BootstrapRuntime,
        purpose: LoadPurpose,
    ) -> AuthorityResult<LoadedAuthority> {
        let manifest = Arc::new(ValidatedBootstrapManifest::validate(
            manifest.input,
            &runtime.catalog,
        )?);
        validate_runtime(&manifest, &runtime)?;
        let configuration_sql = sql.clone();
        let outcome = authority::transaction(&*sql, move |mut tx| {
            Box::pin(async move {
                let read: AuthorityResult<LoadedAuthority> = async {
                    let epoch = tx
                        .epoch()
                        .await?
                        .ok_or(AuthorityRefusal::Unavailable("authority not initialized"))?;
                    let facts =
                        read_metadata(&mut tx, &manifest, &runtime.keys, &runtime.catalog, epoch)
                            .await?;
                    if matches!(purpose, LoadPurpose::Active)
                        && tx
                            .transition(facts.epoch.epoch_id, facts.anchor.id)
                            .await?
                            .is_some()
                    {
                        return Err(AuthorityRefusal::NotEffective.into());
                    }
                    let mut store = AuthorityStore::from_initialized(
                        configuration_sql,
                        configuration(&manifest, &runtime, facts.epoch, facts.anchor),
                    )?;
                    store.bootstrap_manifest = Some(manifest);
                    Ok(LoadedAuthority {
                        store,
                        current_revision: facts.current_revision,
                        initialization: facts.summary,
                    })
                }
                .await;
                match read {
                    Ok(value) => Ok(Ok(value)),
                    Err(AuthorityError::Refused(refusal)) => Ok(Err(refusal)),
                    Err(AuthorityError::Storage(error)) => Err(error),
                }
            })
        })
        .await?;
        outcome.map_err(AuthorityError::Refused)
    }

    pub async fn read_initialization_change(
        &self,
        context: Arc<dyn AdmissionContext>,
        marker_action_id: Uuid,
        event_id: Uuid,
    ) -> AuthorityResult<BootstrapHistoricalChange> {
        let expected = self
            .bootstrap_manifest
            .clone()
            .ok_or(AuthorityRefusal::Unavailable(
                "validated bootstrap history context missing",
            ))?;
        let configuration = self.configuration.clone();
        let mut canonical = b"KHBI\0\x01".to_vec();
        canonical.extend_from_slice(marker_action_id.as_bytes());
        canonical.extend_from_slice(event_id.as_bytes());
        let outcome = authority::transaction(&*self.sql, move |mut tx| {
            Box::pin(async move {
                let read: AuthorityResult<BootstrapHistoricalChange> = async {
                    let mut current = super::current::Current::open(
                        &configuration,
                        &mut tx,
                        &*context,
                        AdministrationOperation::History,
                        &canonical,
                    )
                    .await?;
                    if marker_action_id != configuration.epoch.marker_id {
                        return Err(reject("history action is not initialization"));
                    }
                    let (association, history) = current
                        .tx
                        .change_event(configuration.epoch.epoch_id, event_id)
                        .await?
                        .ok_or_else(|| reject("initialization history event missing"))?;
                    if !current
                        .caller
                        .readable_namespaces
                        .contains(&history.namespace)
                    {
                        return Err(AuthorityRefusal::Denied(
                            "initialization history outside current read scope",
                        )
                        .into());
                    }
                    let epoch = &configuration.epoch;
                    check_manifest(&expected, epoch)?;
                    let implementation = historical_implementation(&history)?;
                    match association.kind {
                        ChangeKind::Initialization => {
                            if association.record_id != epoch.marker_id {
                                return Err(reject("marker history target mismatch"));
                            }
                            let payload = metadata_payload(
                                &expected,
                                epoch,
                                epoch.marker_id,
                                ChangeKind::Initialization,
                                implementation,
                            )?;
                            check_event(
                                &history,
                                &association,
                                epoch,
                                &expected.input.administrator,
                                &epoch.namespace,
                                "initialize",
                                payload,
                            )?;
                            Ok(BootstrapHistoricalChange::MarkerInitialize {
                                event: history,
                                epoch_id: epoch.epoch_id,
                                marker_id: epoch.marker_id,
                                administrator: expected.input.administrator.clone(),
                                manifest_digest: BodyDigest(epoch.manifest_digest),
                            })
                        }
                        ChangeKind::Anchor => {
                            let anchor = current.anchor().await?;
                            if association.record_id != anchor.id {
                                return Err(reject("anchor history target mismatch"));
                            }
                            let payload = metadata_payload(
                                &expected,
                                epoch,
                                anchor.id,
                                ChangeKind::Anchor,
                                implementation,
                            )?;
                            check_event(
                                &history,
                                &association,
                                epoch,
                                &expected.input.administrator,
                                &anchor.namespace,
                                "initialize",
                                payload,
                            )?;
                            Ok(BootstrapHistoricalChange::AnchorInitialize {
                                event: history,
                                anchor: RecordRef::of(&anchor),
                                administrator: expected.input.administrator.clone(),
                                manifest_digest: BodyDigest(epoch.manifest_digest),
                            })
                        }
                        ChangeKind::Grant => {
                            let seed = authenticated_seed(
                                current.tx,
                                &expected,
                                &configuration.keys,
                                &configuration.catalog,
                                epoch,
                                configuration.anchor.id,
                                association.record_id,
                            )
                            .await?;
                            let grant = &seed.envelope.body().grant;
                            let slot = seed
                                .record
                                .bootstrap_slot
                                .as_deref()
                                .ok_or_else(|| reject("import slot missing"))?;
                            let payload =
                                import_payload(&expected, epoch, grant, slot, implementation)?;
                            check_event(
                                &history,
                                &association,
                                epoch,
                                &expected.input.administrator,
                                &seed.record.namespace,
                                "import",
                                payload,
                            )?;
                            Ok(BootstrapHistoricalChange::ImportedSeed {
                                event: history,
                                record: RecordRef::of(&seed.record),
                                grantee: grant.grantee.clone(),
                                rights: grant.rights.clone(),
                                validity: public_validity(grant.validity),
                                terminal_state: seed.terminal_state,
                            })
                        }
                    }
                }
                .await;
                match read {
                    Ok(value) => Ok(Ok(value)),
                    Err(AuthorityError::Refused(refusal)) => Ok(Err(refusal)),
                    Err(AuthorityError::Storage(error)) => Err(error),
                }
            })
        })
        .await?;
        outcome.map_err(AuthorityError::Refused)
    }
}
