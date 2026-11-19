use std::sync::Arc;
use tokio::sync::Barrier;

tokio::task_local! {
    pub(super) static AFTER_CREATE: (Arc<Barrier>, Arc<Barrier>);
}

pub(super) async fn pause_after_create() {
    if let Ok((arrived, resume)) = AFTER_CREATE.try_with(Clone::clone) {
        tokio::time::timeout(std::time::Duration::from_secs(10), arrived.wait())
            .await
            .expect("public verb did not reach post-create seam");
        resume.wait().await;
    }
}
