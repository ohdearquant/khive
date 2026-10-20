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

    def test_probe_reports_require_explicit_statistic(self):
        probes = [dict(row, statistic="report_contract_probe") for row in self.rows]
        with self.assertRaisesRegex(ValueError, "expected report statistic p95_ns"):
            contract.parse_reports(self.encode(probes), self.ids)
        self.assertEqual(
            set(contract.parse_reports(
                self.encode(probes), self.ids, expected_statistic="report_contract_probe"
            )),
            self.ids,
        )
        with self.assertRaisesRegex(ValueError, "expected report statistic report_contract_probe"):
            contract.parse_reports(
                self.encode(self.rows), self.ids, expected_statistic="report_contract_probe"
            )

    def test_mixed_report_statistics_are_rejected(self):
        rows = [dict(self.rows[0], statistic="report_contract_probe")] + self.rows[1:]
        for statistic in ("p95_ns", "report_contract_probe"):
            with self.subTest(statistic=statistic), self.assertRaises(ValueError):
                contract.parse_reports(self.encode(rows), self.ids, expected_statistic=statistic)

    def test_measurement_check_rejects_probe_rows(self):
        def output(binary, args, environment, timeout):
            cohort = "real-model" if environment["KHIVE_BENCH_REAL_MODEL"] == "1" else "synthetic-v1"
            ids = ["p95_gate/" + cohort + "/" + case for case in ("m1", "m3", "m4-informational")]
            if "--list" in args:
                return "\n".join(name + ": benchmark" for name in ids)
            if "--regression-case=report-contract" in args:
                statistic = "report_contract_probe"
            else:
                self.assertEqual(args, ["--bench"])
                statistic = measured_statistic
            return self.encode([dict(id=name, statistic=statistic, value=250_000.0, n=200) for name in ids])

        with (
            patch.object(contract, "check_smoke"),
            patch.object(contract, "check_argument_refusals"),
            patch.object(contract, "check_caller_argv"),
            patch.object(contract, "check_lifecycle"),
            patch.dict(contract.os.environ, {"KHIVE_BENCH_REAL_MODEL": "0"}),
            patch.object(contract, "invoke", side_effect=output) as invoke,
        ):
            measured_statistic = "p95_ns"
            contract.check("unused", measure=True)
            self.assertEqual(invoke.call_args.args[1], ["--bench"])
            measured_statistic = "report_contract_probe"
            with self.assertRaisesRegex(ValueError, "expected report statistic p95_ns"):
                contract.check("unused", measure=True)
            self.assertEqual(invoke.call_args.args[1], ["--bench"])

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
