//! Content-free execution facts for one search dispatch.
//!
//! Arms are recorded when their futures run, even if they return no hits or
//! degrade. This is separate from hit provenance and executed-work counters.
//! Request-owned spawned children explicitly inherit the recorder; detached
//! background work never does. Every dispatch masks its parent's recorder.

use std::future::Future;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

/// Retrieval arm that has started executing in this dispatch.
#[derive(Clone, Copy)]
pub enum SearchArm {
    Text = 1,
    Vector = 2,
}

/// The union of arms started by this dispatch and its joined children.
#[derive(Clone, Copy, Default)]
pub(crate) struct SearchArms(u8);

impl SearchArms {
    pub(crate) fn as_str(self) -> &'static str {
        match self.0 {
            1 => "text",
            2 => "vector",
            3 => "both",
            _ => "none",
        }
    }
}

tokio::task_local! {
    static CURRENT: Option<Arc<AtomicU8>>;
}

pub(crate) async fn capture<F: Future>(enabled: bool, future: F) -> (F::Output, SearchArms) {
    let recorder = enabled.then(|| Arc::new(AtomicU8::new(0)));
    let output = CURRENT.scope(recorder.clone(), future).await;
    let arms = recorder.map_or(0, |r| r.load(Ordering::Relaxed));
    (output, SearchArms(arms))
}

/// Record an actual arm entry. No allocation or effect outside search dispatch.
pub fn record(arm: SearchArm) {
    let _ = CURRENT.try_with(|recorder| {
        if let Some(recorder) = recorder {
            recorder.fetch_or(arm as u8, Ordering::Relaxed);
        }
    });
}

/// Capture before spawning a request-owned child, and re-enter inside it.
/// The returned future must be joined before the dispatch audit is produced.
pub fn inherit<F: Future>(future: F) -> impl Future<Output = F::Output> {
    let recorder = CURRENT.try_with(Clone::clone).ok().flatten();
    async move { CURRENT.scope(recorder, future).await }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn joined_children_union_arms_and_nested_dispatches_are_isolated() {
        let (_, outer) = capture(true, async {
            record(SearchArm::Text);
            tokio::spawn(inherit(async { record(SearchArm::Vector) }))
                .await
                .unwrap();
            let (_, nested) = capture(true, async { record(SearchArm::Text) }).await;
            assert_eq!(nested.as_str(), "text");
        })
        .await;
        assert_eq!(outer.as_str(), "both");

        let (_, outer) = capture(true, async {
            capture(false, async { record(SearchArm::Vector) }).await;
            // Uninherited background work must not contaminate the parent.
            tokio::spawn(async { record(SearchArm::Text) })
                .await
                .unwrap();
        })
        .await;
        assert_eq!(outer.as_str(), "none");
    }

    #[tokio::test]
    async fn concurrent_dispatches_keep_distinct_arm_sets() {
        let ((_, text), (_, vector)) = tokio::join!(
            capture(true, async {
                record(SearchArm::Text);
                tokio::task::yield_now().await;
            }),
            capture(true, async {
                record(SearchArm::Vector);
                tokio::task::yield_now().await;
            }),
        );
        assert_eq!(text.as_str(), "text");
        assert_eq!(vector.as_str(), "vector");
    }
}
