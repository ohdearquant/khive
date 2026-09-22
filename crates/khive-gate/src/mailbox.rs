use std::collections::BTreeSet;
use std::fmt;

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{ActorRef, Gate, GateDecision, GateError, GateRef, GateRequest};

const FINGERPRINT_VERSION: &[u8] = b"khive.mailbox-read-gate.v1\0";

/// Malformed mailbox policy or selector. Labels are exact values, never namespaces.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum MailboxPolicyError {
    #[error("mailbox readers require an explicit, non-local actor owner")]
    InvalidOwner,
    #[error("mailbox reader must be a non-anonymous, non-local actor with a nonblank label of at most 255 bytes and no control characters")]
    InvalidReader,
    #[error("mailbox_readers accepts at most 256 entries before deduplication")]
    TooManyReaders,
    #[error("mailbox_actor must be a nonblank, non-local string of at most 255 bytes with no control characters")]
    InvalidSelector,
}

/// Whether an exact mailbox actor label is eligible for explicit selection.
///
/// Colons and other ordinary characters are literal; this does not parse a
/// namespace, trim the label, split a hierarchy, or match a wildcard.
pub fn is_valid_mailbox_actor_label(label: &str) -> bool {
    !label.trim().is_empty()
        && label.len() <= 255
        && !label.chars().any(char::is_control)
        && label != "local"
}

fn valid_actor(actor: &ActorRef) -> bool {
    !actor.is_anonymous()
        && !actor.kind.trim().is_empty()
        && actor.kind.len() <= 255
        && !actor.kind.chars().any(char::is_control)
        && is_valid_mailbox_actor_label(&actor.id)
}

/// Validate a read selector and return its owner only for a delegated view.
///
/// Runtime labels resolve to `ActorRef { kind: "actor", id: full_label }`.
/// An omitted selector retains legacy behavior, including the local mailbox.
pub fn mailbox_read_owner(req: &GateRequest) -> Result<Option<ActorRef>, MailboxPolicyError> {
    if !matches!(req.verb.as_str(), "comm.inbox" | "comm.thread") {
        return Ok(None);
    }
    let Some(value) = req.args.get("mailbox_actor") else {
        return Ok(None);
    };
    let label = value
        .as_str()
        .filter(|label| is_valid_mailbox_actor_label(label))
        .ok_or(MailboxPolicyError::InvalidSelector)?;
    let owner = ActorRef::new("actor", label);
    Ok((owner != req.actor).then_some(owner))
}

fn apply_mailbox_policy(
    gate: &(impl Gate + ?Sized),
    req: &GateRequest,
    base: GateDecision,
) -> Result<GateDecision, GateError> {
    let GateDecision::Allow { mut obligations } = base else {
        return Ok(base);
    };
    let Some(owner) = mailbox_read_owner(req).map_err(|e| GateError::Policy(e.to_string()))? else {
        return Ok(GateDecision::allow_with(obligations));
    };
    if !valid_actor(&req.actor) {
        return Ok(GateDecision::deny("mailbox_read_not_granted"));
    }
    match gate.check_mailbox_read(req, &owner)? {
        GateDecision::Allow { obligations: extra } => {
            obligations.extend(extra);
            Ok(GateDecision::allow_with(obligations))
        }
        denied => Ok(denied),
    }
}

/// Compose ordinary admission with the separate, default-deny mailbox capability.
///
/// Dispatchers use this even when no mailbox policy is installed, so a generic
/// permissive gate cannot accidentally admit an explicit cross-actor selector.
pub fn check_with_mailbox_policy(
    gate: &(impl Gate + ?Sized),
    req: &GateRequest,
) -> Result<GateDecision, GateError> {
    apply_mailbox_policy(gate, req, gate.check(req)?)
}

/// Immutable trusted-local owner/reader policy, composed with an existing gate.
///
/// Grants compare both actor kind and complete id. A caller label containing
/// colons is not split into a hierarchy. This configuration is trusted host
/// policy, not caller authentication. Replacing it requires a new serving epoch.
#[derive(Clone)]
pub struct MailboxReadGate {
    inner: GateRef,
    owner: ActorRef,
    readers: BTreeSet<(String, String)>,
    fingerprint: String,
}

impl MailboxReadGate {
    /// Construct and validate a bounded policy. Duplicate readers are harmless;
    /// the raw list is bounded before deduplication.
    pub fn new(
        inner: GateRef,
        owner: ActorRef,
        readers: Vec<ActorRef>,
    ) -> Result<Self, MailboxPolicyError> {
        if !valid_actor(&owner) {
            return Err(MailboxPolicyError::InvalidOwner);
        }
        if readers.len() > 256 {
            return Err(MailboxPolicyError::TooManyReaders);
        }
        if readers.iter().any(|reader| !valid_actor(reader)) {
            return Err(MailboxPolicyError::InvalidReader);
        }
        let readers: BTreeSet<_> = readers.into_iter().map(|a| (a.kind, a.id)).collect();
        let mut hash = Sha256::new();
        hash.update(FINGERPRINT_VERSION);
        match inner.configuration_fingerprint() {
            Some(value) => {
                hash.update([1]);
                hash_field(&mut hash, value);
            }
            None => hash.update([0]),
        }
        // Include the owner even for an empty list, and encode every pair
        // structurally rather than concatenating colon-containing labels.
        hash_field(&mut hash, &owner.kind);
        hash_field(&mut hash, &owner.id);
        hash.update((readers.len() as u64).to_be_bytes());
        for (kind, id) in &readers {
            hash_field(&mut hash, kind);
            hash_field(&mut hash, id);
        }
        let fingerprint = format!("sha256:{:x}", hash.finalize());
        Ok(Self {
            inner,
            owner,
            readers,
            fingerprint,
        })
    }
}

fn hash_field(hash: &mut Sha256, value: &str) {
    hash.update((value.len() as u64).to_be_bytes());
    hash.update(value.as_bytes());
}

impl fmt::Debug for MailboxReadGate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MailboxReadGate")
            .field("inner", &self.inner.impl_name())
            .field("reader_count", &self.readers.len())
            .finish_non_exhaustive()
    }
}

impl Gate for MailboxReadGate {
    fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError> {
        apply_mailbox_policy(self, req, self.inner.check(req)?)
    }

    fn check_mailbox_read(
        &self,
        req: &GateRequest,
        owner: &ActorRef,
    ) -> Result<GateDecision, GateError> {
        if matches!(req.verb.as_str(), "comm.inbox" | "comm.thread")
            && valid_actor(&req.actor)
            && owner == &self.owner
            && self
                .readers
                .contains(&(req.actor.kind.clone(), req.actor.id.clone()))
        {
            Ok(GateDecision::allow())
        } else {
            Ok(GateDecision::deny("mailbox_read_not_granted"))
        }
    }

    fn configuration_fingerprint(&self) -> Option<&str> {
        Some(&self.fingerprint)
    }
}

#[cfg(test)]
mod tests;
