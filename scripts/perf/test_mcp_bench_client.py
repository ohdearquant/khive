#!/usr/bin/env python3
"""Unit tests for scripts/perf/mcp_bench_client.py (stdlib unittest, no deps).

Run: cd scripts/perf && python3 -m unittest test_mcp_bench_client -v

No real kkernel binary, no production DB: the stdio-MCP layer is exercised
against an in-memory fake subprocess, and the raw daemon-socket layer is
exercised against a tiny mock Unix-socket server speaking the same
length-prefixed JSON frame protocol as the real daemon, bound to a scratch
tempdir path per test.
"""

from __future__ import annotations

import io
import json
import os
import socket as socketlib
import struct
import sys
import tempfile
import threading
import unittest

sys.path.insert(0, os.path.dirname(__file__))
import mcp_bench_client as mbc


# ── Fake stdio MCP subprocess ──────────────────────────────────────────────

class _FakeProc:
    """Stands in for subprocess.Popen for the stdio MCP layer: `stdin` is a
    BytesIO the test can inspect after the call, `stdout` is pre-loaded with
    the exact response lines the fake server would have written.
    """

    def __init__(self, response_lines: list[dict]):
        self.stdin = io.BytesIO()
        self._responses = [json.dumps(r).encode() + b"\n" for r in response_lines]
        self._next = 0

    @property
    def stdout(self):
        return self

    def readline(self):
        if self._next >= len(self._responses):
            return b""
        line = self._responses[self._next]
        self._next += 1
        return line

    def written_requests(self):
        self.stdin.seek(0)
        return [json.loads(line) for line in self.stdin.read().splitlines() if line]


class StdioMcpTransportTests(unittest.TestCase):
    def test_handshake_sends_initialize_and_initialized_notification(self):
        proc = _FakeProc([{"jsonrpc": "2.0", "id": 1, "result": {}}])
        mbc.handshake(proc, client_name="unit-test", client_version="9.9.9")
        reqs = proc.written_requests()
        self.assertEqual(reqs[0]["method"], "initialize")
        self.assertEqual(reqs[0]["params"]["clientInfo"]["name"], "unit-test")
        self.assertEqual(reqs[1]["method"], "notifications/initialized")

    def test_handshake_raises_on_error_response(self):
        proc = _FakeProc([{"jsonrpc": "2.0", "id": 1, "error": {"message": "boom"}}])
        with self.assertRaises(RuntimeError):
            mbc.handshake(proc)

    def test_call_request_decodes_content_text_as_json(self):
        body = {"results": [{"ok": True, "result": {"count": 3}}]}
        proc = _FakeProc([
            {"jsonrpc": "2.0", "id": 1, "result": {"content": [{"type": "text", "text": json.dumps(body)}]}}
        ])
        out = mbc.call_request(proc, "stats()")
        self.assertEqual(out, body)

    def test_call_request_raises_on_rpc_error(self):
        proc = _FakeProc([{"jsonrpc": "2.0", "id": 1, "error": {"message": "bad ops"}}])
        with self.assertRaises(RuntimeError):
            mbc.call_request(proc, "not_a_verb()")

    def test_call_request_raises_on_protocol_error(self):
        proc = _FakeProc([
            {
                "jsonrpc": "2.0",
                "id": 1,
                "result": {"isError": True, "content": [{"type": "text", "text": "parse failure"}]},
            }
        ])
        with self.assertRaises(RuntimeError):
            mbc.call_request(proc, "(((")

    def test_call_verb_unwraps_first_result(self):
        body = {"results": [{"ok": True, "result": {"entities": 5}}], "summary": {"total": 1}}
        proc = _FakeProc([
            {"jsonrpc": "2.0", "id": 1, "result": {"content": [{"type": "text", "text": json.dumps(body)}]}}
        ])
        out = mbc.call_verb(proc, "stats", {})
        self.assertEqual(out, {"entities": 5})

    def test_call_verb_raises_on_failed_op(self):
        body = {"results": [{"ok": False, "error": "invalid kind"}]}
        proc = _FakeProc([
            {"jsonrpc": "2.0", "id": 1, "result": {"content": [{"type": "text", "text": json.dumps(body)}]}}
        ])
        with self.assertRaises(RuntimeError):
            mbc.call_verb(proc, "create", {"kind": "bogus"})


# ── Percentile ────────────────────────────────────────────────────────────────

class PercentileTests(unittest.TestCase):
    def test_empty_list_returns_zero(self):
        self.assertEqual(mbc.pct([], 0.5), 0.0)

    def test_nearest_rank_p50_and_p99(self):
        data = list(range(1, 101))  # 1..100
        self.assertEqual(mbc.pct(data, 0.5), 51)
        self.assertEqual(mbc.pct(data, 0.99), 100)


# ── Safety guard ──────────────────────────────────────────────────────────────

class SafetyGuardTests(unittest.TestCase):
    def test_scratch_path_is_not_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            # Must not raise/exit for an ordinary scratch path.
            mbc.assert_not_live_db(os.path.join(tmp, "bench.db"))

    def test_live_db_path_exits(self):
        with self.assertRaises(SystemExit):
            mbc.assert_not_live_db(os.path.expanduser("~/.khive/khive.db"))


# ── Daemon-engagement proof (process-facing helpers) ──────────────────────────

class DaemonEngagementTests(unittest.TestCase):
    def test_read_pid_file_round_trip(self):
        with tempfile.TemporaryDirectory() as tmp:
            pid_path = os.path.join(tmp, "khived.pid")
            with open(pid_path, "w") as f:
                f.write(str(os.getpid()))
            pid, raw = mbc.read_pid_file(pid_path)
            self.assertEqual(pid, os.getpid())
            self.assertEqual(raw, str(os.getpid()))

    def test_read_pid_file_missing_returns_none(self):
        pid, raw = mbc.read_pid_file("/nonexistent/path/khived.pid")
        self.assertIsNone(pid)
        self.assertIsNone(raw)

    def test_pid_alive_true_for_self(self):
        self.assertTrue(mbc.pid_alive(os.getpid()))

    def test_pid_alive_false_for_unlikely_pid(self):
        self.assertFalse(mbc.pid_alive(2**30))

    def test_argv_is_khive_daemon_false_for_test_runner(self):
        # This test process is a python unittest runner, never kkernel.
        self.assertFalse(mbc.argv_is_khive_daemon(os.getpid()))

    def test_assert_no_daemon_spawned_passes_when_socket_absent(self):
        with tempfile.TemporaryDirectory() as tmp:
            mbc.assert_no_daemon_spawned(os.path.join(tmp, "khived.sock"))

    def test_assert_no_daemon_spawned_exits_when_socket_present(self):
        with tempfile.TemporaryDirectory() as tmp:
            sock_path = os.path.join(tmp, "khived.sock")
            s = socketlib.socket(socketlib.AF_UNIX, socketlib.SOCK_STREAM)
            s.bind(sock_path)
            try:
                with self.assertRaises(SystemExit):
                    mbc.assert_no_daemon_spawned(sock_path)
            finally:
                s.close()

    def test_assert_daemon_engaged_exits_when_socket_missing(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(SystemExit):
                mbc.assert_daemon_engaged(
                    os.path.join(tmp, "missing.sock"),
                    os.path.join(tmp, "missing.pid"),
                    os.path.join(tmp, "bench.db"),
                )


# ── Mock raw daemon-socket server ─────────────────────────────────────────────

class _MockDaemonServer:
    """A minimal Unix-socket server speaking the daemon's length-prefixed JSON
    frame protocol, so the raw-frame layer can be tested without a real
    kkernel binary. `responder(request_frame) -> response_frame | None`
    (returning None closes the connection without writing a response, to
    simulate a hang for timeout tests).
    """

    def __init__(self, responder, delay_s: float = 0.0):
        self._tmp = tempfile.TemporaryDirectory()
        self.sock_path = os.path.join(self._tmp.name, "khived.sock")
        self._responder = responder
        self._delay_s = delay_s
        self._server = socketlib.socket(socketlib.AF_UNIX, socketlib.SOCK_STREAM)
        self._server.bind(self.sock_path)
        self._server.listen(16)
        self._stop = False
        self._thread = threading.Thread(target=self._serve, daemon=True)
        self._thread.start()

    def _serve(self):
        while not self._stop:
            self._server.settimeout(0.2)
            try:
                conn, _ = self._server.accept()
            except socketlib.timeout:
                continue
            except OSError:
                return
            threading.Thread(target=self._handle, args=(conn,), daemon=True).start()

    def _handle(self, conn):
        try:
            len_buf = mbc.recv_exact(conn, 4)
            (length,) = struct.unpack(">I", len_buf)
            raw = mbc.recv_exact(conn, length)
            frame = json.loads(raw)
            if self._delay_s:
                import time as _time
                _time.sleep(self._delay_s)
            resp = self._responder(frame)
            if resp is None:
                return
            payload = json.dumps(resp).encode()
            conn.sendall(struct.pack(">I", len(payload)) + payload)
        except Exception:
            pass
        finally:
            conn.close()

    def close(self):
        self._stop = True
        self._thread.join(timeout=2)
        self._server.close()
        self._tmp.cleanup()


def _base_response(frame, **overrides):
    resp = {
        "ok": True,
        "result": None,
        "error": None,
        "namespace_mismatch": False,
        "config_mismatch": False,
        "served_config_id": "cfg-under-test",
        "version_mismatch": False,
        "daemon_protocol_version": mbc.PROTOCOL_VERSION,
        "metrics": None,
    }
    resp.update(overrides)
    return resp


class RawDaemonFrameTests(unittest.TestCase):
    def test_base_daemon_frame_shape(self):
        frame = mbc.base_daemon_frame("stats()", "cfg1", probe_only=True, metrics_only=False)
        self.assertEqual(frame["ops"], "stats()")
        self.assertEqual(frame["config_id"], "cfg1")
        self.assertTrue(frame["probe_only"])
        self.assertFalse(frame["metrics_only"])
        self.assertEqual(frame["protocol_version"], mbc.PROTOCOL_VERSION)

    def test_raw_daemon_roundtrip_echoes_response(self):
        server = _MockDaemonServer(lambda frame: _base_response(frame))
        try:
            resp = mbc.raw_daemon_roundtrip(
                server.sock_path, mbc.base_daemon_frame("stats()", "cfg-under-test", probe_only=False)
            )
            self.assertTrue(resp["ok"])
            self.assertEqual(resp["served_config_id"], "cfg-under-test")
        finally:
            server.close()

    def test_raw_daemon_roundtrip_times_out_on_slow_server(self):
        server = _MockDaemonServer(lambda frame: _base_response(frame), delay_s=1.0)
        try:
            with self.assertRaises(socketlib.timeout):
                mbc.raw_daemon_roundtrip(server.sock_path, mbc.base_daemon_frame("", "cfg", True), timeout_s=0.2)
        finally:
            server.close()

    def test_probe_metrics_snapshot_live_when_metrics_key_present(self):
        def responder(frame):
            if frame.get("metrics_only"):
                return _base_response(frame, metrics=None if False else {"wal_pages": 12})
            return _base_response(frame)

        server = _MockDaemonServer(responder)
        try:
            out = mbc.probe_metrics_snapshot(server.sock_path)
            self.assertEqual(out["oracle"], "LIVE")
            self.assertEqual(out["metrics"], {"wal_pages": 12})
            self.assertEqual(out["config_id"], "cfg-under-test")
        finally:
            server.close()

    def test_probe_metrics_snapshot_pending_when_metrics_key_absent(self):
        server = _MockDaemonServer(lambda frame: _base_response(frame))
        try:
            out = mbc.probe_metrics_snapshot(server.sock_path)
            self.assertEqual(out["oracle"], "PENDING")
        finally:
            server.close()

    def test_probe_metrics_snapshot_pending_on_connection_failure(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = mbc.probe_metrics_snapshot(os.path.join(tmp, "no-such.sock"))
            self.assertEqual(out["oracle"], "PENDING")
            self.assertIsNone(out["config_id"])


class ConcurrentFrameFloorTests(unittest.TestCase):
    def test_measure_concurrent_frames_all_success(self):
        server = _MockDaemonServer(lambda frame: _base_response(frame))
        try:
            frame = mbc.base_daemon_frame("stats()", "cfg-under-test", probe_only=False)
            out = mbc.measure_concurrent_frames(server.sock_path, frame, attempts=20, concurrency=5, deadline_ms=2000)
            self.assertEqual(out["attempts"], 20)
            self.assertEqual(out["successes"], 20)
            self.assertEqual(out["timed_out"], 0)
            self.assertEqual(out["errors_by_code"], {})
            self.assertIsNotNone(out["p50_us"])
            self.assertIsNotNone(out["max_us"])
        finally:
            server.close()

    def test_measure_concurrent_frames_censors_timeouts_not_into_latency(self):
        server = _MockDaemonServer(lambda frame: None, delay_s=0.5)
        try:
            frame = mbc.base_daemon_frame("stats()", "cfg", probe_only=False)
            out = mbc.measure_concurrent_frames(server.sock_path, frame, attempts=6, concurrency=3, deadline_ms=100)
            self.assertEqual(out["attempts"], 6)
            self.assertEqual(out["successes"], 0)
            self.assertEqual(out["timed_out"], 6)
            self.assertIsNone(out["p50_us"])
            self.assertIsNone(out["max_us"])
        finally:
            server.close()

    def test_measure_concurrent_frames_classifies_connection_errors(self):
        # No server listening at this path at all.
        with tempfile.TemporaryDirectory() as tmp:
            frame = mbc.base_daemon_frame("stats()", "cfg", probe_only=False)
            out = mbc.measure_concurrent_frames(
                os.path.join(tmp, "absent.sock"), frame, attempts=4, concurrency=2, deadline_ms=500
            )
            self.assertEqual(out["attempts"], 4)
            self.assertEqual(out["successes"], 0)
            self.assertEqual(out["timed_out"], 0)
            self.assertEqual(sum(out["errors_by_code"].values()), 4)

    def test_measure_probe_only_floor_and_stats_dispatch_floor_build_expected_ops(self):
        seen_ops = []

        def responder(frame):
            seen_ops.append((frame["ops"], frame["probe_only"], frame["metrics_only"]))
            return _base_response(frame)

        server = _MockDaemonServer(responder)
        try:
            mbc.measure_probe_only_floor(server.sock_path, "cfg", attempts=3, concurrency=3, deadline_ms=1000)
            mbc.measure_stats_dispatch_floor(server.sock_path, "cfg", attempts=3, concurrency=3, deadline_ms=1000)
        finally:
            server.close()
        probe_ops = [o for o in seen_ops if o[1] is True]
        stats_ops = [o for o in seen_ops if o[1] is False]
        self.assertEqual(len(probe_ops), 3)
        self.assertTrue(all(op[0] == "" for op in probe_ops))
        self.assertEqual(len(stats_ops), 3)
        self.assertTrue(all(op[0] == "stats()" for op in stats_ops))
        self.assertTrue(all(op[2] is False for op in stats_ops))


if __name__ == "__main__":
    unittest.main()
