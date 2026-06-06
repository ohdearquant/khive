use serde::{Deserialize, Serialize};

// ---------- Obligation ----------

/// Side-effects a policy may attach to an `Allow` decision.
///
/// v0 obligation handling is intentionally narrow:
/// - `Audit` obligations are persisted inside the dispatch `AuditEvent` when an
///   `EventStore` is wired; otherwise they are emitted through tracing only.
/// - `RateLimit` and `Custom` obligations are NOT enforced in v0.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Obligation {
    Audit {
        tag: String,
    },
    RateLimit {
        window_secs: u64,
        max: u32,
    },
    /// Escape hatch for policy-specific obligations. `value` accepts ARBITRARY
    /// JSON (objects, arrays, scalars, null) — the struct-like variant shape
    /// is required because serde's internally-tagged enums cannot merge the
    /// `kind` discriminator into a non-object newtype payload.
    Custom {
        value: serde_json::Value,
    },
}
