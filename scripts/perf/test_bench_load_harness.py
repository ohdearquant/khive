"""Deterministic arrival, accounting, lifecycle and legacy-report fixtures."""

import contextlib
import io
import json
import os
import tempfile
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
    def drive(self, delays, operations, workers=1, drain_timeout=1.0, failures=(), request_id_prefix=None):
        clock = VirtualClock()
        pool = SlowServicePool(clock, delays)
        connections = [(None, harness.WorkerResult(0, idx)) for idx in range(workers)]
        self.served_request_ids = []

        def service(proc, tenant, sequence, request_id=None):
            self.served_request_ids.append((sequence, request_id))
            return harness.OpOutcome("memory.recall", sequence not in failures, 40,
                                     "fake service failure" if sequence in failures else None)

        with patch.dict(harness._OP_NAMES, {service: "memory.recall"}):
            report = harness._drive_open_loop(
                connections, pool, 10.0, operations, drain_timeout,
                pick_op=lambda: service, clock=clock, sleep=clock.sleep,
                request_id_prefix=request_id_prefix,
            )
        return report

    def test_each_arrival_carries_its_run_unique_request_id_to_the_service(self):
        report = self.drive([0.01] * 3, 3, request_id_prefix=0xABC)
        expected = [(0xABC << 20) | sequence for sequence in range(3)]
        self.assertEqual([row["request_id"] for row in report["records"]], expected)
        self.assertEqual(self.served_request_ids, list(zip(range(3), expected)))
        self.assertEqual(report["request_id_prefix"], 0xABC)

    def test_refused_arrival_keeps_its_request_id_and_no_prefix_means_no_ids(self):
        refused = self.drive([0.4] * 2, 2, request_id_prefix=7)
        self.assertEqual([row["state"] for row in refused["records"]], ["completed", "refused"])
        self.assertEqual(refused["records"][1]["request_id"], (7 << 20) | 1)
        plain = self.drive([0.01], 1)
        self.assertIsNone(plain["records"][0]["request_id"])
        self.assertEqual(self.served_request_ids, [(0, None)])

    def test_request_id_sequence_must_fit_its_bits(self):
        self.assertEqual(harness.arrival_request_id(1, (1 << 20) - 1), (1 << 21) - 1)
        with self.assertRaisesRegex(ValueError, "does not fit"):
            harness.arrival_request_id(1, 1 << 20)
        with self.assertRaisesRegex(ValueError, "carry request ids"):
            harness._drive_open_loop([], None, 1.0, (1 << 20) + 1, 0, request_id_prefix=1)

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

        def drive(workers, selected_pool, rate, operations, timeout, request_id_prefix=None, pick_op=None, menu=None):
            self.assertIs(menu, harness._OP_MENU)
            self.assertEqual(len([pick_op() for _ in range(operations)]), operations)
            self.assertIsInstance(request_id_prefix, int)
            self.assertTrue(0 < request_id_prefix < (1 << 32))
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


class NamedMixTests(unittest.TestCase):
    def test_mix_names_its_classes_write_set_and_wall_clock_window(self):
        clock = VirtualClock()
        pool = SlowServicePool(clock, [0.01] * 4)
        connections = [(None, harness.WorkerResult(0, 0))]
        menu = harness.MIXES["f7"]
        plan = [harness._op_audit_append, harness._op_bulk_ingest, harness._op_audit_append, harness._op_comm_send]
        calls = []

        def served(fn):
            def service(proc, tenant, sequence, request_id=None):
                calls.append(harness._OP_NAMES[fn])
                return harness.OpOutcome(harness._OP_NAMES[fn], True, 40)
            return service

        fakes = {fn: served(fn) for fn, _ in menu}
        with patch.dict(harness._OP_NAMES, {fakes[fn]: harness._OP_NAMES[fn] for fn in fakes}):
            report = harness._drive_open_loop(
                connections, pool, 10.0, 4, 1.0, pick_op=iter([fakes[fn] for fn in plan]).__next__,
                clock=clock, sleep=clock.sleep, menu=menu, wall_clock=lambda: 1_763_424_000.25 + clock(),
            )
        self.assertEqual(calls, ["stream.append", "stream.batch", "stream.append", "comm.send"])
        self.assertEqual(sorted(report["classes"]),
                         ["comm.mark_read", "comm.send", "stream.append", "stream.batch", "update"])
        self.assertEqual(report["classes"]["stream.append"]["submitted"], 2)
        self.assertAlmostEqual(report["classes"]["update"]["configured_offered_rate_per_s"], 2.0)
        self.assertEqual(report["write_workload"]["classes"],
                         ["stream.append", "comm.mark_read", "update", "comm.send", "stream.batch"])
        self.assertEqual(report["write_workload"]["bulk_ingest_members"], harness.BULK_INGEST_MEMBERS)
        self.assertEqual(report["window_utc"]["start"], "2025-11-18T00:00:00.250Z")
        self.assertEqual(report["window_utc"]["end"], "2025-11-18T00:00:00.650Z")

    def test_plan_draws_one_op_per_arrival_from_the_menu(self):
        plan = harness.plan_arrivals(harness.MIXES["read-receipt"], 5)
        self.assertEqual(plan, [harness._op_read_receipt] * 5)
        self.assertEqual(harness.plan_arrivals(harness.MIXES["f7"], 0), [])

    def test_mix_without_arrival_rate_is_refused(self):
        argv = ["bench_load_harness.py", "--mix", "comm-send"]
        with patch.object(harness.sys, "argv", argv), contextlib.redirect_stderr(io.StringIO()) as err:
            with self.assertRaises(SystemExit):
                harness.main()
        self.assertIn("needs --arrival-rate", err.getvalue())


class FixtureTests(unittest.TestCase):
    def fake_server(self, inbox_ids, page=2):
        calls = []

        def call(proc, verb, args, request_id=None):
            calls.append((proc, verb, args))
            if verb == "create":
                return {"id": f"entity-{proc}"}
            if verb == "comm.send":
                return {"id": "outbound"}
            if verb == "comm.inbox":
                offset = args["offset"]
                chunk = inbox_ids[offset:offset + page]
                more = offset + page < len(inbox_ids)
                return {"messages": [{"id": i} for i in chunk], "has_more": more,
                        "next_offset": offset + page if more else None}
            if verb == "comm.mark_read":
                return {"marked_count": 1 if args["ids"][0] != "bad" else 0}
            raise AssertionError(verb)
        return calls, call

    def test_first_worker_fills_the_tenant_pool_and_siblings_share_it(self):
        calls, call = self.fake_server(["m1", "m2", "m3"])
        pool = harness.deque()
        planned = {"update": 2, "comm.mark_read": 3}
        with patch.object(harness.bpd, "_call_verb", side_effect=call):
            first = harness._prepare_fixtures("w0", 4, 0, planned, pool)
            second = harness._prepare_fixtures("w1", 4, 1, planned, pool)
        sends = [args for _, verb, args in calls if verb == "comm.send"]
        self.assertEqual(len(sends), 3)
        self.assertTrue(all(a["to"] == "tenant_4_actor" and a["self_send"] for a in sends))
        self.assertEqual([proc for proc, verb, _ in calls if verb == "comm.inbox"], ["w0", "w0"])
        self.assertEqual(list(pool), ["m1", "m2", "m3"])
        self.assertIs(first["receipts"], second["receipts"])
        self.assertEqual((first["entity_id"], second["entity_id"]), ("entity-w0", "entity-w1"))

    def test_inbox_count_unlike_the_plan_stops_setup(self):
        _, call = self.fake_server(["m1"])
        with patch.object(harness.bpd, "_call_verb", side_effect=call):
            with self.assertRaisesRegex(RuntimeError, "sent 2 messages to tenant 0, its inbox lists 1 unread"):
                harness._prepare_fixtures("w0", 0, 0, {"comm.mark_read": 2}, harness.deque())

    def test_no_fixture_shape_means_no_fixture_call(self):
        calls, call = self.fake_server([])
        with patch.object(harness.bpd, "_call_verb", side_effect=call):
            self.assertEqual(harness._prepare_fixtures("w0", 0, 0, {"stream.append": 9}, harness.deque()), {})
        self.assertEqual(calls, [])

    def test_receipt_marks_each_message_once_and_reports_exhaustion_and_misses(self):
        calls, call = self.fake_server([])
        fixtures = {"receipts": harness.deque(["m1", "bad"])}
        with patch.object(harness.bpd, "_call_verb", side_effect=call):
            first = harness._op_read_receipt("w0", 0, 0, fixtures=fixtures)
            missed = harness._op_read_receipt("w0", 0, 1, fixtures=fixtures)
            empty = harness._op_read_receipt("w0", 0, 2, fixtures=fixtures)
        self.assertEqual([args["ids"] for _, _, args in calls], [["m1"], ["bad"]])
        self.assertTrue(first.ok)
        self.assertFalse(missed.ok)
        self.assertIn("unexpected result", missed.error)
        self.assertEqual((empty.ok, empty.error), (False, "read-receipt fixture pool exhausted"))

    def test_arrival_hands_fixtures_only_to_a_worker_that_has_them(self):
        seen = []

        def op(proc, tenant, sequence, request_id=None, **kwargs):
            seen.append(kwargs)
            return harness.OpOutcome("update", True, 1)

        plain, fitted = harness.WorkerResult(0, 0), harness.WorkerResult(0, 1)
        fitted.fixtures = {"entity_id": "e"}
        clock = VirtualClock()
        harness._execute_arrival((None, plain), op, 0, clock, request_id=5)
        harness._execute_arrival((None, fitted), op, 1, clock, request_id=6)
        self.assertEqual(seen, [{}, {"fixtures": {"entity_id": "e"}}])


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
                patch.object(harness.mbc, "require_all_audit_success_rows"),
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


class AuditSuccessRowsLoadPreflightTests(unittest.TestCase):
    def run_to_worker_boundary(self, arrival_rate=None, refuse=False, extra_argv=()):
        events, worker_envs, process = [], [], Mock()
        def preflight(proc):
            self.assertIs(proc, process)
            events.append("preflight")
            if refuse:
                raise RuntimeError("reduced mode refused")
        def oracle(*args):
            events.append("oracle")
            return {"oracle": "PENDING", "detail": "fixture"}
        def worker(*args, **kwargs):
            events.append("worker")
            worker_envs.append(dict(args[2] if arrival_rate else args[3]))
            raise RuntimeError("worker boundary")
        argv = ["bench_load_harness.py", "--mode", "bench", "--workers", "1",
                "--tenants", "1", "--ops-per-worker", "1"]
        if arrival_rate:
            argv.extend(["--arrival-rate", str(arrival_rate)])
        argv.extend(extra_argv)
        output = io.StringIO()
        with contextlib.ExitStack() as stack:
            stack.enter_context(contextlib.redirect_stdout(output))
            stack.enter_context(patch.dict(harness.os.environ, {"KHIVE_AUDIT_SUCCESS_ROWS": "reduced",
                                                                "KHIVE_WRITER_ATTEMPT_LOG": "1"}))
            patches = [patch.object(harness.sys, "argv", argv),
                       patch.object(harness, "_resolve_binary", return_value="fixture-binary"),
                       patch.object(harness.tempfile, "mkdtemp", return_value="/fixture"),
                       patch.object(harness.pathlib.Path, "write_text", return_value=0),
                       patch.object(harness.pathlib.Path, "exists", return_value=True),
                       patch.object(harness, "_assert_not_live_db"),
                       patch.object(harness.subprocess, "Popen", return_value=process),
                       patch.object(harness.bpd, "_handshake"),
                       patch.object(harness.bpd, "_call_verb"),
                       patch.object(harness.bpd, "assert_daemon_engaged", side_effect=lambda *a, **k: events.append("engaged")),
                       patch.object(harness.mbc, "require_all_audit_success_rows", side_effect=preflight),
                       patch.object(harness.bpd, "_teardown_daemon"),
                       patch.object(harness.bpd, "_git_sha", return_value="fixture-sha"),
                       patch.object(harness.bpd, "_iso_now", return_value="fixture-time"),
                       patch.object(harness, "probe_oracle_channel", side_effect=oracle),
                       patch.object(harness, "_run_worker", side_effect=worker),
                       patch.object(harness, "_run_open_workers", side_effect=worker),
                       patch.object(harness, "ThreadPoolExecutor", ImmediatePool)]
            mocks = [stack.enter_context(selected) for selected in patches]
            self.assertEqual(harness.main(), 1)
        process.stdin.close.assert_called_once()
        process.wait.assert_called_once()
        mocks[11].assert_called_once()
        self.assertIn("reduced mode refused" if refuse else "worker boundary", output.getvalue())
        return events, mocks[6].call_args.kwargs["env"], worker_envs

    def test_preflight_precedes_oracle_and_worker_fanout(self):
        for rate in (None, 10):
            with self.subTest(rate=rate):
                events, _, _ = self.run_to_worker_boundary(rate)
                self.assertEqual(events, ["engaged", "preflight", "oracle", "worker"])

    def test_refusal_prevents_oracle_and_worker_fanout(self):
        for rate in (None, 10):
            with self.subTest(rate=rate):
                events, _, _ = self.run_to_worker_boundary(rate, refuse=True)
                self.assertEqual(events, ["engaged", "preflight"])

    def test_bootstrap_and_workers_inherit_all(self):
        for rate in (None, 10):
            with self.subTest(rate=rate):
                _, bootstrap_env, worker_envs = self.run_to_worker_boundary(rate)
                self.assertEqual(bootstrap_env["KHIVE_AUDIT_SUCCESS_ROWS"], "all")
                self.assertEqual(len(worker_envs), 1)
                self.assertEqual(worker_envs[0]["KHIVE_AUDIT_SUCCESS_ROWS"], "all")

    def test_attempt_log_reaches_the_daemon_only_when_asked_and_lands_in_the_sink_dir(self):
        with tempfile.TemporaryDirectory() as tmp:
            sink = os.path.join(tmp, "sink")
            _, off_env, _ = self.run_to_worker_boundary(10, extra_argv=["--sink-dir", sink])
            self.assertNotIn("KHIVE_WRITER_ATTEMPT_LOG", off_env)
            self.assertEqual(off_env["KHIVE_WRITER_TIMEOUT_SINK_DIR"], os.path.realpath(sink))
            self.assertTrue(os.path.isdir(sink))
            _, on_env, worker_envs = self.run_to_worker_boundary(10, extra_argv=["--sink-dir", sink, "--writer-attempt-log"])
            self.assertEqual(on_env["KHIVE_WRITER_ATTEMPT_LOG"], "1")
            self.assertEqual(worker_envs[0]["KHIVE_WRITER_ATTEMPT_LOG"], "1")

    def test_attempt_log_without_sink_dir_is_refused(self):
        argv = ["bench_load_harness.py", "--arrival-rate", "10", "--writer-attempt-log"]
        with patch.object(harness.sys, "argv", argv), contextlib.redirect_stderr(io.StringIO()) as err:
            with self.assertRaises(SystemExit):
                harness.main()
        self.assertIn("needs --sink-dir", err.getvalue())


if __name__ == "__main__":
    unittest.main()
