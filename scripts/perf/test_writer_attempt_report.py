import json
import os
import sys
import tempfile
import unittest
import io
import contextlib

sys.path.insert(0, os.path.dirname(__file__))
import writer_attempt_report as war

BITS = 20
PREFIX = 0x5A


def rid(sequence):
    return (PREFIX << BITS) | sequence


WINDOW_START = "2026-11-18T00:00:00.000Z"
INSIDE = "2026-11-18T00:00:00.500Z"


def attempt(seq, request_id=None, outcome="ok", acquire=10, body=100, commit=40, queue=5, ts=INSIDE):
    row = {"kind": "writer_attempt", "ts_utc": ts, "attempt_seq": seq, "outcome": outcome,
           "transaction_acquire_micros": acquire, "body_micros": body, "commit_micros": commit,
           "queue_wait_micros": queue}
    if request_id is not None:
        row["request_id"] = request_id
    return row


def line(row):
    return json.dumps(row)


STARTUP = {"kind": "startup", "sqlite": {"version": "3.50.0", "source_id": "x", "compile_options": []}}
# The daemon's heartbeat after the run window: it closes the segment's drop counter.
CLOSING_BEAT = {"kind": "heartbeat", "ts_utc": "2026-11-18T00:00:05.000Z"}


def arrival(sequence, op, state="completed", latency_us=1000):
    return {"sequence": sequence, "request_id": rid(sequence), "op": op, "state": state, "latency_us": latency_us}


def arrival_report(records, window_s=1.0):
    end = f"2026-11-18T00:00:{window_s:06.3f}Z"
    return {"request_id_prefix": PREFIX, "request_id_sequence_bits": BITS, "records": records,
            "observed_duration_s": window_s, "configured_rate_per_s": 10.0,
            "window_utc": {"start": WINDOW_START, "end": end}}


class SinkLossTests(unittest.TestCase):
    def test_segments_restart_at_each_startup_row_and_count_drops(self):
        lines = [line(STARTUP), line(attempt(1)), line(attempt(2)),
                 line(STARTUP), line(attempt(1)), line(attempt(3)),
                 line({"kind": "sink_error_summary", "error": "sink drops since last summary: 4"}), "{not json"]
        segments = war.parse_sink_lines(lines)
        self.assertEqual([len(s["attempts"]) for s in segments], [2, 2])
        self.assertEqual(war.segment_loss(segments[0]),
                         {"expected": 2, "written": 2, "gaps": 0, "dropped": 0, "loss_upper": 0.0})
        second = war.segment_loss(segments[1])
        self.assertEqual((second["expected"], second["written"], second["gaps"], second["dropped"]), (3, 2, 1, 4))
        self.assertAlmostEqual(second["loss_upper"], 4 / 6)
        self.assertEqual(segments[1]["unparsed"], 1)

    def test_more_gaps_than_drops_is_broken_not_a_reading(self):
        segment = war.parse_sink_lines([line(STARTUP), line(attempt(1)), line(attempt(4))])[0]
        with self.assertRaisesRegex(war.ReportError, "2 sequence gaps but its drop counter reads 0"):
            war.segment_loss(segment)

    def test_repeated_sequence_is_broken(self):
        segment = war.parse_sink_lines([line(STARTUP), line(attempt(1)), line(attempt(1))])[0]
        with self.assertRaisesRegex(war.ReportError, "repeats an attempt_seq"):
            war.segment_loss(segment)


class JoinTests(unittest.TestCase):
    def test_join_counts_attempts_per_request_and_keeps_unjoined_rows_visible(self):
        arrivals = [arrival(0, "create"), arrival(1, "create"), arrival(2, "memory.recall"),
                    arrival(3, "create", state="refused")]
        attempts = [attempt(1, rid(0)), attempt(2, rid(0), outcome="error"), attempt(3, rid(1)),
                    attempt(4), attempt(5, (PREFIX + 1) << BITS), attempt(6, rid(9))]
        out = war.join_point(arrivals, attempts, 2.0, prefix=PREFIX, sequence_bits=BITS)
        create = out["classes"]["create"]
        self.assertEqual((create["arrivals"], create["completed"], create["joined"]), (3, 2, 2))
        self.assertEqual(create["joined_fraction"], 1.0)
        self.assertEqual(create["attempts_per_request"]["max"], 2)
        self.assertEqual(create["service_micros"]["n"], 3)
        self.assertAlmostEqual(create["error_rate"], 1 / 3)
        self.assertEqual(out["classes"]["memory.recall"]["joined"], 0)
        self.assertEqual((out["unscoped_attempts"], out["foreign_attempts"], out["orphan_attempts"]), (1, 1, 1))
        self.assertAlmostEqual(out["utilization"], 6 * 150 / 2_000_000)


class CostSplitTests(unittest.TestCase):
    def test_service_splits_into_fixed_and_per_row_parts(self):
        rows = [dict(attempt(i, body=100 + 5 * k), rows_changed=k) for i, k in enumerate([1, 2, 3, 3], start=1)]
        rows.append(attempt(9))
        out = war.fixed_variable_cost(rows)
        self.assertEqual((out["n"], out["without_rows_changed"]), (4, 1))
        self.assertAlmostEqual(out["fit"]["fixed_micros"], 150)
        self.assertAlmostEqual(out["fit"]["per_row_micros"], 5)
        self.assertAlmostEqual(out["fit"]["r2"], 1.0)
        self.assertEqual(out["fit"]["rows_changed_range"], [1, 3])

    def test_one_row_count_has_no_slope(self):
        out = war.fixed_variable_cost([dict(attempt(1), rows_changed=1), dict(attempt(2), rows_changed=1)])
        self.assertIsNone(out["fit"])
        self.assertIn("1 distinct rows_changed value", out["reason"])


class BurstDrainTests(unittest.TestCase):
    def row(self, burst, state="completed", scheduled=1.0, finished=1.1):
        return {"burst": burst, "state": state, "scheduled_s": scheduled,
                "finished_s": finished if state != "outstanding" else None}

    def test_drain_runs_to_the_last_completed_finish_and_misses_name_their_cause(self):
        records = [self.row(None, finished=0.2),
                   self.row(1, finished=1.05), self.row(1, finished=1.08),
                   self.row(2, scheduled=2.0, finished=2.5), self.row(2, scheduled=2.0, finished=2.01),
                   self.row(3, scheduled=3.0, finished=3.02), self.row(3, state="refused", scheduled=3.0, finished=3.0),
                   self.row(4, state="outstanding", scheduled=4.0)]
        out = war.burst_drain(records, 0.1)
        by_id = {b["burst"]: b for b in out["per_burst"]}
        self.assertAlmostEqual(by_id[1]["drain_s"], 0.08)
        self.assertFalse(by_id[1]["missed"])
        self.assertAlmostEqual(by_id[2]["drain_s"], 0.5)
        self.assertTrue(by_id[2]["missed"])
        self.assertTrue(by_id[3]["missed"])
        self.assertEqual((by_id[3]["refused"], by_id[3]["unfinished"]), (1, 1))
        self.assertIsNone(by_id[4]["drain_s"])
        self.assertEqual((out["bursts"], out["missed"], out["refused"], out["sizes"]), (4, 3, 1, [1, 2]))
        # Burst 2 drained late, burst 4 never completed; burst 3 missed only through its refused arrival.
        self.assertEqual([by_id[i]["drain_late"] for i in (1, 2, 3, 4)], [False, True, False, True])
        self.assertEqual((out["drain_missed"], out["refused_only"]), (2, 1))
        self.assertEqual(out["drain_s"]["n"], 3)

    def test_a_late_drain_counts_whatever_the_refusals(self):
        records = [self.row(1, finished=1.5) for _ in range(39)] + [self.row(1, state="refused", finished=1.0)]
        out = war.burst_drain(records, 0.1)
        self.assertEqual((out["missed"], out["refused"], out["drain_missed"], out["refused_only"]), (1, 1, 1, 0))

    def test_no_burst_rows_is_no_reading_and_a_burst_without_a_deadline_is_broken(self):
        self.assertIsNone(war.burst_drain([self.row(None)], None))
        with self.assertRaisesRegex(war.ReportError, "positive drain deadline"):
            war.burst_drain([self.row(1)], None)


class VerdictTests(unittest.TestCase):
    def report(self, sink_lines, records=None, closed=True, **limits):
        records = records if records is not None else [arrival(0, "create")]
        if closed:
            sink_lines = list(sink_lines) + [line(CLOSING_BEAT)]
        return war.build_point_report(arrival_report(records), {"writer_timeouts.1.ndjson": sink_lines}, **limits)

    def test_a_segment_with_no_heartbeat_after_the_window_is_broken(self):
        sink = [line(STARTUP), line(attempt(1, rid(0)))]
        with self.assertRaisesRegex(war.ReportError, "no heartbeat after the run window ends"):
            self.report(sink, closed=False)
        early = [line(STARTUP), line({"kind": "heartbeat", "ts_utc": INSIDE}), line(attempt(1, rid(0)))]
        with self.assertRaisesRegex(war.ReportError, "no heartbeat after the run window ends"):
            self.report(early, closed=False)
        # A drop counted by the closing heartbeat's summary still voids the point.
        late_drop = sink + [line(CLOSING_BEAT),
                            line({"kind": "sink_error_summary", "error": "sink drops since last summary: 1"})]
        self.assertEqual(self.report(late_drop, closed=False)["verdict"], "VOID")

    def test_loss_over_the_limit_voids_the_point(self):
        sink = [line(STARTUP), line(attempt(1, rid(0))),
                line({"kind": "sink_error_summary", "error": "sink drops since last summary: 1"})]
        out = self.report(sink)
        self.assertEqual(out["verdict"], "VOID")
        self.assertEqual(out["loss"]["dropped"], 1)
        self.assertEqual(out["sqlite"]["version"], "3.50.0")

    def test_within_and_outside_the_envelope(self):
        sink = [line(STARTUP), line(attempt(1, rid(0)))]
        self.assertEqual(self.report(sink)["verdict"], "WITHIN")
        heavy = [line(STARTUP), line(attempt(1, rid(0), body=900_000))]
        out = self.report(heavy)
        self.assertEqual(out["verdict"], "OUTSIDE")
        self.assertIn("utilization", out["verdict_reason"])
        out = self.report(sink, queue_p99_limit_micros=5)
        self.assertEqual(out["verdict"], "OUTSIDE")
        self.assertIn("queue wait p99", out["verdict_reason"])

    def test_setup_rows_before_arrival_zero_are_kept_out_of_utilization(self):
        setup = attempt(1, body=900_000, ts="2026-11-17T23:59:59.000Z")
        sink = [line(STARTUP), line(setup), line(attempt(2, rid(0)))]
        out = self.report(sink)
        self.assertEqual(out["verdict"], "WITHIN")
        self.assertEqual((out["attempts"], out["outside_window_attempts"], out["unscoped_attempts"]), (2, 1, 1))
        self.assertAlmostEqual(out["utilization"], 150 / 1_000_000)
        self.assertEqual(out["queue_wait_micros"]["n"], 1)

    def test_missing_window_or_untimed_row_is_broken(self):
        records = arrival_report([arrival(0, "create")])
        del records["window_utc"]
        with self.assertRaisesRegex(war.ReportError, "no wall-clock window"):
            war.build_point_report(records, {"f": [line(STARTUP), line(attempt(1, rid(0)))]})
        with self.assertRaisesRegex(war.ReportError, "not an RFC 3339 UTC time"):
            self.report([line(STARTUP), line(attempt(1, rid(0), ts=None))])

    def test_missing_prefix_or_rows_is_broken(self):
        with self.assertRaisesRegex(war.ReportError, "no request id prefix"):
            war.build_point_report({"records": [], "observed_duration_s": 1.0, "window_utc": {"start": WINDOW_START, "end": INSIDE}},
                                   {"f": [line(STARTUP)]})
        with self.assertRaisesRegex(war.ReportError, "no writer_attempt rows"):
            self.report([line(STARTUP)])
        with self.assertRaisesRegex(war.ReportError, "no sink file"):
            war.build_point_report(arrival_report([]), {})

    def test_main_prints_broken_and_exits_two(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "arrivals.json")
            with open(path, "w") as fh:
                json.dump(arrival_report([arrival(0, "create")]), fh)
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                rc = war.main(["--arrivals", path, "--sink-dir", tmp])
            self.assertEqual(rc, 2)
            self.assertEqual(json.loads(out.getvalue())["verdict"], "BROKEN")


class PairedCostTests(unittest.TestCase):
    def test_cost_is_on_minus_off_pooled_over_rounds(self):
        off = [{"records": [arrival(0, "create", latency_us=100), arrival(1, "create", latency_us=200)]}]
        on = [{"records": [arrival(0, "create", latency_us=150), arrival(1, "create", latency_us=260),
                           arrival(2, "create", state="failed", latency_us=9_999)]}]
        out = war.paired_latency_cost(off, on)
        self.assertEqual(out["classes"]["create"]["p50_cost_us"], 60)
        self.assertEqual(out["classes"]["create"]["on"]["n"], 2)
        with self.assertRaisesRegex(war.ReportError, "same positive number"):
            war.paired_latency_cost(off, [])


if __name__ == "__main__":
    unittest.main()
