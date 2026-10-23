use super::{AuthorityError, GrantRequest, RetireAnchorRequest, RevokeRequest};
use crate::durable::{
    BootstrapSource, ClosedCatalog, EnvelopeError, IssuanceAssurance, KeyRef, Principal,
    SuppliedKeys,
};
use khive_db::authority::{
    AuthorityEpoch, AuthorityRecord, AuthorityTransition, RecordKind, TerminalState,
};
use khive_storage::SqlAccess;
use khive_types::Timestamp;
use serde::{Deserialize, Serialize};
use std::{any::Any, sync::Arc};
use uuid::Uuid;

pub type AuthorityResult<T> = Result<T, AuthorityError>;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AuthorityRefusal {
    #[error("invalid administration input: {0}")]
    InvalidInput(&'static str),
    #[error("authority rejected: {0}")]
    Reject(&'static str),
    #[error("authority denied: {0}")]
    Denied(&'static str),
    #[error("authority is not currently effective")]
    NotEffective,
    #[error("authority unavailable: {0}")]
    Unavailable(&'static str),
    #[error("request id already holds different canonical bytes")]
    RequestConflict,
    #[error("bootstrap manifest conflicts with initialized authority")]
    InitializationConflict,
    #[error(transparent)]
    Envelope(#[from] EnvelopeError),
}
impl From<EnvelopeError> for AuthorityError {
    fn from(value: EnvelopeError) -> Self {
        Self::Refused(value.into())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdministrationOperation {
    Grant,
    Revoke,
    RetireAnchor,
    History,
}
impl AdministrationOperation {
    pub fn canonical_verb(self) -> &'static str {
        match self {
            Self::Grant => "authority.grant",
            Self::Revoke => "authority.revoke",
            Self::RetireAnchor => "authority.retire_anchor",
            Self::History => "authority.history",
        }
    }
}

/// Implemented only by the trusted composition's opaque per-request context.
/// The configured verifier must authenticate its concrete type and bound data.
pub trait AdmissionContext: Any + Send + Sync {
    fn as_any(&self) -> &dyn Any;
}

#[derive(Clone, Debug)]
pub struct AdmissionBinding {
    pub service_id: Uuid,
    pub store_id: Uuid,
    pub epoch_id: Uuid,
    pub operation: AdministrationOperation,
    pub canonical_request_digest: [u8; 32],
    pub evaluated_at: Timestamp,
    pub authority_revision: i64,
}

#[derive(Clone, Debug)]
pub struct AdmittedCaller {
    pub principal: Principal,
    pub assurance: IssuanceAssurance,
    pub gate_implementation: &'static str,
    /// Explicit read view, established by the configured policy, not GrantAdmin.
    pub readable_namespaces: Vec<String>,
}

pub trait AdmissionVerifier: Send + Sync {
    /// Recheck actual identity, assurance, request binding and current outer policy.
    /// An outer denial or error must remain a refusal.
    fn revalidate(
        &self,
        context: &dyn AdmissionContext,
        binding: &AdmissionBinding,
    ) -> Result<AdmittedCaller, AuthorityRefusal>;
}

pub trait AuthorityClock: Send + Sync {
    fn verification_time(&self) -> Result<Timestamp, AuthorityRefusal>;
}

/// Already validated, immutable initialized configuration from the trusted host.
/// Privileged initialization is deliberately separate from this data contract.
pub struct AuthorityConfiguration {
    pub service_id: Uuid,
    pub epoch: AuthorityEpoch,
    pub anchor: AuthorityRecord,
    pub boot_principal: Principal,
    pub bootstrap_sources: Vec<BootstrapSource>,
    pub issuance_key: KeyRef,
    pub keys: Arc<SuppliedKeys>,
    pub catalog: Arc<ClosedCatalog>,
    pub clock: Arc<dyn AuthorityClock>,
    pub admission: Arc<dyn AdmissionVerifier>,
}

pub struct AuthorityStore {
    pub(super) sql: Arc<dyn SqlAccess>,
    pub(super) configuration: Arc<AuthorityConfiguration>,
    pub(super) bootstrap_manifest: Option<Arc<super::bootstrap::ValidatedBootstrapManifest>>,
}
impl AuthorityStore {
    /// Trusted construction over an initialized store. It never creates a marker,
    /// imports seeds, authenticates an incoming caller or restores a retired root.
    pub fn from_initialized(
        sql: Arc<dyn SqlAccess>,
        configuration: AuthorityConfiguration,
    ) -> AuthorityResult<Self> {
        let c = &configuration;
        c.boot_principal.validate()?;
        if c.epoch.manifest.is_empty()
            || c.epoch.manifest.len() > khive_db::authority::MAX_AUTHORITY_MANIFEST_BYTES
            || *blake3::hash(&c.epoch.manifest).as_bytes() != c.epoch.manifest_digest
            || c.boot_principal.leg.is_some()
            || c.anchor.principal != crate::durable::canonical_principal(&c.boot_principal)?
            || c.anchor.kind != RecordKind::Anchor
            || c.anchor.epoch_id != c.epoch.epoch_id
            || c.anchor.body.is_empty()
            || c.anchor.body.len() > crate::durable::MAX_RECORD_BYTES
            || !c.anchor.right_types.is_empty()
            || c.anchor.parent_id.is_some()
            || c.anchor.bootstrap_slot.is_some()
            || c.anchor.root_anchor_id.is_some()
            || c.bootstrap_sources.len() > crate::durable::MAX_COLLECTION_ITEMS
        {
            return Err(AuthorityRefusal::InvalidInput("invalid initialized configuration").into());
        }
        let mut slots = std::collections::BTreeSet::new();
        for source in &c.bootstrap_sources {
            source.owner.validate()?;
            source.validity.validate()?;
            let _ = c.catalog.validate_source_rights(
                &source.rights,
                &source.namespace,
                &source.owner,
            )?;
            if source.store_id != c.epoch.store_id
                || source.epoch_id != c.epoch.epoch_id
                || source.root_anchor_id != c.anchor.id
                || source.manifest_digest.0 != c.epoch.manifest_digest
                || source.owner != c.boot_principal
                || source.owner.leg.is_some()
                || source.slot.is_empty()
                || !slots.insert(source.slot.clone())
            {
                return Err(
                    AuthorityRefusal::InvalidInput("invalid bootstrap source binding").into(),
                );
            }
        }
        Ok(Self {
            sql,
            configuration: Arc::new(configuration),
            bootstrap_manifest: None,
        })
    }

    pub async fn grant(
        &self,
        context: Arc<dyn AdmissionContext>,
        request: GrantRequest,
    ) -> AuthorityResult<MutationResult> {
        self.execute(context, super::mutation::Request::Grant(request))
            .await
    }
    pub async fn revoke(
        &self,
        context: Arc<dyn AdmissionContext>,
        request: RevokeRequest,
    ) -> AuthorityResult<MutationResult> {
        self.execute(context, super::mutation::Request::Revoke(request))
            .await
    }
    pub async fn retire_anchor(
        &self,
        context: Arc<dyn AdmissionContext>,
        request: RetireAnchorRequest,
    ) -> AuthorityResult<MutationResult> {
        self.execute(context, super::mutation::Request::Retire(request))
            .await
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BodyDigest(pub [u8; 32]);
impl Serialize for BodyDigest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct View {
            algorithm: &'static str,
            hex: String,
        }
        let alphabet = b"0123456789abcdef";
        let mut hex = String::with_capacity(64);
        for byte in self.0 {
            hex.push(alphabet[usize::from(byte >> 4)] as char);
            hex.push(alphabet[usize::from(byte & 15)] as char);
        }
        View {
            algorithm: "blake3-256",
            hex,
        }
        .serialize(serializer)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RecordRef {
    pub id: Uuid,
    pub body_digest: BodyDigest,
}
impl RecordRef {
    pub(super) fn of(record: &AuthorityRecord) -> Self {
        Self {
            id: record.id,
            body_digest: BodyDigest(*blake3::hash(&record.body).as_bytes()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordedState {
    Disabled,
    Revoked,
    Retired,
}
impl RecordedState {
    pub(super) fn database(self) -> TerminalState {
        match self {
            Self::Disabled => TerminalState::Disabled,
            Self::Revoked => TerminalState::Revoked,
            Self::Retired => TerminalState::Retired,
        }
    }
    pub(super) fn from_database(value: TerminalState) -> Self {
        match value {
            TerminalState::Disabled => Self::Disabled,
            TerminalState::Revoked => Self::Revoked,
            TerminalState::Retired => Self::Retired,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordOutcome {
    pub record_id: Uuid,
    pub changed: bool,
    pub terminal_state: Option<RecordedState>,
    pub action_id: Uuid,
    pub transition_id: Option<Uuid>,
    pub change_event_id: Uuid,
    pub validity: Option<PublicValidity>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicValidity {
    pub not_before: u64,
    pub expires_at: Option<u64>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MutationResult {
    pub changed: bool,
    pub action_id: Option<Uuid>,
    pub authority_revision: i64,
    pub targets: Vec<RecordOutcome>,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct ProofSet {
    pub references: Vec<Vec<AdministrationProof>>,
    pub coverage: Vec<CoverageGroup>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum AdministrationProof {
    DurableGrant {
        record: RecordRef,
    },
    Bootstrap {
        anchor: RecordRef,
        slot: String,
        manifest_digest: BodyDigest,
    },
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct CoverageGroup {
    pub right_ordinal: u8,
    /// target permission, reference index, admin right, admin permission, role.
    pub permissions: Vec<[u16; 5]>,
}

pub(super) fn terminal_bytes(
    value: &AuthorityTransition,
    target: &AuthorityRecord,
    now: Timestamp,
) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(130);
    bytes.extend_from_slice(b"KHTR\0\x01");
    bytes.extend_from_slice(value.id.as_bytes());
    bytes.extend_from_slice(value.epoch_id.as_bytes());
    bytes.push(u8::from(target.kind == RecordKind::Anchor));
    bytes.extend_from_slice(target.id.as_bytes());
    bytes.push(1);
    bytes.extend_from_slice(&RecordRef::of(target).body_digest.0);
    bytes.push(0);
    bytes.push(match value.state {
        TerminalState::Disabled => 1,
        TerminalState::Revoked => 2,
        TerminalState::Retired => 3,
    });
    bytes.extend_from_slice(value.action_id.as_bytes());
    bytes.extend_from_slice(value.event_id.as_bytes());
    bytes.extend_from_slice(&now.as_micros().to_be_bytes());
    bytes
}

pub(super) fn validate_terminal(
    value: &AuthorityTransition,
    target: &AuthorityRecord,
) -> AuthorityResult<()> {
    let micros = u64::try_from(value.created_at)
        .map_err(|_| AuthorityRefusal::Reject("negative terminal time"))?;
    if value.record_id != target.id
        || value.epoch_id != target.epoch_id
        || value.body != terminal_bytes(value, target, Timestamp::from_micros(micros))
        || (target.kind == RecordKind::Anchor) != (value.state == TerminalState::Retired)
    {
        return Err(AuthorityRefusal::Reject("terminal binding mismatch").into());
    }
    Ok(())
}

pub(super) fn sql_time(now: Timestamp) -> AuthorityResult<i64> {
    i64::try_from(now.as_micros())
        .map_err(|_| AuthorityRefusal::Unavailable("clock outside SQL timestamp range").into())
}
