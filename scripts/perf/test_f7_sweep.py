import contextlib
import io
import json
import os
import pathlib
import sys
import tempfile
import types
import unittest

sys.path.insert(0, os.path.dirname(__file__))
import bench_load_harness as harness
import f7_sweep as sweep
import writer_attempt_report as war

BITS = 20
PREFIX = 7
STARTUP = {"kind": "startup", "ts_utc": "2026-11-18T00:00:00.000Z", "sqlite": {"version": "3.54.1"}}


def args(**overrides):
    base = dict(workers=[100, 200], workers_per_tenant=5, rate_per_worker=2.0, seconds=3.0, mix="f7", mode="bench",
                packs=None, worker_timeout=60.0, loss_limit=0.01, void_retries=1, util_limit=0.70,
                queue_p99_limit_micros=None, burst_every=5.0, burst_deadline_ms=None)
    base.update(overrides)
    return types.SimpleNamespace(**base)


class FakeHarness:
    """Writes what one harness run would: the report and the daemon's sink file. `drops` per run name."""

    def __init__(self, drops=(), stray_rows=(), latency=None, burst_finish=None, states=None, failing=()):
        self.drops, self.stray_rows, self.latency = dict(drops), set(stray_rows), latency or {}
        self.burst_finish = burst_finish or {}
        self.states, self.failing = states or {}, set(failing)
        self.argvs = []

    def __call__(self, argv, stdout=None, stderr=None):
        self.argvs.append(argv)
        run_dir = pathlib.Path(argv[argv.index("--report") + 1]).parent
        name = run_dir.name
        log_on = "--writer-attempt-log" in argv
        lat = self.latency.get(name, 100)
        states = self.states.get(name, ["completed"] * 4)
        records = [{"sequence": i, "request_id": (PREFIX << BITS) | i, "op": "stream.append", "state": states[i],
                    "latency_us": lat + i, "burst": None} for i in range(4)]
        counts = {state: states.count(state) for state in ("completed", "failed", "refused", "outstanding")}
        smoke = "FAIL" if counts["outstanding"] or name in self.failing else "PASS"
        if "--burst-size" in argv:
            for row in records[2:]:
                row.update(burst=1, scheduled_s=1.0, finished_s=1.0 + self.burst_finish.get(name, 0.01))
        errors = [harness.format_worker_error(0, i, harness.DRAIN_TIMEOUT_CRASH)
                  for i, state in enumerate(states) if state == "outstanding"]
        if name in self.failing:
            errors.append(harness.format_worker_error(0, 0, "daemon exited with code 137"))
        report = {"smoke_result": smoke, "smoke_errors": errors, "arrival": {
            "request_id_prefix": PREFIX, "request_id_sequence_bits": BITS, "records": records,
            "observed_duration_s": 1.0, "counts": {"submitted": 4, **counts},
            "sink_heartbeat_ms": 1000, "sink_settle_s": 3.0,
            "window_utc": {"start": "2026-11-18T00:00:00.000Z", "end": "2026-11-18T00:00:01.000Z"}}}
        (run_dir / "harness.json").write_text(json.dumps(report))
        sink = run_dir / "sink"
        sink.mkdir()
        lines = [STARTUP]
        if log_on or name in self.stray_rows:
            for i in range(4):
                lines.append({"kind": "writer_attempt", "ts_utc": "2026-11-18T00:00:00.500Z", "attempt_seq": i + 1,
                              "request_id": (PREFIX << BITS) | i, "outcome": "ok", "queue_wait_micros": 5,
                              "transaction_acquire_micros": 1, "body_micros": 10, "commit_micros": 4})
        lines.append({"kind": "heartbeat", "ts_utc": "2026-11-18T00:00:05.000Z"})
        if self.drops.get(name):
            lines.append({"kind": "sink_error_summary", "error": f"sink drops since last summary: {self.drops[name]}"})
        (sink / "writer_timeouts.1.ndjson").write_text("\n".join(json.dumps(x) for x in lines) + "\n")
        return types.SimpleNamespace(returncode=0 if smoke == "PASS" else 1)


class PlanTests(unittest.TestCase):
    def test_aa_pair_first_then_sweep_then_alternating_pairs(self):
        runs = sweep.plan_runs([100, 200, 400], aa=True, paired_workers=100, paired_rounds=3)
        self.assertEqual([r["name"] for r in runs],
                         ["aa-1", "aa-2", "sweep-100", "sweep-200", "sweep-400",
                          "pair-1-off", "pair-1-on", "pair-2-on", "pair-2-off", "pair-3-off", "pair-3-on"])
        self.assertTrue(all(r["workers"] == 100 for r in runs if r["role"] in ("aa", "pair")))
        self.assertEqual([r["attempt_log"] for r in runs if r["role"] == "pair"],
                         [False, True, True, False, False, True])

    def test_argv_sizes_the_run_and_sets_the_log_only_on_its_arm(self):
        run_dir = pathlib.Path("/x/sweep-200")
        on = sweep.harness_argv({"workers": 200, "attempt_log": True}, run_dir, args(), python="py")
        off = sweep.harness_argv({"workers": 200, "attempt_log": False}, run_dir, args(), python="py")
        flag = lambda argv, name: argv[argv.index(name) + 1]
        self.assertEqual((flag(on, "--tenants"), flag(on, "--ops-per-worker"), flag(on, "--arrival-rate")),
                         ("40", "6", "400.0"))
        self.assertEqual(flag(on, "--sink-dir"), "/x/sweep-200/sink")
        self.assertIn("--writer-attempt-log", on)
        self.assertNotIn("--writer-attempt-log", off)
        with self.assertRaisesRegex(ValueError, "do not split"):
            sweep.harness_argv({"workers": 101, "attempt_log": True}, run_dir, args(), python="py")


class BurstTests(unittest.TestCase):
    def test_burst_runs_close_the_plan_and_carry_their_size(self):
        runs = sweep.plan_runs([100], aa=False, paired_workers=50, paired_rounds=0, burst_sizes=[10, 40])
        self.assertEqual([(r["name"], r["workers"]) for r in runs[-2:]], [("burst-10", 50), ("burst-40", 50)])
        argv = sweep.harness_argv(runs[-1], pathlib.Path("/x/burst-40"), args(), python="py")
        self.assertEqual(argv[argv.index("--burst-size") + 1:argv.index("--burst-size") + 4],
                         ["40", "--burst-every", "5.0"])
        self.assertNotIn("--burst-size", sweep.harness_argv(runs[0], pathlib.Path("/x/s"), args(), python="py"))

    def test_knee_is_the_first_size_with_a_missed_burst(self):
        fake = FakeHarness(burst_finish={"burst-10": 0.01, "burst-40": 0.5, "burst-80": 0.7})
        lines = []
        with tempfile.TemporaryDirectory() as tmp:
            runs = sweep.plan_runs([100], aa=False, paired_workers=100, paired_rounds=0, burst_sizes=[10, 40, 80])
            summary, rc = sweep.execute(runs, pathlib.Path(tmp), args(burst_deadline_ms=100.0), spawn=fake,
                                        emit=lines.append)
        self.assertEqual(rc, 0)
        self.assertIn("bursts=1 missed=1 drain_missed=1 refused_only=0 refused_arrivals=0 "
                      "drain_p99_ms=500.0 deadline_ms=100", lines[2])
        self.assertEqual(lines[-1],
                         "burst-drain knee at deadline 100 ms: size 40 (first size with a burst that drained late)")

    def test_a_size_with_refusals_and_a_late_drain_is_the_knee(self):
        records = [{"burst": 1, "state": "completed", "scheduled_s": 1.0, "finished_s": 1.5} for _ in range(39)]
        records.append({"burst": 1, "state": "refused", "scheduled_s": 1.0, "finished_s": 1.0})
        refused_and_late = war.burst_drain(records, 0.1)
        fast = war.burst_drain([{"burst": 1, "state": "completed", "scheduled_s": 1.0, "finished_s": 1.01}], 0.1)
        self.assertEqual(sweep.knee_line([(10, fast), (40, refused_and_late)], 0.1),
                         "burst-drain knee at deadline 100 ms: size 40 (first size with a burst that drained late)")

    def test_refusal_only_bursts_are_named_apart(self):
        records = [{"burst": 1, "state": "completed", "scheduled_s": 1.0, "finished_s": 1.01},
                   {"burst": 1, "state": "refused", "scheduled_s": 1.0, "finished_s": 1.0}]
        refusal_only = war.burst_drain(records, 0.1)
        self.assertEqual((refusal_only["drain_missed"], refusal_only["refused_only"]), (0, 1))
        self.assertEqual(sweep.knee_line([(10, refusal_only), (40, refusal_only)], 0.1),
                         "burst-drain knee at deadline 100 ms: none up to size 40; "
                         "bursts missed only through refused arrivals at size 10, 40")

    def test_burst_sizes_at_or_over_the_worker_pool_are_refused(self):
        base = ["--out", "/x/o", "--rate-per-worker", "1", "--burst-deadline-ms", "100", "--paired-workers", "50"]
        self.assertEqual(sweep.parse_args(base + ["--burst-sizes", "10,49"]).burst_sizes, [10, 49])
        err = io.StringIO()
        with contextlib.redirect_stderr(err), self.assertRaises(SystemExit):
            sweep.parse_args(base + ["--burst-sizes", "10,50"])
        self.assertIn("--burst-sizes must stay below --paired-workers", err.getvalue())

    def test_burst_run_without_burst_rows_is_broken(self):
        class NoBurst(FakeHarness):
            def __call__(self, argv, stdout=None, stderr=None):
                return super().__call__([a for a in argv if a != "--burst-size"], stdout, stderr)
        with tempfile.TemporaryDirectory() as tmp:
            runs = sweep.plan_runs([100], aa=False, paired_workers=100, paired_rounds=0, burst_sizes=[10])[1:]
            _, rc = sweep.execute(runs, pathlib.Path(tmp), args(burst_deadline_ms=100.0), spawn=NoBurst(),
                                  emit=lambda _: None)
        self.assertEqual(rc, 2)


class ExecuteTests(unittest.TestCase):
    def run_plan(self, fake, **overrides):
        lines = []
        with tempfile.TemporaryDirectory() as tmp:
            runs = sweep.plan_runs([100], aa=False, paired_workers=100, paired_rounds=2)
            summary, rc = sweep.execute(runs, pathlib.Path(tmp), args(**overrides), spawn=fake, emit=lines.append)
            names = sorted(os.listdir(tmp))
        return summary, rc, lines, names

    def test_each_log_run_prints_its_loss_accounting_and_the_cost_line_closes(self):
        fake = FakeHarness(latency={"pair-1-off": 100, "pair-1-on": 130, "pair-2-on": 150, "pair-2-off": 110})
        summary, rc, lines, _ = self.run_plan(fake)
        self.assertEqual(rc, 0)
        self.assertIn("[sweep-100] workers=100 attempts expected=4 written=4 gaps=0 dropped=0 loss_upper=0.0000",
                      lines[0])
        self.assertIn("verdict=WITHIN", lines[0])
        self.assertTrue(lines[1].startswith("[pair-1-off] workers=100 attempt log off"))
        self.assertTrue(lines[-1].startswith("attempt-log cost, on minus off, pooled over 2 rounds: stream.append"))
        self.assertEqual(summary["paired_cost"]["classes"]["stream.append"]["off"]["n"], 8)
        self.assertEqual(lines[-2], "sink heartbeat 1000 ms and settle 3.0 s on every run, both paired arms included")
        self.assertEqual(summary["sink_conditions"], [(1000, 3.0)])

    def test_void_point_runs_again_and_keeps_the_void_run_on_record(self):
        summary, rc, lines, names = self.run_plan(FakeHarness(drops={"sweep-100": 1}))
        self.assertEqual(rc, 0)
        self.assertEqual([r["verdict"] for r in summary["runs"]][:2], ["VOID", "WITHIN"])
        self.assertIn("sweep-100-retry1", names)

    def test_still_void_after_retries_is_no_reading(self):
        fake = FakeHarness(drops={"sweep-100": 1, "sweep-100-retry1": 1})
        _, rc, lines, _ = self.run_plan(fake)
        self.assertEqual(rc, 1)
        self.assertIn("[sweep-100] still VOID after 1 retries; no reading at this point", lines)

    def test_a_void_pair_run_runs_its_whole_round_again_in_order(self):
        summary, rc, _, _ = self.run_plan(FakeHarness(drops={"pair-1-on": 1}))
        self.assertEqual(rc, 0)
        paired = [r["name"] for r in summary["runs"] if r["role"] == "pair"]
        self.assertEqual(paired, ["pair-1-off", "pair-1-on", "pair-1-off-retry1", "pair-1-on-retry1",
                                  "pair-2-on", "pair-2-off"])
        self.assertEqual(summary["paired_cost"]["rounds"], 2)
        self.assertEqual(summary["paired_cost"]["classes"]["stream.append"]["off"]["n"], 8)
        self.assertEqual(summary["paired_cost"]["classes"]["stream.append"]["on"]["n"], 8)

    def test_a_round_still_void_after_retries_leaves_both_arms(self):
        off_retry_states = ["completed", "completed", "completed", "outstanding"]
        fake = FakeHarness(drops={"pair-1-on": 1, "pair-1-on-retry1": 1},
                           states={"pair-1-off-retry1": off_retry_states})
        summary, rc, lines, _ = self.run_plan(fake)
        self.assertEqual(rc, 1)
        self.assertIn("[pair-1] still VOID after 1 retries; no reading at this round; the round leaves both arms",
                      lines)
        self.assertEqual(summary["paired_cost"]["rounds"], 1)
        self.assertEqual(summary["paired_cost"]["classes"]["stream.append"]["off"]["n"], 4)
        self.assertNotIn("dropped_paired_latency", summary)

    def test_a_drain_timeout_is_a_reading_outside_the_envelope(self):
        states = ["completed", "completed", "outstanding", "outstanding"]
        summary, rc, lines, _ = self.run_plan(FakeHarness(states={"sweep-100": states}))
        self.assertEqual(rc, 0)
        self.assertEqual(summary["runs"][0]["verdict"], "OUTSIDE")
        self.assertIn("(2 of 4 arrivals outstanding at the drain timeout)", lines[0])

    def test_failed_arrivals_put_the_point_outside(self):
        states = ["completed", "failed", "completed", "completed"]
        summary, _, lines, _ = self.run_plan(FakeHarness(states={"sweep-100": states}))
        self.assertEqual(summary["runs"][0]["verdict"], "OUTSIDE")
        self.assertIn("(1 of 4 arrivals failed)", lines[0])

    def test_a_pair_round_with_unfinished_arrivals_leaves_both_arms(self):
        states = ["completed", "failed", "completed", "outstanding"]
        fake = FakeHarness(states={"pair-2-off": states}, latency={"pair-2-off": 1000, "pair-2-on": 2400})
        summary, rc, lines, _ = self.run_plan(fake)
        self.assertEqual(rc, 0)
        self.assertIn("[pair-2] pair-2-off had failed or outstanding arrivals; the round leaves both arms", lines)
        self.assertEqual(summary["paired_cost"]["rounds"], 1)
        self.assertIn("paired arm: 1 of 2 rounds left both arms for failed or outstanding arrivals; the cost line "
                      "covers the clean rounds only", lines)
        self.assertEqual(summary["paired_rounds"], {"planned": 2, "unclean": 1})
        self.assertEqual(summary["paired_cost"]["classes"]["stream.append"]["off"]["p99"], 103)
        dropped = summary["dropped_paired_latency"]
        self.assertEqual(dropped["rounds"], 1)
        op = dropped["classes"]["stream.append"]
        self.assertEqual((op["off"]["n"], op["off"]["p50"], op["off"]["p99"]), (2, 1002, 1002))
        self.assertEqual((op["on"]["n"], op["on"]["p50"], op["on"]["p99"]), (4, 2402, 2403))
        self.assertIn("completed client latency for dropped paired rounds (count=1) | stream.append "
                      "(off n=2 p50=1002 us p99=1002 us, on n=4 p50=2402 us p99=2403 us)", lines)

    def test_no_clean_paired_round_is_no_reading(self):
        states = ["outstanding"] * 4
        summary, rc, lines, _ = self.run_plan(FakeHarness(states={"pair-1-off": states, "pair-2-off": states}))
        self.assertEqual(rc, 1)
        self.assertIn("paired arm: none of 2 rounds was clean; no cost line", lines)
        self.assertIsNone(summary["paired_cost"])
        self.assertFalse(any(line.startswith("attempt-log cost") for line in lines))
        op = summary["dropped_paired_latency"]["classes"]["stream.append"]
        self.assertEqual(op["off"], {"n": 0})
        self.assertEqual(op["on"]["n"], 8)
        self.assertIsNone(op["p50_cost_us"])
        self.assertIn("completed client latency for dropped paired rounds (count=2) | stream.append "
                      "(off n=0, on n=8 p50=102 us p99=103 us)", lines)

    def test_a_harness_that_failed_with_nothing_outstanding_is_broken(self):
        summary, rc, lines, _ = self.run_plan(FakeHarness(failing={"sweep-100"}))
        self.assertEqual(rc, 2)
        self.assertIn("[sweep-100] BROKEN: sweep-100: the harness exited 1 with smoke_result=FAIL", lines[-1])
        self.assertEqual(summary["runs"][-1]["verdict"], "BROKEN")

    def test_a_harness_that_failed_otherwise_beside_a_drain_timeout_is_broken(self):
        states = ["completed", "completed", "completed", "outstanding"]
        summary, rc, lines, _ = self.run_plan(FakeHarness(states={"sweep-100": states}, failing={"sweep-100"}))
        self.assertEqual(rc, 2)
        self.assertIn("for a reason other than a drain timeout alone", lines[-1])
        self.assertIn("daemon exited with code 137", lines[-1])
        self.assertEqual(summary["runs"][-1]["verdict"], "BROKEN")

    def test_a_drain_timeout_with_no_error_named_is_broken(self):
        with tempfile.TemporaryDirectory() as tmp:
            run_dir = pathlib.Path(tmp)
            report = {"smoke_result": "FAIL", "smoke_errors": [], "arrival": {
                "records": [{"state": "outstanding"}], "counts": {"submitted": 1, "outstanding": 1}}}
            (run_dir / "harness.json").write_text(json.dumps(report))
            with self.assertRaisesRegex(war.ReportError, "other than a drain timeout"):
                sweep.read_run({"name": "x", "attempt_log": False}, run_dir, loss_limit=0.01, util_limit=0.7,
                               queue_p99_limit_micros=None, harness_rc=1)

    def test_a_harness_that_wrote_no_report_is_broken(self):
        summary, rc, lines, _ = self.run_plan(lambda argv, stdout=None, stderr=None: types.SimpleNamespace(returncode=2))
        self.assertEqual(rc, 2)
        self.assertIn("[sweep-100] BROKEN: sweep-100: the harness exited 2 and left no readable report "
                      "(FileNotFoundError", lines[-1])
        self.assertEqual(summary["runs"][-1]["verdict"], "BROKEN")

    def test_a_harness_report_that_is_not_an_object_is_broken(self):
        with tempfile.TemporaryDirectory() as tmp:
            run_dir = pathlib.Path(tmp)
            (run_dir / "harness.json").write_text("[]")
            with self.assertRaisesRegex(war.ReportError, "the harness report is a list, not an object"):
                sweep.read_run({"name": "x", "attempt_log": False}, run_dir, loss_limit=0.01, util_limit=0.7,
                               queue_p99_limit_micros=None, harness_rc=0)

    def test_read_run_requires_the_harness_exit_status(self):
        with self.assertRaisesRegex(TypeError, "harness_rc"):
            sweep.read_run(
                {"name": "x", "attempt_log": False}, pathlib.Path("."), loss_limit=0.01,
                util_limit=0.7, queue_p99_limit_micros=None,
            )

    def test_a_pair_from_two_rounds_is_refused(self):
        plan = sweep.plan_runs([100], aa=False, paired_workers=100, paired_rounds=2)
        by_name = {run["name"]: run for run in plan}
        runs = [by_name[name] for name in ("sweep-100", "pair-1-on", "pair-2-off", "pair-1-off", "pair-2-on")]
        with tempfile.TemporaryDirectory() as tmp, self.assertRaisesRegex(ValueError, "same round"):
            sweep.execute(runs, pathlib.Path(tmp), args(), spawn=FakeHarness(), emit=lambda _: None)

    def test_attempt_rows_in_a_log_off_run_break_the_paired_arm(self):
        summary, rc, lines, _ = self.run_plan(FakeHarness(stray_rows={"pair-1-off"}))
        self.assertEqual(rc, 2)
        self.assertIn("attempt log off, but the sink holds 4 writer_attempt rows", lines[-1])
        self.assertEqual(summary["runs"][-1]["verdict"], "BROKEN")
        self.assertIsNone(summary["paired_cost"])


if __name__ == "__main__":
    unittest.main()
