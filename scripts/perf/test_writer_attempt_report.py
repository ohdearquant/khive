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


def attempt(seq, request_id=None, outcome="ok", acquire=10, body=100, commit=40, queue=5):
    row = {"kind": "writer_attempt", "attempt_seq": seq, "outcome": outcome,
           "transaction_acquire_micros": acquire, "body_micros": body, "commit_micros": commit,
           "queue_wait_micros": queue}
    if request_id is not None:
        row["request_id"] = request_id
    return row


def line(row):
    return json.dumps(row)


STARTUP = {"kind": "startup", "sqlite": {"version": "3.50.0", "source_id": "x", "compile_options": []}}


def arrival(sequence, op, state="completed", latency_us=1000):
    return {"sequence": sequence, "request_id": rid(sequence), "op": op, "state": state, "latency_us": latency_us}


def arrival_report(records, window_s=1.0):
    return {"request_id_prefix": PREFIX, "request_id_sequence_bits": BITS, "records": records,
            "observed_duration_s": window_s, "configured_rate_per_s": 10.0}


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


class VerdictTests(unittest.TestCase):
    def report(self, sink_lines, records=None, **limits):
        records = records if records is not None else [arrival(0, "create")]
        return war.build_point_report(arrival_report(records), {"writer_timeouts.1.ndjson": sink_lines}, **limits)

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

    def test_missing_prefix_or_rows_is_broken(self):
        with self.assertRaisesRegex(war.ReportError, "no request id prefix"):
            war.build_point_report({"records": [], "observed_duration_s": 1.0}, {"f": [line(STARTUP)]})
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
