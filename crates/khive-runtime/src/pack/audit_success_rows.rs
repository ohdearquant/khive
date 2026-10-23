use std::sync::atomic::{AtomicU64, Ordering};

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

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "Constructed by the deferred success-row cut")
)]
#[derive(Clone, Copy)]
pub(super) enum AuditSuccessRowDecision {
    Omitted,
    SampledOut,
    Forced,
}

impl VerbRegistry {
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "Called by the deferred success-row cut")
    )]
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
