use std::sync::Arc;
use tokio::sync::Barrier;

/// Which of the drain's windows a gate is armed for. A gate trips at the
/// point it names and nowhere else, so a drain that passes through both
/// seams parks once, at the one the test asked for. Without this a test
/// arming the earlier window would also be caught by the later one and
/// hang waiting for a second handshake it never planned to perform.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PausePoint {
    /// Between the page-query snapshot and the CAS claim.
    BeforeClaim,
    /// After claim and dispatch, immediately before the finalizer's fresh
    /// current-properties read.
    BeforeFinalizeRead,
}

/// Two-phase handshake: `reached` lets the driving test learn the drain
/// task has arrived at the pause point (i.e. genuinely parked, not just
/// scheduled) before it performs a concurrent write; `release` then lets
/// the driving test resume the drain task only once that write has
/// landed. A single shared `Barrier` cannot express this — both parties
/// would resume together with no window for the test to act in between.
#[derive(Clone)]
pub(crate) struct PauseGate {
    pub(crate) at: PausePoint,
    pub(crate) reached: Arc<Barrier>,
    pub(crate) release: Arc<Barrier>,
}

tokio::task_local! {
    pub(crate) static PAUSE_GATE: PauseGate;
}

async fn pause_at(point: PausePoint) {
    if let Ok(gate) = PAUSE_GATE.try_with(Clone::clone) {
        if gate.at == point {
            gate.reached.wait().await;
            gate.release.wait().await;
        }
    }
}

pub(crate) async fn pause_before_claim() {
    pause_at(PausePoint::BeforeClaim).await;
}

pub(crate) async fn pause_before_finalize_read() {
    pause_at(PausePoint::BeforeFinalizeRead).await;
}
