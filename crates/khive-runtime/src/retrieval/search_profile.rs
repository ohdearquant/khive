//! Opt-in wall-clock timings for the runtime's hybrid retrieval stages.
//!
//! Text and vector arms overlap; their inclusive durations must not be added
//! as if they were sequential. Coordinator and runtime spans share call IDs.
//! No request contents are included in events. Synchronous stderr writes
//! can perturb timings while profiling is enabled. Runtime spans are active only
//! while a caller has explicitly opened a shared profiling scope.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

static NEXT_CALL: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy)]
struct Call {
    id: u64,
    iteration: Option<u32>,
}

tokio::task_local! {
    static CURRENT: Call;
}

pub(super) async fn scope_if<F: Future>(enabled: bool, future: F) -> F::Output {
    if CURRENT.try_with(|_| ()).is_ok() || !enabled {
        future.await
    } else {
        let call = Call {
            id: NEXT_CALL.fetch_add(1, Ordering::Relaxed),
            iteration: None,
        };
        CURRENT.scope(call, future).await
    }
}

pub(super) fn call_id() -> Option<u64> {
    CURRENT.try_with(|call| call.id).ok()
}

/// Capture the current call before a future moves to a spawned task.
pub(super) fn inherit<F: Future + Send>(future: F) -> impl Future<Output = F::Output> + Send {
    let call = CURRENT.try_with(|call| *call).ok();
    async move {
        match call {
            Some(call) => CURRENT.scope(call, future).await,
            None => future.await,
        }
    }
}

/// Identify a per-kind arm without exposing its caller-supplied kind.
/// The ordinal is zero-based and saturates at `u32::MAX`.
pub(super) async fn iteration<F: Future>(index: usize, future: F) -> F::Output {
    if let Ok(mut call) = CURRENT.try_with(|call| *call) {
        call.iteration = Some(u32::try_from(index).unwrap_or(u32::MAX));
        CURRENT.scope(call, future).await
    } else {
        future.await
    }
}

#[derive(Clone, Copy)]
pub(super) enum Stage {
    Text,
    Vector,
    ModelResolution,
    ModelMetadata,
    Embedding,
    VectorStore,
    Fusion,
    Hydration,
}

impl Stage {
    fn label(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Vector => "vector_total",
            Self::ModelResolution => "model_resolution",
            Self::ModelMetadata => "model_metadata",
            Self::Embedding => "embedding",
            Self::VectorStore => "vector_store",
            Self::Fusion => "fusion",
            Self::Hydration => "hydration_and_filter",
        }
    }
}

#[cfg(test)]
std::thread_local! {
    static CLOCK_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

// The only profiler clock entry; the test counter measures the real span path.
fn now() -> Instant {
    #[cfg(test)]
    CLOCK_READS.with(|reads| reads.set(reads.get() + 1));
    Instant::now()
}

pub(super) struct Span {
    call: Call,
    stage: Stage,
    started: Instant,
    completed: bool,
}

pub(super) fn span(stage: Stage) -> Option<Span> {
    CURRENT.try_with(|call| *call).ok().map(|call| Span {
        call,
        stage,
        started: now(),
        completed: false,
    })
}

/// Mark normal completion; early errors and cancellation remain unfinished.
pub(super) fn complete(span: &mut Option<Span>) {
    if let Some(span) = span {
        span.completed = true;
    }
}

impl Span {
    fn outcome(&self) -> &'static str {
        if self.completed {
            "ok"
        } else {
            "unfinished"
        }
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        use std::io::Write;

        let micros =
            u64::try_from(now().duration_since(self.started).as_micros()).unwrap_or(u64::MAX);
        let mut stderr = std::io::stderr().lock();
        if let Some(index) = self.call.iteration {
            let _ = writeln!(stderr,
                "{{\"layer\":\"runtime\",\"c\":{},\"i\":{},\"s\":\"{}\",\"us\":{},\"clock\":\"inclusive_wall\",\"outcome\":\"{}\"}}",
                self.call.id, index, self.stage.label(), micros, self.outcome());
        } else {
            let _ = writeln!(stderr,
                "{{\"layer\":\"runtime\",\"c\":{},\"s\":\"{}\",\"us\":{},\"clock\":\"inclusive_wall\",\"outcome\":\"{}\"}}",
                self.call.id, self.stage.label(), micros, self.outcome());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn disabled_scope_never_reads_the_profile_clock() {
        CLOCK_READS.with(|reads| reads.set(0));
        let result = scope_if(false, async {
            let mut stage = span(Stage::Text);
            assert!(stage.is_none());
            complete(&mut stage);
            drop(stage);
            42
        })
        .await;
        assert_eq!(result, 42);
        CLOCK_READS.with(|reads| assert_eq!(reads.get(), 0));
    }

    #[tokio::test]
    async fn enabled_scope_reads_clock_and_preserves_result() {
        CLOCK_READS.with(|reads| reads.set(0));
        let result = scope_if(true, async {
            let mut stage = span(Stage::Text);
            assert_eq!(stage.as_ref().unwrap().outcome(), "unfinished");
            complete(&mut stage);
            assert_eq!(stage.as_ref().unwrap().outcome(), "ok");
            drop(stage);
            42
        })
        .await;
        assert_eq!(result, 42);
        CLOCK_READS.with(|reads| assert_eq!(reads.get(), 2));
    }

    #[tokio::test]
    async fn nested_scopes_reuse_a_call_and_independent_calls_do_not() {
        let (outer, inner) = scope_if(true, async {
            let outer = CURRENT.with(|call| call.id);
            let inner = iteration(7, async {
                scope_if(true, async {
                    CURRENT.with(|call| (call.id, call.iteration))
                })
                .await
            })
            .await;
            (outer, inner)
        })
        .await;
        assert_eq!(inner, (outer, Some(7)));
        let next = scope_if(true, async { CURRENT.with(|call| call.id) }).await;
        assert_ne!(outer, next);
    }

    #[tokio::test]
    async fn profiling_does_not_change_returned_errors() {
        async fn result(with_profile: bool) -> Result<(), &'static str> {
            scope_if(with_profile, async {
                let stage = span(Stage::Text);
                if let Some(stage) = &stage {
                    assert_eq!(stage.outcome(), "unfinished");
                }
                Err("synthetic")
            })
            .await
        }
        assert_eq!(result(false).await, result(true).await);
    }
}
