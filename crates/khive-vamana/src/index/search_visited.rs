use std::sync::Mutex;

use crate::graph::VisitedSet;

pub(super) const IDLE_CAPACITY: usize = 8;

#[derive(Debug)]
struct CachedVisited {
    nodes: usize,
    visited: VisitedSet,
}

#[derive(Debug, Default)]
pub(super) struct SearchVisitedPool {
    available: Mutex<Vec<CachedVisited>>,
    #[cfg(test)]
    pub(super) allocations: std::sync::atomic::AtomicUsize,
}

impl SearchVisitedPool {
    pub(super) fn checkout(&self, nodes: usize) -> SearchVisitedLease<'_> {
        let cached = self
            .available
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop();
        let cached = match cached {
            Some(cached) if cached.nodes == nodes => cached,
            _ => {
                #[cfg(test)]
                self.allocations
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                CachedVisited {
                    nodes,
                    visited: VisitedSet::new(nodes),
                }
            }
        };
        SearchVisitedLease {
            pool: self,
            cached: Some(cached),
        }
    }

    pub(super) fn clear(&mut self) {
        self.available
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    #[cfg(test)]
    pub(super) fn idle_nodes(&self) -> Vec<usize> {
        self.available
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|cached| cached.nodes)
            .collect()
    }
}

pub(super) struct SearchVisitedLease<'a> {
    pool: &'a SearchVisitedPool,
    cached: Option<CachedVisited>,
}

impl std::ops::Deref for SearchVisitedLease<'_> {
    type Target = VisitedSet;

    fn deref(&self) -> &Self::Target {
        &self.cached.as_ref().expect("live search lease").visited
    }
}

impl std::ops::DerefMut for SearchVisitedLease<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.cached.as_mut().expect("live search lease").visited
    }
}

impl Drop for SearchVisitedLease<'_> {
    fn drop(&mut self) {
        let cached = self.cached.take().expect("live search lease");
        let overflow = {
            let mut available = self
                .pool
                .available
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if available.len() < IDLE_CAPACITY {
                available.push(cached);
                None
            } else {
                Some(cached)
            }
        };
        // Releasing a corpus-sized buffer must not hold up other checkouts.
        drop(overflow);
    }
}
