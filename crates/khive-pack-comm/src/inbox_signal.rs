use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::Notify;

/// Process-local wake signal for newly committed inbox messages.
pub(crate) struct InboxSignal {
    generation: AtomicU64,
    notify: Notify,
}

impl InboxSignal {
    pub(crate) fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            notify: Notify::new(),
        }
    }

    pub(crate) fn snapshot(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub(crate) fn publish(&self) {
        self.generation.fetch_add(1, Ordering::Release);
        self.notify.notify_waiters();
    }

    pub(crate) async fn wait_for_change(&self, observed: u64) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.snapshot() != observed {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::InboxSignal;

    #[tokio::test]
    async fn publish_before_wait_is_not_lost() {
        let signal = InboxSignal::new();
        let observed = signal.snapshot();
        signal.publish();

        tokio::time::timeout(Duration::from_millis(20), signal.wait_for_change(observed))
            .await
            .expect("generation change must make the wait immediately ready");
    }
}
