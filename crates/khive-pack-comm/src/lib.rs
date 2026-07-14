//! pack-comm — Communication pack providing the five `comm.*` verbs.

pub mod handlers;
pub(crate) mod message;
pub(crate) mod pack;
pub(crate) mod params;
pub(crate) mod vocab;

pub use pack::CommPack;

/// The namespace `comm.heartbeat` always writes to, regardless of caller
/// namespace — the only comm handler pinned to this constant. `comm.health`
/// reads it only as the default when the caller passes no explicit
/// `namespace=`; an explicitly-scoped call reads its own namespace instead.
/// See crates/khive-pack-comm/docs/handlers.md#librschannel_health_namespace--rationale
/// for the incident history (khive #606, #877).
pub const CHANNEL_HEALTH_NAMESPACE: &str = "local";
