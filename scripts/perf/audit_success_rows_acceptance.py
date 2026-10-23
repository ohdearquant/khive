#!/usr/bin/env python3
"""Native host acceptance for success-audit joins (issue #5948).

The two live cases require an explicitly supplied built binary. --self-test
checks only JSON/fixture helpers and never starts a binary or opens a socket.
"""
from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time
import unittest

import mcp_bench_client as mbc

SELECTED_50 = (0, 2, 8, 12, 34, 47)
SAMPLED = ("comm.heartbeat", "comm.cleanup_expired_quarantine")
OMITTED_BENCH = ("comm.inbox", "comm.probe", "whoami")


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), allow_nan=False).encode()


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def source_lock_path():
    return Path(__file__).resolve().parents[2] / "crates" / "Cargo.lock"


def iso_now():
    return dt.datetime.now(dt.timezone.utc).isoformat(timespec="microseconds")


def poll_args(verb):
    if verb == "comm.probe":
        return {"actor": "audit-acceptance"}
    if verb == "comm.heartbeat":
        return {"channel_kind": "email", "channel_slug": "acceptance", "outcome": "success"}
    if verb == "comm.cleanup_expired_quarantine":
        return {"channel_kind": "email", "channel_slug": "acceptance"}
    return {}


def check_frame(reply, config_id, request_id=None):
    assert reply.get("ok") is True, f"daemon request failed: {reply}"
    assert not any(reply.get(k) for k in ("config_mismatch", "version_mismatch", "namespace_mismatch")), reply
    assert reply.get("served_config_id") == config_id, reply
    assert reply.get("daemon_protocol_version") == mbc.PROTOCOL_VERSION, reply
    if request_id is not None:
        assert type(reply.get("request_id")) is int and reply["request_id"] == request_id, reply
    body = json.loads(reply["result"])
    entries = body.get("results")
    assert isinstance(entries, list) and len(entries) == 1 and entries[0].get("ok") is True, body
    return entries[0]["result"]


def project_audits(events):
    """Adapt complete stored Events to the existing join helper's flat input."""
    projected = []
    for event in events:
        if event.get("kind") != "audit":
            continue
        resource = event.get("payload", {}).get("resource")
        assert isinstance(resource, dict), event
        projected.append({**event, "resource": resource})
    return projected


def require_joins(samples, events):
    joined = mbc.join_request_ids(samples, project_audits(events))
    assert len(joined) == len(samples)
    missing = [row for row in joined if row["join_status"] != "joined"]
    assert not missing, f"poll_samples_join_one_audit_row_each: {len(missing)} missing/ambiguous audit rows: {missing}"
    assert all(type(row["server_duration_us"]) is int and row["server_duration_us"] >= 0 for row in joined), joined
    return joined


def require_maintenance(samples, events, before, after):
    audits = [row for row in project_audits(events) if row.get("verb") in SAMPLED]
    assert len(samples) == 100 and len(audits) == 12, (len(samples), len(audits))
    for verb in SAMPLED:
        sent = [row for row in samples if row["verb"] == verb]
        kept = [row for row in audits if row["verb"] == verb]
        assert len(sent) == 50 and len(kept) == 6
        actual = sorted(row["resource"]["sampling"]["seq"] for row in kept)
        assert actual == list(SELECTED_50), (verb, actual)
        for row in kept:
            metadata = row["resource"]["sampling"]
            assert canonical(metadata) == canonical({"every": 10, "seq": metadata["seq"], "selected_by": "sample"}), row
        selected_samples = [row for row in sent if row["sequence"] in SELECTED_50]
        require_joins(selected_samples, events)
        skipped_samples = [row for row in sent if row["sequence"] not in SELECTED_50]
        skipped = mbc.join_request_ids(skipped_samples, audits)
        assert len(skipped) == 44 and all(row["join_status"] == "no_audit_row" for row in skipped)
        assert counter(after, "audit_success_rows_sampled_out", verb) - counter(before, "audit_success_rows_sampled_out", verb) == 44
        assert counter(after, "audit_success_rows_forced", verb) - counter(before, "audit_success_rows_forced", verb) == 0


def find_field(value, key):
    if isinstance(value, dict):
        if key in value:
            return value[key]
        for item in value.values():
            found = find_field(item, key)
            if found is not None:
                return found
    if isinstance(value, list):
        for item in value:
            found = find_field(item, key)
            if found is not None:
                return found
    return None


def counter(value, kind, verb):
    mapping = find_field(value, kind)
    assert isinstance(mapping, dict) and type(mapping.get(verb)) is int, (kind, verb, value)
    return mapping[verb]


def read_pages(call, since, until):
    args = {"since": since, "until": until, "kind": "audit", "limit": 1000}
    events, cursors = [], set()
    for _ in range(20):
        page = call("brain.event_page", args)
        assert isinstance(page.get("events"), list) and page.get("count") == len(page["events"]), page
        events.extend(page["events"])
        if page.get("has_more") is False:
            assert page.get("next_after") is None, page
            ids = [row["id"] for row in events]
            assert len(ids) == len(set(ids)), "duplicate event IDs across pages"
            return events
        cursor = page.get("next_after")
        assert isinstance(cursor, str) and cursor and cursor not in cursors, page
        cursors.add(cursor)
        args = {**args, "after": cursor}
    raise AssertionError("audit page bound exhausted")


def run_live(binary, case, receipt):
    binary = Path(binary).resolve(strict=True)
    assert binary.is_file() and os.access(binary, os.X_OK), binary
    mode = "all" if case == "benchmark-all" else "reduced"
    scratch = tempfile.mkdtemp(prefix="kh-as-", dir="/tmp")
    work = Path(scratch)
    config = work / "config.toml"
    config.write_text("")
    db, sock, pid = (work / name for name in ("data.db", "s", "p"))
    mbc.assert_not_live_db(db)
    env = {key: value for key, value in os.environ.items() if key in ("PATH", "DYLD_FALLBACK_LIBRARY_PATH", "LD_LIBRARY_PATH")}
    env.update({"KHIVE_DB": str(db), "KHIVE_SOCKET": str(sock), "KHIVE_PID": str(pid), "KHIVE_LOCK": str(work / "l"), "KHIVE_RECOVERER_LOCK": str(work / "r"), "KHIVE_CONFIG": str(config), "KHIVE_PACKS": "kg,brain,comm", "KHIVE_ACTOR": "audit-acceptance", "KHIVE_AUDIT_SUCCESS_ROWS": mode, "KHIVE_EVENTS_SPLIT": "0", "KHIVE_DAEMON_STRICT": "1", "XDG_CONFIG_HOME": str(work / "config"), "XDG_CACHE_HOME": str(work / "cache")})
    command = [str(binary), "mcp", "--log", "warn"]
    report = {"case": case, "mode": mode, "status": "FAIL", "command": command, "environment": env, "binary_sha256": digest(binary), "source_sha256": digest(__file__), "lock_sha256": digest(source_lock_path()), "config_sha256": digest(config), "protocol_version": mbc.PROTOCOL_VERSION, "samples": [], "replies": [], "rows": [], "teardown": "pending"}
    proc = None
    stderr = open(work / "stderr", "wb")
    try:
        proc = subprocess.Popen(command, cwd=work, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr)
        mbc.handshake(proc, client_name="audit-success-acceptance")
        mbc.call_verb(proc, "stats", {})
        mbc.assert_daemon_engaged(str(sock), str(pid), str(db), label=case)
        if case == "benchmark-all":
            mbc.require_all_audit_success_rows(proc)
        discovery = mbc.raw_daemon_roundtrip(str(sock), mbc.base_daemon_frame("", "__audit_discovery__", probe_only=True))
        config_id = discovery.get("served_config_id")
        assert isinstance(config_id, str) and config_id and not discovery.get("version_mismatch"), discovery
        report["discovery"] = discovery
        next_id = 1

        def call(verb, args):
            nonlocal next_id
            rid = next_id
            next_id += 1
            frame = mbc.base_daemon_frame(json.dumps([{"tool": verb, "args": args}]), config_id, request_id=rid)
            frame["actor_id"] = "audit-acceptance"
            frame["format"] = "json"
            frame["presentation"] = "verbose"
            started = time.monotonic_ns()
            reply = mbc.raw_daemon_roundtrip(str(sock), frame, timeout_s=15)
            ended = time.monotonic_ns()
            result = check_frame(reply, config_id, rid)
            report["replies"].append({"frame": frame, "reply": reply})
            return result, {"request_id": rid, "verb": verb, "client_send_ts": started, "client_recv_ts": ended}

        before, _ = call("db_diagnostics", {"size_composition": False})
        assert before.get("audit_success_rows_mode") == mode, before
        report["before"] = before
        since = iso_now()
        verbs = OMITTED_BENCH if case == "benchmark-all" else SAMPLED
        for verb in verbs:
            for sequence in range(50):
                result, sample = call(verb, poll_args(verb))
                if verb == "comm.heartbeat":
                    assert result.get("outcome") == "success", result
                if verb == "comm.cleanup_expired_quarantine":
                    assert type(result.get("deleted")) is int and result["deleted"] == 0, result
                    assert type(result.get("routed_owners_detached")) is int and result["routed_owners_detached"] == 0, result
                report["samples"].append({**sample, "sequence": sequence, "result": result})
        until = iso_now()
        report["window"] = {"since": since, "until": until}
        report["rows"] = read_pages(lambda verb, args: call(verb, args)[0], since, until)
        after, _ = call("db_diagnostics", {"size_composition": False})
        report["after"] = after
        if case == "benchmark-all":
            assert len(report["samples"]) == 150
            report["joined"] = require_joins(report["samples"], report["rows"])
            assert len(report["rows"]) == 150
        else:
            require_maintenance(report["samples"], report["rows"], before, after)
        report["status"] = "PASS"
    except BaseException as error:
        report["error"] = repr(error)
        raise
    finally:
        if proc is not None:
            try:
                proc.stdin.close()
                proc.wait(timeout=5)
            except (OSError, subprocess.TimeoutExpired):
                proc.kill()
                proc.wait(timeout=5)
            report["frontend_exit"] = proc.returncode
        stderr.close()
        report["stderr"] = (work / "stderr").read_text(errors="replace")
        daemon_pid, _ = mbc.read_pid_file(str(pid))
        mbc.teardown_daemon(str(pid), scratch)
        gone = daemon_pid is None or not mbc.pid_alive(daemon_pid)
        report["teardown"] = "PASS" if gone else "FAIL"
        if not gone:
            report["status"] = "FAIL"
        Path(receipt).write_bytes(canonical(report) + b"\n")
        assert gone, "private daemon did not terminate"


class HelperTests(unittest.TestCase):
    """Synthetic helper checks, never evidence that a native daemon ran."""
    def test_source_lock_path_exists(self):
        path = source_lock_path()
        self.assertEqual(path.parent.name, "crates")
        self.assertTrue(path.is_file(), str(path))
        self.assertEqual(len(digest(path)), 64)

    def test_join_preserves_payload_and_duration(self):
        event = {"id": "a", "kind": "audit", "duration_us": 7, "payload": {"resource": {"request_id": 1}}}
        self.assertEqual(require_joins([{"request_id": 1}], [event])[0]["server_duration_us"], 7)
        self.assertEqual(event, {"id": "a", "kind": "audit", "duration_us": 7, "payload": {"resource": {"request_id": 1}}})

    def test_missing_and_duplicate_rows_refuse(self):
        event = {"id": "a", "kind": "audit", "duration_us": 7, "payload": {"resource": {"request_id": 1}}}
        for rows in ([], [event, {**event, "id": "b"}]):
            with self.assertRaisesRegex(AssertionError, "poll_samples_join_one_audit_row_each"):
                require_joins([{"request_id": 1}], rows)

    def test_frame_rejects_wrong_config_and_failed_result(self):
        good = {"ok": True, "served_config_id": "a", "daemon_protocol_version": mbc.PROTOCOL_VERSION, "request_id": 1, "result": json.dumps({"results": [{"ok": True, "result": {}}]})}
        self.assertEqual(check_frame(good, "a", 1), {})
        for reply in ({**good, "served_config_id": "b"}, {**good, "version_mismatch": True}, {**good, "request_id": True}, {**good, "result": '{"results":[{"ok":false}]}'}):
            with self.assertRaises(AssertionError):
                check_frame(reply, "a", 1)

    def test_pages_require_progress_and_unique_ids(self):
        page = {"events": [{"id": "a"}], "count": 1, "has_more": False, "next_after": None}
        self.assertEqual(read_pages(lambda *_: page, "s", "u"), [{"id": "a"}])
        for bad in ({**page, "has_more": True, "next_after": "repeat"}, {**page, "events": [{"id": "a"}, {"id": "a"}], "count": 2}):
            with self.assertRaises(AssertionError):
                read_pages(lambda *_: bad, "s", "u")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary")
    parser.add_argument("--case", choices=("benchmark-all", "maintenance-reduced"))
    parser.add_argument("--receipt")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        unittest.main(argv=[__file__], exit=False, verbosity=2).result.wasSuccessful() or parser.exit(1)
        return
    if not (args.binary and args.case and args.receipt):
        parser.error("--binary, --case and --receipt are required for live acceptance")
    def expired(_signal, _frame):
        raise TimeoutError("native acceptance exceeded 240 seconds")
    signal.signal(signal.SIGALRM, expired)
    signal.alarm(240)
    try:
        run_live(args.binary, args.case, args.receipt)
    finally:
        signal.alarm(0)


if __name__ == "__main__":
    main()
