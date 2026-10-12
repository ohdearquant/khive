use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::Barrier;

pub(super) struct OneShotPause {
    barrier: Arc<Barrier>,
    used: AtomicBool,
}

impl OneShotPause {
    pub(super) fn new(barrier: Arc<Barrier>) -> Self {
        Self {
            barrier,
            used: AtomicBool::new(false),
        }
    }
}

tokio::task_local! {
    pub(super) static AFTER_ROW_READ: Arc<OneShotPause>;
}

pub(super) async fn pause_after_row_read() {
    let Ok(pause) = AFTER_ROW_READ.try_with(Arc::clone) else {
        return;
    };
    if !pause.used.swap(true, Ordering::AcqRel) {
        pause.barrier.wait().await;
    }
}
