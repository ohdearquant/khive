//! Process-wide open-transaction registry (ADR-091 Plank 0).
//!
//! Every caller-controllable SQL transaction span (`WriterGuard::transaction`,
//! `atomic_unit`'s own registered span, raw `BEGIN IMMEDIATE`/`COMMIT`
//! batch-writer spans, and admitted cached-reader transactions) registers here
//! on open and deregisters via `TxHandle`'s `Drop`. This is observe-only: no
//! enforcement reads the registry in this plank. It exists so the checkpoint
//! task can name which caller, if any, is holding a WAL snapshot open.

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

/// Whether an attribution view currently observes an open span carrying
/// `label`.
///
/// [`oldest_for`] cannot answer this. It reports the oldest span on the
/// backend, so any other span opened earlier on the same backend masks the
/// one the caller is asking about — a caller reasoning about the lifetime of
/// *its own* span reads a different span's presence as its own. Callers that
/// need "is this span still open" need this predicate; callers that need
/// "how long has this backend held a read open" need [`oldest_for`].
///
/// Recovers a poisoned lock via `into_inner()` (see [`register_scoped`]).
pub fn any_open_labeled(filter: &TxOriginFilter, label: &str) -> bool {
    let registry = REGISTRY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    registry
        .values()
        .any(|meta| filter.matches(&meta.origin) && meta.label.as_deref() == Some(label))
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
#[path = "tx_registry/tests.rs"]
mod tests;
