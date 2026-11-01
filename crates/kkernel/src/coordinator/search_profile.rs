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
//! - `backend_deadline`: the selected phase snapshot for a timed-out or late
//!   backend, with the same call/backend identity. `snapshot_origin` distinguishes
//!   a deadline capture from a fallback observation. Its `phase_elapsed_ms` is
//!   an inclusive phase duration, not backend `us` or native execution time.
//! - `merge`: rank fusion across backends (multi-backend only).
//! - `fanout`: candidate collection as a whole, with the backend count.
//! - `finalize`: score floor and ordering of the merged hits.
//! - `hydrate`: record reads that attach kind, name and timestamps to hits.
//! - `search`: the whole coordinated search.

use std::time::Duration;

use khive_runtime::{KhiveRuntime, NoteSearchHit, SearchHit};

use super::dispatch::{BackendSearchFailureKind, BackendSearchResult};

#[cfg(test)]
tokio::task_local! {
    static CAPTURE: std::sync::Arc<std::sync::Mutex<Vec<String>>>;
}

/// Whether `KHIVE_SEARCH_PROFILE` is set, read once and reported through tracing.
pub(super) fn enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        let enabled = std::env::var("KHIVE_SEARCH_PROFILE").is_ok();
        tracing::info!(
            target: "khive::config_lock",
            name = "KHIVE_SEARCH_PROFILE",
            value = enabled,
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
    if !on || KhiveRuntime::search_profile_call_id().is_some() {
        return search.await;
    }
    KhiveRuntime::search_profile_scope(true, async {
        let call = KhiveRuntime::search_profile_call_id().expect("profile scope has a call id");
        let started = tokio::time::Instant::now();
        let output = search.await;
        emit(&line(call, "search", started.elapsed(), &[]));
        output
    })
    .await
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

/// Export the snapshot already selected by the coordinator, on its parent task
/// so the existing call scope is retained. No field construction occurs when off.
pub(super) fn backend_deadline(
    backend: &str,
    observation: &str,
    snapshot_origin: &str,
    snapshot: &khive_storage::request_context::SearchPhaseSnapshot,
) {
    emit_if_on(|| {
        let call = KhiveRuntime::search_profile_call_id().unwrap_or(0);
        record(
            call,
            "backend_deadline",
            None,
            &[
                ("b", backend.into()),
                ("o", "timeout".into()),
                ("observation", observation.into()),
                ("snapshot_origin", snapshot_origin.into()),
                ("in_flight_phase", snapshot.in_flight_phase.as_str().into()),
                ("phase_elapsed_ms", snapshot.phase_elapsed_ms.into()),
                ("active_phases", snapshot.active_phases.as_str().into()),
                (
                    "completed_phases",
                    snapshot.completed_phases.as_str().into(),
                ),
            ],
        )
    });
}

fn emit_if_on(build: impl FnOnce() -> String) {
    if on() {
        emit(&build());
    }
}

fn write(stage: &str, elapsed: Duration, fields: &[(&str, serde_json::Value)]) {
    let call = KhiveRuntime::search_profile_call_id().unwrap_or(0);
    emit(&line(call, stage, elapsed, fields));
}

fn line(call: u64, stage: &str, elapsed: Duration, fields: &[(&str, serde_json::Value)]) -> String {
    record(call, stage, Some(elapsed), fields)
}

fn record(
    call: u64,
    stage: &str,
    elapsed: Option<Duration>,
    fields: &[(&str, serde_json::Value)],
) -> String {
    let mut line = serde_json::Map::new();
    line.insert("c".into(), call.into());
    line.insert("s".into(), stage.into());
    if let Some(elapsed) = elapsed {
        line.insert(
            "us".into(),
            u64::try_from(elapsed.as_micros())
                .unwrap_or(u64::MAX)
                .into(),
        );
    }
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
        KhiveRuntime::search_profile_call_id()
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
        let (outer, nested) = KhiveRuntime::search_profile_scope(true, async {
            let outer = current();
            let nested = scoped(true, async { current() }).await;
            (outer, nested)
        })
        .await;
        assert!(outer.is_some_and(|id| id > 0), "{outer:?}");
        assert_eq!(nested, outer);
        assert_eq!(current(), None);
        // switched off, a search runs outside any call
        assert_eq!(scoped(false, async { current() }).await, None);
    }

    #[tokio::test]
    async fn spawned_worker_inherits_the_coordinator_call() {
        let (outer, worker) = scoped(true, async {
            let outer = current();
            let worker = tokio::spawn(KhiveRuntime::search_profile_inherit(async {
                KhiveRuntime::search_profile_call_id()
            }))
            .await
            .expect("worker completed");
            (outer, worker)
        })
        .await;
        assert!(outer.is_some_and(|id| id > 0));
        assert_eq!(worker, outer);
        let outside = tokio::spawn(KhiveRuntime::search_profile_inherit(async {
            KhiveRuntime::search_profile_call_id()
        }))
        .await
        .expect("unscoped worker completed");
        assert_eq!(outside, None);
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn deadline_profile_off_does_not_construct_a_record() {
        assert!(
            !enabled(),
            "KHIVE_SEARCH_PROFILE is set in the test environment"
        );
        emit_if_on(|| panic!("disabled profiling constructed a deadline record"));
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn deadline_profile_keeps_call_identity_and_snapshot_fields() {
        let snapshot = khive_storage::request_context::SearchPhaseSnapshot {
            in_flight_phase: "vector_embedding".into(),
            phase_elapsed_ms: 17,
            active_phases: "vector_search=21ms,vector_embedding=17ms".into(),
            completed_phases: "text_reader_admission=0ms".into(),
        };
        let name = "main\"x\\y\n";
        let ((), lines) = capture(profiled(async {
            backend_deadline(name, "timed_out", "deadline_capture", &snapshot);
            profiled(async {
                backend_deadline(
                    name,
                    "completed_after_deadline",
                    "fallback_observation",
                    &snapshot,
                );
            })
            .await;
        }))
        .await;
        let records: Vec<serde_json::Value> = lines
            .iter()
            .map(|line| {
                assert!(!line.contains('\n'), "{line}");
                serde_json::from_str(line).expect("profile JSON")
            })
            .collect();
        assert_eq!(records.len(), 3, "{records:?}");
        let call = records[2]["c"].as_u64().expect("search call id");
        assert!(call > 0);
        assert_eq!(records[2]["s"], "search");
        for record in &records[..2] {
            assert_eq!(record["c"], call);
            assert_eq!(record["s"], "backend_deadline");
            assert_eq!(record["b"], name);
            assert_eq!(record["o"], "timeout");
            assert_eq!(record["in_flight_phase"], snapshot.in_flight_phase);
            assert_eq!(record["phase_elapsed_ms"], 17);
            assert_eq!(record["active_phases"], snapshot.active_phases);
            assert_eq!(record["completed_phases"], snapshot.completed_phases);
            assert!(record.get("us").is_none());
        }
        assert_eq!(records[0]["observation"], "timed_out");
        assert_eq!(records[0]["snapshot_origin"], "deadline_capture");
        assert_eq!(records[1]["observation"], "completed_after_deadline");
        assert_eq!(records[1]["snapshot_origin"], "fallback_observation");
    }
}
