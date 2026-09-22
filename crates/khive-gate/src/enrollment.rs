use std::collections::BTreeSet;
use std::fmt;

use sha2::{Digest, Sha256};

use crate::{
    classify_operation, Gate, GateDecision, GateError, GateRequest, GateValidationError,
    OperationAccess, OPERATION_CLASSIFIER_VERSION,
};

/// Immutable caller-enrollment policy for the built-in configuration gate.
///
/// Explicit actors are matched by their resolved actor id. The implicit
/// anonymous actor is governed separately by `grant_unattributed`, so a list
/// entry named `local` can never accidentally enroll an unattributed caller.
#[derive(Clone)]
pub struct CallerEnrollmentGate {
    granted_actors: BTreeSet<String>,
    grant_unattributed: bool,
    deny_writes_for: BTreeSet<String>,
    invalid_write_policy: bool,
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
            deny_writes_for: BTreeSet::new(),
            invalid_write_policy: false,
            configuration_fingerprint,
        }
    }

    /// Add whole-ID, case-sensitive write restrictions after enrollment.
    /// `*` matches zero or more characters, including `:`; every other
    /// character is literal. No segment hierarchy or escape syntax applies.
    /// An anonymous caller admitted by `grant_unattributed` is restricted if
    /// its fallback ID `local` matches, independently of attributed enrollment.
    /// Empty restrictions preserve [`Self::new`]'s behavior and fingerprint.
    /// Invalid programmatic policy fails every check closed; config-file loaders
    /// should call [`Self::validate_write_denials`] to report it before startup.
    pub fn with_write_denials(
        granted_actors: Vec<String>,
        grant_unattributed: bool,
        deny_writes_for: Vec<String>,
    ) -> Self {
        let mut gate = Self::new(granted_actors, grant_unattributed);
        gate.invalid_write_policy = Self::validate_write_denials(&deny_writes_for).is_err();
        gate.deny_writes_for = deny_writes_for.into_iter().collect();
        if !gate.deny_writes_for.is_empty() || gate.invalid_write_policy {
            gate.configuration_fingerprint = write_policy_fingerprint(
                &gate.configuration_fingerprint,
                OPERATION_CLASSIFIER_VERSION,
                gate.invalid_write_policy,
                &gate.deny_writes_for,
            );
        }
        gate
    }

    /// Validate the bounded, literal-except-`*` pattern format without changing it.
    pub fn validate_write_denials(patterns: &[String]) -> Result<(), GateValidationError> {
        if patterns.len() > 256 {
            return Err(GateValidationError::InvalidWriteDenyPatterns(
                "at most 256 patterns are allowed".into(),
            ));
        }
        for pattern in patterns {
            if pattern.trim().is_empty() || pattern.len() > 256 {
                return Err(GateValidationError::InvalidWriteDenyPatterns(
                    "each pattern must be non-blank and at most 256 UTF-8 bytes".into(),
                ));
            }
        }
        Ok(())
    }

    fn actor_is_granted(&self, req: &GateRequest) -> bool {
        if req.actor.is_anonymous() {
            self.grant_unattributed
        } else {
            self.granted_actors.contains(&req.actor.id)
        }
    }
}

fn write_policy_fingerprint(
    enrollment: &str,
    classifier_version: &str,
    invalid: bool,
    patterns: &BTreeSet<String>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"khive.caller-write-denials.v1\0");
    for field in [enrollment, classifier_version] {
        hasher.update((field.len() as u64).to_be_bytes());
        hasher.update(field.as_bytes());
    }
    hasher.update([u8::from(invalid)]);
    hasher.update((patterns.len() as u64).to_be_bytes());
    for pattern in patterns {
        hasher.update((pattern.len() as u64).to_be_bytes());
        hasher.update(pattern.as_bytes());
    }
    format!("sha256:{:x}", hasher.finalize())
}

#[cfg(test)]
#[path = "write_denials_tests.rs"]
mod write_denials_tests;

impl fmt::Debug for CallerEnrollmentGate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CallerEnrollmentGate")
            .field("granted_actor_count", &self.granted_actors.len())
            .field("grant_unattributed", &self.grant_unattributed)
            .field("write_denial_pattern_count", &self.deny_writes_for.len())
            .field("invalid_write_policy", &self.invalid_write_policy)
            .finish_non_exhaustive()
    }
}

impl Gate for CallerEnrollmentGate {
    fn check(&self, req: &GateRequest) -> Result<GateDecision, GateError> {
        if self.invalid_write_policy {
            return Err(GateError::Policy(
                "invalid deny_writes_for configuration".into(),
            ));
        }
        if self.actor_is_granted(req) {
            if self
                .deny_writes_for
                .iter()
                .any(|pattern| actor_matches(pattern, &req.actor.id))
                && classify_operation(&req.verb) != Some(OperationAccess::Read)
            {
                return Ok(GateDecision::deny(
                    "[gate].deny_writes_for denies this operation",
                ));
            }
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

/// Anchored glob matching with only `*`; no escaping or segment hierarchy.
fn actor_matches(pattern: &str, actor: &str) -> bool {
    let pattern = pattern.as_bytes();
    let actor = actor.as_bytes();
    let (mut p, mut a, mut star, mut retry) = (0, 0, None, 0);
    while a < actor.len() {
        if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            retry = a;
        } else if p < pattern.len() && pattern[p] == actor[a] {
            p += 1;
            a += 1;
        } else if let Some(last_star) = star {
            retry += 1;
            a = retry;
            p = last_star + 1;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}
