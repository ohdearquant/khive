//! Deterministic, one-shot, namespace-scoped injectable failure seams for
//! the finalizer's five outcomes (ADR-115 Amendment 1 §4; executable
//! contract §5: "one-shot and namespace-scoped for `ManifestInvalid`,
//! record write, stamp, success-audit, and failure-audit").
//!
//! Mirrors the `FTS_FAIL_NS`/`arm_fault`/`consume_fault` pattern in
//! `crate::operations` (see docs/operations.md#fault-injection-arm-migration)
//! but keeps its own arm sets so finalizer fault injection cannot interact
//! with unrelated operations' seams.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use super::outcome::ManifestFault;

type FaultArmSet<T> = Mutex<HashMap<String, (Arc<()>, T)>>;
const MAX_FAULT_ARMS: usize = 64;

/// Scoped ownership of a process-wide fault-injection arm. Disarms on drop.
#[must_use = "the fault injection is disarmed when this guard is dropped"]
pub(crate) struct FaultArm<T: 'static> {
    namespace: String,
    token: Arc<()>,
    arms: &'static FaultArmSet<T>,
}

impl<T: 'static> Drop for FaultArm<T> {
    fn drop(&mut self) {
        let mut arms = self.arms.lock().unwrap();
        if arms
            .get(&self.namespace)
            .is_some_and(|(token, _)| Arc::ptr_eq(token, &self.token))
        {
            arms.remove(&self.namespace);
        }
    }
}

fn arm<T: 'static>(arms: &'static FaultArmSet<T>, namespace: &str, payload: T) -> FaultArm<T> {
    let token = Arc::new(());
    let refusal = {
        let mut active = arms.lock().unwrap();
        if active.contains_key(namespace) {
            Some("the namespace is already armed")
        } else if active.len() >= MAX_FAULT_ARMS {
            Some("the arm set is at capacity")
        } else {
            active.insert(namespace.to_string(), (Arc::clone(&token), payload));
            None
        }
    };
    if let Some(reason) = refusal {
        panic!(
            "cannot arm secret-gate-finalizer fault injection for namespace `{namespace}`: {reason}"
        );
    }
    FaultArm {
        namespace: namespace.to_string(),
        token,
        arms,
    }
}

fn consume<T: Clone + 'static>(arms: &'static FaultArmSet<T>, namespace: &str) -> Option<T> {
    arms.lock()
        .unwrap()
        .remove(namespace)
        .map(|(_, payload)| payload)
}

static MANIFEST_INVALID_FAIL_NS: LazyLock<FaultArmSet<ManifestFault>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static RECORD_WRITE_FAIL_NS: LazyLock<FaultArmSet<()>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static STAMP_FAIL_NS: LazyLock<FaultArmSet<()>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static SUCCESS_AUDIT_FAIL_NS: LazyLock<FaultArmSet<()>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static FAILURE_AUDIT_FAIL_NS: LazyLock<FaultArmSet<()>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Arm a one-shot `ManifestInvalid` injection for namespace `ns`: the next
/// `finalize` call for that namespace returns `ManifestInvalid(fault)`
/// before any transactional state is touched, then disarms.
pub(crate) fn arm_manifest_invalid(ns: &str, fault: ManifestFault) -> FaultArm<ManifestFault> {
    arm(&MANIFEST_INVALID_FAIL_NS, ns, fault)
}

pub(crate) fn consume_manifest_invalid(ns: &str) -> Option<ManifestFault> {
    consume(&MANIFEST_INVALID_FAIL_NS, ns)
}

/// Arm a one-shot record-write failure for namespace `ns`.
pub(crate) fn arm_record_write_fail(ns: &str) -> FaultArm<()> {
    arm(&RECORD_WRITE_FAIL_NS, ns, ())
}

pub(crate) fn consume_record_write_fail(ns: &str) -> bool {
    consume(&RECORD_WRITE_FAIL_NS, ns).is_some()
}

/// Arm a one-shot stamp-write failure for namespace `ns`.
pub(crate) fn arm_stamp_fail(ns: &str) -> FaultArm<()> {
    arm(&STAMP_FAIL_NS, ns, ())
}

pub(crate) fn consume_stamp_fail(ns: &str) -> bool {
    consume(&STAMP_FAIL_NS, ns).is_some()
}

/// Arm a one-shot success-audit-write failure for namespace `ns`.
pub(crate) fn arm_success_audit_fail(ns: &str) -> FaultArm<()> {
    arm(&SUCCESS_AUDIT_FAIL_NS, ns, ())
}

pub(crate) fn consume_success_audit_fail(ns: &str) -> bool {
    consume(&SUCCESS_AUDIT_FAIL_NS, ns).is_some()
}

/// Arm a one-shot second-order failure: the failure-audit write attempted
/// *after* a primary failure and rollback also fails, for namespace `ns`.
pub(crate) fn arm_failure_audit_fail(ns: &str) -> FaultArm<()> {
    arm(&FAILURE_AUDIT_FAIL_NS, ns, ())
}

pub(crate) fn consume_failure_audit_fail(ns: &str) -> bool {
    consume(&FAILURE_AUDIT_FAIL_NS, ns).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_invalid_arm_is_one_shot_and_carries_payload() {
        let ns = "faults-test-manifest-one-shot";
        let _arm = arm_manifest_invalid(ns, ManifestFault::CorpusIdentityMismatch);
        assert_eq!(
            consume_manifest_invalid(ns),
            Some(ManifestFault::CorpusIdentityMismatch)
        );
        assert_eq!(consume_manifest_invalid(ns), None);
    }

    #[test]
    fn record_write_arm_disarms_on_drop() {
        let ns = "faults-test-record-write-disarm";
        {
            let _arm = arm_record_write_fail(ns);
        }
        assert!(!consume_record_write_fail(ns));
    }

    #[test]
    fn distinct_namespaces_do_not_interfere() {
        let ns_a = "faults-test-ns-a";
        let ns_b = "faults-test-ns-b";
        let _arm_a = arm_stamp_fail(ns_a);
        assert!(!consume_stamp_fail(ns_b));
        assert!(consume_stamp_fail(ns_a));
    }

    #[test]
    #[should_panic(expected = "already armed")]
    fn re_arming_same_namespace_panics() {
        let ns = "faults-test-re-arm-panic";
        let _first = arm_success_audit_fail(ns);
        let _second = arm_success_audit_fail(ns);
    }
}
