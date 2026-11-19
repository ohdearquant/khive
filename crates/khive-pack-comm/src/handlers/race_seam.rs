use std::sync::Arc;
use tokio::sync::Barrier;

tokio::task_local! {
    pub(crate) static AFTER_READ_BARRIER: Arc<Barrier>;
    pub(crate) static AFTER_QUARANTINE_ROLE_READ: (Arc<Barrier>, Arc<Barrier>);
}

pub(crate) async fn pause_after_read() {
    if let Ok(barrier) = AFTER_READ_BARRIER.try_with(Arc::clone) {
        barrier.wait().await;
    }
}

pub(crate) async fn pause_after_quarantine_role_read() {
    if let Ok((arrived, resume)) = AFTER_QUARANTINE_ROLE_READ.try_with(Clone::clone) {
        arrived.wait().await;
        resume.wait().await;
    }
}
