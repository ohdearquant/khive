//! pack-comm — Communication pack providing the five `comm.*` verbs.

pub mod handlers;
pub(crate) mod message;
pub(crate) mod pack;
pub(crate) mod params;
pub(crate) mod vocab;

pub use pack::CommPack;

/// The namespace the local single-tenant channel poll loop passes explicitly
/// when it writes heartbeat rows. `comm.heartbeat` no longer pins every write
/// to this constant (khive #917): it persists under `token.namespace()`, the
/// same dispatch-authorized namespace every other comm verb uses, so an
/// authorized per-tenant writer produces heartbeat rows under its own
/// namespace. The local poll loop (`khive-mcp`'s `record_channel_heartbeat`)
/// keeps writing here by passing this constant as its explicit `namespace=`.
/// `comm.health` reads via `token.namespace()` too (khive #877); an unscoped
/// call still defaults to `"local"` and so observes the local poll loop's rows.
/// See crates/khive-pack-comm/docs/api/channel-health.md#librschannel_health_namespace--rationale
/// for the incident history (khive #606, #877, #917).
pub const CHANNEL_HEALTH_NAMESPACE: &str = "local";
