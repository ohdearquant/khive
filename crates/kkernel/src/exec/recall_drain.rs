//! Best-effort settling of recall producers in a short-lived local exec process.
//!
//! A local exec process is assumed to contain only its own recall producers.
//! Wait only for already admitted recall ledger tasks, before closing audit
//! admission. Capacity skips, producer errors, tasks admitted after a zero
//! observation, and a timeout can still leave best-effort events unwritten.
//! Timeout does not change the exec result or retry an event.

use std::time::Duration;

const POLL_INTERVAL: Duration = Duration::from_millis(10);

pub(super) async fn wait_for_pending(timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let pending = khive_runtime::recall_ledger_snapshot().pending;
        if pending == 0 {
            return;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            tracing::warn!(
                target: "kkernel::exec",
                pending,
                "exec: recall ledger producers did not settle before audit shutdown"
            );
            return;
        }
        tokio::time::sleep_until(std::cmp::min(deadline, now + POLL_INTERVAL)).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Metadata, Subscriber};

    #[derive(Clone, Default)]
    struct Warnings(Arc<Mutex<Vec<Fields>>>);

    #[derive(Default)]
    struct Fields {
        text: String,
        pending: Option<u64>,
    }

    impl Visit for Fields {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            use std::fmt::Write;
            let _ = write!(self.text, "{}={value:?} ", field.name());
        }

        fn record_u64(&mut self, field: &Field, value: u64) {
            if field.name() == "pending" {
                self.pending = Some(value);
            }
        }
    }

    impl Subscriber for Warnings {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            metadata.level() == &tracing::Level::WARN && metadata.target() == "kkernel::exec"
        }

        fn new_span(&self, _: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }
        fn record(&self, _: &Id, _: &Record<'_>) {}
        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn enter(&self, _: &Id) {}
        fn exit(&self, _: &Id) {}

        fn event(&self, event: &Event<'_>) {
            let mut fields = Fields::default();
            event.record(&mut fields);
            self.0.lock().unwrap().push(fields);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pending_producer_warns_and_returns_after_bound() {
        // This test has no storage or producer seam.  Admission is synchronous,
        // and the receiver keeps the real process-wide slot occupied.
        let before = khive_runtime::recall_ledger_snapshot();
        assert_eq!(
            before.pending, 0,
            "fixture requires a fresh process with no pending producers"
        );
        assert!(
            before.timeout_ms > 12_000,
            "fixture requires a producer timeout longer than the watchdog"
        );
        let (release, held) = tokio::sync::oneshot::channel::<()>();
        let (started, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (finished, mut finished_rx) = tokio::sync::oneshot::channel::<()>();
        khive_runtime::track_recall_ledger_task(async move {
            let _ = started.send(());
            let _ = held.await;
            let _ = finished.send(());
        });
        let admitted = khive_runtime::recall_ledger_snapshot();
        assert!(
            admitted.pending > before.pending,
            "fixture did not admit its producer"
        );
        tokio::time::timeout(Duration::from_secs(2), started_rx)
            .await
            .expect("held producer must start")
            .expect("held producer signalled start");

        let warnings = Warnings::default();
        let events = Arc::clone(&warnings.0);
        // The current-thread runtime keeps this thread-local subscriber active
        // across every poll of the drain future.
        let _guard = tracing::subscriber::set_default(warnings);
        let start = tokio::time::Instant::now();
        tokio::time::timeout(
            Duration::from_secs(10),
            super::wait_for_pending(Duration::from_secs(5)),
        )
        .await
        .expect("bounded drain returned");
        assert!(start.elapsed() >= Duration::from_secs(5));
        assert!(
            khive_runtime::recall_ledger_snapshot().pending > 0,
            "fixture producer must remain pending at return"
        );
        assert!(
            matches!(
                finished_rx.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ),
            "the fixture producer must still be held at return"
        );
        let captured = events.lock().unwrap();
        let matching: Vec<_> = captured
            .iter()
            .filter(|fields| {
                fields
                    .text
                    .contains("exec: recall ledger producers did not settle before audit shutdown")
            })
            .collect();
        assert_eq!(matching.len(), 1, "expected exactly one drain warning");
        assert!(
            matches!(matching[0].pending, Some(count) if count > 0),
            "warning must report a positive pending count"
        );
        drop(captured);
        drop(release);
        tokio::time::timeout(Duration::from_secs(2), finished_rx)
            .await
            .expect("held producer should finish after release")
            .expect("held producer sent completion");
    }
}
