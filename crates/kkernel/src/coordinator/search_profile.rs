//! Opt-in stage timing for coordinated search (`KHIVE_SEARCH_PROFILE`).
//!
//! Coordinated search runs when a deployment configures more than one backend.
//! When the variable is set, each such search writes one JSON object per stage
//! to stderr: `{"c":<call>,"s":<stage>,"us":<elapsed microseconds>, ...}`.
//! A call id groups the lines of one search; lines written outside a profiled
//! search carry call 0.
//!
//! With the switch off no line is written and no stage timer reads the clock:
//! every timer comes from [`start`], which is `None` unless profiling is on.
//!
//! Stages:
//! - `backend`: one per backend that served the search, with its name (`b`),
//!   hit count (`n`) and outcome (`o`: `ok`, `error` or `timeout`). On a
//!   fan-out it runs from fan-out start to that backend's completion (or to the
//!   coordinator giving up on it). When one backend serves the searched
//!   substrate there is no fan-out, and its line carries the `fanout` time.
//! - `merge`: rank fusion across backends (multi-backend only).
//! - `fanout`: candidate collection as a whole, with the backend count.
//! - `finalize`: score floor and ordering of the merged hits.
//! - `hydrate`: record reads that attach kind, name and timestamps to hits.
//! - `search`: the whole coordinated search.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use khive_runtime::{NoteSearchHit, SearchHit};

use super::dispatch::{BackendSearchFailureKind, BackendSearchResult};

static CALL_ID: AtomicU64 = AtomicU64::new(0);

tokio::task_local! {
    static CALL: u64;
}

#[cfg(test)]
tokio::task_local! {
    static CAPTURE: std::sync::Arc<std::sync::Mutex<Vec<String>>>;
}

/// Whether `KHIVE_SEARCH_PROFILE` is set, read once and recorded in the config ledger.
pub(super) fn enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        let enabled = std::env::var("KHIVE_SEARCH_PROFILE").is_ok();
        khive_runtime::config_ledger::record_config_locked(
            "KHIVE_SEARCH_PROFILE",
            enabled.to_string(),
        );
        enabled
    })
}

/// Run one coordinated search as a profiled call: a fresh call id for its stage
/// lines and a closing `search` line. A search nested inside a profiled call
/// keeps the outer id and writes no second `search` line.
pub(super) async fn profiled<F: std::future::Future>(search: F) -> F::Output {
    scoped(on(), search).await
}

async fn scoped<F: std::future::Future>(on: bool, search: F) -> F::Output {
    if !on || CALL.try_with(|_| ()).is_ok() {
        return search.await;
    }
    let call = CALL_ID.fetch_add(1, Ordering::Relaxed) + 1;
    let started = tokio::time::Instant::now();
    let output = CALL.scope(call, search).await;
    emit(&line(call, "search", started.elapsed(), &[]));
    output
}

/// Start a stage timer: the current instant while profiling is on, else `None`
/// without reading the clock.
pub(super) fn start() -> Option<tokio::time::Instant> {
    on().then(tokio::time::Instant::now)
}

/// Write one stage line for the current call, timed from `started`; nothing
/// when the timer was not started.
pub(super) fn stage(
    stage: &str,
    started: Option<tokio::time::Instant>,
    fields: &[(&str, serde_json::Value)],
) {
    if let Some(started) = started {
        write(stage, started.elapsed(), fields);
    }
}

/// Write the `fanout` line: candidate collection across every backend. When
/// one backend served the search there was no fan-out, so its `backend` line
/// is written here with the same time.
pub(super) fn fanout(
    started: Option<tokio::time::Instant>,
    entity_hits: &[SearchHit],
    note_hits: &[NoteSearchHit],
    per_backend: &[BackendSearchResult],
) {
    let Some(started) = started else {
        return;
    };
    let elapsed = started.elapsed();
    if let [result] = per_backend {
        backend(result, elapsed);
    }
    write(
        "fanout",
        elapsed,
        &[
            ("backends", per_backend.len().into()),
            ("n", (entity_hits.len() + note_hits.len()).into()),
        ],
    );
}

/// Write the `backend` line for one backend result. Callers hold a started
/// timer, so profiling is on.
pub(super) fn backend(result: &BackendSearchResult, elapsed: Duration) {
    let outcome = match result.error.as_ref().map(|error| error.kind) {
        None => "ok",
        Some(BackendSearchFailureKind::BackendError) => "error",
        Some(BackendSearchFailureKind::Timeout) => "timeout",
    };
    write(
        "backend",
        elapsed,
        &[
            ("b", result.backend_id.as_str().into()),
            ("n", (result.hits.len() + result.note_hits.len()).into()),
            ("o", outcome.into()),
        ],
    );
}

fn write(stage: &str, elapsed: Duration, fields: &[(&str, serde_json::Value)]) {
    let call = CALL.try_with(|call| *call).unwrap_or(0);
    emit(&line(call, stage, elapsed, fields));
}

fn line(call: u64, stage: &str, elapsed: Duration, fields: &[(&str, serde_json::Value)]) -> String {
    let mut line = serde_json::Map::new();
    line.insert("c".into(), call.into());
    line.insert("s".into(), stage.into());
    line.insert(
        "us".into(),
        u64::try_from(elapsed.as_micros())
            .unwrap_or(u64::MAX)
            .into(),
    );
    for (key, value) in fields {
        line.insert((*key).to_string(), value.clone());
    }
    serde_json::Value::Object(line).to_string()
}

/// Profiling is on: the switch is set, or a test is capturing lines.
fn on() -> bool {
    #[cfg(test)]
    {
        if CAPTURE.try_with(|_| ()).is_ok() {
            return true;
        }
    }
    enabled()
}

fn emit(line: &str) {
    #[cfg(test)]
    {
        let captured = CAPTURE.try_with(|lines| {
            lines
                .lock()
                .expect("profile capture lock")
                .push(line.to_string())
        });
        if captured.is_ok() {
            return;
        }
    }
    eprintln!("{line}");
}

/// Test-only: run `fut` with profiling on, collecting its lines instead of writing them.
#[cfg(test)]
pub(super) async fn capture<F: std::future::Future>(fut: F) -> (F::Output, Vec<String>) {
    let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let output = CAPTURE.scope(std::sync::Arc::clone(&lines), fut).await;
    let lines = lines.lock().expect("profile capture lock").clone();
    (output, lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_backend_name_with_quotes_and_backslashes_stays_one_json_object() {
        let name = "main\"x\\y\n";
        let text = line(
            7,
            "backend",
            Duration::from_micros(1_234),
            &[("b", name.into()), ("n", 3.into()), ("o", "ok".into())],
        );
        assert!(!text.contains('\n'), "{text}");
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("one JSON object");
        assert_eq!(parsed["c"], 7);
        assert_eq!(parsed["s"], "backend");
        assert_eq!(parsed["us"], 1_234);
        assert_eq!(parsed["b"], name);
        assert_eq!(parsed["n"], 3);
    }

    fn current() -> Option<u64> {
        CALL.try_with(|call| *call).ok()
    }

    // `enabled` records the switch in the config ledger the first time it runs.
    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn a_stage_timer_starts_only_while_profiling_is_on() {
        assert!(
            !enabled(),
            "KHIVE_SEARCH_PROFILE is set in the test environment"
        );
        assert!(start().is_none());
        let (started, lines) = capture(async {
            let started = start();
            stage("merge", started, &[]);
            stage("finalize", None, &[]);
            started
        })
        .await;
        assert!(started.is_some());
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("\"s\":\"merge\""), "{lines:?}");
    }

    #[tokio::test]
    async fn each_profiled_call_gets_its_own_id_and_a_nested_call_keeps_the_outer_one() {
        let first = scoped(true, async { current() }).await;
        let second = scoped(true, async { current() }).await;
        assert!(first.is_some_and(|id| id > 0), "{first:?}");
        assert!(second.is_some() && second != first, "{first:?} {second:?}");
        let nested = CALL.scope(41, scoped(true, async { current() })).await;
        assert_eq!(nested, Some(41));
        assert_eq!(current(), None);
        // switched off, a search runs outside any call
        assert_eq!(scoped(false, async { current() }).await, None);
    }
}
