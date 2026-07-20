//! Process-wide open-transaction registry (ADR-091 Plank 0).
//!
//! Every caller-controllable SQL transaction span (`WriterGuard::transaction`,
//! `atomic_unit`'s own registered span, and the raw `BEGIN IMMEDIATE`/`COMMIT`
//! batch-writer spans) registers here on open and deregisters via `TxHandle`'s
//! `Drop`. This is observe-only: no enforcement reads the registry in this
//! plank. It exists so the checkpoint task can name which caller, if any, is
//! holding a WAL snapshot open.

use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Identifier for one registered transaction span.
///
/// Public so consumers of [`oldest`] can detect the oldest entry *changing
/// identity* between observations without a live registration of their own.
/// Equality is the only supported operation; the numeric value carries no
/// meaning beyond "same span" vs. "different span". See
/// `crates/khive-storage/docs/api/tx-registry.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TxId(pub u64);

/// Opaque identity for a file-backed database, keyed on its already-minted
/// canonical path. Minting (resolving aliases to one canonical path) happens
/// at exactly one point — the pool in `khive-db` — every other layer treats
/// this constructor as accepting that output only.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DbIdentity(OsString);

impl DbIdentity {
    pub fn new(canonical_path: impl Into<OsString>) -> Self {
        Self(canonical_path.into())
    }
}

/// Which database, if any, a registered span runs against. Three states,
/// never an option: "no database file" ([`TxOrigin::Memory`]) and "not yet
/// threaded to an origin" ([`TxOrigin::Unscoped`]) are different facts and
/// must never share a representation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TxOrigin {
    /// A file-backed database, identified by its minted [`DbIdentity`].
    Database(DbIdentity),
    /// An in-memory backend: no database file, no WAL, no sidecar —
    /// excluded from WAL-pin attribution entirely.
    Memory,
    /// A registration site not yet threaded to an origin. Observed by the
    /// main backend's attribution view, exactly as every entry was before
    /// origins existed, so scoping can never silently drop a span.
    Unscoped,
}

/// An attribution view over the registry: which origins it observes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TxOriginFilter {
    /// The main backend's view: spans scoped to `main`, plus every
    /// [`TxOrigin::Unscoped`] span (the never-silently-drop fallback).
    Main(DbIdentity),
    /// A secondary backend's view: spans scoped to exactly this database.
    /// Never falls back to unscoped spans — only the main view does.
    Secondary(DbIdentity),
}

impl TxOriginFilter {
    fn matches(&self, origin: &TxOrigin) -> bool {
        match (self, origin) {
            (TxOriginFilter::Main(id), TxOrigin::Database(origin_id)) => origin_id == id,
            (TxOriginFilter::Main(_), TxOrigin::Unscoped) => true,
            (TxOriginFilter::Main(_), TxOrigin::Memory) => false,
            (TxOriginFilter::Secondary(id), TxOrigin::Database(origin_id)) => origin_id == id,
            (TxOriginFilter::Secondary(_), TxOrigin::Unscoped | TxOrigin::Memory) => false,
        }
    }
}

/// Identity, age, label, and origin of the oldest span an attribution view
/// observes. Origin lets the consumer distinguish an evidence-backed
/// `Database(_)` winner from an `Unscoped` fallback winner when writing the
/// attribution-basis field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OldestSpan {
    pub id: TxId,
    pub age: Duration,
    pub label: Option<String>,
    pub origin: TxOrigin,
}

#[derive(Clone, Debug)]
struct TxMeta {
    opened_at: Instant,
    label: Option<String>,
    origin: TxOrigin,
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

/// Register a new open transaction span with an optional diagnostic label
/// and an explicit origin. Recovers a poisoned lock via `into_inner()`
/// rather than dropping the write — a poisoned registry must keep tracking
/// spans, not go silently blind. See `crates/khive-storage/docs/api/tx-registry.md`.
pub fn register_scoped(label: Option<String>, origin: TxOrigin) -> TxHandle {
    let id = TxId(NEXT_ID.fetch_add(1, Ordering::Relaxed));
    let meta = TxMeta {
        opened_at: Instant::now(),
        label,
        origin,
    };
    let mut registry = REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    registry.insert(id, meta);
    drop(registry);
    TxHandle { id }
}

/// Register a new open transaction span with an optional diagnostic label.
/// Delegates to [`register_scoped`] with [`TxOrigin::Unscoped`].
pub fn register(label: Option<String>) -> TxHandle {
    register_scoped(label, TxOrigin::Unscoped)
}

/// Identity, age, and label of the oldest currently-open registry entry, if
/// any. The [`TxId`] lets callers distinguish "still the same oldest span"
/// from "a different span became oldest" (must re-arm latched escalation
/// state). Recovers a poisoned lock via `into_inner()` (see [`register`])
/// rather than returning `None`, which would read identically to the
/// genuinely-empty case. See `crates/khive-storage/docs/api/tx-registry.md`.
pub fn oldest() -> Option<(TxId, Duration, Option<String>)> {
    let registry = REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    registry
        .iter()
        .min_by_key(|(_, meta)| meta.opened_at)
        .map(|(id, meta)| (*id, meta.opened_at.elapsed(), meta.label.clone()))
}

/// Identity, age, label, and origin of the oldest entry an attribution view
/// observes, if any. See [`TxOriginFilter`] for the main/secondary view
/// semantics and [`oldest`] for the process-wide aggregate this narrows.
/// Recovers a poisoned lock via `into_inner()` (see [`register_scoped`]).
pub fn oldest_for(filter: &TxOriginFilter) -> Option<OldestSpan> {
    let registry = REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    registry
        .iter()
        .filter(|(_, meta)| filter.matches(&meta.origin))
        .min_by_key(|(_, meta)| meta.opened_at)
        .map(|(id, meta)| OldestSpan {
            id: *id,
            age: meta.opened_at.elapsed(),
            label: meta.label.clone(),
            origin: meta.origin.clone(),
        })
}

/// Age and label of every currently-open registry entry. Recovers a poisoned
/// lock via `into_inner()` (see [`register`]) instead of returning an empty
/// `Vec` — see `crates/khive-storage/docs/api/tx-registry.md`.
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
    fn oldest_for_partitions_by_origin() {
        let _guard = TEST_LOCK.lock().unwrap();
        let main_id = DbIdentity::new("main.db");
        let secondary_id = DbIdentity::new("secondary.db");
        let main_view = TxOriginFilter::Main(main_id.clone());
        let secondary_view = TxOriginFilter::Secondary(secondary_id.clone());

        let main_handle = register_scoped(
            Some("main_span".to_string()),
            TxOrigin::Database(main_id.clone()),
        );
        let secondary_handle = register_scoped(
            Some("secondary_span".to_string()),
            TxOrigin::Database(secondary_id.clone()),
        );

        let main_oldest = oldest_for(&main_view).expect("main span visible in main view");
        assert_eq!(main_oldest.label.as_deref(), Some("main_span"));
        assert_eq!(main_oldest.origin, TxOrigin::Database(main_id));

        let secondary_oldest =
            oldest_for(&secondary_view).expect("secondary span visible in secondary view");
        assert_eq!(secondary_oldest.label.as_deref(), Some("secondary_span"));

        drop(main_handle);
        drop(secondary_handle);
    }

    #[test]
    fn register_delegates_to_unscoped_origin() {
        let _guard = TEST_LOCK.lock().unwrap();
        let main_id = DbIdentity::new("main.db");
        let main_view = TxOriginFilter::Main(main_id);
        let handle = register(Some("legacy_span".to_string()));

        let oldest = oldest_for(&main_view).expect("register() delegates to Unscoped");
        assert_eq!(oldest.origin, TxOrigin::Unscoped);

        drop(handle);
    }

    #[test]
    fn unscoped_visible_in_main_view_and_oldest() {
        let _guard = TEST_LOCK.lock().unwrap();
        let main_id = DbIdentity::new("main.db");
        let main_view = TxOriginFilter::Main(main_id);
        let handle = register_scoped(Some("unscoped_span".to_string()), TxOrigin::Unscoped);

        let via_filter = oldest_for(&main_view).expect("unscoped span falls back to main view");
        assert_eq!(via_filter.label.as_deref(), Some("unscoped_span"));
        assert_eq!(via_filter.origin, TxOrigin::Unscoped);

        let via_aggregate = oldest().expect("unscoped span visible in aggregate oldest()");
        assert_eq!(via_aggregate.2.as_deref(), Some("unscoped_span"));

        drop(handle);
    }

    #[test]
    fn memory_absent_from_attribution_views_but_present_in_oldest() {
        let _guard = TEST_LOCK.lock().unwrap();
        let main_id = DbIdentity::new("main.db");
        let secondary_id = DbIdentity::new("secondary.db");
        let main_view = TxOriginFilter::Main(main_id);
        let secondary_view = TxOriginFilter::Secondary(secondary_id);
        let handle = register_scoped(Some("memory_span".to_string()), TxOrigin::Memory);

        assert!(oldest_for(&main_view).is_none());
        assert!(oldest_for(&secondary_view).is_none());

        let via_aggregate = oldest().expect("memory span visible in aggregate oldest()");
        assert_eq!(via_aggregate.2.as_deref(), Some("memory_span"));

        drop(handle);
    }

    #[test]
    fn database_entries_never_leak_across_views() {
        let _guard = TEST_LOCK.lock().unwrap();
        let main_id = DbIdentity::new("main.db");
        let secondary_id = DbIdentity::new("secondary.db");
        let main_view = TxOriginFilter::Main(main_id);
        let secondary_view = TxOriginFilter::Secondary(secondary_id.clone());
        let handle = register_scoped(
            Some("secondary_span".to_string()),
            TxOrigin::Database(secondary_id),
        );

        assert!(oldest_for(&main_view).is_none());
        let via_secondary =
            oldest_for(&secondary_view).expect("secondary span visible in its own view");
        assert_eq!(via_secondary.label.as_deref(), Some("secondary_span"));

        drop(handle);
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
