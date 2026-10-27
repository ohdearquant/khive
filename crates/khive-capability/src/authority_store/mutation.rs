use super::authorize::administration;
use super::current::{right_types, Current};
use super::history::change_event;
use super::model::{sql_time, terminal_bytes, ProofSet, PublicValidity};
use super::*;
use crate::durable::{
    canonical_principal, AuthenticatedEnvelope, BootstrapSource, CreationOperation, EnvelopeDraft,
    EnvelopeIssuer, ParentBinding, RightSpec, Validity,
};
use khive_db::authority::{
    self, AuthorityChange, AuthorityRecord, AuthorityReplay, AuthorityTransition, ChangeKind,
    RecordKind, ReplayKey,
};
use khive_storage::{StorageError, StorageResult};
use khive_types::Timestamp;
use std::sync::Arc;
use uuid::Uuid;

pub(super) enum Request {
    Grant(GrantRequest),
    Revoke(RevokeRequest),
    Retire(RetireAnchorRequest),
}
impl Request {
    fn operation(&self) -> AdministrationOperation {
        match self {
            Self::Grant(_) => AdministrationOperation::Grant,
            Self::Revoke(_) => AdministrationOperation::Revoke,
            Self::Retire(_) => AdministrationOperation::RetireAnchor,
        }
    }
    fn canonical(&self) -> AuthorityResult<Vec<u8>> {
        match self {
            Self::Grant(r) => r.canonical(),
            Self::Revoke(r) => r.canonical(),
            Self::Retire(r) => r.canonical(),
        }
    }
    fn identity(&self) -> (&str, &str) {
        match self {
            Self::Grant(r) => (&r.request_id, &r.rationale),
            Self::Revoke(r) => (&r.request_id, &r.rationale),
            Self::Retire(r) => (&r.request_id, &r.rationale),
        }
    }
    fn count(&self) -> usize {
        match self {
            Self::Grant(r) => r.grants.len(),
            Self::Revoke(r) => r.grant_ids.len(),
            Self::Retire(_) => 1,
        }
    }
}
struct CandidateIds {
    record: Uuid,
    issuance: Uuid,
    transition: Uuid,
    event: Uuid,
}
struct Candidates {
    action: Uuid,
    items: Vec<CandidateIds>,
}
impl Candidates {
    fn new(count: usize) -> Self {
        Self {
            action: Uuid::new_v4(),
            items: (0..count)
                .map(|_| CandidateIds {
                    record: Uuid::new_v4(),
                    issuance: Uuid::new_v4(),
                    transition: Uuid::new_v4(),
                    event: Uuid::new_v4(),
                })
                .collect(),
        }
    }
}

pub(super) struct PlannedChange {
    pub association: AuthorityChange,
    pub record: Option<AuthorityRecord>,
    pub transition: Option<AuthorityTransition>,
    pub target: RecordRef,
    pub namespace: String,
    pub proof: Option<ProofSet>,
}
struct Prepared {
    changes: Vec<PlannedChange>,
    result: MutationResult,
    key: ReplayKey,
    replay: AuthorityReplay,
    now: Timestamp,
    caller: AdmittedCaller,
    rationale: String,
    operation: AdministrationOperation,
}
enum Preparation {
    Replay(MutationResult),
    Write(Box<Prepared>),
}
enum Parent {
    Grant(Box<AuthenticatedEnvelope>),
    Bootstrap(Box<BootstrapSource>),
}
impl Parent {
    fn validity(&self) -> &Validity {
        match self {
            Self::Grant(parent) => &parent.body().grant.validity,
            Self::Bootstrap(source) => &source.validity,
        }
    }
}

impl AuthorityStore {
    pub(super) async fn execute(
        &self,
        context: Arc<dyn AdmissionContext>,
        request: Request,
    ) -> AuthorityResult<MutationResult> {
        let canonical = request.canonical()?;
        if let Request::Grant(grant) = &request {
            for item in &grant.grants {
                let _ = self.configuration.catalog.validate_rights(
                    &item.rights,
                    &item.namespace,
                    &item.grantee,
                )?;
            }
        }
        // UUID generation may consult external entropy; never do it under writer.
        let candidates = Candidates::new(request.count());
        let configuration = Arc::clone(&self.configuration);
        let outcome = authority::transaction(&*self.sql, move |mut tx| {
            Box::pin(async move {
                let prepared = {
                    let attempt = async {
                        let mut current = Current::open(
                            &configuration,
                            &mut tx,
                            &*context,
                            request.operation(),
                            &canonical,
                        )
                        .await?;
                        prepare(&mut current, request, canonical, candidates).await
                    }
                    .await;
                    match attempt {
                        Ok(value) => value,
                        Err(AuthorityError::Refused(refusal)) => return Ok(Err(refusal)),
                        Err(AuthorityError::Storage(error)) => return Err(error),
                    }
                };
                match prepared {
                    Preparation::Replay(result) => Ok(Ok(result)),
                    Preparation::Write(prepared) => {
                        apply(&configuration, &mut tx, &prepared).await?;
                        Ok(Ok(prepared.result))
                    }
                }
            })
        })
        .await?;
        outcome.map_err(AuthorityError::Refused)
    }
}

async fn selected_parent(
    current: &mut Current<'_, '_>,
    item: &GrantItem,
) -> AuthorityResult<Parent> {
    let parent = match &item.parent {
        GrantParent::Grant(id) => {
            let (_, envelope) = current.effective(*id).await?;
            let body = &envelope.body().grant;
            if body.grantee != current.caller.principal
                || body.namespace != item.namespace
                || !current
                    .configuration
                    .catalog
                    .covers_rights(&body.rights, &item.rights)
            {
                return Err(AuthorityRefusal::Denied(
                    "one caller-owned parent must cover complete requested rights",
                )
                .into());
            }
            Parent::Grant(Box::new(envelope))
        }
        GrantParent::BootstrapSlot(slot) => {
            let source = current.source(slot)?;
            if source.owner != current.caller.principal
                || source.namespace != item.namespace
                || !source.validity.contains(current.now)
                || !current
                    .configuration
                    .catalog
                    .covers_rights(&source.rights, &item.rights)
            {
                return Err(AuthorityRefusal::Denied(
                    "caller does not own covering live bootstrap source",
                )
                .into());
            }
            current.live_anchor().await?;
            Parent::Bootstrap(Box::new(source))
        }
    };
    if item.grantee.leg.is_some() && item.grantee.parent() != current.caller.principal {
        return Err(AuthorityRefusal::Denied("only parent may grant to subactor").into());
    }
    Ok(parent)
}

async fn prepare(
    current: &mut Current<'_, '_>,
    request: Request,
    canonical: Vec<u8>,
    ids: Candidates,
) -> AuthorityResult<Preparation> {
    let operation = request.operation();
    let (request_id, rationale) = request.identity();
    let key = ReplayKey {
        epoch_id: current.configuration.epoch.epoch_id,
        structural_caller: canonical_principal(&current.caller.principal)?,
        canonical_verb: operation.canonical_verb().into(),
        request_id: request_id.to_owned(),
    };
    let rationale = rationale.to_owned();
    let mut parents = Vec::new();
    let mut targets = Vec::new();
    let mut proofs = Vec::new();
    match &request {
        Request::Grant(request) => {
            for item in &request.grants {
                parents.push(selected_parent(current, item).await?);
                proofs.push(Some(
                    administration(current, &item.namespace, &item.rights).await?,
                ));
            }
        }
        Request::Revoke(request) => {
            for id in &request.grant_ids {
                let (record, envelope) = current.authentic(*id).await?;
                proofs.push(Some(
                    administration(current, &record.namespace, &envelope.body().grant.rights)
                        .await?,
                ));
                let terminal = current.terminal(&record).await?;
                targets.push((record, Some(envelope), terminal));
            }
        }
        Request::Retire(request) => {
            if request.root_anchor_id != current.configuration.anchor.id {
                return Err(AuthorityRefusal::Reject("anchor not found").into());
            }
            if current.caller.principal != current.configuration.boot_principal {
                return Err(AuthorityRefusal::Denied(
                    "only structural boot principal may retire anchor",
                )
                .into());
            }
            let anchor = current.anchor().await?;
            let terminal = current.terminal(&anchor).await?;
            targets.push((anchor, None, terminal));
            proofs.push(None);
        }
    }
    // Every requested item has full current authorization before this lookup.
    if let Some(replay) = current.tx.replay(&key).await? {
        if replay.canonical_request != canonical {
            return Err(AuthorityRefusal::RequestConflict.into());
        }
        let result = serde_json::from_slice(&replay.result)
            .map_err(|_| AuthorityRefusal::Reject("malformed saved administration result"))?;
        return Ok(Preparation::Replay(result));
    }
    let mut changes = Vec::new();
    let mut outcomes = Vec::new();
    match request {
        Request::Grant(request) => {
            for (((item, parent), proof), candidate) in request
                .grants
                .into_iter()
                .zip(parents)
                .zip(proofs)
                .zip(&ids.items)
            {
                let validity = Validity {
                    not_before: current.now,
                    expires_at: parent.validity().expires_at,
                };
                validity.validate()?;
                let draft = EnvelopeDraft {
                    id: candidate.record,
                    store_id: current.configuration.epoch.store_id,
                    epoch_id: current.configuration.epoch.epoch_id,
                    root_anchor_id: current.configuration.anchor.id,
                    issuer: current.caller.principal.clone(),
                    grantee: item.grantee,
                    namespace: item.namespace,
                    rights: item.rights,
                    validity,
                    created_at: current.now,
                    assurance: current.caller.assurance,
                    operation: CreationOperation::Grant,
                    action_id: ids.action,
                    configuration_source: None,
                    issuance_id: candidate.issuance,
                    key: current.configuration.issuance_key.clone(),
                };
                let issuer = EnvelopeIssuer::new(
                    &current.configuration.keys,
                    &current.configuration.catalog,
                );
                let envelope = match parent {
                    Parent::Grant(parent) => issuer.delegate(&parent, draft)?,
                    Parent::Bootstrap(source) => issuer.issue_from_bootstrap(&source, draft)?,
                };
                let body = &envelope.body().grant;
                let (parent_id, bootstrap_slot) = match &envelope.body().parent {
                    ParentBinding::Grant { grant_id, .. } => (Some(*grant_id), None),
                    ParentBinding::BootstrapSlot { slot, .. } => (None, Some(slot.clone())),
                };
                let record = AuthorityRecord {
                    id: body.id,
                    epoch_id: body.epoch_id,
                    kind: RecordKind::Grant,
                    principal: canonical_principal(&body.grantee)?,
                    namespace: body.namespace.clone(),
                    right_types: right_types(&body.rights),
                    parent_id,
                    bootstrap_slot,
                    root_anchor_id: Some(body.root_anchor_id),
                    created_at: sql_time(body.created_at)?,
                    body: envelope.as_bytes().to_vec(),
                };
                outcomes.push(RecordOutcome {
                    record_id: record.id,
                    changed: true,
                    terminal_state: None,
                    action_id: ids.action,
                    transition_id: None,
                    change_event_id: candidate.event,
                    validity: Some(PublicValidity {
                        not_before: validity.not_before.as_micros(),
                        expires_at: validity.expires_at.map(|end| end.as_micros()),
                    }),
                });
                changes.push(PlannedChange {
                    association: AuthorityChange {
                        event_id: candidate.event,
                        epoch_id: record.epoch_id,
                        action_id: ids.action,
                        kind: ChangeKind::Grant,
                        record_id: record.id,
                    },
                    target: RecordRef::of(&record),
                    namespace: record.namespace.clone(),
                    record: Some(record),
                    transition: None,
                    proof,
                });
            }
        }
        Request::Revoke(_) | Request::Retire(_) => {
            for (((record, envelope, terminal), proof), candidate) in
                targets.into_iter().zip(proofs).zip(&ids.items)
            {
                if let Some(terminal) = terminal {
                    outcomes.push(RecordOutcome {
                        record_id: record.id,
                        changed: false,
                        terminal_state: Some(RecordedState::from_database(terminal.state)),
                        action_id: terminal.action_id,
                        transition_id: Some(terminal.id),
                        change_event_id: terminal.event_id,
                        validity: None,
                    });
                    continue;
                }
                let state = if record.kind == RecordKind::Anchor {
                    RecordedState::Retired
                } else if envelope.as_ref().is_some_and(|value| {
                    value
                        .body()
                        .grant
                        .rights
                        .iter()
                        .any(|right| matches!(right, RightSpec::RecordObservation(_)))
                }) {
                    RecordedState::Disabled
                } else {
                    RecordedState::Revoked
                };
                let mut terminal = AuthorityTransition {
                    id: candidate.transition,
                    epoch_id: record.epoch_id,
                    record_id: record.id,
                    state: state.database(),
                    action_id: ids.action,
                    event_id: candidate.event,
                    created_at: sql_time(current.now)?,
                    body: Vec::new(),
                };
                terminal.body = terminal_bytes(&terminal, &record, current.now);
                outcomes.push(RecordOutcome {
                    record_id: record.id,
                    changed: true,
                    terminal_state: Some(state),
                    action_id: ids.action,
                    transition_id: Some(terminal.id),
                    change_event_id: candidate.event,
                    validity: None,
                });
                changes.push(PlannedChange {
                    association: AuthorityChange {
                        event_id: candidate.event,
                        epoch_id: record.epoch_id,
                        action_id: ids.action,
                        kind: if record.kind == RecordKind::Anchor {
                            ChangeKind::Anchor
                        } else {
                            ChangeKind::Grant
                        },
                        record_id: record.id,
                    },
                    target: RecordRef::of(&record),
                    namespace: record.namespace,
                    record: None,
                    transition: Some(terminal),
                    proof,
                });
            }
        }
    }
    let changed = !changes.is_empty();
    let revision = if changed {
        current
            .revision
            .checked_add(1)
            .ok_or(AuthorityRefusal::Unavailable(
                "authority revision exhausted",
            ))?
    } else {
        current.revision
    };
    let result = MutationResult {
        changed,
        action_id: changed.then_some(ids.action),
        authority_revision: revision,
        targets: outcomes,
    };
    let result_bytes = serde_json::to_vec(&result)
        .map_err(|_| AuthorityRefusal::Reject("administration result encoding failed"))?;
    Ok(Preparation::Write(Box::new(Prepared {
        changes,
        result,
        key,
        replay: AuthorityReplay {
            canonical_request: canonical,
            result: result_bytes,
        },
        now: current.now,
        caller: current.caller.clone(),
        rationale,
        operation,
    })))
}

async fn apply(
    configuration: &AuthorityConfiguration,
    tx: &mut authority::AuthorityTransaction<'_>,
    prepared: &Prepared,
) -> StorageResult<()> {
    for change in &prepared.changes {
        if let Some(record) = &change.record {
            tx.insert_record(record).await?;
        }
        // Full typed JSON is built, appended and released per target in this TX.
        let event = change_event(
            configuration,
            change,
            &prepared.caller,
            prepared.operation,
            &prepared.rationale,
            prepared.now,
        )?;
        tx.append_change(&change.association, &event).await?;
        if let Some(terminal) = &change.transition {
            tx.insert_transition(terminal).await?;
        }
    }
    if prepared.result.changed {
        let revision = tx
            .advance_revision(
                prepared.key.epoch_id,
                prepared.result.authority_revision - 1,
            )
            .await?;
        if revision != prepared.result.authority_revision {
            return Err(internal("authority revision postcondition mismatch"));
        }
    }
    tx.insert_replay(&prepared.key, &prepared.replay).await?;
    Ok(())
}
pub(super) fn internal(message: impl Into<String>) -> StorageError {
    StorageError::Internal(message.into())
}
