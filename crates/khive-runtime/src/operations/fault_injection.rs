// Test-only fault-injection state; see docs/operations.md#fault-injection-static-state.
#[cfg(test)]
std::thread_local! {
    pub(super) static LINK_FAIL_AFTER: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(any(test, feature = "fault-injection"))]
std::thread_local! {
    pub(super) static VECTOR_FAIL_AFTER: std::cell::Cell<Option<usize>> =
        const { std::cell::Cell::new(None) };
}

/// Arm the count-targetable vector-INSERT fault: let `n` inserts succeed, then fail
/// the next one. See docs/operations.md#fault-injection-static-state.
#[cfg(any(test, feature = "fault-injection"))]
pub fn arm_vector_fail_after(n: usize) {
    VECTOR_FAIL_AFTER.with(|cell| cell.set(Some(n)));
}

// Namespace-keyed one-shot arm sets — see docs/operations.md#fault-injection-static-state
// (rationale for keying by namespace instead of a single Option<String> slot, #1095).
#[cfg(any(test, feature = "fault-injection"))]
pub(super) type FaultArmSet =
    std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<()>>>;
#[cfg(any(test, feature = "fault-injection"))]
const MAX_FAULT_ARMS: usize = 64;
#[cfg(any(test, feature = "fault-injection"))]
pub(super) static FTS_FAIL_NS: std::sync::LazyLock<FaultArmSet> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));
#[cfg(any(test, feature = "fault-injection"))]
pub(super) static VECTOR_FAIL_NS: std::sync::LazyLock<FaultArmSet> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));
/// Entity-create compensation failure injection; see docs/operations.md#fault-injection-static-state.
#[cfg(any(test, feature = "fault-injection"))]
pub(super) static ENTITY_COMPENSATION_FAIL_NS: std::sync::LazyLock<FaultArmSet> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));
/// `create_many` FTS failure injection, kept separate from `FTS_FAIL_NS` (#1263); see
/// docs/operations.md#fault-injection-static-state.
#[cfg(any(test, feature = "fault-injection"))]
pub(super) static FTS_FAIL_MANY_NS: std::sync::LazyLock<FaultArmSet> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));
/// `create_many` FTS partial-failure injection (exercises the `summary.failed > 0` rollback
/// branch); see docs/operations.md#fault-injection-static-state.
#[cfg(any(test, feature = "fault-injection"))]
pub(super) static FTS_FAIL_MANY_PARTIAL_NS: std::sync::LazyLock<FaultArmSet> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));
/// `resolve_prefix_inner` storage-failure injection, keyed by the scanned prefix string
/// rather than a namespace (the `resolve_prefix_unfiltered*` entry points pass
/// `namespaces: None` by contract, so there is no namespace to key on); see
/// docs/operations.md#fault-injection-static-state.
#[cfg(any(test, feature = "fault-injection"))]
pub(super) static PREFIX_RESOLVE_FAIL_NS: std::sync::LazyLock<FaultArmSet> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Scoped ownership of a process-wide fault-injection arm.
#[cfg(any(test, feature = "fault-injection"))]
#[must_use = "the fault injection is disarmed when this guard is dropped"]
pub struct FaultInjectionArm {
    namespace: String,
    token: std::sync::Arc<()>,
    arms: &'static FaultArmSet,
}

#[cfg(any(test, feature = "fault-injection"))]
impl Drop for FaultInjectionArm {
    fn drop(&mut self) {
        let mut arms = self.arms.lock().unwrap();
        if arms
            .get(&self.namespace)
            .is_some_and(|token| std::sync::Arc::ptr_eq(token, &self.token))
        {
            arms.remove(&self.namespace);
        }
    }
}

#[cfg(any(test, feature = "fault-injection"))]
pub(super) fn arm_fault(
    arms: &'static FaultArmSet,
    namespace: &str,
    max_arms: usize,
) -> FaultInjectionArm {
    let token = std::sync::Arc::new(());
    let refusal = {
        let mut active = arms.lock().unwrap();
        if active.contains_key(namespace) {
            Some("the namespace is already armed")
        } else if active.len() >= max_arms {
            Some("the arm set is at capacity")
        } else {
            active.insert(namespace.to_string(), std::sync::Arc::clone(&token));
            None
        }
    };
    if let Some(reason) = refusal {
        panic!("cannot arm fault injection for namespace `{namespace}`: {reason}");
    }
    FaultInjectionArm {
        namespace: namespace.to_string(),
        token,
        arms,
    }
}

#[cfg(any(test, feature = "fault-injection"))]
pub(super) fn consume_fault(arms: &FaultArmSet, namespace: &str) -> bool {
    arms.lock().unwrap().remove(namespace).is_some()
}
/// Non-parser FTS *search*-leg failure injection for `search_notes`: distinct
/// from `FTS_FAIL_NS` (which injects at the FTS *upsert*/write step of
/// `create_note_inner`). Injects a `StorageError::Timeout` at the `search()`
/// call the FTS fail-open arm guards, so the arm's `is_fts5_syntax_error()`
/// gate can be exercised against a genuine non-parser failure and asserted to
/// propagate rather than degrade.
#[cfg(any(test, feature = "fault-injection"))]
pub(super) static FTS_SEARCH_FAIL_NS: std::sync::Mutex<Option<String>> =
    std::sync::Mutex::new(None);

/// Arm a one-shot FTS failure injection for `create_note_inner`/`create_entity_inner`
/// targeting namespace `ns`. `restore_note`/`restore_entity` consume the same
/// arm at their post-commit reindex step, after the row and its FTS document
/// are already committed in one unit.
///
/// The next `create_note` or `create_entity` call whose namespace equals `ns` returns
/// an injected error at the FTS upsert step (after the row is committed), then disarms
/// — only that namespace's entry is consumed. The arm is process-wide and thread
/// independent: it may be set from one OS thread and consumed by a `create_note`/
/// `create_entity` call running on another (e.g. inside `tokio::spawn`). Concurrent
/// arms of distinct namespaces do not interfere with each other.
/// Keep the returned guard alive until the triggering call completes; dropping it
/// disarms an unconsumed injection.
/// Available when compiled with `cfg(test)` or `feature = "fault-injection"`.
#[cfg(any(test, feature = "fault-injection"))]
pub fn arm_fts_fail_scoped(ns: &str) -> FaultInjectionArm {
    arm_fault(&FTS_FAIL_NS, ns, MAX_FAULT_ARMS)
}

/// Arm the FTS failure injection for `create_many` targeting namespace `ns`.
///
/// The next `create_many` call whose namespace equals `ns` returns an injected
/// error at the first FTS statement inside the atomic batch, then disarms.
/// Calls on other namespaces are unaffected, and concurrent arms of distinct
/// namespaces do not overwrite each other.
/// Keep the returned guard alive until the triggering call completes; dropping it
/// disarms an unconsumed injection.
/// Available when compiled with `cfg(test)` or `feature = "fault-injection"`.
#[cfg(any(test, feature = "fault-injection"))]
pub fn arm_fts_fail_many_scoped(ns: &str) -> FaultInjectionArm {
    arm_fault(&FTS_FAIL_MANY_NS, ns, MAX_FAULT_ARMS)
}

/// Arm a mid-batch FTS failure for `create_many` targeting namespace `ns`.
///
/// The next matching call fails the second FTS statement when the batch contains at
/// least two entities, after one entity/FTS pair has executed in the transaction.
/// A one-entity batch fails its first FTS statement. Then disarms only that namespace.
/// Keep the returned guard alive until the triggering call completes; dropping it
/// disarms an unconsumed injection.
/// Available when compiled with `cfg(test)` or `feature = "fault-injection"`.
#[cfg(any(test, feature = "fault-injection"))]
pub fn arm_fts_fail_many_partial_scoped(ns: &str) -> FaultInjectionArm {
    arm_fault(&FTS_FAIL_MANY_PARTIAL_NS, ns, MAX_FAULT_ARMS)
}

/// Arm a non-parser FTS *search*-leg failure injection for `search_notes` targeting
/// any call whose visible namespaces include `ns`.
///
/// The next `search_notes` call touching `ns` returns `StorageError::Timeout` from
/// the FTS leg instead of calling the real `TextSearch::search`, then disarms.
/// Used to prove the fail-open arm in `search_notes` propagates non-parser
/// `StorageError`s instead of silently degrading them the way a genuine FTS5
/// parser syntax error is degraded.
/// Available when compiled with `cfg(test)` or `feature = "fault-injection"`.
#[cfg(any(test, feature = "fault-injection"))]
pub fn arm_fts_search_fail(ns: &str) {
    *FTS_SEARCH_FAIL_NS.lock().unwrap() = Some(ns.to_string());
}

/// Arm the vector insertion failure injection for `create_note_inner` targeting `ns`.
///
/// The next `create_note` call whose note namespace equals `ns` returns an injected
/// error at the first vector insert step, then disarms.  Calls on other namespaces
/// are unaffected, and concurrent arms of distinct namespaces do not overwrite
/// each other.
/// Keep the returned guard alive until the triggering call completes; dropping it
/// disarms an unconsumed injection.
/// Available when compiled with `cfg(test)` or `feature = "fault-injection"`.
#[cfg(any(test, feature = "fault-injection"))]
pub fn arm_vector_fail_scoped(ns: &str) -> FaultInjectionArm {
    arm_fault(&VECTOR_FAIL_NS, ns, MAX_FAULT_ARMS)
}

/// Arm a one-shot entity-row cleanup failure for `create_entity`
/// compensation in namespace `ns`.
#[cfg(any(test, feature = "fault-injection"))]
pub fn arm_entity_compensation_fail_scoped(ns: &str) -> FaultInjectionArm {
    arm_fault(&ENTITY_COMPENSATION_FAIL_NS, ns, MAX_FAULT_ARMS)
}

/// Arm a one-shot storage failure injection for `resolve_prefix_inner` targeting the
/// exact `prefix` string.
///
/// The next `resolve_prefix`/`resolve_prefix_unfiltered`/`resolve_prefix_including_deleted`/
/// `resolve_prefix_unfiltered_including_deleted` call scanning this `prefix` returns an
/// injected `StorageError::Timeout` instead of performing the table scan, then disarms.
/// Keyed by prefix rather than namespace because the unfiltered entry points pass no
/// namespace at all.
/// Keep the returned guard alive until the triggering call completes; dropping it
/// disarms an unconsumed injection.
/// Available when compiled with `cfg(test)` or `feature = "fault-injection"`.
#[cfg(any(test, feature = "fault-injection"))]
pub fn arm_prefix_resolve_fail_scoped(prefix: &str) -> FaultInjectionArm {
    arm_fault(&PREFIX_RESOLVE_FAIL_NS, prefix, MAX_FAULT_ARMS)
}

/// Failure injection for `delete_note_row_first_for_compensation`'s post-row-removal
/// cleanup step: distinct from `FTS_FAIL_NS`/`VECTOR_FAIL_NS`, which target
/// `create_note_inner`. Lets tests prove that a rollback compensation's cleanup
/// failure still leaves the note row (and thus the live message) gone.
#[cfg(any(test, feature = "fault-injection"))]
pub(super) static ROLLBACK_CLEANUP_FAIL_NS: std::sync::Mutex<Option<String>> =
    std::sync::Mutex::new(None);

/// Arm the rollback-compensation cleanup failure injection targeting `ns`.
///
/// The next `delete_note_row_first_for_compensation` call whose note namespace
/// equals `ns` removes the row as usual, then returns an injected cleanup error
/// instead of running the real graph/FTS/vector cleanup, then disarms.
/// Available when compiled with `cfg(test)` or `feature = "fault-injection"`.
#[cfg(any(test, feature = "fault-injection"))]
pub fn arm_rollback_cleanup_fail(ns: &str) {
    *ROLLBACK_CLEANUP_FAIL_NS.lock().unwrap() = Some(ns.to_string());
}

/// `atomic_message::create_notes_atomic` equivalents of the `FTS_FAIL_NS`/
/// `VECTOR_FAIL_NS` checks above, reusing the SAME arm sets (and thus the
/// SAME `arm_fts_fail_scoped`/`arm_vector_fail_scoped` test API) so a test
/// can arm one call and exercise either write path. `create_notes_atomic`
/// builds raw `PlanStatement`s instead of calling `text_for_notes()`/
/// `vectors_for_model().insert()`, so it cannot reuse `consume_fault`
/// in-line the way `create_note_inner` does above; these wrappers are the
/// seam that lets it check the same arms.
#[cfg(any(test, feature = "fault-injection"))]
pub(crate) fn consume_fts_fail_fault(ns: &str) -> bool {
    consume_fault(&FTS_FAIL_NS, ns)
}
#[cfg(any(test, feature = "fault-injection"))]
pub(crate) fn consume_vector_fail_fault(ns: &str) -> bool {
    consume_fault(&VECTOR_FAIL_NS, ns)
}
