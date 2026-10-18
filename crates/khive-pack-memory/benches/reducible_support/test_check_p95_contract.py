"""Pure Python checker controls; these do not invoke Rust or model workloads."""
import json
import subprocess
import unittest
from unittest.mock import patch

import check_p95_contract as contract


class ContractTests(unittest.TestCase):
    def setUp(self):
        self.ids = {"p95_gate/synthetic-v1/" + x for x in ("m1", "m3", "m4-informational")}
        self.rows = [dict(id=x, statistic="p95_ns", value=250_000.0, n=200) for x in sorted(self.ids)]

    def encode(self, rows):
        return "human table\n" + "\n".join("BENCH-JSON " + json.dumps(row) for row in rows)

    def test_valid_output(self):
        self.assertEqual(set(contract.parse_reports(self.encode(self.rows), self.ids)), self.ids)

    def test_missing_row(self):
        with self.assertRaises(ValueError):
            contract.parse_reports(self.encode(self.rows[:-1]), self.ids)

    def test_duplicate_and_unknown(self):
        for rows in (self.rows + self.rows[:1], [dict(self.rows[0], id="unexpected")] + self.rows[1:]):
            with self.subTest(rows=rows), self.assertRaises(ValueError):
                contract.parse_reports(self.encode(rows), self.ids)

    def test_bad_measurement(self):
        for value in (0, -1, float("nan"), float("inf"), True, "12"):
            rows = [dict(self.rows[0], value=value)] + self.rows[1:]
            with self.subTest(value=value), self.assertRaises(ValueError):
                contract.parse_reports(self.encode(rows), self.ids)

    def test_wrong_units_or_sample_count(self):
        for change in (dict(statistic="recall_at_10"), dict(n=0), dict(n=True), dict(n=199)):
            with self.subTest(change=change), self.assertRaises(ValueError):
                contract.parse_reports(self.encode([dict(self.rows[0], **change)] + self.rows[1:]), self.ids)

    def test_closed_report_shape(self):
        for row in (dict(self.rows[0], extra=1), {k: v for k, v in self.rows[0].items() if k != "n"}):
            with self.subTest(row=row), self.assertRaises(ValueError):
                contract.parse_reports(self.encode([row] + self.rows[1:]), self.ids)

    def test_bad_listing(self):
        for text in ("", "x: benchmark\nx: benchmark", "workload started", "BENCH-JSON {}"):
            with self.subTest(text=text), self.assertRaises(ValueError):
                contract.parse_list(text)
        self.assertEqual(contract.parse_list("x: benchmark\ny: benchmark\n"), {"x", "y"})

    def test_nonzero_probe_is_rejected(self):
        with patch.object(contract.subprocess, "run", return_value=subprocess.CompletedProcess([], 101, "", "list sentinel")):
            with self.assertRaisesRegex(ValueError, "list sentinel"):
                contract.invoke("unused", [], {}, 5)

    def test_timeout_remains_failure(self):
        with patch.object(contract.subprocess, "run", side_effect=subprocess.TimeoutExpired("unused", 5)):
            with self.assertRaises(subprocess.TimeoutExpired):
                contract.invoke("unused", [], {}, 5)


if __name__ == "__main__":
    unittest.main()
