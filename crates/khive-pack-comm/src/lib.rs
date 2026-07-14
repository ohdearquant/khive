//! pack-comm — Communication pack providing the five `comm.*` verbs.

pub mod handlers;
pub(crate) mod message;
pub(crate) mod pack;
pub(crate) mod params;
pub(crate) mod vocab;

pub use pack::CommPack;

/// The namespace the local single-tenant channel poll loop writes heartbeat
/// rows under (khive #606 design review Blocker fix, example actor
/// 2026-07-04).
///
/// Channel heartbeat rows are an OPERATIONAL surface, not message data:
/// `khive-mcp`'s poll loop (`record_channel_heartbeat`) always dispatches
/// `comm.heartbeat` with this constant as its explicit `namespace` param, so
/// its writes must not follow `KHIVE_EMAIL_INGEST_NAMESPACE` (or any other
/// caller-chosen namespace) even though that env var configures the same
/// daemon's message-ingestion namespace.
///
/// `handle_heartbeat` itself no longer pins every write to this constant
/// (khive #917): it persists under `token.namespace()`, the same
/// dispatch-authorized namespace every other comm verb uses, so an
/// authorized per-tenant writer (dispatching via `VerbRegistry::dispatch_as`
/// with a `VerifiedActor` and its own tenant namespace as the explicit
/// `namespace` dispatch param) can produce heartbeat rows for its own
/// namespace. `comm.health`
/// (khive #877) resolves its read namespace the same way
/// (`token.namespace()`); an unscoped `comm.health()` call still defaults to
/// `"local"` and so still observes rows the local poll loop wrote via this
/// constant.
pub const CHANNEL_HEALTH_NAMESPACE: &str = "local";
