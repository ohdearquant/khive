use super::{AuthorityRefusal, AuthorityResult};
use crate::durable::{
    canonical_principal, canonical_rights, Principal, RightSpec, MAX_COLLECTION_ITEMS,
    MAX_REQUEST_BYTES,
};
use std::collections::BTreeSet;
use uuid::Uuid;

#[derive(Clone, Debug)]
pub enum GrantParent {
    Grant(Uuid),
    BootstrapSlot(String),
}
#[derive(Clone, Debug)]
pub struct GrantItem {
    pub grantee: Principal,
    pub namespace: String,
    pub parent: GrantParent,
    pub rights: Vec<RightSpec>,
}
#[derive(Clone, Debug)]
pub struct GrantRequest {
    pub request_id: String,
    pub rationale: String,
    pub grants: Vec<GrantItem>,
}
#[derive(Clone, Debug)]
pub struct RevokeRequest {
    pub request_id: String,
    pub rationale: String,
    pub grant_ids: Vec<Uuid>,
}
#[derive(Clone, Debug)]
pub struct RetireAnchorRequest {
    pub request_id: String,
    pub rationale: String,
    pub root_anchor_id: Uuid,
}

pub(super) struct Canonical {
    bytes: Option<Vec<u8>>,
    measured: usize,
}
impl Canonical {
    pub(super) fn new(verb: &str, request_id: &str, rationale: &str) -> AuthorityResult<Self> {
        if request_id.is_empty() || rationale.trim().is_empty() {
            return Err(AuthorityRefusal::InvalidInput(
                "request id and nonblank rationale required",
            )
            .into());
        }
        if rationale.len() > 4096 {
            return Err(crate::durable::EnvelopeError::OverLimit {
                field: "rationale",
                measured: rationale.len(),
                bound: 4096,
            }
            .into());
        }
        let mut out = Self {
            bytes: Some(Vec::new()),
            measured: 0,
        };
        out.put(b"KHAR\0\x01")?;
        out.field(verb.as_bytes())?;
        out.field(request_id.as_bytes())?;
        out.field(rationale.as_bytes())?;
        Ok(out)
    }
    pub(super) fn put(&mut self, value: &[u8]) -> AuthorityResult<()> {
        self.measured = self
            .measured
            .checked_add(value.len())
            .ok_or(AuthorityRefusal::InvalidInput("request length overflow"))?;
        if self.measured > MAX_REQUEST_BYTES {
            self.bytes = None;
        }
        if let Some(bytes) = &mut self.bytes {
            bytes.extend_from_slice(value);
        }
        Ok(())
    }
    pub(super) fn field(&mut self, value: &[u8]) -> AuthorityResult<()> {
        let length = u32::try_from(value.len())
            .map_err(|_| AuthorityRefusal::InvalidInput("request field length overflow"))?;
        self.put(&length.to_be_bytes())?;
        self.put(value)
    }
    pub(super) fn finish(self) -> AuthorityResult<Vec<u8>> {
        self.bytes.ok_or_else(|| {
            crate::durable::EnvelopeError::OverLimit {
                field: "canonical request",
                measured: self.measured,
                bound: MAX_REQUEST_BYTES,
            }
            .into()
        })
    }
}
fn collection(length: usize) -> AuthorityResult<()> {
    if length == 0 {
        return Err(AuthorityRefusal::InvalidInput("empty administration collection").into());
    }
    if length > MAX_COLLECTION_ITEMS {
        return Err(crate::durable::EnvelopeError::OverLimit {
            field: "administration collection",
            measured: length,
            bound: MAX_COLLECTION_ITEMS,
        }
        .into());
    }
    Ok(())
}
impl GrantRequest {
    pub(super) fn canonical(&self) -> AuthorityResult<Vec<u8>> {
        collection(self.grants.len())?;
        let mut out = Canonical::new("authority.grant", &self.request_id, &self.rationale)?;
        out.put(&(self.grants.len() as u32).to_be_bytes())?;
        for item in &self.grants {
            item.grantee.validate()?;
            if item.namespace.is_empty() {
                return Err(AuthorityRefusal::InvalidInput("empty namespace").into());
            }
            collection(item.rights.len())?;
            out.field(&canonical_principal(&item.grantee)?)?;
            out.field(item.namespace.as_bytes())?;
            match &item.parent {
                GrantParent::Grant(id) => {
                    out.put(&[0])?;
                    out.put(id.as_bytes())?;
                }
                GrantParent::BootstrapSlot(slot) => {
                    if slot.is_empty() {
                        return Err(AuthorityRefusal::InvalidInput("empty bootstrap slot").into());
                    }
                    out.put(&[1])?;
                    out.field(slot.as_bytes())?;
                }
            }
            out.field(&canonical_rights(&item.rights)?)?;
        }
        out.finish()
    }
}
impl RevokeRequest {
    pub(super) fn canonical(&self) -> AuthorityResult<Vec<u8>> {
        collection(self.grant_ids.len())?;
        // Closed canonical intake refuses duplicates; no client knob or reordering.
        if self
            .grant_ids
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .len()
            != self.grant_ids.len()
        {
            return Err(AuthorityRefusal::InvalidInput("duplicate grant id").into());
        }
        let mut out = Canonical::new("authority.revoke", &self.request_id, &self.rationale)?;
        out.put(&(self.grant_ids.len() as u32).to_be_bytes())?;
        for id in &self.grant_ids {
            out.put(id.as_bytes())?;
        }
        out.finish()
    }
}
impl RetireAnchorRequest {
    pub(super) fn canonical(&self) -> AuthorityResult<Vec<u8>> {
        let mut out = Canonical::new("authority.retire_anchor", &self.request_id, &self.rationale)?;
        out.put(self.root_anchor_id.as_bytes())?;
        out.finish()
    }
}
