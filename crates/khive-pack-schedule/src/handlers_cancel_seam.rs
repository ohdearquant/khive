//! Test-only barrier after cancellation prepares a validated note snapshot.

use std::{sync::Arc, time::Duration};
use tokio::sync::Barrier;

tokio::task_local! {
    pub(super) static AFTER_READ: (Arc<Barrier>, Arc<Barrier>);
}

pub(super) async fn pause_after_read() {
    if let Ok((arrived, resume)) = AFTER_READ.try_with(Clone::clone) {
        tokio::time::timeout(Duration::from_secs(10), async {
            arrived.wait().await;
            resume.wait().await;
        })
        .await
        .expect("cancel snapshot barrier timed out");
    }
}
