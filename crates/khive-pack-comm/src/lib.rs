//! pack-comm — Communication pack providing the five `comm.*` verbs.

pub mod handlers;
pub(crate) mod message;
pub(crate) mod pack;
pub(crate) mod params;
pub(crate) mod vocab;

pub use pack::CommPack;

/// The namespace `comm.heartbeat` always writes to (khive #606 design review
/// Blocker fix, example actor 2026-07-04).
///
/// Channel heartbeat rows are an OPERATIONAL surface, not message data: the
/// write must not follow `KHIVE_EMAIL_INGEST_NAMESPACE` (or any other
/// caller-chosen namespace) — `handle_heartbeat` is the ONLY comm handler
/// pinned to this constant.
///
/// `comm.health` no longer reads this constant unconditionally (khive #877):
/// it resolves its read namespace from the dispatch token
/// (`token.namespace()`), the same explicit `namespace=` escape / `"local"`
/// default every other comm verb uses. An unscoped `comm.health()` call
/// still defaults to `"local"` and so still observes rows this constant
/// wrote — but a call with an explicit non-local `namespace=` reads that
/// namespace instead, and must not fall back to this constant to find
/// heartbeat state a single-namespace daemon wrote elsewhere.
pub const CHANNEL_HEALTH_NAMESPACE: &str = "local";
