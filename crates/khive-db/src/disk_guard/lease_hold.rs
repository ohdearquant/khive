//! How long each volume lease was held, by what it was taken for.
//!
//! A held lease excludes every other writer on its volume (ADR-154 section 3),
//! so its hold time is the time those writers can wait. The record is
//! process-wide, like the in-process slot: one histogram per class, from grant
//! to release, recorded when the lease drops. Recording never fails a write.

use std::cell::Cell;
use std::sync::OnceLock;
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
pub(crate) struct LeaseClassScope {
    previous: Option<LeaseClass>,
}

pub(crate) fn lease_class_scope(class: LeaseClass) -> LeaseClassScope {
    LeaseClassScope {
        previous: SCOPED_CLASS.with(|scoped| scoped.replace(Some(class))),
    }
}

impl Drop for LeaseClassScope {
    fn drop(&mut self) {
        SCOPED_CLASS.with(|scoped| scoped.set(self.previous));
    }
}

/// The class a lease taken here is recorded as: the scope's, else the site's.
pub(crate) fn effective_class(site: LeaseClass) -> LeaseClass {
    SCOPED_CLASS.with(Cell::get).unwrap_or(site)
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
