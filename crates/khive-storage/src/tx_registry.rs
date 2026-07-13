//! Process-wide open-transaction registry (ADR-091 Plank 0).
//!
//! Every caller-controllable SQL transaction span (`WriterGuard::transaction`,
//! `atomic_unit`'s own registered span, and the raw `BEGIN IMMEDIATE`/`COMMIT`
//! batch-writer spans) registers here on open and deregisters via `TxHandle`'s
//! `Drop`. This is observe-only: no enforcement reads the registry in this
//! plank. It exists so the checkpoint task can name which caller, if any, is
//! holding a WAL snapshot open.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Identifier for one registered transaction span.
///
/// The wrapped value is public so consumers of [`oldest`] can detect when the
/// registry's oldest entry has *changed identity* between two observations —
/// distinct from "is it still above a threshold" — without needing a live
/// registration of their own (e.g. `khive-db`'s `TxAgeSweepState` pure
/// state-machine unit tests construct synthetic ids directly). Equality is
/// the only operation this type supports; the numeric value carries no
/// meaning beyond "same span" vs. "different span".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TxId(pub u64);

#[derive(Clone, Debug)]
struct TxMeta {
    opened_at: Instant,
    label: Option<String>,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static REGISTRY: LazyLock<Mutex<HashMap<TxId, TxMeta>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// RAII handle for a registered transaction span. Deregisters on `Drop` — this is
/// the only deregistration path, so error and panic returns can never leak an
/// entry in the registry.
pub struct TxHandle {
    id: TxId,
}

impl Drop for TxHandle {
    fn drop(&mut self) {
        let mut registry = REGISTRY
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        registry.remove(&self.id);
    }
}

/// Register a new open transaction span with an optional diagnostic label.
///
/// This is observe-only telemetry: a poisoned lock (some other holder panicked
/// mid-critical-section) must not make the registry silently stop tracking new
/// spans, or a subsequent WAL-pressure diagnosis could read a false "no open
/// transactions" signal. Recovers via `into_inner()` rather than the previous
/// `if let Ok(..)` pattern, which dropped the write on poison.
pub fn register(label: Option<String>) -> TxHandle {
    let id = TxId(NEXT_ID.fetch_add(1, Ordering::Relaxed));
    let meta = TxMeta {
        opened_at: Instant::now(),
        label,
    };
    let mut registry = REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    registry.insert(id, meta);
    drop(registry);
    TxHandle { id }
}

/// Identity, age, and label of the oldest currently-open registry entry, if
/// any.
///
/// The [`TxId`] lets callers distinguish "the same span is still oldest and
/// still above a threshold" from "a *different* span became oldest between
/// two observations" — the latter must re-arm any latched escalation state,
/// since a departed entry's threshold-crossing history says nothing about
/// its replacement (`khive-db`'s `TxAgeSweepState` is the consumer this
/// exists for).
///
/// Recovers a poisoned lock via `into_inner()` (see [`register`]) instead of
/// returning `None`, which would otherwise read identically to "no open
/// transactions" — indistinguishable from the genuinely-empty case.
pub fn oldest() -> Option<(TxId, Duration, Option<String>)> {
    let registry = REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    registry
        .iter()
        .min_by_key(|(_, meta)| meta.opened_at)
        .map(|(id, meta)| (*id, meta.opened_at.elapsed(), meta.label.clone()))
}

/// Age and label of every currently-open registry entry.
///
/// Recovers a poisoned lock via `into_inner()` (see [`register`]) instead of
/// returning an empty `Vec`, which would otherwise read identically to "no
/// open transactions" during the one moment this diagnostic matters most.
pub fn snapshot() -> Vec<(Duration, Option<String>)> {
    let registry = REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    registry
        .values()
        .map(|meta| (meta.opened_at.elapsed(), meta.label.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{self, AssertUnwindSafe};

    // The registry is a process-wide singleton, shared across every test in this
    // binary (cargo runs `#[test]`s in parallel threads of the same process). A
    // module-local lock serializes these tests so one test's entries can't be
    // observed as another's "oldest" or leak into another's snapshot assertion.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn register_reports_oldest_with_label() {
        let _guard = TEST_LOCK.lock().unwrap();
        let handle = register(Some("test_span".to_string()));
        let (_, _, label) = oldest().expect("expected an open entry");
        assert_eq!(label.as_deref(), Some("test_span"));
        drop(handle);
    }

    #[test]
    fn drop_deregisters() {
        let _guard = TEST_LOCK.lock().unwrap();
        let handle = register(Some("drop_me".to_string()));
        let id_present_before = snapshot()
            .iter()
            .any(|(_, label)| label.as_deref() == Some("drop_me"));
        assert!(id_present_before);
        drop(handle);
        let id_present_after = snapshot()
            .iter()
            .any(|(_, label)| label.as_deref() == Some("drop_me"));
        assert!(!id_present_after);
    }

    #[test]
    fn oldest_is_genuinely_oldest() {
        let _guard = TEST_LOCK.lock().unwrap();
        let first = register(Some("first".to_string()));
        std::thread::sleep(Duration::from_millis(5));
        let second = register(Some("second".to_string()));
        let (first_id, _, label) = oldest().expect("expected an open entry");
        assert_eq!(label.as_deref(), Some("first"));
        drop(first);
        let (second_id, _, label) = oldest().expect("expected an open entry");
        assert_eq!(label.as_deref(), Some("second"));
        assert_ne!(
            first_id, second_id,
            "distinct registrations must carry distinct TxIds"
        );
        drop(second);
    }

    #[test]
    fn snapshot_contains_all_open_entries() {
        let _guard = TEST_LOCK.lock().unwrap();
        let a = register(Some("snap_a".to_string()));
        let b = register(Some("snap_b".to_string()));
        let labels: Vec<Option<String>> = snapshot().into_iter().map(|(_, label)| label).collect();
        assert!(labels.contains(&Some("snap_a".to_string())));
        assert!(labels.contains(&Some("snap_b".to_string())));
        drop(a);
        drop(b);
    }

    #[test]
    fn panic_inside_scope_still_deregisters() {
        let _guard = TEST_LOCK.lock().unwrap();
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            let _handle = register(Some("panics".to_string()));
            panic!("boom");
        }));
        assert!(result.is_err());
        let still_present = snapshot()
            .iter()
            .any(|(_, label)| label.as_deref() == Some("panics"));
        assert!(!still_present);
    }
}
