use std::sync::Arc;
use tokio::sync::Barrier;

tokio::task_local! {
    pub(crate) static AFTER_READ_BARRIER: Arc<Barrier>;
    pub(crate) static BEFORE_ENTITY_INDEX_PUBLISH: Arc<(Barrier, Barrier)>;
    pub(crate) static BEFORE_ENTITY_VECTOR_PUBLISH: Arc<(Barrier, Barrier)>;
}

pub(crate) async fn pause_after_read() {
    if let Ok(barrier) = AFTER_READ_BARRIER.try_with(Arc::clone) {
        barrier.wait().await;
    }
}

pub(crate) async fn pause_before_entity_index_publish() {
    if let Ok(barriers) = BEFORE_ENTITY_INDEX_PUBLISH.try_with(Arc::clone) {
        barriers.0.wait().await;
        barriers.1.wait().await;
    }
}

pub(crate) async fn pause_before_entity_vector_publish() {
    if let Ok(barriers) = BEFORE_ENTITY_VECTOR_PUBLISH.try_with(Arc::clone) {
        barriers.0.wait().await;
        barriers.1.wait().await;
    }
}
