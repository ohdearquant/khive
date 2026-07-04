//! pack-comm — Communication pack providing the five `comm.*` verbs.

pub mod handlers;
pub(crate) mod message;
pub(crate) mod pack;
pub(crate) mod params;
pub(crate) mod vocab;

pub use pack::CommPack;

/// The namespace `comm.heartbeat` always writes to and `comm.health` always
/// reads from (khive #606 spec-gate Blocker fix, Leo 2026-07-04).
///
/// Channel heartbeat rows are an OPERATIONAL surface, not message data: they
/// must not follow `KHIVE_EMAIL_INGEST_NAMESPACE` (or any other caller-chosen
/// namespace), or a client-role no-arg `comm.health()` call — which reads the
/// default local-visible scope — would see `role: "client"` with an empty
/// `channels` array even though a daemon has live heartbeat state, violating
/// the binding amendment ("empty channels is correct only when no daemon
/// state exists at all").
///
/// This is a single shared constant, not independent literals in the writer
/// (`khive-mcp`'s channel poll loop) and the reader (`handle_health`) — two
/// string literals that can drift apart is the same bug class one rename
/// away.
pub const CHANNEL_HEALTH_NAMESPACE: &str = "local";
