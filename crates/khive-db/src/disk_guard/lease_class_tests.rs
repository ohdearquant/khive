use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use super::*;

#[test]
fn bootstrap_stays_bootstrap_under_a_migration_scope() {
    let _scope = lease_class_scope(LeaseClass::Migration);
    assert_eq!(
        effective_class(LeaseClass::Bootstrap),
        LeaseClass::Bootstrap
    );
    assert_eq!(effective_class(LeaseClass::Pooled), LeaseClass::Migration);
}

#[test]
fn capture_reads_the_current_scope() {
    assert_eq!(WriteClassContext::capture(), WriteClassContext::default());
    let _scope = lease_class_scope(LeaseClass::Migration);
    assert_eq!(WriteClassContext::capture(), WriteClassContext::migration());
}

#[test]
fn wrap_blocking_applies_the_context_on_another_thread() {
    let wrapped =
        WriteClassContext::migration().wrap_blocking(|| effective_class(LeaseClass::Pooled));
    let class = std::thread::spawn(wrapped).join().expect("thread");
    assert_eq!(class, LeaseClass::Migration);
    let plain = std::thread::spawn(|| effective_class(LeaseClass::Pooled))
        .join()
        .expect("thread");
    assert_eq!(plain, LeaseClass::Pooled);
}

/// Pending on its first poll, ready on its second; records the class it saw
/// on each poll.
struct TwoPolls {
    seen: Vec<LeaseClass>,
}

impl Future for TwoPolls {
    type Output = Vec<LeaseClass>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.seen.push(effective_class(LeaseClass::Pooled));
        if self.seen.len() == 1 {
            cx.waker().wake_by_ref();
            Poll::Pending
        } else {
            Poll::Ready(std::mem::take(&mut self.seen))
        }
    }
}

#[test]
fn scope_future_applies_the_context_per_poll_and_never_across_a_suspension() {
    let mut future = WriteClassContext::migration().scope_future(TwoPolls { seen: Vec::new() });
    let mut cx = Context::from_waker(Waker::noop());
    assert!(Pin::new(&mut future).poll(&mut cx).is_pending());
    // Between polls this thread carries no scope.
    assert_eq!(effective_class(LeaseClass::Pooled), LeaseClass::Pooled);
    let Poll::Ready(seen) = Pin::new(&mut future).poll(&mut cx) else {
        panic!("second poll must be ready");
    };
    assert_eq!(seen, vec![LeaseClass::Migration, LeaseClass::Migration]);
    assert_eq!(effective_class(LeaseClass::Pooled), LeaseClass::Pooled);
}

#[test]
fn a_scope_is_restored_after_a_panic_inside_it() {
    let _outer = lease_class_scope(LeaseClass::Maintenance);
    let result = std::panic::catch_unwind(|| {
        let _inner = lease_class_scope(LeaseClass::Migration);
        panic!("inside the scope");
    });
    assert!(result.is_err());
    assert_eq!(effective_class(LeaseClass::Pooled), LeaseClass::Maintenance);
}
