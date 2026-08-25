//! Brain-state decomposition configuration (ADR-171).
//!
//! Forward-deployed seam for the fold-decoupling worker: when that worker
//! lands, `Some` on [`crate::RuntimeConfig::brain_split`] will route feedback
//! through the event plane, folded asynchronously from the durable
//! `brain_fold_cursor` (V22). Later phases grow this struct with the
//! separate `brain.db` store and brain-daemon fields (database path, socket).

/// Configuration for the decoupled brain fold (ADR-171).
///
/// In this tree the field is a forward-deployed marker: nothing populates it
/// and populating it changes no behavior yet — the feedback fold still runs
/// synchronously inside verb dispatch. The fold worker that consumes it is
/// the next change in the series; until then the only live consumer is the
/// daemon configuration fingerprint, which already distinguishes `Some` from
/// `None` so a warm daemon can never straddle the two once the worker lands.
#[derive(Debug, Clone, Default)]
pub struct BrainSplitConfig {}
