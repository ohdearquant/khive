//! How long each volume lease was held, by what it was taken for.
//!
//! A held lease excludes every other writer on its volume (ADR-154 section 3),
//! so its hold time is the time those writers can wait. The record is
//! process-wide, like the in-process slot: one histogram per class, from grant
//! to release, recorded when the lease drops. Recording never fails a write.

use std::cell::Cell;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::OnceLock;
use std::task::{Context, Poll};
use std::time::Duration;

use parking_lot::Mutex;

use crate::writer_admission_metrics::{Histogram, HistogramSnapshot, Unavailable};

/// What a volume lease was taken for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LeaseClass {
    /// One request on a pool's FIFO writer task.
    WriterTask,
    /// A checkout of a pool's shared writer connection.
    Pooled,
    /// A standalone write connection or statement.
    Standalone,
    /// A new database's first write while its pool opens.
    Bootstrap,
    /// One schema migration transaction.
    Migration,
    /// VACUUM and FTS index maintenance.
    Maintenance,
}

impl LeaseClass {
    pub(crate) const ALL: [LeaseClass; 6] = [
        LeaseClass::WriterTask,
        LeaseClass::Pooled,
        LeaseClass::Standalone,
        LeaseClass::Bootstrap,
        LeaseClass::Migration,
        LeaseClass::Maintenance,
    ];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            LeaseClass::WriterTask => "writer_task",
            LeaseClass::Pooled => "pooled",
            LeaseClass::Standalone => "standalone",
            LeaseClass::Bootstrap => "bootstrap",
            LeaseClass::Migration => "migration",
            LeaseClass::Maintenance => "maintenance",
        }
    }
}

thread_local! {
    static SCOPED_CLASS: Cell<Option<LeaseClass>> = const { Cell::new(None) };
}

/// While it lives, every lease this thread takes is recorded as its class, for
/// operations that reach a general acquisition site (a migration on the pooled
/// writer, FTS maintenance on a standalone connection).
///
/// The scope is thread-local, so it is `!Send`: it cannot be held across an
/// `.await` that may resume on another thread. Work that crosses a thread
/// (the writer task, a blocking task, a spawned future) carries a
/// [`WriteClassContext`] instead.
pub(crate) struct LeaseClassScope {
    previous: Option<LeaseClass>,
    _thread_bound: PhantomData<*const ()>,
}

pub(crate) fn lease_class_scope(class: LeaseClass) -> LeaseClassScope {
    LeaseClassScope {
        previous: SCOPED_CLASS.with(|scoped| scoped.replace(Some(class))),
        _thread_bound: PhantomData,
    }
}

impl Drop for LeaseClassScope {
    fn drop(&mut self) {
        SCOPED_CLASS.with(|scoped| scoped.set(self.previous));
    }
}

/// The class this thread's scope assigns, if any.
fn scoped_class() -> Option<LeaseClass> {
    SCOPED_CLASS.with(Cell::get)
}

/// The class a lease taken here is recorded as: the scope's, else the site's.
/// A new database's first write while its pool opens stays `Bootstrap` under
/// any scope, so opening another pool inside migration work is still reported
/// as the bootstrap it is.
pub(crate) fn effective_class(site: LeaseClass) -> LeaseClass {
    if site == LeaseClass::Bootstrap {
        return site;
    }
    scoped_class().unwrap_or(site)
}

/// What the volume leases taken for a unit of work are recorded as, carried
/// across the threads that work runs on.
///
/// The class a lease is recorded as comes from a thread-local scope at the site
/// that takes it. Work that issues writes from one thread and takes the lease
/// on another (the pool's writer task, a blocking task, a spawned future)
/// loses that scope at the hop; this context carries it. It changes only how a
/// lease is recorded in the lease-hold report, never whether or when a write is
/// admitted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WriteClassContext {
    class: Option<LeaseClass>,
}

impl WriteClassContext {
    /// Schema work: every lease taken under this context is recorded as
    /// migration class.
    pub fn migration() -> Self {
        Self {
            class: Some(LeaseClass::Migration),
        }
    }

    /// The calling thread's current context, to hand to work that runs on
    /// another thread.
    pub fn capture() -> Self {
        Self {
            class: scoped_class(),
        }
    }

    pub(crate) fn enter(self) -> Option<LeaseClassScope> {
        self.class.map(lease_class_scope)
    }

    /// Wraps a synchronous closure so it runs under this context, on whatever
    /// thread calls it.
    pub fn wrap_blocking<F, R>(self, f: F) -> impl FnOnce() -> R + Send
    where
        F: FnOnce() -> R + Send,
    {
        move || {
            let _scope = self.enter();
            f()
        }
    }

    /// Wraps a future so each poll runs under this context. The scope is
    /// installed for the poll and removed before it returns, so it never spans
    /// a suspension.
    pub fn scope_future<F: Future>(self, future: F) -> ScopedFuture<F> {
        ScopedFuture {
            context: self,
            future: Box::pin(future),
        }
    }
}

/// A future polled under a [`WriteClassContext`]; see
/// [`WriteClassContext::scope_future`].
pub struct ScopedFuture<F> {
    context: WriteClassContext,
    future: Pin<Box<F>>,
}

impl<F: Future> Future for ScopedFuture<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let _scope = self.context.enter();
        self.future.as_mut().poll(cx)
    }
}

struct Holds {
    unavailable: Option<Unavailable>,
    by_class: [Histogram; 6],
}

fn holds() -> &'static Mutex<Holds> {
    static HOLDS: OnceLock<Mutex<Holds>> = OnceLock::new();
    HOLDS.get_or_init(|| {
        Mutex::new(Holds {
            unavailable: None,
            by_class: Default::default(),
        })
    })
}

fn index(class: LeaseClass) -> usize {
    LeaseClass::ALL
        .iter()
        .position(|candidate| *candidate == class)
        .unwrap_or(0)
}

/// Records one lease of `class` held for `held`.
pub(super) fn record(class: LeaseClass, held: Duration) {
    let mut holds = holds().lock();
    if holds.unavailable.is_some() {
        return;
    }
    if let Err(reason) = holds.by_class[index(class)].record(held) {
        holds.unavailable = Some(reason);
    }
}

/// Every class's hold histogram since this process started, or why none can
/// be reported.
pub(crate) fn lease_hold_snapshot() -> Result<Vec<(LeaseClass, HistogramSnapshot)>, Unavailable> {
    let holds = holds().lock();
    if let Some(reason) = holds.unavailable {
        return Err(reason);
    }
    LeaseClass::ALL
        .iter()
        .map(|class| Ok((*class, holds.by_class[index(*class)].snapshot()?)))
        .collect()
}

#[cfg(test)]
#[path = "lease_class_tests.rs"]
mod lease_class_tests;
