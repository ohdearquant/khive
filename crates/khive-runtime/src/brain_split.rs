//! Brain-state decomposition configuration (ADR-171).
//!
//! Phase 1 decouples the durable feedback fold from verb dispatch: with the
//! split configured, feedback verbs commit only their public event-plane row
//! and an asynchronous worker in the brain pack folds from a durable cursor.
//! Later phases add the separate `brain.db` store and the brain daemon; this
//! config grows their fields (database path, socket) when they land.

/// `None` on [`crate::RuntimeConfig::brain_split`] = legacy behavior: the
/// feedback fold runs synchronously inside verb dispatch, in the same
/// transaction as the public event append. `Some` = the decoupled fold.
///
/// Populated by the transport hosts' config resolver (with the
/// `KHIVE_BRAIN_SPLIT=0` environment kill-switch restoring legacy behavior);
/// tests and in-memory runtimes leave it `None`.
#[derive(Debug, Clone, Default)]
pub struct BrainSplitConfig {}
