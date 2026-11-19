use super::model::{sql_time, validate_terminal};
use super::{
    AdministrationOperation, AdmissionBinding, AdmissionContext, AdmittedCaller,
    AuthorityConfiguration, AuthorityRefusal, AuthorityResult,
};
use crate::durable::{
    canonical_principal, AuthenticatedEnvelope, BootstrapSource, EnvelopeVerifier, ParentBinding,
    RightSpec,
};
use khive_db::authority::{AuthorityRecord, AuthorityTransaction, AuthorityTransition, RecordKind};
use khive_types::Timestamp;
use std::collections::BTreeSet;
use uuid::Uuid;

pub(super) struct Current<'a, 'w> {
    pub configuration: &'a AuthorityConfiguration,
    pub tx: &'a mut AuthorityTransaction<'w>,
    pub now: Timestamp,
    pub revision: i64,
    pub caller: AdmittedCaller,
}
impl<'a, 'w> Current<'a, 'w> {
    pub async fn open(
        configuration: &'a AuthorityConfiguration,
        tx: &'a mut AuthorityTransaction<'w>,
        context: &dyn AdmissionContext,
        operation: AdministrationOperation,
        canonical_request: &[u8],
    ) -> AuthorityResult<Self> {
        // This is the sole clock read. Every authority and issuance check shares it.
        let now = configuration.clock.verification_time()?;
        sql_time(now)?;
        let epoch = tx
            .epoch()
            .await?
            .ok_or(AuthorityRefusal::Unavailable("authority not initialized"))?;
        if epoch != configuration.epoch {
            return Err(
                AuthorityRefusal::Reject("initialized manifest, store or epoch mismatch").into(),
            );
        }
        let revision = tx
            .revision(epoch.epoch_id)
            .await?
            .ok_or(AuthorityRefusal::Reject("authority revision missing"))?;
        let caller = configuration.admission.revalidate(
            context,
            &AdmissionBinding {
                service_id: configuration.service_id,
                store_id: epoch.store_id,
                epoch_id: epoch.epoch_id,
                operation,
                canonical_request_digest: *blake3::hash(canonical_request).as_bytes(),
                evaluated_at: now,
                authority_revision: revision,
            },
        )?;
        caller.principal.validate()?;
        if caller.principal.leg.is_some() {
            return Err(AuthorityRefusal::Denied("subactor cannot administer authority").into());
        }
        if canonical_principal(&caller.principal)?.len()
            > khive_db::authority::MAX_AUTHORITY_VALUE_BYTES
            || caller.gate_implementation.is_empty()
        {
            return Err(AuthorityRefusal::InvalidInput(
                "unusable structural caller or implementation identity",
            )
            .into());
        }
        let mut current = Self {
            configuration,
            tx,
            now,
            revision,
            caller,
        };
        current.anchor().await?;
        Ok(current)
    }

    pub fn verifier(&self) -> EnvelopeVerifier<'_> {
        EnvelopeVerifier::new(&self.configuration.keys, &self.configuration.catalog)
    }
    pub async fn anchor(&mut self) -> AuthorityResult<AuthorityRecord> {
        let expected = &self.configuration.anchor;
        let record = self
            .tx
            .record(self.configuration.epoch.epoch_id, expected.id)
            .await?
            .ok_or(AuthorityRefusal::Reject("root anchor missing"))?;
        if &record != expected {
            return Err(AuthorityRefusal::Reject("root anchor immutable data mismatch").into());
        }
        Ok(record)
    }
    pub async fn terminal(
        &mut self,
        record: &AuthorityRecord,
    ) -> AuthorityResult<Option<AuthorityTransition>> {
        let transition = self.tx.transition(record.epoch_id, record.id).await?;
        if let Some(value) = &transition {
            validate_terminal(value, record)?;
        }
        Ok(transition)
    }
    pub async fn live_anchor(&mut self) -> AuthorityResult<()> {
        let record = self.anchor().await?;
        if self.terminal(&record).await?.is_some() {
            return Err(AuthorityRefusal::NotEffective.into());
        }
        Ok(())
    }
    pub async fn authentic(
        &mut self,
        id: Uuid,
    ) -> AuthorityResult<(AuthorityRecord, AuthenticatedEnvelope)> {
        let record = self
            .tx
            .record(self.configuration.epoch.epoch_id, id)
            .await?
            .ok_or(AuthorityRefusal::Reject("grant missing"))?;
        if record.kind != RecordKind::Grant {
            return Err(AuthorityRefusal::Reject("target is not a grant").into());
        }
        let envelope = self.verifier().verify(
            &record.body,
            self.configuration.epoch.store_id,
            self.configuration.epoch.epoch_id,
        )?;
        let grant = &envelope.body().grant;
        let (parent_id, bootstrap_slot) = match &envelope.body().parent {
            ParentBinding::Grant { grant_id, .. } => (Some(*grant_id), None),
            ParentBinding::BootstrapSlot { slot, .. } => (None, Some(slot.clone())),
        };
        if record.id != grant.id
            || record.epoch_id != grant.epoch_id
            || record.principal != canonical_principal(&grant.grantee)?
            || record.namespace != grant.namespace
            || record.parent_id != parent_id
            || record.bootstrap_slot != bootstrap_slot
            || record.root_anchor_id != Some(grant.root_anchor_id)
            || grant.root_anchor_id != self.configuration.anchor.id
            || record.created_at
                != i64::try_from(grant.created_at.as_micros()).map_err(|_| {
                    AuthorityRefusal::Reject("grant timestamp projection out of range")
                })?
            || record.right_types != right_types(&grant.rights)
        {
            return Err(AuthorityRefusal::Reject(
                "authenticated grant and SQL projections disagree",
            )
            .into());
        }
        Ok((record, envelope))
    }
    pub fn source(&self, slot: &str) -> AuthorityResult<BootstrapSource> {
        self.configuration
            .bootstrap_sources
            .iter()
            .find(|source| source.slot == slot)
            .cloned()
            .ok_or_else(|| {
                AuthorityRefusal::Reject("bootstrap slot missing from initialized configuration")
                    .into()
            })
    }
    pub async fn effective(
        &mut self,
        id: Uuid,
    ) -> AuthorityResult<(AuthorityRecord, AuthenticatedEnvelope)> {
        let leaf = self.authentic(id).await?;
        let mut child = leaf.1.clone();
        let mut record = leaf.0.clone();
        let mut visited = BTreeSet::new();
        loop {
            if !visited.insert(record.id) {
                return Err(AuthorityRefusal::Reject("durable lineage cycle").into());
            }
            if self.terminal(&record).await?.is_some()
                || !child.body().grant.validity.contains(self.now)
            {
                return Err(AuthorityRefusal::NotEffective.into());
            }
            match &child.body().parent {
                ParentBinding::Grant { grant_id, .. } => {
                    let (parent_record, parent) = self.authentic(*grant_id).await?;
                    self.verifier().verify_delegation_link(&child, &parent)?;
                    child = parent;
                    record = parent_record;
                }
                ParentBinding::BootstrapSlot {
                    slot,
                    manifest_digest,
                    ..
                } => {
                    let source = self.source(slot)?;
                    let grant = &child.body().grant;
                    if source.manifest_digest != *manifest_digest
                        || source.owner != grant.issuer
                        || source.namespace != grant.namespace
                        || source.root_anchor_id != grant.root_anchor_id
                        || !source.validity.contains(self.now)
                        || !source.validity.covers(&grant.validity)
                        || !self
                            .configuration
                            .catalog
                            .covers_rights(&source.rights, &grant.rights)
                    {
                        return Err(AuthorityRefusal::Reject(
                            "initialized bootstrap source does not cover lineage",
                        )
                        .into());
                    }
                    self.live_anchor().await?;
                    return Ok(leaf);
                }
            }
        }
    }
}

pub(super) fn right_types(rights: &[RightSpec]) -> Vec<String> {
    rights
        .iter()
        .map(|right| {
            match right {
                RightSpec::RecordObservation(_) => "record_observation",
                RightSpec::GrantAdmin(_) => "grant_admin",
                RightSpec::Read => "read",
                RightSpec::Create => "create",
                RightSpec::Write => "write",
                RightSpec::Delete => "delete",
                RightSpec::Send => "send",
            }
            .to_owned()
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}
