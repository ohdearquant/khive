//! Pure ADR-099 atomic-admissibility preflight over parsed operations.

use khive_types::pack::{atomic_admissibility, AtomicRejectionReason, ATOMIC_ADMISSIBLE_VERBS};

use crate::types::ParsedOp;

/// One atomic-preflight rejection.
///
/// See `crates/khive-request/docs/api/atomic-admissibility.md` for reason semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtomicRejection {
    /// Zero-based operation position.
    pub op_index: usize,
    /// Rejected tool name.
    pub tool: String,
    /// Static admissibility category.
    pub reason: AtomicRejectionReason,
}

impl std::fmt::Display for AtomicRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let why: std::borrow::Cow<'static, str> = match self.reason {
            AtomicRejectionReason::EmbeddingBearing => {
                "embedding-bearing verbs are not yet atomic-eligible".into()
            }
            AtomicRejectionReason::Read => "read verbs have no write plan to apply".into(),
            AtomicRejectionReason::Unlisted => "not on the v1 atomic-admissible verb list".into(),
            AtomicRejectionReason::KnownUnimplemented if self.tool == "merge" => {
                "merge is not yet supported under --atomic (admissible per ADR-099 D3, but \
                 full-parity field-folding/index-cleanup/edge-conflict-resolution is deferred \
                 this slice); use the non-atomic merge verb instead"
                    .into()
            }
            AtomicRejectionReason::KnownUnimplemented => format!(
                "admissible per ADR-099 D3 but has no --atomic prepare/apply seam implemented \
                 in this slice yet; use the non-atomic {:?} verb instead",
                self.tool
            )
            .into(),
        };
        write!(
            f,
            "op {} (`{}`) is not atomic-admissible: {}. Admissible verbs: {}",
            self.op_index,
            self.tool,
            why,
            ATOMIC_ADMISSIBLE_VERBS.join(", ")
        )
    }
}

/// Checks every tool name against the ADR-099 v1 atomic policy.
///
/// Returns every rejection in input order; an empty vector means every op is
/// admissible. This function performs no I/O or execution and must run before
/// an atomic prepare pass.
/// See `crates/khive-request/docs/api/atomic-admissibility.md` for policy boundaries.
pub fn check_atomic_admissible(ops: &[ParsedOp]) -> Vec<AtomicRejection> {
    ops.iter()
        .enumerate()
        .filter_map(|(op_index, op)| {
            atomic_admissibility(&op.tool).map(|reason| AtomicRejection {
                op_index,
                tool: op.tool.clone(),
                reason,
            })
        })
        .collect()
}

#[cfg(test)]
#[path = "atomic_tests.rs"]
mod tests;
