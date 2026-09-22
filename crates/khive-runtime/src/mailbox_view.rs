use std::sync::Arc;

use khive_gate::{check_with_mailbox_policy, mailbox_read_owner};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    engine_config::ActorConfig, ActorRef, Gate, GateDecision, GateError, GateRef, GateRequest,
    KhiveRuntime, MailboxPolicyError, MailboxReadGate, NamespaceToken, RuntimeError, RuntimeResult,
};

/// An authorized mailbox selection. This never replaces the real caller token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailboxView {
    pub actor_id: String,
    pub delegated: bool,
}

/// The same strict selector check is used before ordinary/intercepted dispatch
/// and before direct pack-handler reads. JSON null is not an omitted selector.
pub(crate) fn validate_mailbox_request(req: &GateRequest) -> RuntimeResult<()> {
    mailbox_read_owner(req)
        .map(|_| ())
        .map_err(|error| RuntimeError::InvalidInput(error.to_string()))
}

impl KhiveRuntime {
    /// Authorize a read-only mailbox view without changing token identity.
    ///
    /// The original arguments are retained as gate input. Direct handler calls
    /// consult both the existing policy and the separate mailbox capability;
    /// a permissive or unconfigured gate never grants cross-actor reads.
    pub fn authorize_mailbox_view(
        &self,
        token: &NamespaceToken,
        verb: &str,
        selector: Option<&str>,
        args: &Value,
    ) -> RuntimeResult<MailboxView> {
        if !matches!(verb, "comm.inbox" | "comm.thread") {
            return Err(RuntimeError::InvalidInput(
                "mailbox views are supported only by comm.inbox and comm.thread".into(),
            ));
        }
        let req = GateRequest::new(
            token.actor().clone(),
            token.gate_namespace().clone(),
            verb,
            args.clone(),
        );
        validate_mailbox_request(&req)?;
        if args.get("mailbox_actor").and_then(Value::as_str) != selector {
            return Err(RuntimeError::InvalidInput(
                "mailbox selector must match the original mailbox_actor argument".into(),
            ));
        }
        match check_with_mailbox_policy(self.config().gate.as_ref(), &req) {
            Ok(GateDecision::Allow { .. }) => {
                let owner = mailbox_read_owner(&req)
                    .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
                Ok(MailboxView {
                    delegated: owner.is_some(),
                    actor_id: owner.map_or_else(|| token.actor().id.clone(), |owner| owner.id),
                })
            }
            Ok(GateDecision::Deny { reason }) => Err(RuntimeError::permission_denied(verb, reason)),
            Err(error) => Err(RuntimeError::GateUnavailable {
                verb: verb.to_string(),
                reason: error.wire_reason().to_string(),
            }),
        }
    }
}

impl ActorConfig {
    pub(crate) fn mailbox_gate(
        &self,
        inner: GateRef,
    ) -> Result<Option<MailboxReadGate>, MailboxPolicyError> {
        if self.mailbox_readers.is_empty() {
            return Ok(None);
        }
        if self.mailbox_readers.len() > 256 {
            return Err(MailboxPolicyError::TooManyReaders);
        }
        if self
            .mailbox_readers
            .iter()
            .any(|id| !crate::is_valid_mailbox_actor_label(id))
        {
            return Err(MailboxPolicyError::InvalidReader);
        }
        // Only the explicit serving-file owner can grant access; never infer
        // this from the effective runtime actor (which may come from env/CLI).
        let owner = self.id.as_deref().ok_or(MailboxPolicyError::InvalidOwner)?;
        crate::Namespace::parse(owner).map_err(|_| MailboxPolicyError::InvalidOwner)?;
        let readers = self
            .mailbox_readers
            .iter()
            .map(|id| ActorRef::new("actor", id.clone()))
            .collect();
        MailboxReadGate::new(inner, ActorRef::new("actor", owner), readers).map(Some)
    }
}

/// Boot conversion is intentionally infallible for existing embedding callers.
/// Invalid programmatic mailbox configuration therefore installs an unavailable
/// gate rather than falling back to the base policy. File loading rejects it.
pub(crate) fn configured_mailbox_gate(actor: &ActorConfig, inner: GateRef) -> GateRef {
    match actor.mailbox_gate(inner.clone()) {
        Ok(Some(gate)) => Arc::new(gate),
        Ok(None) => inner,
        Err(_) => {
            let mut hash = Sha256::new();
            hash.update(b"khive.invalid-mailbox-read-policy.v1\0");
            for field in [inner.configuration_fingerprint(), actor.id.as_deref()] {
                match field {
                    Some(value) => {
                        hash.update([1]);
                        hash.update((value.len() as u64).to_be_bytes());
                        hash.update(value.as_bytes());
                    }
                    None => hash.update([0]),
                }
            }
            hash.update((actor.mailbox_readers.len() as u64).to_be_bytes());
            for reader in &actor.mailbox_readers {
                hash.update((reader.len() as u64).to_be_bytes());
                hash.update(reader.as_bytes());
            }
            Arc::new(InvalidMailboxConfigGate {
                fingerprint: format!("sha256:{:x}", hash.finalize()),
            })
        }
    }
}

#[derive(Debug)]
struct InvalidMailboxConfigGate {
    fingerprint: String,
}

impl Gate for InvalidMailboxConfigGate {
    fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
        Err(GateError::Policy(
            "invalid mailbox reader configuration".into(),
        ))
    }

    fn configuration_fingerprint(&self) -> Option<&str> {
        Some(&self.fingerprint)
    }
}

#[cfg(test)]
mod tests;
