//! Runtime write-route classification and checkpoint ownership admission.

#[cfg(test)]
use super::Arc;
use super::{Condvar, Connection, Mutex, SqliteError};

/// Runtime-owned SQL transactions that share the store write-routing policy.
#[derive(Clone, Copy, Debug)]
pub enum RuntimeWriteOperation {
    MergeEntity,
    MergeNote,
    UpdateSymmetricEdge,
}

impl RuntimeWriteOperation {
    pub(super) fn operation(self) -> &'static str {
        match self {
            Self::MergeEntity => "merge_entity",
            Self::MergeNote => "merge_note",
            Self::UpdateSymmetricEdge => "update_edge",
        }
    }

    pub(super) fn fallback_site(self) -> crate::timeout_sink::Site {
        match self {
            Self::MergeEntity => crate::timeout_sink::Site::DirectRouteRuntimeMergeEntity,
            Self::MergeNote => crate::timeout_sink::Site::DirectRouteRuntimeMergeNote,
            Self::UpdateSymmetricEdge => {
                crate::timeout_sink::Site::DirectRouteRuntimeUpdateSymmetricEdge
            }
        }
    }
}

/// Bounded WAL autocheckpoint applied to writer-capable connections while no
/// dedicated checkpoint owner has claimed the pool (4,000 pages ≈ 16 MiB at
/// SQLite's default 4 KiB page size — SQLite's historic behaviour for this
/// pool). Not a tuning parameter: there is no config field or environment
/// override, and the only way to change the effective value is an actual
/// ownership claim ([`ConnectionPool::claim_checkpoint_ownership`](super::ConnectionPool::claim_checkpoint_ownership)), which a
/// runtime may make only when it really runs the scheduled checkpoint task.
pub(crate) const FALLBACK_WAL_AUTOCHECKPOINT_PAGES: u32 = 4_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CheckpointOwnership {
    Unclaimed,
    Claiming,
    Claimed,
}

pub(super) struct CheckpointOwnershipState {
    pub(super) phase: CheckpointOwnership,
    #[cfg(test)]
    pub(super) connection_waiters: usize,
}

#[cfg(test)]
pub(super) struct CheckpointConnectionConfigPause {
    pub(super) selected: std::sync::Barrier,
    pub(super) resume: std::sync::Barrier,
}

#[cfg(test)]
impl CheckpointConnectionConfigPause {
    pub(super) fn new() -> Self {
        Self {
            selected: std::sync::Barrier::new(2),
            resume: std::sync::Barrier::new(2),
        }
    }
}

pub(super) struct CheckpointOwnershipGate {
    pub(super) state: Mutex<CheckpointOwnershipState>,
    pub(super) changed: Condvar,
    #[cfg(test)]
    pub(super) connection_config_pause: Mutex<Option<Arc<CheckpointConnectionConfigPause>>>,
    #[cfg(test)]
    pub(super) claim_lock_observed: Mutex<Option<std::sync::mpsc::SyncSender<bool>>>,
}

impl CheckpointOwnershipGate {
    pub(super) fn new() -> Self {
        Self {
            state: Mutex::new(CheckpointOwnershipState {
                phase: CheckpointOwnership::Unclaimed,
                #[cfg(test)]
                connection_waiters: 0,
            }),
            changed: Condvar::new(),
            #[cfg(test)]
            connection_config_pause: Mutex::new(None),
            #[cfg(test)]
            claim_lock_observed: Mutex::new(None),
        }
    }

    /// Join an in-flight claim, or become the one caller that configures it.
    /// Returns `false` when another caller has already completed the claim.
    pub(super) fn begin_claim(&self) -> bool {
        #[cfg(test)]
        let claim_lock_observed = self.claim_lock_observed.lock().take();
        #[cfg(test)]
        let mut state = if let Some(observed) = claim_lock_observed {
            match self.state.try_lock() {
                Some(state) => {
                    let _ = observed.send(false);
                    state
                }
                None => {
                    let _ = observed.send(true);
                    self.state.lock()
                }
            }
        } else {
            self.state.lock()
        };
        #[cfg(not(test))]
        let mut state = self.state.lock();
        loop {
            match state.phase {
                CheckpointOwnership::Unclaimed => {
                    state.phase = CheckpointOwnership::Claiming;
                    self.changed.notify_all();
                    return true;
                }
                CheckpointOwnership::Claiming => self.changed.wait(&mut state),
                CheckpointOwnership::Claimed => return false,
            }
        }
    }

    pub(super) fn finish_claim(&self, succeeded: bool) {
        let mut state = self.state.lock();
        debug_assert_eq!(state.phase, CheckpointOwnership::Claiming);
        state.phase = if succeeded {
            CheckpointOwnership::Claimed
        } else {
            CheckpointOwnership::Unclaimed
        };
        self.changed.notify_all();
    }

    fn settled_state(&self) -> parking_lot::MutexGuard<'_, CheckpointOwnershipState> {
        let mut state = self.state.lock();
        while state.phase == CheckpointOwnership::Claiming {
            #[cfg(test)]
            {
                state.connection_waiters += 1;
                self.changed.notify_all();
            }
            self.changed.wait(&mut state);
            #[cfg(test)]
            {
                state.connection_waiters -= 1;
                self.changed.notify_all();
            }
        }
        state
    }

    #[cfg(test)]
    pub(super) fn wal_autocheckpoint_pages(&self) -> u32 {
        let state = self.settled_state();
        match state.phase {
            CheckpointOwnership::Unclaimed => FALLBACK_WAL_AUTOCHECKPOINT_PAGES,
            CheckpointOwnership::Claimed => 0,
            CheckpointOwnership::Claiming => unreachable!("claim wait must settle the state"),
        }
    }

    /// Wait for any in-flight claim, select the resulting posture, and retain
    /// the gate until SQLite has applied that connection-local PRAGMA. A claim
    /// therefore linearizes entirely before or after this configuration,
    /// never between its state sample and side effect.
    pub(super) fn configure_wal_autocheckpoint(
        &self,
        conn: &Connection,
    ) -> Result<(), SqliteError> {
        let state = self.settled_state();
        let pages = match state.phase {
            CheckpointOwnership::Unclaimed => FALLBACK_WAL_AUTOCHECKPOINT_PAGES,
            CheckpointOwnership::Claimed => 0,
            CheckpointOwnership::Claiming => unreachable!("claim wait must settle the state"),
        };
        #[cfg(test)]
        if let Some(pause) = self.connection_config_pause.lock().take() {
            pause.selected.wait();
            pause.resume.wait();
        }
        conn.pragma_update(None, "wal_autocheckpoint", pages)?;
        drop(state);
        Ok(())
    }
}
