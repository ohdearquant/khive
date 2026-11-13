//! Reuse a decoded profile only within the dispatch that returned its exact snapshot.

use std::cell::RefCell;
use std::future::Future;

use serde_json::Value;

use crate::BalancedRecallSnapshot;

pub const LOCAL_FACTOR_MIN: f64 = 0.5;
pub const LOCAL_FACTOR_MAX: f64 = 1.0;

/// Decode the complete snapshot, including validation of entity posteriors.
pub fn decode_profile_snapshot(raw: &Value) -> Option<BalancedRecallSnapshot> {
    if raw.is_null() {
        return None;
    }
    #[cfg(any(test, feature = "test-support"))]
    let _ = DECODE_CALLS.try_with(|calls| calls.set(calls.get() + 1));
    serde_json::from_value(raw.clone()).ok()
}

enum Publication {
    Absent,
    Decoded {
        raw: Value,
        snapshot: Option<BalancedRecallSnapshot>,
    },
    Ambiguous,
}

/// A failed decode is reusable too; absence requires the response-only fallback.
pub enum CapturedSnapshot {
    Unavailable,
    Decoded(Option<BalancedRecallSnapshot>),
}

pub struct ProfileSnapshotCapture {
    profile_id: String,
    namespace: String,
    publication: Publication,
}

impl ProfileSnapshotCapture {
    /// Consume only after successful authorized dispatch, against its final response.
    pub fn into_snapshot(self, response: &Value) -> CapturedSnapshot {
        if response.get("id").and_then(Value::as_str) != Some(self.profile_id.as_str())
            || response.get("namespace").and_then(Value::as_str) != Some(self.namespace.as_str())
        {
            return CapturedSnapshot::Unavailable;
        }
        match self.publication {
            Publication::Decoded { raw, snapshot }
                if response.get("state_snapshot") == Some(&raw) =>
            {
                CapturedSnapshot::Decoded(snapshot)
            }
            _ => CapturedSnapshot::Unavailable,
        }
    }
}

tokio::task_local! {
    static CURRENT: RefCell<Option<ProfileSnapshotCapture>>;
}

pub fn capture_profile_snapshot<F: Future>(
    profile_id: &str,
    namespace: &str,
    future: F,
) -> impl Future<Output = (F::Output, ProfileSnapshotCapture)> {
    // Box before returning, so enclosing dispatch futures do not retain F inline.
    let future = Box::pin(future);
    let capture = ProfileSnapshotCapture {
        profile_id: profile_id.to_owned(),
        namespace: namespace.to_owned(),
        publication: Publication::Absent,
    };
    async move {
        CURRENT
            .scope(RefCell::new(Some(capture)), async move {
                let output = future.await;
                let capture = CURRENT.with(|slot| slot.borrow_mut().take().unwrap());
                (output, capture)
            })
            .await
    }
}

/// Publish an already decoded snapshot. Outside a capture this does not clone JSON.
pub fn publish_profile_snapshot(
    profile_id: &str,
    raw: &Value,
    snapshot: Option<BalancedRecallSnapshot>,
) {
    let _ = CURRENT.try_with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(capture) = slot.as_mut().filter(|c| c.profile_id == profile_id) else {
            return;
        };
        capture.publication = match capture.publication {
            Publication::Absent => Publication::Decoded {
                raw: raw.clone(),
                snapshot,
            },
            // A delegated provider may publish more than once and rewrite its result.
            // Fall back rather than guessing which publication supplied the response.
            _ => Publication::Ambiguous,
        };
    });
}

#[cfg(any(test, feature = "test-support"))]
tokio::task_local! {
    static DECODE_CALLS: std::cell::Cell<usize>;
}

/// Observe full conversion attempts during actual dispatch, without global counters.
#[cfg(any(test, feature = "test-support"))]
pub async fn count_profile_snapshot_decodes<F: Future>(future: F) -> (F::Output, usize) {
    DECODE_CALLS
        .scope(std::cell::Cell::new(0), async move {
            let output = future.await;
            (output, DECODE_CALLS.with(std::cell::Cell::get))
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BalancedRecallState, BetaPosterior};
    use serde_json::json;

    fn snapshot() -> BalancedRecallSnapshot {
        let mut snapshot = BalancedRecallState::new(10).to_snapshot();
        let id = uuid::Uuid::from_u128(42);
        snapshot
            .entity_posteriors
            .insert(id, BetaPosterior::new(9.0, 1.0));
        snapshot.entity_posterior_order = vec![id];
        snapshot.total_events = 17;
        snapshot.exploration_epoch = 3;
        snapshot
    }

    fn response(namespace: &str, raw: Value) -> Value {
        json!({ "id": "profile", "namespace": namespace, "state_snapshot": raw })
    }

    #[tokio::test]
    async fn capture_preserves_full_state_and_binds_the_final_response() {
        let snapshot = snapshot();
        let raw = serde_json::to_value(&snapshot).unwrap();
        for field in [None, Some("id"), Some("namespace"), Some("state_snapshot")] {
            let (_, capture) = capture_profile_snapshot("profile", "local", async {
                publish_profile_snapshot("profile", &raw, Some(snapshot.clone()));
            })
            .await;
            let mut response = response("local", raw.clone());
            if let Some(field) = field {
                response[field] = json!("changed");
                assert!(matches!(
                    capture.into_snapshot(&response),
                    CapturedSnapshot::Unavailable
                ));
            } else {
                let CapturedSnapshot::Decoded(Some(actual)) = capture.into_snapshot(&response)
                else {
                    panic!("matching successful decode must be reusable");
                };
                assert_eq!(actual, snapshot);
            }
        }
    }

    #[tokio::test]
    async fn invalid_is_distinct_from_absent_and_multiple_publications_are_ambiguous() {
        for publications in 0..=2 {
            let (_, capture) = capture_profile_snapshot("profile", "local", async {
                publish_profile_snapshot("another-profile", &Value::Null, None);
                for _ in 0..publications {
                    publish_profile_snapshot("profile", &json!({ "malformed": true }), None);
                }
            })
            .await;
            let captured = capture.into_snapshot(&response("local", json!({ "malformed": true })));
            assert_eq!(
                matches!(captured, CapturedSnapshot::Decoded(None)),
                publications == 1
            );
        }
    }

    async fn scoped(namespace: &str) -> CapturedSnapshot {
        let (_, capture) = capture_profile_snapshot("profile", namespace, async {
            tokio::task::yield_now().await;
            publish_profile_snapshot("profile", &Value::Null, None);
            tokio::task::yield_now().await;
        })
        .await;
        capture.into_snapshot(&response(namespace, Value::Null))
    }

    #[tokio::test]
    async fn nested_concurrent_and_spawned_captures_do_not_share_publications() {
        let (_, outer) = capture_profile_snapshot("profile", "outer", async {
            let (left, right) = tokio::join!(scoped("left"), scoped("right"));
            assert!(matches!(left, CapturedSnapshot::Decoded(None)));
            assert!(matches!(right, CapturedSnapshot::Decoded(None)));
            tokio::spawn(async { publish_profile_snapshot("profile", &Value::Null, None) })
                .await
                .unwrap();
            publish_profile_snapshot("profile", &Value::Null, None);
        })
        .await;
        assert!(matches!(
            outer.into_snapshot(&response("outer", Value::Null)),
            CapturedSnapshot::Decoded(None)
        ));
        assert!(CURRENT.try_with(|_| ()).is_err());
    }

    #[tokio::test]
    async fn cancellation_restores_the_parent_and_capture_boxes_large_children() {
        let (_, outer) = capture_profile_snapshot("profile", "outer", async {
            let mut child = Box::pin(capture_profile_snapshot("profile", "inner", async {
                publish_profile_snapshot("profile", &Value::Null, None);
                std::future::pending::<()>().await;
            }));
            std::future::poll_fn(|cx| {
                assert!(child.as_mut().poll(cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            drop(child);
            publish_profile_snapshot("profile", &Value::Null, None);
        })
        .await;
        assert!(matches!(
            outer.into_snapshot(&response("outer", Value::Null)),
            CapturedSnapshot::Decoded(None)
        ));

        let bytes = std::hint::black_box([7_u8; 32 * 1024]);
        let future = capture_profile_snapshot("profile", "local", async move {
            tokio::task::yield_now().await;
            std::hint::black_box(bytes)[0]
        });
        assert!(std::mem::size_of_val(&future) < 4096);
        assert_eq!(future.await.0, 7);
    }
}
