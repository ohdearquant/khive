use super::codec::{decode, encode, encoded_len};
use super::keys::KeyRef;
use super::permission::*;
use super::{
    bound, nonempty, ClosedCatalog, EnvelopeError, EnvelopeResult, SuppliedKeys, ALIAS_END,
    ALIAS_START, CARRIER_VERSION, MAX_RECORD_BYTES,
};
use hmac::{Hmac, Mac};
use khive_types::Timestamp;
use lion_core::{Capability, Kernel, Rights, SealedTag, SecurityLevel};
use sha2::Sha256;
use uuid::Uuid;

pub(super) const MAGIC: &[u8; 6] = b"KHAE\0\x01";
pub(super) const DOMAIN: &[u8] = b"khive.authority.grant-envelope\0";
const OUTPUT_END: u128 = 1_u128 << 126;
const PRIVATE_ANCHOR: u128 = u128::MAX - 1;
const RECORD_OVERHEAD: usize = 6 + 4 + 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum IssuanceAssurance {
    ConfiguredLocal,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CreationOperation {
    Grant,
    Import,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvelopeDraft {
    pub id: Uuid,
    pub store_id: Uuid,
    pub epoch_id: Uuid,
    pub root_anchor_id: Uuid,
    pub issuer: Principal,
    pub grantee: Principal,
    pub namespace: String,
    pub rights: Vec<RightSpec>,
    pub validity: Validity,
    pub created_at: Timestamp,
    pub assurance: IssuanceAssurance,
    pub operation: CreationOperation,
    pub action_id: Uuid,
    pub configuration_source: Option<String>,
    pub issuance_id: Uuid,
    pub key: KeyRef,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeCapability {
    pub id: u128,
    pub holder: u128,
    pub target: u128,
    pub rights: u16,
    pub parent: Option<u128>,
    pub epoch: u64,
    pub signature: [u8; 32],
}
impl NativeCapability {
    fn from_core(cap: &Capability) -> Self {
        let mut signature = [0; 32];
        signature.copy_from_slice(cap.signature().as_bytes());
        Self {
            id: cap.id(),
            holder: cap.holder(),
            target: cap.target(),
            rights: cap.rights().to_bits(),
            parent: cap.parent(),
            epoch: cap.epoch(),
            signature,
        }
    }
    fn to_core(&self, id: u128) -> EnvelopeResult<Capability> {
        if self.parent == Some(u128::MAX) || self.rights & !Rights::all().to_bits() != 0 {
            return Err(EnvelopeError::Reject(
                "invalid native parent or rights bits",
            ));
        }
        Capability::new(
            id,
            self.holder,
            self.target,
            Rights::from_bits(self.rights),
            self.parent,
            self.epoch,
            SealedTag::from_bytes(self.signature),
        )
        .map_err(|_| EnvelopeError::Reject("invalid native capability"))
    }
    pub fn canonical_digest(&self) -> EnvelopeResult<ContractDigest> {
        Ok(ContractDigest::blake3(&encode(self)?))
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParentBinding {
    Grant {
        grant_id: Uuid,
        issuance_id: Uuid,
        original_local_id: u128,
        alias_id: u128,
        native_digest: ContractDigest,
    },
    BootstrapSlot {
        slot: String,
        manifest_digest: ContractDigest,
        anchor_local_id: u128,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssuanceContext {
    pub id: Uuid,
    pub holder: u128,
    pub holder_principal: Principal,
    pub target: u128,
    pub namespace: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantBody {
    pub schema_version: u16,
    pub profile_version: u16,
    pub carrier_version: u16,
    pub grant: EnvelopeDraft,
    pub parent: ParentBinding,
    pub context: IssuanceContext,
    pub native: NativeCapability,
}

/// A cryptographically authenticated immutable record, not effective authority.
/// The store's current terminal state, full UUID ancestry and root anchor still
/// have to be checked in the transaction that commits a protected operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthenticatedEnvelope {
    body: GrantBody,
    bytes: Vec<u8>,
}
impl AuthenticatedEnvelope {
    pub fn body(&self) -> &GrantBody {
        &self.body
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Finite composition-root source; this is not a persisted unsealed capability.
/// The service may use it only after checking its immutable initialized manifest
/// and live epoch/root anchor in its active writer transaction.
#[derive(Clone, Debug)]
pub struct BootstrapSource {
    pub store_id: Uuid,
    pub epoch_id: Uuid,
    pub root_anchor_id: Uuid,
    pub owner: Principal,
    pub namespace: String,
    pub slot: String,
    pub manifest_digest: ContractDigest,
    pub rights: Vec<RightSpec>,
    pub validity: Validity,
}

pub struct EnvelopeVerifier<'a> {
    keys: &'a SuppliedKeys,
    catalog: &'a ClosedCatalog,
}
impl<'a> EnvelopeVerifier<'a> {
    pub fn new(keys: &'a SuppliedKeys, catalog: &'a ClosedCatalog) -> Self {
        Self { keys, catalog }
    }
    pub fn verify(
        &self,
        bytes: &[u8],
        store_id: Uuid,
        epoch_id: Uuid,
    ) -> EnvelopeResult<AuthenticatedEnvelope> {
        bound("canonical record", bytes.len(), MAX_RECORD_BYTES)?;
        if bytes.len() < RECORD_OVERHEAD || bytes.get(..6) != Some(MAGIC.as_slice()) {
            return Err(EnvelopeError::Reject("unknown envelope header"));
        }
        let length = u32::from_be_bytes(
            bytes[6..10]
                .try_into()
                .map_err(|_| EnvelopeError::Reject("invalid envelope length"))?,
        ) as usize;
        if length.checked_add(RECORD_OVERHEAD) != Some(bytes.len()) {
            return Err(EnvelopeError::Reject(
                "record length or trailing bytes mismatch",
            ));
        }
        let encoded = &bytes[10..10 + length];
        let body: GrantBody = decode(encoded)?;
        let mac = envelope_mac(self.keys.extension(&body.grant.key)?, encoded)?;
        mac.verify_slice(&bytes[10 + length..])
            .map_err(|_| EnvelopeError::Reject("extension MAC mismatch"))?;
        self.validate_body(&body, store_id, epoch_id)?;
        let native = body.native.to_core(body.native.id)?;
        let kernel = Kernel::with_key(self.keys.native(&body.grant.key)?)
            .map_err(|_| EnvelopeError::Unavailable("native key unavailable"))?;
        if !kernel.state().kernel().verify_cap_seal(&native) {
            return Err(EnvelopeError::Reject("native seal mismatch"));
        }
        Ok(AuthenticatedEnvelope {
            body,
            bytes: bytes.to_vec(),
        })
    }

    /// Verify one immutable UUID lineage link. This does not check live DB state.
    pub fn verify_delegation_link(
        &self,
        child: &AuthenticatedEnvelope,
        parent: &AuthenticatedEnvelope,
    ) -> EnvelopeResult<()> {
        let c = self.verify(
            child.as_bytes(),
            child.body.grant.store_id,
            child.body.grant.epoch_id,
        )?;
        let p = self.verify(
            parent.as_bytes(),
            c.body.grant.store_id,
            c.body.grant.epoch_id,
        )?;
        let ParentBinding::Grant {
            grant_id,
            issuance_id,
            original_local_id,
            alias_id,
            native_digest,
        } = &c.body.parent
        else {
            return Err(EnvelopeError::Reject("child does not name a grant parent"));
        };
        if *grant_id != p.body.grant.id
            || *issuance_id != p.body.grant.issuance_id
            || *original_local_id != p.body.native.id
            || *native_digest != p.body.native.canonical_digest()?
            || c.body.native.parent != Some(*alias_id)
            || c.body.native.target != p.body.native.target
        {
            return Err(EnvelopeError::Reject(
                "authenticated parent alias does not match durable parent",
            ));
        }
        self.check_parent_scope(&p.body.grant, &c.body.grant)
    }
    fn check_parent_scope(
        &self,
        parent: &EnvelopeDraft,
        child: &EnvelopeDraft,
    ) -> EnvelopeResult<()> {
        if parent.id == child.id
            || parent.issuance_id == child.issuance_id
            || parent.store_id != child.store_id
            || parent.epoch_id != child.epoch_id
            || parent.root_anchor_id != child.root_anchor_id
            || parent.namespace != child.namespace
            || parent.grantee != child.issuer
            || !parent.validity.covers(&child.validity)
            || !self.catalog.covers_rights(&parent.rights, &child.rights)
        {
            return Err(EnvelopeError::Reject(
                "parent holder, scope or interval does not cover child",
            ));
        }
        Ok(())
    }
    fn validate_body(&self, body: &GrantBody, store: Uuid, epoch: Uuid) -> EnvelopeResult<()> {
        if body.schema_version != 1
            || body.profile_version != 1
            || body.carrier_version != CARRIER_VERSION
        {
            return Err(EnvelopeError::Reject(
                "unsupported envelope or carrier version",
            ));
        }
        validate_draft(self.catalog, &body.grant)?;
        if body.grant.store_id != store
            || body.grant.epoch_id != epoch
            || body.grant.id == body.grant.root_anchor_id
        {
            return Err(EnvelopeError::Reject(
                "store, epoch or record role mismatch",
            ));
        }
        let carriers = self.catalog.validate_rights(
            &body.grant.rights,
            &body.grant.namespace,
            &body.grant.grantee,
        )?;
        if body.native.epoch != 0
            || body.native.id >= OUTPUT_END
            || body.native.rights != carriers.to_bits()
            || body.native.holder != 1
            || body.native.target != 1
            || body.context.id != body.grant.issuance_id
            || body.context.holder != body.native.holder
            || body.context.target != body.native.target
            || body.context.holder_principal != body.grant.grantee
            || body.context.namespace != body.grant.namespace
        {
            return Err(EnvelopeError::Reject(
                "native capability or local interpretation mismatch",
            ));
        }
        match &body.parent {
            ParentBinding::Grant {
                grant_id,
                issuance_id,
                original_local_id,
                alias_id,
                ..
            } => {
                check_alias(*alias_id)?;
                if *grant_id == body.grant.id
                    || *issuance_id == body.grant.issuance_id
                    || *original_local_id >= OUTPUT_END
                    || body.native.parent != Some(*alias_id)
                {
                    return Err(EnvelopeError::Reject("invalid durable parent binding"));
                }
            }
            ParentBinding::BootstrapSlot {
                slot,
                anchor_local_id,
                ..
            } => {
                nonempty(slot)?;
                if !(ALIAS_END..u128::MAX).contains(anchor_local_id)
                    || body.native.parent != Some(*anchor_local_id)
                {
                    return Err(EnvelopeError::Reject(
                        "invalid private bootstrap parent binding",
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Trusted issuance helper, not a caller-facing authorization decision.
/// The service must validate live durable ancestry, current administration and
/// its configured policy inside its writer before calling this helper. The
/// helper enforces cryptography, structural holder and immutable attenuation;
/// each private scratch Kernel is discarded before returning.
pub struct EnvelopeIssuer<'a> {
    verifier: EnvelopeVerifier<'a>,
}
impl<'a> EnvelopeIssuer<'a> {
    pub fn new(keys: &'a SuppliedKeys, catalog: &'a ClosedCatalog) -> Self {
        Self {
            verifier: EnvelopeVerifier::new(keys, catalog),
        }
    }
    pub fn issue_from_bootstrap(
        &self,
        source: &BootstrapSource,
        grant: EnvelopeDraft,
    ) -> EnvelopeResult<AuthenticatedEnvelope> {
        bound("unsigned grant", encoded_len(&grant)?, MAX_RECORD_BYTES)?;
        validate_draft(self.verifier.catalog, &grant)?;
        source.owner.validate()?;
        source.validity.validate()?;
        nonempty(&source.slot)?;
        let source_carriers = self.verifier.catalog.validate_source_rights(
            &source.rights,
            &source.namespace,
            &source.owner,
        )?;
        if source.owner.leg.is_some()
            || source.owner != grant.issuer
            || source.store_id != grant.store_id
            || source.epoch_id != grant.epoch_id
            || source.root_anchor_id != grant.root_anchor_id
            || source.namespace != grant.namespace
            || !source.validity.covers(&grant.validity)
            || !self
                .verifier
                .catalog
                .covers_rights(&source.rights, &grant.rights)
        {
            return Err(EnvelopeError::Reject(
                "bootstrap source does not cover issued record",
            ));
        }
        let parent = ParentBinding::BootstrapSlot {
            slot: source.slot.clone(),
            manifest_digest: source.manifest_digest,
            anchor_local_id: PRIVATE_ANCHOR,
        };
        let mut kernel = self.kernel(&grant)?;
        let anchor = Capability::new(
            PRIVATE_ANCHOR,
            1,
            1,
            source_carriers,
            None,
            0,
            SealedTag::from_bytes([0; 32]),
        )
        .map_err(|e| EnvelopeError::Native(e.to_string()))?;
        kernel
            .insert_cap_raw(anchor, 1)
            .map_err(|e| EnvelopeError::Native(e.to_string()))?;
        self.delegate_and_seal(kernel, PRIVATE_ANCHOR, grant, parent)
    }
    pub fn delegate(
        &self,
        parent: &AuthenticatedEnvelope,
        grant: EnvelopeDraft,
    ) -> EnvelopeResult<AuthenticatedEnvelope> {
        bound("unsigned grant", encoded_len(&grant)?, MAX_RECORD_BYTES)?;
        validate_draft(self.verifier.catalog, &grant)?;
        let alias = issuance_alias(grant.issuance_id);
        check_alias(alias)?;
        let parent = self
            .verifier
            .verify(parent.as_bytes(), grant.store_id, grant.epoch_id)?;
        self.verifier
            .check_parent_scope(&parent.body.grant, &grant)?;
        let binding = ParentBinding::Grant {
            grant_id: parent.body.grant.id,
            issuance_id: parent.body.grant.issuance_id,
            original_local_id: parent.body.native.id,
            alias_id: alias,
            native_digest: parent.body.native.canonical_digest()?,
        };
        let mut kernel = self.kernel(&grant)?;
        let reconstructed = parent.body.native.to_core(alias)?;
        kernel
            .insert_cap_raw(reconstructed, parent.body.native.holder)
            .map_err(|e| EnvelopeError::Native(e.to_string()))?;
        let child = self.delegate_and_seal(kernel, alias, grant, binding)?;
        self.verifier.verify_delegation_link(&child, &parent)?;
        Ok(child)
    }
    fn kernel(&self, grant: &EnvelopeDraft) -> EnvelopeResult<Kernel> {
        let mut kernel = Kernel::with_key(self.verifier.keys.native(&grant.key)?)
            .map_err(|_| EnvelopeError::Unavailable("native key unavailable"))?;
        kernel
            .register_plugin(1, SecurityLevel::BOT, 0)
            .map_err(|e| EnvelopeError::Native(e.to_string()))?;
        Ok(kernel)
    }
    fn delegate_and_seal(
        &self,
        mut kernel: Kernel,
        parent_id: u128,
        grant: EnvelopeDraft,
        parent: ParentBinding,
    ) -> EnvelopeResult<AuthenticatedEnvelope> {
        check_output_capacity(kernel.state().kernel().next_cap_id())?;
        let rights = self.verifier.catalog.validate_rights(
            &grant.rights,
            &grant.namespace,
            &grant.grantee,
        )?;
        // The same shape is sized before minting; native ids/signatures have fixed width.
        let mut body = GrantBody {
            schema_version: 1,
            profile_version: 1,
            carrier_version: CARRIER_VERSION,
            context: IssuanceContext {
                id: grant.issuance_id,
                holder: 1,
                holder_principal: grant.grantee.clone(),
                target: 1,
                namespace: grant.namespace.clone(),
            },
            grant,
            parent,
            native: NativeCapability {
                id: 0,
                holder: 1,
                target: 1,
                rights: rights.to_bits(),
                parent: Some(parent_id),
                epoch: 0,
                signature: [0; 32],
            },
        };
        check_record_size(encoded_len(&body)?)?;
        let id = kernel
            .delegate_cap(parent_id, 1, rights)
            .map_err(|e| EnvelopeError::Native(e.to_string()))?;
        let cap = kernel
            .get_cap(id)
            .ok_or(EnvelopeError::Reject("native delegate returned no record"))?;
        if *cap.rights() != rights {
            return Err(EnvelopeError::Reject(
                "native intersection narrowed requested authority",
            ));
        }
        body.native = NativeCapability::from_core(cap);
        let bytes = seal_body(&body, self.verifier.keys)?;
        self.verifier
            .verify(&bytes, body.grant.store_id, body.grant.epoch_id)
    }
}
fn validate_draft(catalog: &ClosedCatalog, grant: &EnvelopeDraft) -> EnvelopeResult<()> {
    grant.issuer.validate()?;
    grant.grantee.validate()?;
    grant.validity.validate()?;
    nonempty(&grant.namespace)?;
    nonempty(&grant.key.id)?;
    if grant.issuer.leg.is_some() {
        return Err(EnvelopeError::Reject("subactor cannot issue grants"));
    }
    if grant.grantee.leg.is_some() && grant.grantee.parent() != grant.issuer {
        return Err(EnvelopeError::Reject("only parent can grant to its leg"));
    }
    match (&grant.operation, &grant.configuration_source) {
        (CreationOperation::Import, Some(source)) => nonempty(source)?,
        (CreationOperation::Grant, None) => {}
        _ => {
            return Err(EnvelopeError::InvalidInput(
                "creation provenance does not match operation",
            ))
        }
    }
    catalog.validate_rights(&grant.rights, &grant.namespace, &grant.grantee)?;
    Ok(())
}
pub(super) fn issuance_alias(id: Uuid) -> u128 {
    ALIAS_START | (id.as_u128() & ((1_u128 << 126) - 1))
}
pub(super) fn check_alias(alias: u128) -> EnvelopeResult<()> {
    if !(ALIAS_START..ALIAS_END).contains(&alias) {
        return Err(EnvelopeError::IdSpaceExhausted);
    }
    Ok(())
}
pub(super) fn check_output_capacity(next: u128) -> EnvelopeResult<()> {
    if next >= OUTPUT_END {
        return Err(EnvelopeError::IdSpaceExhausted);
    }
    Ok(())
}
fn check_record_size(body_length: usize) -> EnvelopeResult<usize> {
    let length = body_length
        .checked_add(RECORD_OVERHEAD)
        .ok_or(EnvelopeError::OverLimit {
            field: "canonical record",
            measured: usize::MAX,
            bound: MAX_RECORD_BYTES,
        })?;
    bound("canonical record", length, MAX_RECORD_BYTES)?;
    Ok(length)
}
pub(super) fn envelope_mac(key: &[u8; 32], body: &[u8]) -> EnvelopeResult<Hmac<Sha256>> {
    let length = u32::try_from(body.len())
        .map_err(|_| EnvelopeError::InvalidInput("body length exceeds u32"))?;
    let mut mac = Hmac::<Sha256>::new_from_slice(key)
        .map_err(|_| EnvelopeError::Unavailable("extension key unavailable"))?;
    mac.update(DOMAIN);
    mac.update(&1_u16.to_be_bytes());
    mac.update(&length.to_be_bytes());
    mac.update(body);
    Ok(mac)
}
pub(super) fn seal_body(body: &GrantBody, keys: &SuppliedKeys) -> EnvelopeResult<Vec<u8>> {
    let capacity = check_record_size(encoded_len(body)?)?;
    let encoded = encode(body)?;
    let tag = envelope_mac(keys.extension(&body.grant.key)?, &encoded)?
        .finalize()
        .into_bytes();
    let mut bytes = Vec::with_capacity(capacity);
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&(encoded.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&encoded);
    bytes.extend_from_slice(&tag);
    Ok(bytes)
}
