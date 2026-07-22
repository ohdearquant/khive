//! ADR-103 Amendment 2: dispatch-scoped executed-usage counters.
//!
//! A closed seven-counter vocabulary of work a dispatch actually executed,
//! collected by a dispatch-accounting context armed around each verb dispatch
//! and surfaced (a) as a per-op `usage` object in the response envelope and
//! (b) as `resource.units` payload enrichment on the per-dispatch audit row.
//!
//! Propagation contract (Amendment 2 Part 2): the context is an Arc-shared
//! accumulator carried in a task-local scope. Futures the dispatch awaits or
//! `join!`s directly observe it automatically; a **request-owned spawned
//! child** (a `tokio::spawn` whose `JoinHandle` is awaited before the
//! response is produced) must capture the handle with [`current`] before the
//! spawn and re-enter it with [`scope`] inside the child, because Tokio
//! task-locals do not cross `tokio::spawn`. Detached background work receives
//! no context and is attributed via phase-span events instead.
//!
//! Reporting is best-effort and can never fail a verb: every increment path
//! is a no-op when no context is armed, and a dispatch whose counters cannot
//! be trusted ships no `usage` object at all — never a partial one.
//!
//! **Issued vs returned.** Each counter's doc below says which it is, and the
//! distinction decides where its increment goes relative to the `?` on the
//! fallible call. An *issued* counter (`embed_calls`, `fts_passes`,
//! `vector_passes`, `db_round_trips`) counts work handed to an engine or
//! store, so it must be incremented whether or not the call resolved `Ok` —
//! a request that ran real work and then failed reporting zero is the exact
//! case a consumer of these numbers cannot afford. A *returned* counter
//! (`graph_hops`, `ann_jobs_consumed`, `event_rows`) counts what came back,
//! so a failed call legitimately contributes nothing. Increment an issued
//! counter *after* the resource is resolved (so a lookup that never reached
//! the engine counts nothing) and *before* the error propagates.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde_json::Value;

/// One executed-work counter in the closed Amendment 2 vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageUnit {
    /// Issued. One text handed to one embedding engine (a batch of k texts
    /// through one engine counts k, per engine).
    EmbedCalls,
    /// Issued. One FTS5 query execution.
    FtsPasses,
    /// Issued. One vector/ANN probe, per engine per query.
    VectorPasses,
    /// Returned. One adjacency entry returned by storage during
    /// BFS/traversal, counted before visited-set de-duplication.
    GraphHops,
    /// Issued. One batched storage round-trip issued by the handler path.
    DbRoundTrips,
    /// Returned. One `ann_write_log` job drained by the inline warm-path
    /// consumer.
    AnnJobsConsumed,
    /// Returned. One event-plane row successfully appended by this dispatch,
    /// excluding the enclosing per-dispatch audit row (the snapshot is frozen
    /// before that row is written; it cannot count itself).
    EventRows,
}

#[derive(Debug, Default)]
struct UsageInner {
    frozen: std::sync::OnceLock<Value>,
    embed_calls: AtomicU64,
    fts_passes: AtomicU64,
    vector_passes: AtomicU64,
    graph_hops: AtomicU64,
    db_round_trips: AtomicU64,
    ann_jobs_consumed: AtomicU64,
    event_rows: AtomicU64,
}

/// The Arc-shared dispatch-accounting context (Amendment 2 Part 2).
#[derive(Debug, Clone, Default)]
pub struct UsageContext {
    inner: Arc<UsageInner>,
}

impl UsageContext {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add `n` to one counter. Saturating: a counter never wraps.
    pub fn add(&self, unit: UsageUnit, n: u64) {
        let cell = match unit {
            UsageUnit::EmbedCalls => &self.inner.embed_calls,
            UsageUnit::FtsPasses => &self.inner.fts_passes,
            UsageUnit::VectorPasses => &self.inner.vector_passes,
            UsageUnit::GraphHops => &self.inner.graph_hops,
            UsageUnit::DbRoundTrips => &self.inner.db_round_trips,
            UsageUnit::AnnJobsConsumed => &self.inner.ann_jobs_consumed,
            UsageUnit::EventRows => &self.inner.event_rows,
        };
        let mut cur = cell.load(Ordering::Relaxed);
        loop {
            let next = cur.saturating_add(n);
            match cell.compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(observed) => cur = observed,
            }
        }
    }

    /// Freeze the counters once, at the audit-snapshot point (Amendment 2
    /// Part 3 ordering: after request-owned children are joined and non-audit
    /// appends have resolved, before the enclosing audit row is written).
    /// The first call takes the snapshot; every later call — including the
    /// response-envelope read — returns the same frozen object, so both read
    /// paths carry the identical value even if stray increments (e.g. the
    /// post-audit dispatch hook) land afterwards.
    pub fn freeze(&self) -> Value {
        self.inner.frozen.get_or_init(|| self.snapshot()).clone()
    }

    /// The frozen object if [`Self::freeze`] ran, else a live snapshot.
    pub fn frozen_or_snapshot(&self) -> Value {
        match self.inner.frozen.get() {
            Some(v) => v.clone(),
            None => self.snapshot(),
        }
    }

    /// Freeze the counters into the wire `usage` object. Zero-valued counters
    /// are omitted; a fully zero snapshot still returns an (empty) object —
    /// "measured, nothing counted" is distinct from "not measured", which is
    /// represented by the absence of the object entirely.
    pub fn snapshot(&self) -> Value {
        let mut map = serde_json::Map::new();
        let mut put = |key: &str, cell: &AtomicU64| {
            let v = cell.load(Ordering::Relaxed);
            if v > 0 {
                map.insert(key.to_string(), Value::from(v));
            }
        };
        put("embed_calls", &self.inner.embed_calls);
        put("fts_passes", &self.inner.fts_passes);
        put("vector_passes", &self.inner.vector_passes);
        put("graph_hops", &self.inner.graph_hops);
        put("db_round_trips", &self.inner.db_round_trips);
        put("ann_jobs_consumed", &self.inner.ann_jobs_consumed);
        put("event_rows", &self.inner.event_rows);
        Value::Object(map)
    }
}

tokio::task_local! {
    static CURRENT: UsageContext;
}

/// Run `fut` with `ctx` armed as the current dispatch-accounting context.
pub async fn scope<F: std::future::Future>(ctx: UsageContext, fut: F) -> F::Output {
    CURRENT.scope(ctx, fut).await
}

/// The currently armed context, if any. Request-owned spawned children
/// capture this before `tokio::spawn` and re-enter it with [`scope`] inside
/// the child; every other caller should prefer [`count`].
pub fn current() -> Option<UsageContext> {
    CURRENT.try_with(Clone::clone).ok()
}

/// Add `n` to one counter of the currently armed context. No-op when no
/// context is armed (background work, tests, un-instrumented entry points) —
/// reporting can never fail or perturb the verb itself.
pub fn count(unit: UsageUnit, n: u64) {
    if n == 0 {
        return;
    }
    if let Ok(ctx) = CURRENT.try_with(Clone::clone) {
        ctx.add(unit, n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn count_is_noop_without_scope_and_counts_inside() {
        count(UsageUnit::EmbedCalls, 3);
        let ctx = UsageContext::new();
        scope(ctx.clone(), async {
            count(UsageUnit::EmbedCalls, 2);
            count(UsageUnit::FtsPasses, 1);
            count(UsageUnit::GraphHops, 0);
        })
        .await;
        let snap = ctx.snapshot();
        assert_eq!(snap["embed_calls"], 2);
        assert_eq!(snap["fts_passes"], 1);
        assert!(
            snap.get("graph_hops").is_none(),
            "zero counters are omitted"
        );
    }

    #[tokio::test]
    async fn joined_spawned_child_counts_via_explicit_handle() {
        let ctx = UsageContext::new();
        scope(ctx.clone(), async {
            let handle = current().expect("scope armed");
            let child = tokio::spawn(scope(handle, async {
                count(UsageUnit::VectorPasses, 2);
            }));
            child.await.expect("join child");
        })
        .await;
        assert_eq!(ctx.snapshot()["vector_passes"], 2);
    }

    #[tokio::test]
    async fn detached_spawn_without_handle_contributes_nothing() {
        let ctx = UsageContext::new();
        scope(ctx.clone(), async {
            // A detached task spawned WITHOUT re-entering the scope: its
            // counts must not reach the dispatch context.
            let orphan = tokio::spawn(async {
                count(UsageUnit::EmbedCalls, 99);
            });
            orphan.await.expect("join orphan");
        })
        .await;
        assert_eq!(
            ctx.snapshot(),
            serde_json::json!({}),
            "task-locals do not cross tokio::spawn; only an explicit handle propagates"
        );
    }

    #[test]
    fn saturating_add_never_wraps() {
        let ctx = UsageContext::new();
        ctx.add(UsageUnit::EventRows, u64::MAX);
        ctx.add(UsageUnit::EventRows, 5);
        assert_eq!(ctx.snapshot()["event_rows"], u64::MAX);
    }
}
