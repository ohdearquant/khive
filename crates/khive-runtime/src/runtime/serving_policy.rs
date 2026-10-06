//! Per-runtime serving policies fixed before the runtime serves: ADR-118's fresh-tail
//! policy and the outbound email recipient policy.

use super::KhiveRuntime;

impl KhiveRuntime {
    /// Return the immutable ADR-118 fresh-tail serving policy captured when
    /// this runtime was constructed.
    pub fn ann_fresh_tail_enabled(&self) -> bool {
        self.ann_fresh_tail_enabled
    }

    /// Install before registering or cloning serving handles; no request-time mutation.
    pub fn with_outbound_email_policy(mut self, policy: crate::OutboundEmailPolicy) -> Self {
        self.outbound_email_policy = policy;
        self
    }

    pub fn outbound_email_policy(&self) -> &crate::OutboundEmailPolicy {
        &self.outbound_email_policy
    }

    /// Override ADR-118's fresh-tail serving policy for this runtime instance.
    ///
    /// This is primarily useful for embedded runtimes and deterministic tests:
    /// it avoids mutating process-global environment state. Clones and `core()`
    /// handles preserve the chosen value.
    pub fn with_ann_fresh_tail_enabled(mut self, enabled: bool) -> Self {
        self.ann_fresh_tail_enabled = enabled;
        self
    }
}
