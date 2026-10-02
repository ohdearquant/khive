//! Request-local reference membership and test observation of actual primitives.

use std::collections::HashSet;
use std::hash::{Hash, Hasher};

use khive_runtime::KhiveRuntime;

#[derive(Clone, Default)]
pub(crate) struct MembershipWork {
    #[cfg(test)]
    counters: std::sync::Arc<Counters>,
}

impl MembershipWork {
    pub(crate) fn for_runtime(runtime: &KhiveRuntime) -> Self {
        #[cfg(test)]
        if let Some(root) = &runtime.config().exec.root {
            let root = std::path::Path::new(root);
            if let Some(counters) = OBSERVERS
                .lock()
                .unwrap()
                .iter()
                .find_map(|(key, counters)| {
                    if key == root {
                        counters.upgrade()
                    } else {
                        None
                    }
                })
            {
                return Self { counters };
            }
        }
        #[cfg(not(test))]
        let _ = runtime;
        Self::default()
    }

    #[cfg(test)]
    pub(crate) fn declared_build(&self, bytes: usize, lookups: usize, copied: usize) {
        use std::sync::atomic::Ordering::Relaxed;
        self.counters.build_bytes.fetch_add(bytes as u64, Relaxed);
        self.counters
            .build_lookups
            .fetch_add(lookups as u64, Relaxed);
        self.counters.build_copied.fetch_add(copied as u64, Relaxed);
    }

    #[cfg(test)]
    pub(crate) fn declared_query(&self, bytes: usize, lookups: usize) {
        use std::sync::atomic::Ordering::Relaxed;
        self.counters.query_bytes.fetch_add(bytes as u64, Relaxed);
        self.counters
            .query_lookups
            .fetch_add(lookups as u64, Relaxed);
        self.counters.queries.fetch_add(1, Relaxed);
    }
}

struct RefKey<'a> {
    value: &'a str,
    #[cfg(test)]
    work: MembershipWork,
}

impl<'a> RefKey<'a> {
    fn new(value: &'a str, work: &MembershipWork) -> Self {
        #[cfg(not(test))]
        let _ = work;
        Self {
            value,
            #[cfg(test)]
            work: work.clone(),
        }
    }
}

impl PartialEq for RefKey<'_> {
    fn eq(&self, other: &Self) -> bool {
        #[cfg(test)]
        self.work
            .counters
            .ref_equalities
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.value == other.value
    }
}

impl Eq for RefKey<'_> {}

impl Hash for RefKey<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        #[cfg(test)]
        {
            use std::sync::atomic::Ordering::Relaxed;
            self.work.counters.ref_hashes.fetch_add(1, Relaxed);
            self.work
                .counters
                .ref_hash_bytes
                .fetch_add(self.value.len() as u64, Relaxed);
        }
        self.value.hash(state);
    }
}

pub(crate) struct SuppliedRefs<'a> {
    keys: HashSet<RefKey<'a>>,
    work: MembershipWork,
}

impl<'a> SuppliedRefs<'a> {
    // Test counters observe work; only the immutable value participates in key equality and hashing.
    #[cfg_attr(test, allow(clippy::mutable_key_type))]
    pub(crate) fn new(references: &'a [String], work: MembershipWork) -> Self {
        let keys = references
            .iter()
            .map(|reference| RefKey::new(reference, &work))
            .collect();
        Self { keys, work }
    }

    pub(crate) fn contains(&self, reference: &str) -> bool {
        self.keys.contains(&RefKey::new(reference, &self.work))
    }
}

#[cfg(test)]
#[derive(Default)]
struct Counters {
    ref_equalities: std::sync::atomic::AtomicU64,
    ref_hashes: std::sync::atomic::AtomicU64,
    ref_hash_bytes: std::sync::atomic::AtomicU64,
    build_bytes: std::sync::atomic::AtomicU64,
    build_lookups: std::sync::atomic::AtomicU64,
    build_copied: std::sync::atomic::AtomicU64,
    query_bytes: std::sync::atomic::AtomicU64,
    query_lookups: std::sync::atomic::AtomicU64,
    queries: std::sync::atomic::AtomicU64,
    pub(crate) declaration_attempts: std::sync::atomic::AtomicU64,
}

#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct WorkSnapshot {
    pub(crate) ref_equalities: u64,
    pub(crate) ref_hashes: u64,
    pub(crate) ref_hash_bytes: u64,
    pub(crate) build_bytes: u64,
    pub(crate) build_lookups: u64,
    pub(crate) build_copied: u64,
    pub(crate) query_bytes: u64,
    pub(crate) query_lookups: u64,
    pub(crate) queries: u64,
    pub(crate) declaration_attempts: u64,
}

#[cfg(test)]
impl MembershipWork {
    pub(crate) fn snapshot(&self) -> WorkSnapshot {
        use std::sync::atomic::Ordering::Relaxed;
        let c = &self.counters;
        WorkSnapshot {
            ref_equalities: c.ref_equalities.load(Relaxed),
            ref_hashes: c.ref_hashes.load(Relaxed),
            ref_hash_bytes: c.ref_hash_bytes.load(Relaxed),
            build_bytes: c.build_bytes.load(Relaxed),
            build_lookups: c.build_lookups.load(Relaxed),
            build_copied: c.build_copied.load(Relaxed),
            query_bytes: c.query_bytes.load(Relaxed),
            query_lookups: c.query_lookups.load(Relaxed),
            queries: c.queries.load(Relaxed),
            declaration_attempts: c.declaration_attempts.load(Relaxed),
        }
    }

    pub(crate) fn declaration_attempt(&self) {
        self.counters
            .declaration_attempts
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
type ObserverEntry = (std::path::PathBuf, std::sync::Weak<Counters>);

#[cfg(test)]
static OBSERVERS: std::sync::Mutex<Vec<ObserverEntry>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
pub(crate) struct WorkObserver {
    root: std::path::PathBuf,
    pub(crate) work: MembershipWork,
}

#[cfg(test)]
impl WorkObserver {
    pub(crate) fn new(root: &std::path::Path) -> Self {
        let work = MembershipWork::default();
        OBSERVERS.lock().unwrap().push((
            root.to_path_buf(),
            std::sync::Arc::downgrade(&work.counters),
        ));
        Self {
            root: root.to_path_buf(),
            work,
        }
    }
}

#[cfg(test)]
impl Drop for WorkObserver {
    fn drop(&mut self) {
        let own = std::sync::Arc::downgrade(&self.work.counters);
        OBSERVERS
            .lock()
            .unwrap()
            .retain(|(root, counters)| root != &self.root || !counters.ptr_eq(&own));
    }
}
