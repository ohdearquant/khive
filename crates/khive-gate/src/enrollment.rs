use std::collections::BTreeSet;
use std::fmt;

use sha2::{Digest, Sha256};

use crate::{Gate, GateDecision, GateError, GateRequest};

/// Immutable caller-enrollment policy for the built-in configuration gate.
///
/// Explicit actors are matched by their resolved actor id. The implicit
/// anonymous actor is governed separately by `grant_unattributed`, so a list
/// entry named `local` can never accidentally enroll an unattributed caller.
#[derive(Clone)]
pub struct CallerEnrollmentGate {
    granted_actors: BTreeSet<String>,
    grant_unattributed: bool,
    configuration_fingerprint: String,
}

impl CallerEnrollmentGate {
    /// Construct a deterministic enrollment policy.
    pub fn new(granted_actors: Vec<String>, grant_unattributed: bool) -> Self {
        let granted_actors: BTreeSet<String> = granted_actors.into_iter().collect();
        let mut hasher = Sha256::new();
        hasher.update(b"khive.caller-enrollment-gate.v1\0");
        hasher.update([u8::from(grant_unattributed)]);
        hasher.update((granted_actors.len() as u64).to_be_bytes());
        for actor in &granted_actors {
            hasher.update((actor.len() as u64).to_be_bytes());
            hasher.update(actor.as_bytes());
        }
        let configuration_fingerprint = format!("sha256:{:x}", hasher.finalize());
        Self {
            granted_actors,
            grant_unattributed,
            configuration_fingerprint,
        }
    }

    fn actor_is_granted(&self, req: &GateRequest) -> bool {
        if req.actor.is_anonymous() {
            self.grant_unattributed
        } else {
            self.granted_actors.contains(&req.actor.id)
        }
    }
}

impl fmt::Debug for CallerEnrollmentGate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CallerEnrollmentGate")
            .field("granted_actor_count", &self.granted_actors.len())
            .field("grant_unattributed", &self.grant_unattributed)
            .finish_non_exhaustive()
    }
}

impl Gate for CallerEnrollmentGate {
    fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError> {
        if self.actor_is_granted(req) {
            return Ok(GateDecision::allow());
        }
        let reason = if req.actor.is_anonymous() {
            "unattributed caller is not enrolled"
        } else {
            "actor is not enrolled"
        };
        Ok(GateDecision::deny(reason))
    }

    fn impl_name(&self) -> &'static str {
        "CallerEnrollmentGate"
    }

    fn configuration_fingerprint(&self) -> Option<&str> {
        Some(&self.configuration_fingerprint)
    }
}
