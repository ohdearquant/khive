"""Negative protocol examples, independent of the Rust producers."""

import json
import unittest
from check import check_list, check_report, tail_ids


class ProtocolTests(unittest.TestCase):
    def setUp(self):
        self.ids = tail_ids()
        self.rows = [
            dict(id=id_, statistic="median_ns", value=12, n=1)
            for id_ in sorted(self.ids)
        ]

    def output(self, rows=None):
        return "\n".join(
            "BENCH-JSON " + json.dumps(row)
            for row in (self.rows if rows is None else rows)
        )

    def test_valid_protocol_and_human_diagnostics(self):
        check_list("\n".join(id_ + ": benchmark" for id_ in sorted(self.ids)), self.ids)
        check_report(
            "diagnostic\n" + self.output() + "\nKNOWLEDGE_TAIL_REPLAY {}", self.ids
        )

    def test_listing_rejects_workload_output(self):
        with self.assertRaises(ValueError):
            check_list(
                "\n".join(id_ + ": benchmark" for id_ in self.ids)
                + "\nworkload started",
                self.ids,
            )

    def test_listing_rejects_duplicate_and_missing_id(self):
        ids = sorted(self.ids)
        for changed in (ids[:-1], ids + [ids[0]]):
            with self.assertRaises(ValueError):
                check_list("\n".join(id_ + ": benchmark" for id_ in changed), self.ids)

    def test_missing_report(self):
        with self.assertRaises(ValueError):
            check_report(self.output(self.rows[:-1]), self.ids)

    def test_duplicate_report(self):
        with self.assertRaises(ValueError):
            check_report(self.output(self.rows + [self.rows[0]]), self.ids)

    def test_invalid_measurement(self):
        for value in (True, "12", 0, -1, float("nan"), float("inf")):
            with self.subTest(value=value), self.assertRaises(ValueError):
                check_report(
                    self.output([{**r, "value": value} for r in self.rows]), self.ids
                )

    def test_wrong_count_statistic_and_fields(self):
        for patch in (
            {"n": True},
            {"n": 0},
            {"n": 2},
            {"statistic": "p95_ns"},
            {"extra": 1},
        ):
            with self.subTest(patch=patch), self.assertRaises(ValueError):
                check_report(self.output([{**r, **patch} for r in self.rows]), self.ids)

    def test_wrong_cohort(self):
        with self.assertRaises(ValueError):
            check_report(self.output(), tail_ids((64,), 4))


if __name__ == "__main__":
    unittest.main()
