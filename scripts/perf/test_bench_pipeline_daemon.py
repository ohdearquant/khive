#!/usr/bin/env python3
"""Response-shape regressions for the deterministic pipeline quality gate."""

import contextlib
import io
import unittest
from unittest.mock import Mock, patch

import bench_pipeline_daemon as pipeline


class RecallScoreInputTests(unittest.TestCase):
    def score(self, response):
        with patch.object(pipeline, "_call_verb", return_value=response):
            _, contents = pipeline._query_once(None, "query")
        return pipeline.precision_at_k(contents, "topic")

    def test_excess_hits_cannot_raise_precision_above_one(self):
        rows = [{"content": f"topic note {i}"} for i in range(20)]
        with self.assertRaisesRegex(ValueError, "requested limit"):
            self.score({"results": rows})

    def test_eleventh_hit_cannot_rescue_first_ten_below_floor(self):
        rows = ([{"content": f"topic note {i}"} for i in range(6)]
                + [{"content": f"other note {i}"} for i in range(4)]
                + [{"content": "topic note 11"}])
        with self.assertRaisesRegex(ValueError, "requested limit"):
            self.score({"results": rows})

    def test_repeated_hit_cannot_count_ten_times(self):
        with self.assertRaisesRegex(ValueError, "duplicate recall content"):
            self.score({"results": [{"content": "topic note 1"}] * 10})

    def test_missing_content_and_conflicting_list_fields_fail(self):
        with self.assertRaisesRegex(ValueError, "string content"):
            self.score({"results": [{"id": "missing-content"}]})
        with self.assertRaisesRegex(ValueError, "exactly one"):
            self.score({"results": [], "items": [{"content": f"topic {i}"} for i in range(7)]})

    def test_distinct_short_and_empty_controls_keep_their_scores(self):
        rows = ([{"content": f"topic note {i}"} for i in range(7)]
                + [{"content": f"other note {i}"} for i in range(3)])
        self.assertEqual(self.score({"results": rows}), 0.7)
        self.assertEqual(self.score([{"content": "topic note 1"}]), 0.1)
        self.assertEqual(self.score({"results": []}), 0.0)


class AuditSuccessRowsLauncherTests(unittest.TestCase):
    def run_main_to_settle(self, refuse=False):
        events, process = [], Mock()
        def preflight(proc):
            self.assertIs(proc, process)
            events.append("preflight")
            if refuse:
                raise RuntimeError("reduced mode refused")
        def settle(*args):
            events.append("settle")
            raise RuntimeError("settle boundary")
        with contextlib.ExitStack() as stack:
            stack.enter_context(contextlib.redirect_stdout(io.StringIO()))
            stack.enter_context(patch.dict(pipeline.os.environ, {
                "KHIVE_AUDIT_SUCCESS_ROWS": "reduced", "KKERNEL_BENCH_BINARY": "fixture-binary"}))
            patches = [patch.object(pipeline.pathlib.Path, "exists", return_value=True),
                       patch.object(pipeline.tempfile, "mkdtemp", return_value="/fixture"),
                       patch.object(pipeline, "_assert_not_live_db"),
                       patch.object(pipeline.subprocess, "Popen", return_value=process),
                       patch.object(pipeline, "_handshake"),
                       patch.object(pipeline, "generate_corpus", return_value=["fixture"]),
                       patch.object(pipeline, "_ingest"),
                       patch.object(pipeline, "_call_request", return_value={"summary": {"failed": 0}}),
                       patch.object(pipeline, "_query_once", return_value=(1, ["fixture"])),
                       patch.object(pipeline, "assert_daemon_engaged", side_effect=lambda *a, **k: events.append("engaged")),
                       patch.object(pipeline.mbc, "require_all_audit_success_rows", side_effect=preflight),
                       patch.object(pipeline, "_wait_for_ann_convergence", side_effect=settle),
                       patch.object(pipeline, "_append_ledger"),
                       patch.object(pipeline, "_teardown_daemon")]
            mocks = [stack.enter_context(selected) for selected in patches]
            with self.assertRaisesRegex(RuntimeError, "reduced mode refused" if refuse else "settle boundary"):
                pipeline.main()
        process.stdin.close.assert_called_once()
        process.wait.assert_called_once()
        mocks[-1].assert_called_once()
        mocks[-2].assert_not_called()
        self.assertEqual(mocks[8].call_count, 1, "only bootstrap warmup may precede preflight")
        return events, mocks[3].call_args.kwargs["env"]

    def test_pipeline_requires_all_before_measuring(self):
        events, env = self.run_main_to_settle()
        self.assertEqual(events, ["engaged", "preflight", "settle"])
        self.assertEqual(env["KHIVE_AUDIT_SUCCESS_ROWS"], "all")

    def test_pipeline_refusal_prevents_measuring(self):
        events, _ = self.run_main_to_settle(refuse=True)
        self.assertEqual(events, ["engaged", "preflight"])

    def test_nodaemon_launches_all_before_its_control_operations(self):
        events, process = [], Mock()
        with contextlib.ExitStack() as stack:
            stack.enter_context(contextlib.redirect_stdout(io.StringIO()))
            stack.enter_context(patch.dict(pipeline.os.environ, {"KHIVE_AUDIT_SUCCESS_ROWS": "reduced"}))
            patches = [patch.object(pipeline.tempfile, "mkdtemp", return_value="/fixture-control"),
                       patch.object(pipeline, "_assert_not_live_db"),
                       patch.object(pipeline.subprocess, "Popen", return_value=process),
                       patch.object(pipeline, "_handshake"),
                       patch.object(pipeline.mbc, "require_all_audit_success_rows", side_effect=lambda p: events.append("preflight")),
                       patch.object(pipeline, "generate_corpus", return_value=["fixture"]),
                       patch.object(pipeline, "_call_request", side_effect=lambda *a: events.append("ingest")),
                       patch.object(pipeline, "_call_verb", side_effect=lambda *a: events.append("recall")),
                       patch.object(pipeline, "assert_no_daemon_spawned", side_effect=lambda *a, **k: events.append("no-daemon")),
                       patch("shutil.rmtree")]
            mocks = [stack.enter_context(selected) for selected in patches]
            pipeline._run_nodaemon_control("fixture-binary")
        self.assertEqual(events, ["preflight", "ingest", "recall", "no-daemon"])
        env = mocks[2].call_args.kwargs["env"]
        self.assertEqual(env["KHIVE_AUDIT_SUCCESS_ROWS"], "all")
        self.assertEqual(env["KHIVE_NO_DAEMON"], "1")
        process.stdin.close.assert_called_once()
        process.wait.assert_called_once()


if __name__ == "__main__":
    unittest.main()
