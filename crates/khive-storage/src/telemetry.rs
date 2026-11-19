//! Typed payload structs for the ADR-094 lifecycle telemetry events.
//!
//! These are a documentation/test convenience only: the persisted
//! discriminant on a stored [`crate::Event`] remains `khive_types::EventKind`,
//! and each payload here is serialized into that event's JSON `payload`
//! field by the emitting call site. Nothing here changes storage schema.

use serde::{Deserialize, Serialize};

/// Mirrors the eight ADR-094 lifecycle `EventKind` variants plus the three
/// ADR-103 Stage 1 phase-span variants. Not itself persisted — see the
/// module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleEvent {
    ChannelPollStarted,
    ChannelPollSucceeded,
    ChannelPollFailed,
    ChannelBackoffArmed,
    ChannelBackoffReset,
    ChannelHeartbeatPersistFailed,
    ConfigLocked,
    CheckpointOutcomeRecorded,
    PhaseStarted,
    PhaseCompleted,
    PhaseCancelled,
}

/// Payload for [`khive_types::EventKind::ChannelPollStarted`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelPollStartedPayload {
    pub channel_kind: String,
    pub channel_slug: String,
    pub since_rfc3339: String,
}

/// Payload for [`khive_types::EventKind::ChannelPollSucceeded`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelPollSucceededPayload {
    pub channel_kind: String,
    pub channel_slug: String,
    pub envelope_count: usize,
    pub previous_backoff_attempt: u32,
}

/// Payload for [`khive_types::EventKind::ChannelPollFailed`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelPollFailedPayload {
    pub channel_kind: String,
    pub channel_slug: String,
    pub error_class: String,
    pub error_message: String,
}

/// Payload for [`khive_types::EventKind::ChannelBackoffArmed`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelBackoffArmedPayload {
    pub channel_kind: String,
    pub channel_slug: String,
    pub attempt: u32,
    pub step_ms: u64,
    pub delay_ms: u64,
}

/// Payload for [`khive_types::EventKind::ChannelBackoffReset`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelBackoffResetPayload {
    pub channel_kind: String,
    pub channel_slug: String,
    pub previous_backoff_attempt: u32,
}

/// Payload for [`khive_types::EventKind::ChannelHeartbeatPersistFailed`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelHeartbeatPersistFailedPayload {
    pub channel_kind: String,
    pub channel_slug: String,
    pub error: String,
}

/// Payload for [`khive_types::EventKind::ConfigLocked`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigLockedPayload {
    pub key: String,
    pub value: String,
}

/// Payload for [`khive_types::EventKind::CheckpointOutcomeRecorded`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckpointOutcomeRecordedPayload {
    pub wal_pages: u64,
    pub warn_pages: u64,
    pub high_water_pages: u64,
    pub truncate_high_water_pages: u64,
    pub above_warn: bool,
    pub above_high_water: bool,
    pub above_truncate_high_water: bool,
    /// Number of elevated observations aggregated into this episode so far.
    /// Absent on rows written before the #1838 transition-summary contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode_elevated_ticks: Option<u64>,
    /// Highest WAL frame count observed during this episode so far. Absent on
    /// rows written before the #1838 transition-summary contract.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode_peak_wal_pages: Option<u64>,
}

/// Payload for [`khive_types::EventKind::PhaseStarted`] (ADR-103 Stage 1).
///
/// `work_class` is the closed ADR-103 enum (`interactive` | `warm` |
/// `maintenance` | `inference`), carried as a plain string here since the
/// enum itself is defined in a downstream crate; producers are responsible
/// for using the closed set of values. `corpus_size` is populated only when
/// it is cheaply known at phase start (e.g. a corpus count already on hand);
/// `None` when unknown at this point.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhaseStartedPayload {
    pub work_class: String,
    pub phase: String,
    pub corpus_size: Option<u64>,
}

/// Payload for [`khive_types::EventKind::PhaseCompleted`] (ADR-103 Stage 1).
///
/// `cpu_us` is a process-level `getrusage` delta across the phase (see
/// `khive_runtime::resource`), not a per-thread measurement — `None` when
/// the underlying read is unavailable on this platform.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhaseCompletedPayload {
    pub work_class: String,
    pub phase: String,
    pub wall_us: i64,
    pub cpu_us: Option<i64>,
}

/// Payload for [`khive_types::EventKind::PhaseCancelled`] (ADR-103 Stage 1).
/// Same shape as [`PhaseCompletedPayload`] — the phase ran for `wall_us`
/// before being cut short rather than returning a result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhaseCancelledPayload {
    pub work_class: String,
    pub phase: String,
    pub wall_us: i64,
    pub cpu_us: Option<i64>,
}

#[cfg(test)]
#[path = "telemetry/tests.rs"]
mod tests;
