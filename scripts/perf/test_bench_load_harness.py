"""Deterministic arrival, accounting, lifecycle and legacy-report fixtures."""

import contextlib
import io
import json
import unittest
import threading
from concurrent.futures import ThreadPoolExecutor
from unittest.mock import Mock, patch

from scripts.perf import bench_load_harness as harness


class VirtualClock:
    def __init__(self):
        self.now = 0.0

    def __call__(self):
        return self.now

    def sleep(self, delay):
        self.now += delay


class SlowFuture:
    def __init__(self, clock, ready, fn, args):
        self.clock, self.ready, self.fn, self.args = clock, ready, fn, args
        self.value = None

    def done(self):
        return self.clock() >= self.ready

    def result(self, timeout=None):
        # A blocking wait consumes virtual service time, making the closed-loop
        # mutation observable without threads or wall-clock timing assertions.
        self.clock.now = max(self.clock.now, self.ready)
        if self.value is None:
            self.value = self.fn(*self.args)
        return self.value


class SlowServicePool:
    def __init__(self, clock, delays):
        self.clock, self.delays = clock, iter(delays)
        self.submitted = []

    def submit(self, fn, *args):
        future = SlowFuture(self.clock, self.clock() + next(self.delays), fn, args)
        self.submitted.append(future)
        return future


class ImmediateFuture:
    def __init__(self, fn, args):
        try:
            self.value, self.error = fn(*args), None
        except BaseException as exc:
            self.value, self.error = None, exc

    def result(self, timeout=None):
        if self.error is not None:
            raise self.error
        return self.value


class ImmediatePool:
    def __init__(self, **kwargs):
        self.shutdowns = []

    def __enter__(self):
        return self

    def __exit__(self, *args):
        return False

    def submit(self, fn, *args):
        return ImmediateFuture(fn, args)

    def shutdown(self, **kwargs):
        self.shutdowns.append(kwargs)


class OpenLoopArrivalTests(unittest.TestCase):
    def drive(self, delays, operations, workers=1, drain_timeout=1.0, failures=()):
        clock = VirtualClock()
        pool = SlowServicePool(clock, delays)
        connections = [(None, harness.WorkerResult(0, idx)) for idx in range(workers)]

        def service(proc, tenant, sequence):
            return harness.OpOutcome("memory.recall", sequence not in failures, 40,
                                     "fake service failure" if sequence in failures else None)

        with patch.dict(harness._OP_NAMES, {service: "memory.recall"}):
            report = harness._drive_open_loop(
                connections, pool, 10.0, operations, drain_timeout,
                pick_op=lambda: service, clock=clock, sleep=clock.sleep,
            )
        return report

    def test_slow_completions_do_not_move_arrivals(self):
        report = self.drive([0.4] * 4, 4)
        records = report["records"]
        self.assertEqual([round(row["scheduled_s"], 6) for row in records], [0.0, 0.1, 0.2, 0.3])
        self.assertEqual([round(row["offered_s"], 6) for row in records], [0.0, 0.1, 0.2, 0.3])
        self.assertEqual([row["state"] for row in records], ["completed", "refused", "refused", "refused"])
        self.assertEqual(report["counts"], {"completed": 1, "failed": 0, "refused": 3, "outstanding": 0, "submitted": 4})

    def test_terminal_partition_and_per_class_rates_include_refusal_and_outstanding(self):
        report = self.drive([0.05, 2.0, 0.05, 2.0], 5, workers=2, drain_timeout=0.0, failures=(2,))
        counts = report["counts"]
        self.assertEqual(counts, {"completed": 1, "failed": 1, "refused": 1, "outstanding": 2, "submitted": 5})
        self.assertEqual(counts["submitted"], sum(counts[key] for key in ("completed", "failed", "refused", "outstanding")))
        self.assertEqual(report["max_in_flight"], 2)
        self.assertEqual(report["status"], "drain_timeout")
        recall = report["classes"]["memory.recall"]
        self.assertEqual(recall["offered_rate_per_s"], 10.0)
        self.assertEqual(recall["achieved_rate_per_s"], 2.0)
        self.assertEqual(set(report["classes"]), {"memory.recall", "knowledge.search", "knowledge.compose", "memory.remember", "create"})
        self.assertEqual(report["classes"]["create"]["submitted"], 0)
        self.assertEqual(report["warmup_boundary"]["monotonic_s"], 0.0)
        self.assertEqual(report["warmup_boundary"]["discarded_arrivals"], 0)

    def test_zero_arrivals_and_invalid_rate_do_not_dispatch(self):
        report = self.drive([], 0)
        self.assertEqual(report["records"], [])
        self.assertEqual(report["counts"], {"completed": 0, "failed": 0, "refused": 0, "outstanding": 0, "submitted": 0})
        self.assertEqual(report["classes"]["memory.remember"]["offered_rate_per_s"], 0.0)
        for rate in (0.0, -1.0, float("inf"), float("nan")):
            with self.subTest(rate=rate), self.assertRaisesRegex(ValueError, "arrival rate"):
                harness._drive_open_loop([], None, rate, 1, 0)


class OpenLoopLifecycleTests(unittest.TestCase):
    def test_worker_handshakes_and_attribution_precede_arrival_zero(self):
        events, registry = [], {}
        pool = ImmediatePool()
        processes = [Mock(), Mock()]
        for proc in processes:
            proc.poll.return_value = None

        def handshake(proc):
            events.append("handshake")

        def attribution(proc, tenant):
            events.append("attribution")
            return {"status": "consistent", "attributed_actor": "tenant_0"}

        def drive(workers, selected_pool, rate, operations, timeout):
            self.assertEqual(events, ["handshake", "attribution", "handshake"])
            self.assertIs(selected_pool, pool)
            self.assertEqual(len(workers), 2)
            events.append("arrivals")
            return {"counts": {"outstanding": 0}}

        with patch.object(harness, "ThreadPoolExecutor", return_value=pool), \
                patch.object(harness, "_spawn_worker_proc", side_effect=processes), \
                patch.object(harness.bpd, "_handshake", side_effect=handshake), \
                patch.object(harness, "_attribution_probe", side_effect=attribution), \
                patch.object(harness, "_drive_open_loop", side_effect=drive):
            results, _ = harness._run_open_workers("fake", [(0, 0), (0, 1)], {}, "warn", "/fake", registry, 4, 10.0, 1.0)
        self.assertEqual(events[-1], "arrivals")
        self.assertEqual(len(results), 2)
        for proc in processes:
            proc.kill.assert_called_once()
            proc.wait.assert_called_once_with(timeout=5)
        self.assertEqual(pool.shutdowns, [{"wait": False, "cancel_futures": True}, {"wait": True, "cancel_futures": True}])

    def test_setup_failure_kills_registered_process_before_pool_join(self):
        failed, blocked = Mock(), Mock()
        failed.poll.return_value = 0
        blocked.poll.return_value = None
        entered, release = threading.Event(), threading.Event()
        order, registry = [], {}
        pool = ThreadPoolExecutor(max_workers=2)
        shutdown = pool.shutdown

        def handshake(proc):
            if proc is blocked:
                entered.set()
                if not release.wait(5):
                    raise AssertionError("blocked handshake cleanup watchdog expired")
            else:
                if not entered.wait(5):
                    raise AssertionError("sibling handshake never started")
                raise RuntimeError("fake handshake failed")

        def kill_blocked():
            order.append("kill")
            blocked.poll.return_value = -9
            release.set()

        def join_pool(**kwargs):
            if kwargs["wait"]:
                order.append("join")
                # Release the fixture even when the kill loop is removed.
                release.set()
            shutdown(**kwargs)

        blocked.kill.side_effect = kill_blocked
        try:
            with patch.object(harness, "ThreadPoolExecutor", return_value=pool), \
                    patch.object(pool, "shutdown", side_effect=join_pool), \
                    patch.object(harness, "_spawn_worker_proc", side_effect=lambda *args: failed if args[-1].endswith("w0.stderr.log") else blocked), \
                    patch.object(harness.bpd, "_handshake", side_effect=handshake), \
                    patch.object(harness, "_drive_open_loop") as drive:
                with self.assertRaisesRegex(RuntimeError, "fake handshake failed"):
                    harness._run_open_workers("fake", [(0, 0), (0, 1)], {}, "warn", "/fake", registry, 4, 10.0, 1.0)
        finally:
            release.set()
            shutdown(wait=True, cancel_futures=True)
        drive.assert_not_called()
        self.assertTrue(entered.is_set())
        blocked.kill.assert_called_once()
        self.assertEqual(order, ["kill", "join"])
        self.assertEqual(registry, {(0, 0): failed, (0, 1): blocked})


# Fixed synthetic values, with the complete legacy report shape and field order.
# This is a serializer compatibility fixture, not a measured benchmark result.
CLOSED_LOOP_GOLDEN = {
    "meta": {
        "mode": "bench", "workers": 1, "tenants": 1, "workers_per_tenant": 1,
        "ops_per_worker": 1, "git_sha": "fixture-sha", "started_at": "fixture-time",
        "db_path": "/fixture/loadharness.db",
        "run_posture": "KHIVE_DAEMON_STRICT=1 KHIVE_WRITE_QUEUE=1", "finished_at": "fixture-time",
    },
    "smoke_result": "PASS", "smoke_errors": [],
    "oracle_probe_t0": {"oracle": "PENDING", "detail": "fixture"},
    "oracle_probe_post_load": {"oracle": "PENDING", "detail": "fixture"},
    "op_counts": {"memory.recall": 1}, "op_error_counts": {"memory.recall": 0},
    "dimensions": {
        "1_fallback": {
            "channel": "worker-stderr-scrape", "daemon_fallback_lines": 0,
            "reason_breakdown": {"config_mismatch": 0, "namespace_mismatch": 0, "other": 0},
            "note": "grep for literal 'daemon_fallback' event across every worker front-end's stderr; STRICT=1 elevates config/namespace-mismatch fallbacks to error-level but the substring is the same either way",
        },
        "2_recall_latency": {
            "channel": "client-measured", "n": 1, "p50_us": 40, "p95_us": 40, "p99_us": 40,
            "note": "meaningful cold-spike read requires --mode real; bench-embedder has no cold init to spike",
        },
        "3_embed_cold_start": {
            "channel": "daemon-log-scrape", "status": "not-implemented-this-round",
            "note": "no confirmed embedder-init log-event text found in this worktree during recon; only indirect corroboration available via dim-2's latency shape (no direct probe here)",
        },
        "4_wal_floor": {
            "channel": "daemon-frame (oracle)", "status": "PENDING",
            "t0": {"oracle": "PENDING", "detail": "fixture"},
            "post_load": {"oracle": "PENDING", "detail": "fixture"},
        },
        "5_wal_pin": {"channel": "daemon-frame (oracle)", "status": "PENDING"},
        "6_write_backpressure": {
            "channel": "client-op-results", "sqlite_busy_or_locked_count": 0,
            "note": "should be 0 under KHIVE_WRITE_QUEUE=1; a nonzero count means the write-queue is not absorbing write-write contention",
        },
        "7_backpressure_surfaced": {
            "channel": "client-op-results + daemon-frame (oracle)",
            "write_queue_full_typed_errors": 0, "oracle_status": "PENDING",
        },
        "8_attribution": {
            "channel": "client-readback", "checked": 0, "consistent_write_then_read": 0,
            "inconsistent_write_then_read": 0, "errored": 0, "distinct_attributed_actors": 0,
            "expected_distinct_actors": 1,
            "note": "does NOT assert a specific actor-string convention (see docstring on _attribution_probe for a real finding: the KHIVE_ACTOR-per-session pinning this harness's spec recommends is superseded by ADR-096's explicit-namespace fill rule in this worktree, so the attributed actor equals the namespace string, not '<namespace>_actor'). This checks write-then-read consistency and that distinct tenants get distinct attributed identities (no cross-tenant collapse).",
            "detail": [],
        },
        "9_knowledge_latency": {
            "channel": "client-measured",
            "compose": {"n": 0, "p50_us": 0.0, "p95_us": 0.0, "p99_us": 0.0},
            "search": {"n": 0, "p50_us": 0.0, "p95_us": 0.0, "p99_us": 0.0},
            "note": "renamed from 9_brain_slot_throughput, which mislabeled this block: it aggregated knowledge.compose and knowledge.search client latencies into one percentile set and measured nothing brain-slot-related",
        },
    },
}


class ClosedLoopCompatibilityTests(unittest.TestCase):
    def test_mode_off_report_matches_frozen_closed_loop_json_bytes(self):
        output = io.StringIO()
        process = Mock()
        result = harness.WorkerResult(0, 0)
        result.outcomes = [harness.OpOutcome("memory.recall", True, 40)]
        argv = ["bench_load_harness.py", "--mode", "bench", "--workers", "1",
                "--tenants", "1", "--ops-per-worker", "1"]
        with contextlib.ExitStack() as stack:
            stack.enter_context(contextlib.redirect_stdout(output))
            patches = [
                patch.object(harness.sys, "argv", argv),
                patch.object(harness, "_resolve_binary", return_value="fixture-binary"),
                patch.object(harness.tempfile, "mkdtemp", return_value="/fixture"),
                patch.object(harness.pathlib.Path, "write_text", return_value=0),
                patch.object(harness.pathlib.Path, "exists", return_value=True),
                patch.object(harness, "_assert_not_live_db"),
                patch.object(harness.subprocess, "Popen", return_value=process),
                patch.object(harness.bpd, "_handshake"),
                patch.object(harness.bpd, "_call_verb"),
                patch.object(harness.bpd, "assert_daemon_engaged"),
                patch.object(harness.bpd, "_teardown_daemon"),
                patch.object(harness.bpd, "_git_sha", return_value="fixture-sha"),
                patch.object(harness.bpd, "_iso_now", return_value="fixture-time"),
                patch.object(harness, "probe_oracle_channel", return_value={"oracle": "PENDING", "detail": "fixture"}),
                patch.object(harness, "_run_worker", return_value=result),
                patch.object(harness, "ThreadPoolExecutor", ImmediatePool),
            ]
            for selected in patches:
                stack.enter_context(selected)
            self.assertEqual(harness.main(), 0)
        text = output.getvalue()
        actual_json = text[text.index("{\n"):text.rindex("}") + 1]
        self.assertEqual(actual_json, json.dumps(CLOSED_LOOP_GOLDEN, indent=2, default=str))


if __name__ == "__main__":
    unittest.main()
