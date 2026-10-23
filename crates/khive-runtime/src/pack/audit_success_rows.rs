use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};

use super::VerbRegistry;

/// Success-audit population fixed at registry construction (ADR-103 Amendment 7).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AuditSuccessRowsMode {
    /// Omit or sample only the explicitly reviewed trusted handlers.
    #[default]
    Reduced,
    /// Preserve every success row and disable success-row decision counters.
    All,
}

impl AuditSuccessRowsMode {
    /// Value reported by diagnostics and accepted by `KHIVE_AUDIT_SUCCESS_ROWS`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reduced => "reduced",
            Self::All => "all",
        }
    }

    pub(super) fn from_env() -> Self {
        match std::env::var_os("KHIVE_AUDIT_SUCCESS_ROWS") {
            None => Self::Reduced,
            Some(value) => match value.to_str() {
                Some("reduced") => Self::Reduced,
                Some("all") => Self::All,
                _ => {
                    tracing::warn!(
                        value = ?value,
                        "unrecognized KHIVE_AUDIT_SUCCESS_ROWS; using all"
                    );
                    Self::All
                }
            },
        }
    }
}

/// Canonical result predicate that forces a sampled success row to be retained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuditSuccessRowForcingRule {
    /// Retain unless `outcome` is exactly the string `success`.
    PollNotSuccessful,
    /// Retain unless both cleanup counts are exactly the integer zero.
    QuarantineChanged,
}

impl VerbRegistry {
    /// Reviewed `(owning pack, verb)` pairs whose trusted success rows may be omitted.
    pub const AUDIT_SUCCESS_ROW_OMITTED_VERBS: &'static [(&'static str, &'static str)] = &[
        ("comm", "comm.inbox"),
        ("comm", "comm.probe"),
        ("comm", "comm.cursor_get"),
        ("kg", "whoami"),
    ];

    /// Reviewed `(owning pack, verb, every, forcing rule)` sampling policies.
    pub const AUDIT_SUCCESS_ROW_SAMPLED_VERBS: &'static [(
        &'static str,
        &'static str,
        u64,
        AuditSuccessRowForcingRule,
    )] = &[
        (
            "comm",
            "comm.heartbeat",
            10,
            AuditSuccessRowForcingRule::PollNotSuccessful,
        ),
        (
            "comm",
            "comm.cleanup_expired_quarantine",
            10,
            AuditSuccessRowForcingRule::QuarantineChanged,
        ),
    ];

    /// This registry's fixed success-row mode, including when no audit sink exists.
    pub fn audit_success_rows_mode(&self) -> AuditSuccessRowsMode {
        self.audit_success_rows_mode
    }
}

static OMITTED_COUNTS: [(&str, AtomicU64); 4] = [
    ("comm.inbox", AtomicU64::new(0)),
    ("comm.probe", AtomicU64::new(0)),
    ("comm.cursor_get", AtomicU64::new(0)),
    ("whoami", AtomicU64::new(0)),
];
static SAMPLED_OUT_COUNTS: [(&str, AtomicU64); 2] = [
    ("comm.heartbeat", AtomicU64::new(0)),
    ("comm.cleanup_expired_quarantine", AtomicU64::new(0)),
];
static FORCED_COUNTS: [(&str, AtomicU64); 2] = [
    ("comm.heartbeat", AtomicU64::new(0)),
    ("comm.cleanup_expired_quarantine", AtomicU64::new(0)),
];

#[derive(Clone, Copy)]
pub(super) enum AuditSuccessRowDecision {
    Omitted,
    SampledOut,
    Forced,
}

impl VerbRegistry {
    pub(super) fn record_audit_success_row_decision(
        &self,
        verb: &str,
        decision: AuditSuccessRowDecision,
    ) {
        if self.audit_success_rows_mode != AuditSuccessRowsMode::Reduced {
            return;
        }
        let counters: &[(&str, AtomicU64)] = match decision {
            AuditSuccessRowDecision::Omitted => &OMITTED_COUNTS,
            AuditSuccessRowDecision::SampledOut => &SAMPLED_OUT_COUNTS,
            AuditSuccessRowDecision::Forced => &FORCED_COUNTS,
        };
        if let Some((_, count)) = counters.iter().find(|(name, _)| *name == verb) {
            count.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(super) fn audit_success_rows_omitted_counts(&self) -> [(&'static str, u64); 4] {
        std::array::from_fn(|i| {
            (
                OMITTED_COUNTS[i].0,
                OMITTED_COUNTS[i].1.load(Ordering::Relaxed),
            )
        })
    }

    pub(super) fn audit_success_rows_sampled_out_counts(&self) -> [(&'static str, u64); 2] {
        std::array::from_fn(|i| {
            (
                SAMPLED_OUT_COUNTS[i].0,
                SAMPLED_OUT_COUNTS[i].1.load(Ordering::Relaxed),
            )
        })
    }

    pub(super) fn audit_success_rows_forced_counts(&self) -> [(&'static str, u64); 2] {
        std::array::from_fn(|i| {
            (
                FORCED_COUNTS[i].0,
                FORCED_COUNTS[i].1.load(Ordering::Relaxed),
            )
        })
    }
}

static SAMPLED_SEQUENCES: [(&str, AtomicU64); 2] = [
    ("comm.heartbeat", AtomicU64::new(0)),
    ("comm.cleanup_expired_quarantine", AtomicU64::new(0)),
];

#[derive(Clone, Copy)]
struct Sampling {
    every: u64,
    seq: u64,
    selected_by: &'static str,
}

#[derive(Clone, Copy)]
pub(super) struct AuditSuccessRowAction {
    skipped: bool,
    sampling: Option<Sampling>,
}

impl AuditSuccessRowAction {
    const KEEP: Self = Self {
        skipped: false,
        sampling: None,
    };

    fn skip() -> Self {
        // Match the retained row's snapshot point without building a resource.
        if let Some(context) = crate::usage::current() {
            context.freeze();
        }
        Self {
            skipped: true,
            sampling: None,
        }
    }

    pub(super) fn is_skipped(self) -> bool {
        self.skipped
    }

    pub(super) fn resource(self, mut resource: Value) -> Value {
        if let Some(sampling) = self.sampling {
            resource["sampling"] = json!({
                "every": sampling.every,
                "seq": sampling.seq,
                "selected_by": sampling.selected_by,
            });
        }
        resource
    }
}

fn mix64(mut value: u64) -> u64 {
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
    value ^ (value >> 31)
}

impl VerbRegistry {
    // Called only after the deferred append's existing event-store guard.
    pub(super) fn audit_success_row_action(
        &self,
        verb: &str,
        result: Option<&Value>,
    ) -> AuditSuccessRowAction {
        if self.audit_success_rows_mode != AuditSuccessRowsMode::Reduced {
            return AuditSuccessRowAction::KEEP;
        }
        let Some(result) = result else {
            return AuditSuccessRowAction::KEEP;
        };
        if self.audit_success_row_omitted_verbs.contains(verb) {
            self.record_audit_success_row_decision(verb, AuditSuccessRowDecision::Omitted);
            return AuditSuccessRowAction::skip();
        }
        let Some(&(every, forcing)) = self.audit_success_row_sampled_verbs.get(verb) else {
            return AuditSuccessRowAction::KEEP;
        };
        let Some((_, sequence)) = SAMPLED_SEQUENCES.iter().find(|(name, _)| *name == verb) else {
            return AuditSuccessRowAction::KEEP;
        };
        let seq = sequence.fetch_add(1, Ordering::Relaxed);
        #[cfg(test)]
        super::audit_success_rows_cut_tests::observe_allocation(verb, seq);
        let selected = mix64(seq) % every == 0;
        let forced = match forcing {
            AuditSuccessRowForcingRule::PollNotSuccessful => {
                result.get("outcome").and_then(Value::as_str) != Some("success")
            }
            AuditSuccessRowForcingRule::QuarantineChanged => {
                result.get("deleted").and_then(Value::as_u64) != Some(0)
                    || result.get("routed_owners_detached").and_then(Value::as_u64) != Some(0)
            }
        };
        let selected_by = if selected {
            "sample"
        } else if forced {
            self.record_audit_success_row_decision(verb, AuditSuccessRowDecision::Forced);
            "forced"
        } else {
            self.record_audit_success_row_decision(verb, AuditSuccessRowDecision::SampledOut);
            return AuditSuccessRowAction::skip();
        };
        AuditSuccessRowAction {
            skipped: false,
            sampling: Some(Sampling {
                every,
                seq,
                selected_by,
            }),
        }
    }
}
