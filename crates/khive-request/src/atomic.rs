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
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn op(tool: &str) -> ParsedOp {
        ParsedOp {
            tool: tool.to_string(),
            args: BTreeMap::new(),
        }
    }

    #[test]
    fn all_admissible_ops_pass_with_no_rejections() {
        let ops = vec![op("update"), op("delete"), op("link")];
        assert!(check_atomic_admissible(&ops).is_empty());
    }

    #[test]
    fn embedding_bearing_verb_named_in_rejection() {
        let ops = vec![op("update"), op("create"), op("delete")];
        let rejections = check_atomic_admissible(&ops);
        assert_eq!(rejections.len(), 1);
        assert_eq!(rejections[0].op_index, 1);
        assert_eq!(rejections[0].tool, "create");
        assert_eq!(
            rejections[0].reason,
            AtomicRejectionReason::EmbeddingBearing
        );
    }

    #[test]
    fn read_verb_named_in_rejection_before_any_write() {
        let ops = vec![op("search"), op("update")];
        let rejections = check_atomic_admissible(&ops);
        assert_eq!(rejections.len(), 1);
        assert_eq!(rejections[0].op_index, 0);
        assert_eq!(rejections[0].tool, "search");
        assert_eq!(rejections[0].reason, AtomicRejectionReason::Read);
    }

    #[test]
    fn every_offending_line_is_reported_not_just_the_first() {
        let ops = vec![op("create"), op("update"), op("comm.send"), op("search")];
        let rejections = check_atomic_admissible(&ops);
        let indices: Vec<usize> = rejections.iter().map(|r| r.op_index).collect();
        assert_eq!(indices, vec![0, 2, 3]);
    }

    #[test]
    fn rejection_display_names_verb_and_lists_admissible_set() {
        let rejection = AtomicRejection {
            op_index: 2,
            tool: "create".to_string(),
            reason: AtomicRejectionReason::EmbeddingBearing,
        };
        let msg = rejection.to_string();
        assert!(msg.contains("op 2"));
        assert!(msg.contains("`create`"));
        assert!(
            msg.contains("update"),
            "must list the admissible set: {msg}"
        );
    }

    #[test]
    fn all_v1_admissible_verbs_from_the_ordered_ops_file_pass() {
        let ops: Vec<ParsedOp> = ATOMIC_ADMISSIBLE_VERBS
            .iter()
            .filter(|v| !khive_types::pack::ATOMIC_KNOWN_UNIMPLEMENTED_VERBS.contains(v))
            .map(|v| op(v))
            .collect();
        assert!(check_atomic_admissible(&ops).is_empty());
    }

    #[test]
    fn known_unimplemented_verbs_rejected_before_any_write() {
        let ops: Vec<ParsedOp> = khive_types::pack::ATOMIC_KNOWN_UNIMPLEMENTED_VERBS
            .iter()
            .map(|v| op(v))
            .collect();
        let rejections = check_atomic_admissible(&ops);
        assert_eq!(rejections.len(), ops.len());
        for rejection in &rejections {
            assert_eq!(
                rejection.reason,
                AtomicRejectionReason::KnownUnimplemented,
                "rejection: {rejection:?}"
            );
            let msg = rejection.to_string();
            assert!(
                msg.contains("use the non-atomic") && msg.contains("instead"),
                "display must name the non-atomic verb as the supported route: {msg}"
            );
        }
    }

    #[test]
    fn known_unimplemented_merge_names_non_atomic_merge_as_the_supported_route() {
        let rejection = AtomicRejection {
            op_index: 0,
            tool: "merge".to_string(),
            reason: AtomicRejectionReason::KnownUnimplemented,
        };
        let msg = rejection.to_string();
        assert!(
            msg.contains("use the non-atomic merge verb instead"),
            "display: {msg}"
        );
    }
}
